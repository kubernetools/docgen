use std::{
    collections::HashMap,
    net::SocketAddr,
    num::NonZeroU32,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing, Router,
};
use clap::Parser;
use governor::{
    clock::{Clock, DefaultClock},
    middleware::StateInformationMiddleware,
    state::keyed::DefaultKeyedStateStore,
    Quota, RateLimiter,
};
use rmcp::{
    model::{
        CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    RoleServer, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tracing::{info, warn};

use docgen::model::{Field, FieldType};
use docgen::parser::ParseResult;
use docgen::{fetcher, parser};

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
struct Cli {
    /// Kubernetes versions to pre-load, comma-separated (e.g. v1.31,v1.32,v1.33)
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "v1.33",
        env = "K8S_VERSIONS"
    )]
    k8s_versions: Vec<String>,
    #[arg(long, env = "GITHUB_TOKEN")]
    token: Option<String>,
    #[arg(long, default_value = "3000")]
    port: u16,
    /// Path to a JSON file mapping API keys to tiers.
    /// Format: [{ "key": "abc123", "tier": "free" }, …]
    /// When omitted, the server runs in anonymous mode: no auth required,
    /// all connections rate-limited per source IP at the free-tier limit.
    #[arg(long, env = "KEY_STORE_PATH")]
    key_store: Option<String>,
    /// Allowed Host header values, comma-separated (e.g. mcp.example.com,mcp.example.com:443).
    /// When omitted, host validation is disabled (suitable for local development only).
    #[arg(long, value_delimiter = ',', env = "ALLOWED_HOSTS")]
    allowed_hosts: Vec<String>,
}

// ── Auth / rate-limit types ───────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
enum Tier {
    Free,
    Paid,
}

#[derive(Deserialize)]
struct KeyEntry {
    key: String,
    tier: String,
}

type KeyStore = Arc<HashMap<String, Tier>>;
type KeyLimiter = Arc<
    RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock, StateInformationMiddleware>,
>;

fn make_limiter(burst: u32, period_secs: u64) -> KeyLimiter {
    let quota = Quota::with_period(Duration::from_secs(period_secs))
        .expect("non-zero period")
        .allow_burst(NonZeroU32::new(burst).expect("non-zero burst"));
    Arc::new(RateLimiter::new(
        quota,
        DefaultKeyedStateStore::default(),
        DefaultClock::default(),
    ))
}

fn mask_key(key: &str) -> String {
    if key.len() <= 4 {
        "****".into()
    } else {
        format!("***{}", &key[key.len() - 4..])
    }
}

fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

// ── Shared state ──────────────────────────────────────────────────────────────

type VersionMap = Arc<HashMap<String, Arc<ParseResult>>>;
type McpService = StreamableHttpService<McpHandler, LocalSessionManager>;

#[derive(Clone)]
struct AppState {
    versions: VersionMap,
    services: Arc<HashMap<String, McpService>>,
    keys: KeyStore,
    free_limiter: KeyLimiter,
    paid_limiter: KeyLimiter,
    auth_limiter: KeyLimiter,
    ready: Arc<AtomicBool>,
    anon_mode: bool,
}

// ── Tool response types ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct ResourceSummary {
    kind: String,
    group: String,
    api_version: String,
    description: String,
}

#[derive(Serialize)]
struct ResourceDetail {
    kind: String,
    group: String,
    api_version: String,
    description: String,
    fields: Vec<FieldDetail>,
    spec_fields: Vec<FieldDetail>,
    status_fields: Vec<FieldDetail>,
    list_fields: Vec<FieldDetail>,
}

#[derive(Serialize)]
struct FieldDetail {
    name: String,
    field_type: String,
    required: bool,
    description: String,
    sub_fields: Vec<FieldDetail>,
    type_ref: Option<String>,
}

#[derive(Serialize)]
struct TypeDetail {
    name: String,
    description: String,
    fields: Vec<FieldDetail>,
}

// ── MCP handler ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct McpHandler {
    parsed: Arc<ParseResult>,
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        let mut impl_info = Implementation::from_build_env();
        impl_info.name = "kubernetools-mcp".to_string();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(impl_info)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![
            make_tool_list_resources(),
            make_tool_get_resource(),
            make_tool_get_type(),
        ]))
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let json_str = match request.name.as_ref() {
            "list_resources" => self.handle_list_resources(&args),
            "get_resource" => self.handle_get_resource(&args),
            "get_type" => self.handle_get_type(&args),
            name => Err(ErrorData::invalid_request(
                format!("unknown tool '{name}'"),
                None,
            )),
        }?;
        let mut result = CallToolResult::default();
        result.content = vec![Content::text(json_str)];
        Ok(result)
    }
}

