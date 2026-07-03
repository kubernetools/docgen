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
    http::{header, request::Parts as HttpParts, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
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
        CallToolResult, Content, ErrorCode, ErrorData, Implementation, ListToolsResult,
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
    /// URL to redirect non-MCP browser requests to (e.g. https://www.kubernetools.com).
    /// When omitted, browser GETs receive 400 Bad Request.
    #[arg(long, env = "BROWSER_REDIRECT_URL")]
    browser_redirect: Option<String>,
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

/// Rate-limit identity injected into HTTP request extensions by `inject_rate_identity`
/// and read from `call_tool` via `RequestContext → http::request::Parts`.
#[derive(Clone)]
struct RateIdentity {
    key: String,
    display: String,
    tier: Tier,
}

/// Check the rate limiter for the given identity.
/// Returns `Ok(burst_remaining)` when allowed, `Err(retry_after_secs)` when limited.
fn check_rate_limit(identity: &RateIdentity, state: &AppState) -> Result<u32, u64> {
    let limiter = match identity.tier {
        Tier::Free => &state.free_limiter,
        Tier::Paid => &state.paid_limiter,
    };
    match limiter.check_key(&identity.key) {
        Ok(snapshot) => Ok(snapshot.remaining_burst_capacity()),
        Err(not_until) => {
            let retry = not_until.wait_time_from(DefaultClock::default().now());
            Err(retry.as_secs().max(1))
        }
    }
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
    browser_redirect: Option<String>,
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

/// Structural field-level differences between two versions of a schema.
/// Entries are human-readable: "spec.hostUsers (boolean)", "spec.foo: type string -> integer".
/// Description-only changes are ignored.
#[derive(Serialize, Default, PartialEq, Debug)]
struct StructDiff {
    added_fields: Vec<String>,
    removed_fields: Vec<String>,
    changed_fields: Vec<String>,
}

impl StructDiff {
    fn is_empty(&self) -> bool {
        self.added_fields.is_empty()
            && self.removed_fields.is_empty()
            && self.changed_fields.is_empty()
    }
}

#[derive(Serialize)]
struct ResourceDiff {
    kind: String,
    group: String,
    api_version: String,
    #[serde(flatten)]
    diff: StructDiff,
}

#[derive(Serialize)]
struct TypeDiff {
    name: String,
    #[serde(flatten)]
    diff: StructDiff,
}

#[derive(Serialize)]
struct VersionDiff {
    from_k8s_version: String,
    to_k8s_version: String,
    added_resources: Vec<ResourceSummary>,
    removed_resources: Vec<ResourceSummary>,
    changed_resources: Vec<ResourceDiff>,
    added_types: Vec<String>,
    removed_types: Vec<String>,
    changed_types: Vec<TypeDiff>,
}

#[derive(Serialize)]
struct ResourceVersionsDiff {
    kind: String,
    group: String,
    from_api_version: String,
    to_api_version: String,
    #[serde(flatten)]
    diff: StructDiff,
}

// ── MCP handler ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct McpHandler {
    parsed: Arc<ParseResult>,
    /// The k8s version this instance serves (e.g. "v1.36").
    version: String,
    /// The closest older loaded k8s version, if any.
    prev: Option<(String, Arc<ParseResult>)>,
    /// The closest newer loaded k8s version, if any.
    next: Option<(String, Arc<ParseResult>)>,
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
        let mut tools = vec![
            make_tool_list_resources(),
            make_tool_get_resource(),
            make_tool_get_type(),
            make_tool_compare_resource_versions(&self.version),
        ];
        if let Some((prev, _)) = &self.prev {
            tools.push(make_tool_diff_previous_version(&self.version, prev));
        }
        if let Some((next, _)) = &self.next {
            tools.push(make_tool_diff_next_version(&self.version, next));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(parts) = context.extensions.get::<HttpParts>() {
            if let (Some(app_state), Some(identity)) = (
                parts.extensions.get::<AppState>(),
                parts.extensions.get::<RateIdentity>(),
            ) {
                match check_rate_limit(identity, app_state) {
                    Ok(burst_remaining) => {
                        info!(
                            identity = %identity.display,
                            burst_remaining,
                            "request allowed"
                        );
                    }
                    Err(retry_secs) => {
                        warn!(
                            identity = %identity.display,
                            retry_after_secs = retry_secs,
                            "rate limited"
                        );
                        return Err(ErrorData::new(
                            ErrorCode(-32029),
                            "rate limited",
                            Some(json!({ "retry_after_secs": retry_secs })),
                        ));
                    }
                }
            }
        }
        let args = request.arguments.unwrap_or_default();
        let json_str = match request.name.as_ref() {
            "list_resources" => self.handle_list_resources(&args),
            "get_resource" => self.handle_get_resource(&args),
            "get_type" => self.handle_get_type(&args),
            "diff_previous_version" => self.handle_diff_previous_version(),
            "diff_next_version" => self.handle_diff_next_version(),
            "compare_resource_versions" => self.handle_compare_resource_versions(&args),
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

    fn handle_diff_previous_version(&self) -> Result<String, ErrorData> {
        let (prev_version, prev) = self.prev.as_ref().ok_or_else(|| {
            ErrorData::invalid_request(
                format!(
                    "no Kubernetes version older than {} is loaded on this server",
                    self.version
                ),
                None,
            )
        })?;
        let diff = version_diff(prev_version, prev, &self.version, &self.parsed);
        serde_json::to_string(&diff).map_err(internal)
    }

    fn handle_diff_next_version(&self) -> Result<String, ErrorData> {
        let (next_version, next) = self.next.as_ref().ok_or_else(|| {
            ErrorData::invalid_request(
                format!(
                    "no Kubernetes version newer than {} is loaded on this server",
                    self.version
                ),
                None,
            )
        })?;
        let diff = version_diff(&self.version, &self.parsed, next_version, next);
        serde_json::to_string(&diff).map_err(internal)
    }

    fn handle_compare_resource_versions(
        &self,
        args: &Map<String, Value>,
    ) -> Result<String, ErrorData> {
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ErrorData::invalid_params("missing required argument 'kind'", None))?;
        let from_version = args
            .get("from_api_version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ErrorData::invalid_params("missing required argument 'from_api_version'", None)
            })?;
        let to_version = args
            .get("to_api_version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ErrorData::invalid_params("missing required argument 'to_api_version'", None)
            })?;
        let filter_group = args.get("group").and_then(|v| v.as_str());

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
            .collect();

        if candidates.is_empty() {
            return Err(ErrorData::invalid_params(
                format!(
                    "resource '{kind}' not found; call list_resources to discover available kinds"
                ),
                None,
            ));
        }

        let groups: std::collections::HashSet<&str> =
            candidates.iter().map(|r| r.group.as_str()).collect();
        if groups.len() > 1 {
            let mut names: Vec<String> = groups.iter().map(|g| display_group(g)).collect();
            names.sort();
            return Err(ErrorData::invalid_params(
                format!(
                    "kind '{kind}' exists in multiple groups ({}); specify 'group'",
                    names.join(", ")
                ),
                None,
            ));
        }

        let find = |version: &str| {
            candidates
                .iter()
                .find(|r| r.api_version == version)
                .copied()
        };
        let not_found = |version: &str| {
            let mut available: Vec<&str> =
                candidates.iter().map(|r| r.api_version.as_str()).collect();
            available.sort();
            ErrorData::invalid_params(
                format!(
                    "'{kind}' has no API version '{version}'; available: {}",
                    available.join(", ")
                ),
                None,
            )
        };
        let from = find(from_version).ok_or_else(|| not_found(from_version))?;
        let to = find(to_version).ok_or_else(|| not_found(to_version))?;

        let result = ResourceVersionsDiff {
            kind: kind.to_string(),
            group: display_group(&to.group),
            from_api_version: from_version.to_string(),
            to_api_version: to_version.to_string(),
            diff: resource_struct_diff(from, to),
        };

        serde_json::to_string(&result).map_err(internal)
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

fn make_tool_diff_previous_version(current: &str, prev: &str) -> Tool {
    let mut t = Tool::default();
    t.name = "diff_previous_version".into();
    t.description = Some(
        format!(
            "Changes introduced in Kubernetes {current} (this instance's version) since {prev}: \
         resources and types added, removed, or structurally changed."
        )
        .into(),
    );
    t.input_schema = schema_obj(json!({ "type": "object", "properties": {} }));
    t
}

fn make_tool_diff_next_version(current: &str, next: &str) -> Tool {
    let mut t = Tool::default();
    t.name = "diff_next_version".into();
    t.description = Some(
        format!(
            "Changes the next Kubernetes version ({next}) introduces relative to {current} \
         (this instance's version): resources and types added, removed, or structurally changed."
        )
        .into(),
    );
    t.input_schema = schema_obj(json!({ "type": "object", "properties": {} }));
    t
}

fn make_tool_compare_resource_versions(current: &str) -> Tool {
    let mut t = Tool::default();
    t.name = "compare_resource_versions".into();
    t.description = Some(
        format!(
            "Compare a resource kind between two of its API versions within Kubernetes {current} \
         (e.g. v1beta1 vs v1): fields added, removed, or changed."
        )
        .into(),
    );
    t.input_schema = schema_obj(json!({
        "type": "object",
        "required": ["kind", "from_api_version", "to_api_version"],
        "properties": {
            "kind": {
                "type": "string",
                "description": "The resource kind, e.g. 'VolumeAttributesClass'."
            },
            "group": {
                "type": "string",
                "description": "API group, e.g. 'storage.k8s.io' or 'core'. Required only when the kind exists in several groups."
            },
            "from_api_version": {
                "type": "string",
                "description": "Baseline API version, e.g. 'v1beta1'."
            },
            "to_api_version": {
                "type": "string",
                "description": "API version to compare against the baseline, e.g. 'v1'."
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

/// Ranks a Kubernetes release version string ("v1.33") for chronological ordering.
fn k8s_version_rank(v: &str) -> (u32, u32) {
    let s = v.strip_prefix('v').unwrap_or(v);
    let mut parts = s.splitn(2, '.');
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor)
}

/// Compare two field lists by name, recording structural differences into `out`.
/// `prefix` namespaces the field paths (e.g. "spec."). Descriptions are ignored.
fn diff_fields(prefix: &str, old: &[Field], new: &[Field], out: &mut StructDiff) {
    let old_map: HashMap<&str, &Field> = old.iter().map(|f| (f.name.as_str(), f)).collect();
    let new_names: std::collections::HashSet<&str> = new.iter().map(|f| f.name.as_str()).collect();

    for f in new {
        match old_map.get(f.name.as_str()) {
            None => {
                let required = if f.required { ", required" } else { "" };
                out.added_fields.push(format!(
                    "{prefix}{} ({}{required})",
                    f.name,
                    field_type_str(&f.field_type)
                ));
            }
            Some(o) => {
                let old_type = field_type_str(&o.field_type);
                let new_type = field_type_str(&f.field_type);
                if old_type != new_type {
                    out.changed_fields
                        .push(format!("{prefix}{}: type {old_type} -> {new_type}", f.name));
                }
                if o.required != f.required {
                    let change = if f.required {
                        "became required"
                    } else {
                        "became optional"
                    };
                    out.changed_fields
                        .push(format!("{prefix}{}: {change}", f.name));
                }
            }
        }
    }
    for f in old {
        if !new_names.contains(f.name.as_str()) {
            out.removed_fields.push(format!("{prefix}{}", f.name));
        }
    }
}

/// Diff all four field sections of two versions of the same resource.
fn resource_struct_diff(
    old: &docgen::model::Resource,
    new: &docgen::model::Resource,
) -> StructDiff {
    let mut d = StructDiff::default();
    diff_fields("", &old.fields, &new.fields, &mut d);
    diff_fields("spec.", &old.spec_fields, &new.spec_fields, &mut d);
    diff_fields("status.", &old.status_fields, &new.status_fields, &mut d);
    diff_fields("list.", &old.list_fields, &new.list_fields, &mut d);
    d
}

/// All named types (simple + complex) of a parse result: name -> fields.
fn all_types(p: &ParseResult) -> HashMap<&str, &Vec<Field>> {
    p.simple_types
        .iter()
        .chain(p.complex_types.iter())
        .map(|(name, (_, fields))| (name.as_str(), fields))
        .collect()
}

/// Full structural diff between two loaded Kubernetes versions.
fn version_diff(
    from_version: &str,
    from: &ParseResult,
    to_version: &str,
    to: &ParseResult,
) -> VersionDiff {
    let key =
        |r: &docgen::model::Resource| (r.group.clone(), r.kind.clone(), r.api_version.clone());
    let from_map: HashMap<_, &docgen::model::Resource> =
        from.resources.iter().map(|r| (key(r), r)).collect();
    let to_map: HashMap<_, &docgen::model::Resource> =
        to.resources.iter().map(|r| (key(r), r)).collect();

    let summary = |r: &docgen::model::Resource| ResourceSummary {
        kind: r.kind.clone(),
        group: display_group(&r.group),
        api_version: r.api_version.clone(),
        description: first_sentence(&r.description),
    };
    let sort_key = |s: &ResourceSummary| (s.group.clone(), s.kind.clone(), s.api_version.clone());

    let mut added_resources = Vec::new();
    let mut changed_resources = Vec::new();
    for r in &to.resources {
        match from_map.get(&key(r)) {
            None => added_resources.push(summary(r)),
            Some(old) => {
                let diff = resource_struct_diff(old, r);
                if !diff.is_empty() {
                    changed_resources.push(ResourceDiff {
                        kind: r.kind.clone(),
                        group: display_group(&r.group),
                        api_version: r.api_version.clone(),
                        diff,
                    });
                }
            }
        }
    }
    let mut removed_resources: Vec<ResourceSummary> = from
        .resources
        .iter()
        .filter(|r| !to_map.contains_key(&key(r)))
        .map(summary)
        .collect();

    added_resources.sort_by_key(sort_key);
    removed_resources.sort_by_key(sort_key);
    changed_resources.sort_by(|a, b| {
        (&a.group, &a.kind, &a.api_version).cmp(&(&b.group, &b.kind, &b.api_version))
    });

    let from_types = all_types(from);
    let to_types = all_types(to);

    let mut added_types: Vec<String> = to_types
        .keys()
        .filter(|name| !from_types.contains_key(*name))
        .map(|name| name.to_string())
        .collect();
    let mut removed_types: Vec<String> = from_types
        .keys()
        .filter(|name| !to_types.contains_key(*name))
        .map(|name| name.to_string())
        .collect();
    let mut changed_types = Vec::new();
    for (name, new_fields) in &to_types {
        if let Some(old_fields) = from_types.get(name) {
            let mut diff = StructDiff::default();
            diff_fields("", old_fields, new_fields, &mut diff);
            if !diff.is_empty() {
                changed_types.push(TypeDiff {
                    name: name.to_string(),
                    diff,
                });
            }
        }
    }
    added_types.sort();
    removed_types.sort();
    changed_types.sort_by(|a, b| a.name.cmp(&b.name));

    VersionDiff {
        from_k8s_version: from_version.to_string(),
        to_k8s_version: to_version.to_string(),
        added_resources,
        removed_resources,
        changed_resources,
        added_types,
        removed_types,
        changed_types,
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

async fn inject_rate_identity(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let tier = req
        .extensions()
        .get::<Tier>()
        .cloned()
        .unwrap_or(Tier::Free);
    let (key, display) = if state.anon_mode {
        let ip = addr.ip().to_string();
        (ip.clone(), ip)
    } else {
        let raw = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_string();
        let display = mask_key(&raw);
        (raw, display)
    };
    req.extensions_mut()
        .insert(RateIdentity { key, display, tier });
    req.extensions_mut().insert(state);
    next.run(req).await
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
    let is_browser_get = req.method() == Method::GET
        && !req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .contains("text/event-stream");

    if is_browser_get {
        return match &state.browser_redirect {
            Some(url) => Redirect::temporary(url).into_response(),
            None => StatusCode::BAD_REQUEST.into_response(),
        };
    }

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
    let mut sorted_versions: Vec<String> = versions.keys().cloned().collect();
    sorted_versions.sort_by_key(|v| k8s_version_rank(v));

    let mut service_map: HashMap<String, McpService> = HashMap::new();
    for (i, version) in sorted_versions.iter().enumerate() {
        let neighbor = |j: usize| {
            sorted_versions
                .get(j)
                .map(|v| (v.clone(), versions[v].clone()))
        };
        let handler = McpHandler {
            parsed: versions[version].clone(),
            version: version.clone(),
            prev: i.checked_sub(1).and_then(neighbor),
            next: neighbor(i + 1),
        };
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
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
        browser_redirect: cli.browser_redirect,
    };

    let protected = Router::new()
        .route("/", routing::any(mcp_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            inject_rate_identity,
        ))
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
            version: "v1.33".into(),
            prev: None,
            next: None,
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
            browser_redirect: None,
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

    // ── k8s_version_rank ─────────────────────────────────────────────────────

    #[test]
    fn k8s_version_rank_orders_minor() {
        assert!(k8s_version_rank("v1.36") > k8s_version_rank("v1.35"));
        assert!(k8s_version_rank("v1.9") < k8s_version_rank("v1.10"));
    }

    #[test]
    fn k8s_version_rank_orders_major() {
        assert!(k8s_version_rank("v2.0") > k8s_version_rank("v1.36"));
    }

    // ── diff_fields ──────────────────────────────────────────────────────────

    #[test]
    fn diff_fields_added_removed() {
        let old = vec![scalar("a"), scalar("b")];
        let new = vec![scalar("b"), scalar("c")];
        let mut d = StructDiff::default();
        diff_fields("spec.", &old, &new, &mut d);
        assert_eq!(d.added_fields, vec!["spec.c (string)"]);
        assert_eq!(d.removed_fields, vec!["spec.a"]);
        assert!(d.changed_fields.is_empty());
    }

    #[test]
    fn diff_fields_added_required_is_annotated() {
        let new = vec![Field {
            required: true,
            ..scalar("name")
        }];
        let mut d = StructDiff::default();
        diff_fields("", &[], &new, &mut d);
        assert_eq!(d.added_fields, vec!["name (string, required)"]);
    }

    #[test]
    fn diff_fields_type_change() {
        let old = vec![scalar("x")];
        let new = vec![Field {
            field_type: FieldType::Scalar("integer".into()),
            ..scalar("x")
        }];
        let mut d = StructDiff::default();
        diff_fields("", &old, &new, &mut d);
        assert_eq!(d.changed_fields, vec!["x: type string -> integer"]);
    }

    #[test]
    fn diff_fields_required_change() {
        let old = vec![scalar("x")];
        let new = vec![Field {
            required: true,
            ..scalar("x")
        }];
        let mut d = StructDiff::default();
        diff_fields("", &old, &new, &mut d);
        assert_eq!(d.changed_fields, vec!["x: became required"]);
    }

    #[test]
    fn diff_fields_description_change_ignored() {
        let old = vec![scalar("x")];
        let new = vec![Field {
            description: "different description".into(),
            ..scalar("x")
        }];
        let mut d = StructDiff::default();
        diff_fields("", &old, &new, &mut d);
        assert!(d.is_empty());
    }

    // ── version_diff ─────────────────────────────────────────────────────────

    fn parsed_with(resources: Vec<docgen::model::Resource>) -> ParseResult {
        ParseResult {
            resources,
            common_defs: vec![],
            classifications: HashMap::new(),
            simple_types: HashMap::new(),
            complex_types: HashMap::new(),
        }
    }

    #[test]
    fn version_diff_added_and_removed_resources() {
        let from = parsed_with(vec![
            resource("Pod", "", "v1"),
            resource("Old", "apps", "v1"),
        ]);
        let to = parsed_with(vec![
            resource("Pod", "", "v1"),
            resource("New", "apps", "v1"),
        ]);
        let d = version_diff("v1.35", &from, "v1.36", &to);
        assert_eq!(d.from_k8s_version, "v1.35");
        assert_eq!(d.to_k8s_version, "v1.36");
        assert_eq!(d.added_resources.len(), 1);
        assert_eq!(d.added_resources[0].kind, "New");
        assert_eq!(d.removed_resources.len(), 1);
        assert_eq!(d.removed_resources[0].kind, "Old");
        assert!(d.changed_resources.is_empty());
    }

    #[test]
    fn version_diff_promotion_is_added_plus_removed() {
        let from = parsed_with(vec![resource("Foo", "apps", "v1beta1")]);
        let to = parsed_with(vec![resource("Foo", "apps", "v1")]);
        let d = version_diff("v1.35", &from, "v1.36", &to);
        assert_eq!(d.added_resources[0].api_version, "v1");
        assert_eq!(d.removed_resources[0].api_version, "v1beta1");
    }

    #[test]
    fn version_diff_changed_resource_spec_field() {
        let from = parsed_with(vec![resource("Pod", "", "v1")]);
        let mut changed = resource("Pod", "", "v1");
        changed.spec_fields = vec![scalar("hostUsers")];
        let to = parsed_with(vec![changed]);
        let d = version_diff("v1.35", &from, "v1.36", &to);
        assert!(d.added_resources.is_empty());
        assert_eq!(d.changed_resources.len(), 1);
        assert_eq!(
            d.changed_resources[0].diff.added_fields,
            vec!["spec.hostUsers (string)"]
        );
    }

    #[test]
    fn version_diff_types() {
        let mut from = parsed_with(vec![]);
        from.simple_types
            .insert("Gone".into(), ("d".into(), vec![]));
        from.complex_types
            .insert("Container".into(), ("d".into(), vec![scalar("image")]));
        let mut to = parsed_with(vec![]);
        to.simple_types.insert("Fresh".into(), ("d".into(), vec![]));
        to.complex_types.insert(
            "Container".into(),
            ("d".into(), vec![scalar("image"), scalar("restartPolicy")]),
        );
        let d = version_diff("v1.35", &from, "v1.36", &to);
        assert_eq!(d.added_types, vec!["Fresh"]);
        assert_eq!(d.removed_types, vec!["Gone"]);
        assert_eq!(d.changed_types.len(), 1);
        assert_eq!(d.changed_types[0].name, "Container");
        assert_eq!(
            d.changed_types[0].diff.added_fields,
            vec!["restartPolicy (string)"]
        );
    }

    // ── handle_diff_previous_version / handle_diff_next_version ─────────────

    #[test]
    fn diff_previous_version_errors_without_prev() {
        let h = handler_with(parsed_with(vec![]));
        let err = h.handle_diff_previous_version().unwrap_err();
        assert!(err.message.contains("v1.33"));
    }

    #[test]
    fn diff_next_version_errors_without_next() {
        let h = handler_with(parsed_with(vec![]));
        assert!(h.handle_diff_next_version().is_err());
    }

    #[test]
    fn diff_previous_version_reports_additions() {
        let mut h = handler_with(parsed_with(vec![
            resource("Pod", "", "v1"),
            resource("New", "apps", "v1"),
        ]));
        h.version = "v1.36".into();
        h.prev = Some((
            "v1.35".into(),
            Arc::new(parsed_with(vec![resource("Pod", "", "v1")])),
        ));
        let result = h.handle_diff_previous_version().unwrap();
        let d: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(d["from_k8s_version"], "v1.35");
        assert_eq!(d["to_k8s_version"], "v1.36");
        assert_eq!(d["added_resources"][0]["kind"], "New");
    }

    #[test]
    fn diff_next_version_diffs_forward() {
        let mut h = handler_with(parsed_with(vec![resource("Pod", "", "v1")]));
        h.version = "v1.35".into();
        h.next = Some((
            "v1.36".into(),
            Arc::new(parsed_with(vec![
                resource("Pod", "", "v1"),
                resource("New", "apps", "v1"),
            ])),
        ));
        let result = h.handle_diff_next_version().unwrap();
        let d: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(d["from_k8s_version"], "v1.35");
        assert_eq!(d["to_k8s_version"], "v1.36");
        assert_eq!(d["added_resources"][0]["kind"], "New");
    }

    // ── handle_compare_resource_versions ─────────────────────────────────────

    #[test]
    fn compare_resource_versions_basic() {
        let mut beta = resource("VolumeAttributesClass", "storage.k8s.io", "v1beta1");
        beta.fields.push(scalar("deprecatedField"));
        let mut stable = resource("VolumeAttributesClass", "storage.k8s.io", "v1");
        stable.fields.push(scalar("newField"));
        let h = handler_with(parsed_with(vec![beta, stable]));
        let result = h
            .handle_compare_resource_versions(&args(&[
                ("kind", "VolumeAttributesClass"),
                ("from_api_version", "v1beta1"),
                ("to_api_version", "v1"),
            ]))
            .unwrap();
        let d: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(d["kind"], "VolumeAttributesClass");
        assert_eq!(d["group"], "storage.k8s.io");
        assert_eq!(d["from_api_version"], "v1beta1");
        assert_eq!(d["to_api_version"], "v1");
        assert_eq!(d["added_fields"][0], "newField (string)");
        assert_eq!(d["removed_fields"][0], "deprecatedField");
    }

    #[test]
    fn compare_resource_versions_missing_args() {
        let h = handler_with(parsed_with(vec![]));
        assert!(h.handle_compare_resource_versions(&args(&[])).is_err());
        assert!(h
            .handle_compare_resource_versions(&args(&[("kind", "Pod")]))
            .is_err());
    }

    #[test]
    fn compare_resource_versions_unknown_kind() {
        let h = handler_with(parsed_with(vec![]));
        let err = h
            .handle_compare_resource_versions(&args(&[
                ("kind", "Nope"),
                ("from_api_version", "v1beta1"),
                ("to_api_version", "v1"),
            ]))
            .unwrap_err();
        assert!(err.message.contains("Nope"));
    }

    #[test]
    fn compare_resource_versions_unknown_version_lists_available() {
        let h = handler_with(parsed_with(vec![
            resource("CronJob", "batch", "v1"),
            resource("CronJob", "batch", "v1beta1"),
        ]));
        let err = h
            .handle_compare_resource_versions(&args(&[
                ("kind", "CronJob"),
                ("from_api_version", "v2alpha1"),
                ("to_api_version", "v1"),
            ]))
            .unwrap_err();
        assert!(err.message.contains("v2alpha1"));
        assert!(err.message.contains("v1beta1"));
    }

    #[test]
    fn compare_resource_versions_ambiguous_group_requires_group() {
        let h = handler_with(parsed_with(vec![
            resource("Event", "", "v1"),
            resource("Event", "events.k8s.io", "v1"),
        ]));
        let err = h
            .handle_compare_resource_versions(&args(&[
                ("kind", "Event"),
                ("from_api_version", "v1"),
                ("to_api_version", "v1"),
            ]))
            .unwrap_err();
        assert!(err.message.contains("multiple groups"));

        let result = h.handle_compare_resource_versions(&args(&[
            ("kind", "Event"),
            ("group", "core"),
            ("from_api_version", "v1"),
            ("to_api_version", "v1"),
        ]));
        assert!(result.is_ok());
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
            browser_redirect: None,
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
            browser_redirect: None,
        }
    }

    fn req_from(ip: [u8; 4]) -> Request<Body> {
        let mut r = Request::builder().uri("/").body(Body::empty()).unwrap();
        r.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((ip, 1234))));
        r
    }

    fn req_with_bearer(ip: [u8; 4], token: &str) -> Request<Body> {
        let mut r = Request::builder()
            .uri("/")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        r.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((ip, 1234))));
        r
    }

    fn auth_router(state: AppState) -> Router {
        Router::new()
            .route("/", routing::get(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(state.clone(), authenticate))
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

    // ── check_rate_limit: free tier (keyed by IP in anon mode) ──────────────

    fn free_identity(key: &str) -> RateIdentity {
        RateIdentity {
            key: key.into(),
            display: key.into(),
            tier: Tier::Free,
        }
    }

    #[test]
    fn check_rate_limit_free_exhausted() {
        // burst=2: third call is rate-limited
        let state = AppState {
            free_limiter: make_limiter(2, 60),
            ..anon_state()
        };
        let id = free_identity("10.0.0.1");
        assert!(check_rate_limit(&id, &state).is_ok());
        assert!(check_rate_limit(&id, &state).is_ok());
        assert!(check_rate_limit(&id, &state).is_err());
    }

    #[test]
    fn check_rate_limit_different_keys_independent() {
        // burst=1: key A exhausts its bucket; key B still has capacity
        let state = AppState {
            free_limiter: make_limiter(1, 60),
            ..anon_state()
        };
        assert!(check_rate_limit(&free_identity("10.0.0.1"), &state).is_ok());
        assert!(check_rate_limit(&free_identity("10.0.0.1"), &state).is_err());
        assert!(check_rate_limit(&free_identity("10.0.0.2"), &state).is_ok());
    }

    // ── check_rate_limit: auth mode (keyed by token) ─────────────────────────

    #[test]
    fn check_rate_limit_same_token_different_displays_share_bucket() {
        // Same raw token from two "different IPs" shares one governor bucket
        let state = AppState {
            free_limiter: make_limiter(1, 60),
            ..anon_state()
        };
        let id1 = RateIdentity {
            key: "shared-key".into(),
            display: "***rkey".into(),
            tier: Tier::Free,
        };
        let id2 = RateIdentity {
            key: "shared-key".into(),
            display: "***rkey".into(),
            tier: Tier::Free,
        };
        assert!(check_rate_limit(&id1, &state).is_ok());
        assert!(check_rate_limit(&id2, &state).is_err());
    }

    #[test]
    fn check_rate_limit_returns_retry_after_secs() {
        let state = AppState {
            free_limiter: make_limiter(1, 60),
            ..anon_state()
        };
        let id = free_identity("key");
        assert!(check_rate_limit(&id, &state).is_ok());
        let retry = check_rate_limit(&id, &state).unwrap_err();
        assert!(retry >= 1, "retry_after_secs must be >= 1, got {retry}");
    }
}