impl McpHandler {
    fn handle_list_resources(&self, args: &Map<String, Value>) -> Result<String, ErrorData> {
        let filter_group = args.get("group").and_then(|v| v.as_str());
        let filter_version = args.get("api_version").and_then(|v| v.as_str());

        let mut summaries: Vec<ResourceSummary> = self
            .parsed
            .resources
            .iter()
            .filter(|r| {
                filter_group.is_none_or(|g| {
                    if g == "core" {
                        r.group.is_empty()
                    } else {
                        r.group == g
                    }
                })
            })
            .filter(|r| filter_version.is_none_or(|v| r.api_version == v))
            .map(|r| ResourceSummary {
                kind: r.kind.clone(),
                group: display_group(&r.group),
                api_version: r.api_version.clone(),
                description: first_sentence(&r.description),
            })
            .collect();

        summaries.sort_by(|a, b| {
            (&a.group, &a.kind, &a.api_version).cmp(&(&b.group, &b.kind, &b.api_version))
        });

        serde_json::to_string(&summaries).map_err(internal)
    }

    fn handle_get_resource(&self, args: &Map<String, Value>) -> Result<String, ErrorData> {
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("missing required argument 'kind'", None))?;
        let filter_group = args.get("group").and_then(|v| v.as_str());
        let filter_version = args.get("api_version").and_then(|v| v.as_str());

        let candidates: Vec<&docgen::model::Resource> = self
            .parsed
            .resources
            .iter()
            .filter(|r| r.kind == kind)
            .filter(|r| {
                filter_group.is_none_or(|g| {
                    if g == "core" {
                        r.group.is_empty()
                    } else {
                        r.group == g
                    }
                })
            })
            .filter(|r| filter_version.is_none_or(|v| r.api_version == v))
            .collect();

        let resource = if filter_version.is_none() {
            candidates
                .iter()
                .max_by_key(|r| version_rank(&r.api_version))
                .copied()
        } else {
            candidates.first().copied()
        }
        .ok_or_else(|| {
            ErrorData::invalid_params(
                format!(
                    "resource '{kind}' not found; call list_resources to discover available kinds"
                ),
                None,
            )
        })?;

        let detail = ResourceDetail {
            kind: resource.kind.clone(),
            group: display_group(&resource.group),
            api_version: resource.api_version.clone(),
            description: resource.description.clone(),
            fields: to_field_details(&resource.fields, &self.parsed),
            spec_fields: to_field_details(&resource.spec_fields, &self.parsed),
            status_fields: to_field_details(&resource.status_fields, &self.parsed),
            list_fields: to_field_details(&resource.list_fields, &self.parsed),
        };

        serde_json::to_string(&detail).map_err(internal)
    }

    fn handle_get_type(&self, args: &Map<String, Value>) -> Result<String, ErrorData> {
        let type_name = args
            .get("type_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ErrorData::invalid_params("missing required argument 'type_name'", None)
            })?;

        let (description, fields) = self
            .parsed
            .complex_types
            .get(type_name)
            .or_else(|| self.parsed.simple_types.get(type_name))
            .ok_or_else(|| {
                ErrorData::invalid_params(format!("type '{type_name}' not found"), None)
            })?;

        let detail = TypeDetail {
            name: type_name.to_string(),
            description: description.clone(),
            fields: to_field_details(fields, &self.parsed),
        };

        serde_json::to_string(&detail).map_err(internal)
    }
}

// ── Tool descriptors ──────────────────────────────────────────────────────────

fn schema_obj(v: Value) -> Arc<Map<String, Value>> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => Arc::new(Map::new()),
    }
}

fn make_tool_list_resources() -> Tool {
    let mut t = Tool::default();
    t.name = "list_resources".into();
    t.description = Some("Lightweight discovery — one entry per Kubernetes resource. Use this first to discover available kinds.".into());
    t.input_schema = schema_obj(json!({
        "type": "object",
        "properties": {
            "group": {
                "type": "string",
                "description": "Filter by API group, e.g. 'batch' or 'core'. Omit for all groups."
            },
            "api_version": {
                "type": "string",
                "description": "Filter by API version, e.g. 'v1'. Omit for all versions."
            }
        }
    }));
    t
}

fn make_tool_get_resource() -> Tool {
    let mut t = Tool::default();
    t.name = "get_resource".into();
    t.description = Some("Full resource detail — fields, spec, status, and list fields — enough to write a manifest in one call.".into());
    t.input_schema = schema_obj(json!({
        "type": "object",
        "required": ["kind"],
        "properties": {
            "kind": {
                "type": "string",
                "description": "The resource kind, e.g. 'Pod' or 'Deployment'."
            },
            "group": {
                "type": "string",
                "description": "API group, e.g. 'apps' or 'core'. Omit to match any group."
            },
            "api_version": {
                "type": "string",
                "description": "Specific API version, e.g. 'v1'. Omit for the most recent version."
            }
        }
    }));
    t
}

fn make_tool_get_type() -> Tool {
    let mut t = Tool::default();
    t.name = "get_type".into();
    t.description = Some(
        "Drill into a single complex type referenced by get_resource (via type_ref fields).".into(),
    );
    t.input_schema = schema_obj(json!({
        "type": "object",
        "required": ["type_name"],
        "properties": {
            "type_name": {
                "type": "string",
                "description": "Schema short name, e.g. 'Container' or 'PodFailurePolicy'."
            }
        }
    }));
    t
}

// ── Helper functions ──────────────────────────────────────────────────────────

fn display_group(group: &str) -> String {
    if group.is_empty() {
        "core".to_string()
    } else {
        group.to_string()
    }
}

fn first_sentence(s: &str) -> String {
    s.split('.').next().unwrap_or(s).trim().to_string()
}

fn version_rank(v: &str) -> (u32, u32, u32) {
    let s = v.strip_prefix('v').unwrap_or(v);
    if let Some(idx) = s.find("alpha") {
        (
            s[..idx].parse().unwrap_or(0),
            0,
            s[idx + 5..].parse().unwrap_or(0),
        )
    } else if let Some(idx) = s.find("beta") {
        (
            s[..idx].parse().unwrap_or(0),
            1,
            s[idx + 4..].parse().unwrap_or(0),
        )
    } else {
        (s.parse().unwrap_or(0), 2, 0)
    }
}

fn field_type_str(ft: &FieldType) -> String {
    match ft {
        FieldType::Scalar(s) => s.clone(),
        FieldType::Ref(name) => name.clone(),
        FieldType::Array(inner) => format!("[]{}", field_type_str(inner)),
        FieldType::Map(inner) => format!("map[string]{}", field_type_str(inner)),
        FieldType::Object => "object".to_string(),
    }
}

fn leaf_ref(ft: &FieldType) -> Option<&str> {
    match ft {
        FieldType::Ref(name) => Some(name.as_str()),
        FieldType::Array(inner) | FieldType::Map(inner) => leaf_ref(inner),
        _ => None,
    }
}

fn to_field_details(fields: &[Field], parsed: &ParseResult) -> Vec<FieldDetail> {
    fields
        .iter()
        .map(|f| {
            let type_ref = leaf_ref(&f.field_type)
                .filter(|name| parsed.complex_types.contains_key(*name))
                .map(|s| s.to_string());

            let sub_fields = if type_ref.is_none() {
                leaf_ref(&f.field_type)
                    .and_then(|name| parsed.simple_types.get(name))
                    .map(|(_, sub)| to_field_details(sub, parsed))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            FieldDetail {
                name: f.name.clone(),
                field_type: field_type_str(&f.field_type),
                required: f.required,
                description: f.description.clone(),
                sub_fields,
                type_ref,
            }
        })
        .collect()
}

fn internal(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

fn query_version(query: &str) -> Option<&str> {
    query.split('&').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k == "version").then_some(v)
    })
}

// ── Axum middleware ───────────────────────────────────────────────────────────

async fn authenticate(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    if state.anon_mode {
        req.extensions_mut().insert(Tier::Free);
        return next.run(req).await;
    }

    let ip = addr.ip().to_string();
    if state.auth_limiter.check_key(&ip).is_err() {
        warn!(ip = %ip, "auth rate limited");
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }

    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match token.and_then(|t| state.keys.get(&hash_key(t))) {
        Some(tier) => {
            req.extensions_mut().insert(tier.clone());
            next.run(req).await
        }
        None => {
            let masked = token.map(mask_key).unwrap_or_else(|| "<no key>".into());
            warn!(key = %masked, "unauthorized");
            StatusCode::UNAUTHORIZED.into_response()
        }
    }
}

async fn rate_limit(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let tier = req
        .extensions()
        .get::<Tier>()
        .cloned()
        .unwrap_or(Tier::Free);

    let (rate_key, identity) = if state.anon_mode {
        let ip = addr.ip().to_string();
        (ip.clone(), ip)
    } else {
        let raw_key = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_string();
        let masked = mask_key(&raw_key);
        (raw_key, masked)
    };

    let version = req
        .uri()
        .query()
        .and_then(|q| query_version(q))
        .unwrap_or("-");
    let tier_label = match tier {
        Tier::Free => "free",
        Tier::Paid => "paid",
    };

    let limiter = match tier {
        Tier::Free => &state.free_limiter,
        Tier::Paid => &state.paid_limiter,
    };

    match limiter.check_key(&rate_key) {
        Ok(state_snapshot) => {
            info!(
                identity = %identity,
                version,
                tier = tier_label,
                burst_remaining = state_snapshot.remaining_burst_capacity(),
                "request allowed"
            );
            next.run(req).await
        }
        Err(not_until) => {
            let retry_after = not_until.wait_time_from(DefaultClock::default().now());
            warn!(
                identity = %identity,
                version,
                tier = tier_label,
                retry_after_secs = retry_after.as_secs_f32(),
                "rate limited"
            );
            StatusCode::TOO_MANY_REQUESTS.into_response()
        }
    }
}

// ── Health probe ─────────────────────────────────────────────────────────────

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    if state.ready.load(Ordering::Relaxed) {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "loading")
    }
}

// ── Version routing ───────────────────────────────────────────────────────────

async fn mcp_handler(State(state): State<AppState>, req: Request<Body>) -> impl IntoResponse {
    let version_param = req
        .uri()
        .query()
        .and_then(|q| query_version(q))
        .map(str::to_owned);

    let version = version_param
        .as_deref()
        .or_else(|| state.versions.keys().next().map(String::as_str));

    let Some(version) = version else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let Some(service) = state.services.get(version) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    service.handle(req).await.into_response()
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Load API key store, or run in anonymous IP-limited mode if omitted
    let (keys, anon_mode) = if let Some(path) = &cli.key_store {
        let key_json = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read key store '{}': {e}", path))?;
        let entries: Vec<KeyEntry> = serde_json::from_str(&key_json)
            .map_err(|e| anyhow::anyhow!("invalid key store JSON: {e}"))?;
        let keys: HashMap<String, Tier> = entries
            .into_iter()
            .map(|e| {
                let tier = if e.tier == "paid" {
                    Tier::Paid
                } else {
                    Tier::Free
                };
                (hash_key(&e.key), tier)
            })
            .collect();
        (Arc::new(keys), false)
    } else {
        info!("no key store provided — anonymous mode, rate limiting per source IP");
        (Arc::new(HashMap::new()), true)
    };

    let ready = Arc::new(AtomicBool::new(false));

    // Fetch and parse all requested k8s versions concurrently
    let mut handles = Vec::new();
    for version in cli.k8s_versions.clone() {
        let token = cli.token.clone();
        handles.push(tokio::spawn(async move {
            info!(version, "fetching specs");
            let specs = fetcher::fetch_specs(&version, token.as_deref()).await?;
            info!(version, files = specs.len(), "parsing specs");
            let parsed = parser::parse_specs(specs, &version)?;
            info!(
                version,
                resources = parsed.resources.len(),
                common_defs = parsed.common_defs.len(),
                "ready"
            );
            anyhow::Ok((version, Arc::new(parsed)))
        }));
    }

    let mut version_map: HashMap<String, Arc<ParseResult>> = HashMap::new();
    for h in handles {
        let (version, parsed) = h.await??;
        version_map.insert(version, parsed);
    }
    let versions: VersionMap = Arc::new(version_map);
    ready.store(true, Ordering::Relaxed);

    // Build one StreamableHttpService per k8s version
    // Default allows localhost/127.0.0.1/::1 only; --allowed-hosts overrides for production.
    let mcp_config = if cli.allowed_hosts.is_empty() {
        StreamableHttpServerConfig::default()
    } else {
        StreamableHttpServerConfig::default().with_allowed_hosts(cli.allowed_hosts.clone())
    };
    let mut service_map: HashMap<String, McpService> = HashMap::new();
    for (version, parsed) in versions.iter() {
        let p = parsed.clone();
        let service = StreamableHttpService::new(
            move || Ok(McpHandler { parsed: p.clone() }),
            Default::default(),
            mcp_config.clone(),
        );
        service_map.insert(version.clone(), service);
    }

    // Rate limiters: free = 10 req/min (1 token per 6 s, burst 10)
    //                paid = effectively unlimited (100 req/s, burst 1000)
    //                auth = 20 req/min per source IP (burst 20, 1 token per 3 s)
    let free_limiter = make_limiter(10, 6);
    let paid_limiter = make_limiter(1000, 1);
    let auth_limiter = make_limiter(20, 3);

    let state = AppState {
        versions,
        services: Arc::new(service_map),
        keys,
        free_limiter,
        paid_limiter,
        auth_limiter,
        ready,
        anon_mode,
    };

    let protected = Router::new()
        .route("/mcp", routing::any(mcp_handler))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate));

    let app = Router::new()
        .route("/healthz", routing::get(healthz))
        .merge(protected)
        .with_state(state);

    let listener = TcpListener::bind(("0.0.0.0", cli.port)).await?;
    info!(port = cli.port, "listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use docgen::parser::ParseResult;
    use std::collections::HashMap;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn scalar(name: &str) -> Field {
        Field {
            name: name.into(),
            description: format!("{name} description"),
            required: false,
            field_type: FieldType::Scalar("string".into()),
        }
    }

    fn ref_field(name: &str, type_name: &str) -> Field {
        Field {
            name: name.into(),
            description: format!("{name} description"),
            required: false,
            field_type: FieldType::Ref(type_name.into()),
        }
    }

    fn resource(kind: &str, group: &str, api_version: &str) -> docgen::model::Resource {
        docgen::model::Resource {
            kind: kind.into(),
            group: group.into(),
            api_version: api_version.into(),
            k8s_version: "v1.33".into(),
            description: format!("{kind} description."),
            fields: vec![scalar("metadata")],
            list_description: String::new(),
            list_fields: vec![],
            spec_name: String::new(),
            spec_description: String::new(),
            spec_fields: vec![],
            status_name: String::new(),
            status_description: String::new(),
            status_fields: vec![],
        }
    }

    fn empty_parsed() -> Arc<ParseResult> {
        Arc::new(ParseResult {
            resources: vec![],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        })
    }

    fn handler_with(parsed: ParseResult) -> McpHandler {
        McpHandler {
            parsed: Arc::new(parsed),
        }
    }

    fn args(pairs: &[(&str, &str)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect()
    }

    // ── mask_key ─────────────────────────────────────────────────────────────

    #[test]
    fn mask_key_short() {
        assert_eq!(mask_key("abc"), "****");
        assert_eq!(mask_key(""), "****");
        assert_eq!(mask_key("1234"), "****");
    }

    #[test]
    fn mask_key_long() {
        assert_eq!(mask_key("abcde"), "***bcde");
        assert_eq!(mask_key("mysecretkey"), "***tkey");
    }

    // ── hash_key ─────────────────────────────────────────────────────────────

    #[test]
    fn hash_key_known_digest() {
        // SHA-256("mykey") verified externally
        assert_eq!(
            hash_key("mykey"),
            "5e50f405ace6cbdf17379f4b9f2b0c9f4144c5e380ea0b9298cb02ebd8ffe511"
        );
    }

    #[test]
    fn hash_key_different_inputs_differ() {
        assert_ne!(hash_key("key1"), hash_key("key2"));
    }

    #[test]
    fn hash_key_lookup_matches() {
        let raw = "secret-api-key";
        let mut store: HashMap<String, Tier> = HashMap::new();
        store.insert(hash_key(raw), Tier::Free);

        assert_eq!(store.get(&hash_key(raw)), Some(&Tier::Free));
        assert_eq!(store.get(&hash_key("wrong-key")), None);
    }

    // ── healthz ───────────────────────────────────────────────────────────────

    fn state_with_ready(ready: bool) -> AppState {
        let flag = Arc::new(AtomicBool::new(ready));
        AppState {
            versions: Arc::new(HashMap::new()),
            services: Arc::new(HashMap::new()),
            keys: Arc::new(HashMap::new()),
            free_limiter: make_limiter(10, 6),
            paid_limiter: make_limiter(1000, 1),
            auth_limiter: make_limiter(20, 3),
            ready: flag,
            anon_mode: false,
        }
    }

    #[tokio::test]
    async fn healthz_returns_ok_when_ready() {
        let state = State(state_with_ready(true));
        let resp = healthz(state).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_returns_503_when_not_ready() {
        let state = State(state_with_ready(false));
        let resp = healthz(state).await.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── display_group ─────────────────────────────────────────────────────────

    #[test]
    fn display_group_empty_is_core() {
        assert_eq!(display_group(""), "core");
    }

    #[test]
    fn display_group_named() {
        assert_eq!(display_group("apps"), "apps");
        assert_eq!(display_group("networking.k8s.io"), "networking.k8s.io");
    }

    // ── first_sentence ───────────────────────────────────────────────────────

    #[test]
    fn first_sentence_single() {
        assert_eq!(
            first_sentence("A pod runs containers"),
            "A pod runs containers"
        );
    }

    #[test]
    fn first_sentence_multiple() {
        assert_eq!(
            first_sentence("A pod runs containers. It has a spec."),
            "A pod runs containers"
        );
    }

    #[test]
    fn first_sentence_trims_whitespace() {
        assert_eq!(first_sentence("  Hello world.  More text."), "Hello world");
    }

    // ── query_version ────────────────────────────────────────────────────────

    #[test]
    fn query_version_found() {
        assert_eq!(query_version("version=v1.33"), Some("v1.33"));
    }

    #[test]
    fn query_version_among_others() {
        assert_eq!(
            query_version("foo=bar&version=v1.31&baz=qux"),
            Some("v1.31")
        );
    }

    #[test]
    fn query_version_missing() {
        assert_eq!(query_version("foo=bar&baz=qux"), None);
    }

    #[test]
    fn query_version_empty() {
        assert_eq!(query_version(""), None);
    }

    // ── field_type_str ───────────────────────────────────────────────────────

    #[test]
    fn field_type_str_scalar() {
        assert_eq!(
            field_type_str(&FieldType::Scalar("string".into())),
            "string"
        );
        assert_eq!(
            field_type_str(&FieldType::Scalar("integer".into())),
            "integer"
        );
    }

    #[test]
    fn field_type_str_ref() {
        assert_eq!(
            field_type_str(&FieldType::Ref("Container".into())),
            "Container"
        );
    }

    #[test]
    fn field_type_str_array() {
        let ft = FieldType::Array(Box::new(FieldType::Ref("Container".into())));
        assert_eq!(field_type_str(&ft), "[]Container");
    }

    #[test]
    fn field_type_str_map() {
        let ft = FieldType::Map(Box::new(FieldType::Scalar("string".into())));
        assert_eq!(field_type_str(&ft), "map[string]string");
    }

    #[test]
    fn field_type_str_nested() {
        let ft = FieldType::Array(Box::new(FieldType::Map(Box::new(FieldType::Ref(
            "Quantity".into(),
        )))));
        assert_eq!(field_type_str(&ft), "[]map[string]Quantity");
    }

    #[test]
    fn field_type_str_object() {
        assert_eq!(field_type_str(&FieldType::Object), "object");
    }

    // ── leaf_ref ─────────────────────────────────────────────────────────────

    #[test]
    fn leaf_ref_scalar_is_none() {
        assert_eq!(leaf_ref(&FieldType::Scalar("string".into())), None);
    }

    #[test]
    fn leaf_ref_ref_returns_name() {
        assert_eq!(
            leaf_ref(&FieldType::Ref("Container".into())),
            Some("Container")
        );
    }

    #[test]
    fn leaf_ref_through_array() {
        let ft = FieldType::Array(Box::new(FieldType::Ref("Volume".into())));
        assert_eq!(leaf_ref(&ft), Some("Volume"));
    }

    #[test]
    fn leaf_ref_through_map() {
        let ft = FieldType::Map(Box::new(FieldType::Ref("Quantity".into())));
        assert_eq!(leaf_ref(&ft), Some("Quantity"));
    }

    #[test]
    fn leaf_ref_object_is_none() {
        assert_eq!(leaf_ref(&FieldType::Object), None);
    }

    // ── version_rank ─────────────────────────────────────────────────────────

    #[test]
    fn version_rank_stable_beats_beta() {
        assert!(version_rank("v1") > version_rank("v1beta1"));
    }

    #[test]
    fn version_rank_beta_beats_alpha() {
        assert!(version_rank("v1beta1") > version_rank("v1alpha1"));
    }

    #[test]
    fn version_rank_higher_major() {
        assert!(version_rank("v2") > version_rank("v1"));
    }

    #[test]
    fn version_rank_higher_prerelease_number() {
        assert!(version_rank("v1beta2") > version_rank("v1beta1"));
        assert!(version_rank("v1alpha2") > version_rank("v1alpha1"));
    }

    // ── to_field_details ─────────────────────────────────────────────────────

    #[test]
    fn to_field_details_scalar() {
        let fields = vec![scalar("name")];
        let details = to_field_details(&fields, &empty_parsed());
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].name, "name");
        assert_eq!(details[0].field_type, "string");
        assert!(!details[0].required);
        assert!(details[0].type_ref.is_none());
        assert!(details[0].sub_fields.is_empty());
    }

    #[test]
    fn to_field_details_complex_type_ref() {
        let fields = vec![ref_field("spec", "PodSpec")];
        let mut complex_types = HashMap::new();
        complex_types.insert(
            "PodSpec".into(),
            ("PodSpec desc".into(), vec![scalar("nodeName")]),
        );
        let parsed = Arc::new(ParseResult {
            resources: vec![],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types,
        });
        let details = to_field_details(&fields, &parsed);
        assert_eq!(details[0].type_ref.as_deref(), Some("PodSpec"));
        assert!(details[0].sub_fields.is_empty());
    }

    #[test]
    fn to_field_details_simple_type_inlined() {
        let fields = vec![ref_field("tolerations", "Toleration")];
        let mut simple_types = HashMap::new();
        simple_types.insert(
            "Toleration".into(),
            (
                "Toleration desc".into(),
                vec![scalar("key"), scalar("operator")],
            ),
        );
        let parsed = Arc::new(ParseResult {
            resources: vec![],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types,
            complex_types: HashMap::new(),
        });
        let details = to_field_details(&fields, &parsed);
        assert!(details[0].type_ref.is_none());
        assert_eq!(details[0].sub_fields.len(), 2);
        assert_eq!(details[0].sub_fields[0].name, "key");
    }

    #[test]
    fn to_field_details_array_of_complex() {
        let fields = vec![Field {
            name: "containers".into(),
            description: "containers".into(),
            required: true,
            field_type: FieldType::Array(Box::new(FieldType::Ref("Container".into()))),
        }];
        let mut complex_types = HashMap::new();
        complex_types.insert("Container".into(), ("Container desc".into(), vec![]));
        let parsed = Arc::new(ParseResult {
            resources: vec![],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types,
        });
        let details = to_field_details(&fields, &parsed);
        assert_eq!(details[0].field_type, "[]Container");
        assert_eq!(details[0].type_ref.as_deref(), Some("Container"));
        assert!(details[0].required);
    }

    // ── handle_list_resources ────────────────────────────────────────────────

    #[test]
    fn list_resources_no_filter() {
        let h = handler_with(ParseResult {
            resources: vec![
                resource("Pod", "", "v1"),
                resource("Deployment", "apps", "v1"),
            ],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let result = h.handle_list_resources(&args(&[])).unwrap();
        let summaries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(summaries.len(), 2);
    }

    #[test]
    fn list_resources_filter_group_core() {
        let h = handler_with(ParseResult {
            resources: vec![
                resource("Pod", "", "v1"),
                resource("Deployment", "apps", "v1"),
            ],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let result = h
            .handle_list_resources(&args(&[("group", "core")]))
            .unwrap();
        let summaries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["kind"], "Pod");
    }

    #[test]
    fn list_resources_filter_group_named() {
        let h = handler_with(ParseResult {
            resources: vec![
                resource("Pod", "", "v1"),
                resource("Deployment", "apps", "v1"),
            ],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let result = h
            .handle_list_resources(&args(&[("group", "apps")]))
            .unwrap();
        let summaries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["kind"], "Deployment");
    }

    #[test]
    fn list_resources_filter_api_version() {
        let h = handler_with(ParseResult {
            resources: vec![
                resource("CronJob", "batch", "v1"),
                resource("CronJob", "batch", "v1beta1"),
            ],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let result = h
            .handle_list_resources(&args(&[("api_version", "v1beta1")]))
            .unwrap();
        let summaries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["api_version"], "v1beta1");
    }

    #[test]
    fn list_resources_sorted() {
        let h = handler_with(ParseResult {
            resources: vec![
                resource("Pod", "", "v1"),
                resource("Deployment", "apps", "v1"),
            ],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let result = h.handle_list_resources(&args(&[])).unwrap();
        let summaries: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        // "apps/Deployment" sorts before "core/Pod"
        assert_eq!(summaries[0]["kind"], "Deployment");
        assert_eq!(summaries[1]["kind"], "Pod");
    }

    // ── handle_get_resource ──────────────────────────────────────────────────

    #[test]
    fn get_resource_found_by_kind() {
        let h = handler_with(ParseResult {
            resources: vec![resource("Pod", "", "v1")],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let result = h.handle_get_resource(&args(&[("kind", "Pod")])).unwrap();
        let detail: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(detail["kind"], "Pod");
        assert_eq!(detail["group"], "core");
        assert_eq!(detail["api_version"], "v1");
    }

    #[test]
    fn get_resource_not_found() {
        let h = handler_with(ParseResult {
            resources: vec![resource("Pod", "", "v1")],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let err = h
            .handle_get_resource(&args(&[("kind", "Deployment")]))
            .unwrap_err();
        assert!(err.message.contains("Deployment"));
    }

    #[test]
    fn get_resource_missing_kind_arg() {
        let h = handler_with(ParseResult {
            resources: vec![],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        assert!(h.handle_get_resource(&args(&[])).is_err());
    }

    #[test]
    fn get_resource_picks_latest_version_when_unspecified() {
        let h = handler_with(ParseResult {
            resources: vec![
                resource("CronJob", "batch", "v1beta1"),
                resource("CronJob", "batch", "v1"),
            ],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let result = h
            .handle_get_resource(&args(&[("kind", "CronJob")]))
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(detail["api_version"], "v1");
    }

    #[test]
    fn get_resource_specific_version() {
        let h = handler_with(ParseResult {
            resources: vec![
                resource("CronJob", "batch", "v1beta1"),
                resource("CronJob", "batch", "v1"),
            ],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let result = h
            .handle_get_resource(&args(&[("kind", "CronJob"), ("api_version", "v1beta1")]))
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(detail["api_version"], "v1beta1");
    }

    #[test]
    fn get_resource_filter_by_group() {
        let h = handler_with(ParseResult {
            resources: vec![
                resource("Event", "", "v1"),
                resource("Event", "events.k8s.io", "v1"),
            ],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        let result = h
            .handle_get_resource(&args(&[("kind", "Event"), ("group", "core")]))
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(detail["group"], "core");
    }

    // ── handle_get_type ──────────────────────────────────────────────────────

    #[test]
    fn get_type_found_in_complex() {
        let mut complex_types = HashMap::new();
        complex_types.insert(
            "Container".into(),
            ("A container.".into(), vec![scalar("image"), scalar("name")]),
        );
        let h = handler_with(ParseResult {
            resources: vec![],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types,
        });
        let result = h
            .handle_get_type(&args(&[("type_name", "Container")]))
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(detail["name"], "Container");
        assert_eq!(detail["description"], "A container.");
        assert_eq!(detail["fields"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn get_type_found_in_simple() {
        let mut simple_types = HashMap::new();
        simple_types.insert(
            "Toleration".into(),
            ("A toleration.".into(), vec![scalar("key")]),
        );
        let h = handler_with(ParseResult {
            resources: vec![],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types,
            complex_types: HashMap::new(),
        });
        let result = h
            .handle_get_type(&args(&[("type_name", "Toleration")]))
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(detail["name"], "Toleration");
    }

    #[test]
    fn get_type_not_found() {
        let h = handler_with(ParseResult {
            resources: vec![],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        assert!(h
            .handle_get_type(&args(&[("type_name", "Unknown")]))
            .is_err());
    }

    #[test]
    fn get_type_missing_arg() {
        let h = handler_with(ParseResult {
            resources: vec![],
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        });
        assert!(h.handle_get_type(&args(&[])).is_err());
    }
}

// ── Middleware tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod middleware_tests {
    use super::*;
    use axum::{body::Body, extract::ConnectInfo, http::Request, routing, Router};
    use tower::ServiceExt;

    fn anon_state() -> AppState {
        AppState {
            versions: Arc::new(HashMap::new()),
            services: Arc::new(HashMap::new()),
            keys: Arc::new(HashMap::new()),
            free_limiter: make_limiter(10, 6),
            paid_limiter: make_limiter(1000, 1),
            auth_limiter: make_limiter(20, 3),
            ready: Arc::new(AtomicBool::new(true)),
            anon_mode: true,
        }
    }

    fn auth_state(keys: HashMap<String, Tier>) -> AppState {
        AppState {
            versions: Arc::new(HashMap::new()),
            services: Arc::new(HashMap::new()),
            keys: Arc::new(keys),
            free_limiter: make_limiter(10, 6),
            paid_limiter: make_limiter(1000, 1),
            auth_limiter: make_limiter(20, 3),
            ready: Arc::new(AtomicBool::new(true)),
            anon_mode: false,
        }
    }

    fn req_from(ip: [u8; 4]) -> Request<Body> {
        let mut r = Request::builder().uri("/mcp").body(Body::empty()).unwrap();
        r.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((ip, 1234))));
        r
    }

    fn req_with_bearer(ip: [u8; 4], token: &str) -> Request<Body> {
        let mut r = Request::builder()
            .uri("/mcp")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        r.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((ip, 1234))));
        r
    }

    fn auth_router(state: AppState) -> Router {
        Router::new()
            .route("/mcp", routing::get(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(state.clone(), authenticate))
            .with_state(state)
    }

    fn rate_router(state: AppState) -> Router {
        // Pre-inject Tier::Free so rate_limit doesn't need authenticate upstream
        Router::new()
            .route(
                "/mcp",
                routing::get(|| async { StatusCode::OK }).layer(middleware::from_fn(
                    |mut req: Request<Body>, next: Next| async move {
                        req.extensions_mut().insert(Tier::Free);
                        next.run(req).await
                    },
                )),
            )
            .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
            .with_state(state)
    }

    // ── authenticate: anon mode ───────────────────────────────────────────────

    #[tokio::test]
    async fn authenticate_anon_no_token_allowed() {
        let app = auth_router(anon_state());
        let resp = app.oneshot(req_from([127, 0, 0, 1])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn authenticate_anon_bad_token_allowed() {
        let app = auth_router(anon_state());
        let resp = app
            .oneshot(req_with_bearer([127, 0, 0, 1], "garbage"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── authenticate: auth mode ───────────────────────────────────────────────

    #[tokio::test]
    async fn authenticate_auth_no_token_rejected() {
        let app = auth_router(auth_state(HashMap::new()));
        let resp = app.oneshot(req_from([127, 0, 0, 1])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticate_auth_bad_token_rejected() {
        let app = auth_router(auth_state(HashMap::new()));
        let resp = app
            .oneshot(req_with_bearer([127, 0, 0, 1], "bad-key"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticate_auth_valid_token_allowed() {
        let raw = "my-secret-key";
        let mut keys = HashMap::new();
        keys.insert(hash_key(raw), Tier::Free);
        let app = auth_router(auth_state(keys));
        let resp = app
            .oneshot(req_with_bearer([127, 0, 0, 1], raw))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── rate_limit: anon mode (keyed by IP) ───────────────────────────────────

    #[tokio::test]
    async fn rate_limit_anon_same_ip_exhausted() {
        // burst=2 so the third request from the same IP is rejected
        let state = AppState {
            free_limiter: make_limiter(2, 60),
            ..anon_state()
        };

        for expected in [
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let resp = rate_router(state.clone())
                .oneshot(req_from([10, 0, 0, 1]))
                .await
                .unwrap();
            assert_eq!(resp.status(), expected);
        }
    }

    #[tokio::test]
    async fn rate_limit_anon_different_ips_independent() {
        // burst=1: IP A exhausts its bucket, IP B still gets its first request
        let state = AppState {
            free_limiter: make_limiter(1, 60),
            ..anon_state()
        };

        let ok = rate_router(state.clone())
            .oneshot(req_from([10, 0, 0, 1]))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);

        let exhausted = rate_router(state.clone())
            .oneshot(req_from([10, 0, 0, 1]))
            .await
            .unwrap();
        assert_eq!(exhausted.status(), StatusCode::TOO_MANY_REQUESTS);

        let different_ip = rate_router(state)
            .oneshot(req_from([10, 0, 0, 2]))
            .await
            .unwrap();
        assert_eq!(different_ip.status(), StatusCode::OK);
    }

    // ── rate_limit: auth mode (keyed by token, not IP) ───────────────────────

    #[tokio::test]
    async fn rate_limit_auth_same_key_different_ips_share_bucket() {
        // burst=1: two requests with the same Bearer token from different IPs
        // share a bucket — the second is rejected even though it came from a new IP
        let raw = "shared-key";
        let mut keys = HashMap::new();
        keys.insert(hash_key(raw), Tier::Free);
        let state = AppState {
            free_limiter: make_limiter(1, 60),
            ..auth_state(keys)
        };

        let ok = rate_router(state.clone())
            .oneshot(req_with_bearer([10, 0, 0, 1], raw))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);

        let rejected = rate_router(state)
            .oneshot(req_with_bearer([10, 0, 0, 2], raw))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
