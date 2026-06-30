# kubernetools MCP server

The `mcp` binary exposes Kubernetes API documentation as a Model Context Protocol
(MCP) server over HTTP/SSE (Streamable HTTP transport).

LLM clients such as Claude Desktop connect to it and call tools to discover
resources, inspect their fields, and drill into composite types — all from the
in-memory parsed spec, with no network round-trips at query time.

## Quick start

```bash
# 1. Create a minimal key store
echo '[{"key":"mykey","tier":"free"}]' > keys.json

# 2. Build and start the server (loads v1.33–v1.36)
cargo run --release --bin mcp -- \
  --k8s-versions v1.33,v1.34,v1.35,v1.36 \
  --key-store keys.json

# 3. Connect an MCP client to http://localhost:3000/?version=v1.36
#    with header: Authorization: Bearer mykey
```

## Command-line reference

```
USAGE:
    mcp [OPTIONS]

OPTIONS:
    --k8s-versions <LIST>       Comma-separated Kubernetes versions to pre-load
                                [default: v1.33]  [env: K8S_VERSIONS]
                                Example: --k8s-versions v1.33,v1.34,v1.35,v1.36

    --token <TOKEN>             GitHub personal access token for higher API rate limits
                                [env: GITHUB_TOKEN]

    --port <PORT>               TCP port to listen on [default: 3000]

    --key-store <PATH>          Path to the API key JSON file [env: KEY_STORE_PATH]
                                When omitted, the server runs in anonymous mode: no
                                auth is required and all connections are rate-limited
                                per source IP at the free-tier limit.

    --allowed-hosts <LIST>      Comma-separated Host header values that are accepted
                                (e.g. mcp.example.com,mcp.example.com:443).
                                [env: ALLOWED_HOSTS]
                                When omitted, host validation is disabled — suitable
                                for local development only. Always set this in
                                production to prevent DNS-rebinding attacks.

    --browser-redirect <URL>    URL to redirect plain browser GET requests to.
                                [env: BROWSER_REDIRECT_URL]
                                MCP clients are detected by the presence of
                                Accept: text/event-stream on GET requests. A browser
                                hitting the endpoint instead receives a 307 redirect
                                to this URL. When omitted, browser GETs return 400.
```

### Loading multiple versions

```bash
cargo run --release --bin mcp -- \
  --k8s-versions v1.33,v1.34,v1.35,v1.36 \
  --key-store keys.json
```

All versions are fetched and parsed concurrently at startup. Clients select a
version per session via the `version` query parameter:

```
http://localhost:3000/?version=v1.33
http://localhost:3000/?version=v1.36
```

If `version` is omitted, the first version that was loaded is used. An unknown
version returns `400 Bad Request`.

### GitHub token

Fetching specs from the GitHub Contents API is rate-limited at 60 req/h without
auth, which is enough for a single version but tight when loading four at once.
A personal access token (classic, no extra scopes needed) raises the limit to
5 000 req/h.

```bash
export GITHUB_TOKEN=ghp_...
cargo run --release --bin mcp -- \
  --k8s-versions v1.33,v1.34,v1.35,v1.36 \
  --key-store keys.json
```

## API key store

The key store is a flat JSON array loaded once at startup:

```json
[
  { "key": "free-key-abc", "tier": "free" },
  { "key": "paid-key-xyz", "tier": "paid" }
]
```

Every request must include the key in the `Authorization` header:

```
Authorization: Bearer free-key-abc
```

Requests without a valid key receive `401 Unauthorized`.

### Anonymous mode

When `--key-store` is omitted, the server runs without authentication. All
connections are accepted and rate-limited per source IP at the free-tier limit.
This is convenient for local use but should not be exposed publicly.

### Tiers and rate limits

| Tier | Limit |
|------|-------|
| `free` | 10 requests / minute, burst 10 |
| `paid` | ~1 000 requests / second, burst 1 000 (effectively unlimited) |

Requests that exceed the limit receive `429 Too Many Requests`. Limits are
tracked per API key, not per IP address.

## Connecting an MCP client

The server implements the
[MCP Streamable HTTP transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#streamable-http).
The endpoint is:

```
http://<host>:<port>/[?version=<k8s-version>]
```

### Claude Desktop

Add the following to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "kubernetools": {
      "url": "http://localhost:3000/?version=v1.36",
      "headers": {
        "Authorization": "Bearer mykey"
      }
    }
  }
}
```

The `version` query parameter is part of the base URL and is included
automatically in every subsequent request by the client.

## Available tools

### `list_resources`

Lightweight discovery — returns one entry per resource, sorted by
`(group, kind, api_version)`. Use this first to find kind names.

**Input** (all optional):

| Field | Type | Description |
|-------|------|-------------|
| `group` | string | Filter by API group, e.g. `"apps"` or `"core"`. Omit for all groups. |
| `api_version` | string | Filter by API version, e.g. `"v1"`. Omit for all versions. |

**Output**: JSON array of objects:

```json
[
  {
    "kind": "Deployment",
    "group": "apps",
    "api_version": "v1",
    "description": "Deployment enables declarative updates for Pods and ReplicaSets"
  }
]
```

---

### `get_resource`

Full resource detail — fields, spec, status, and list fields — enough to write
a manifest in one call. When `api_version` is omitted, the most recent version
is returned.

**Input**:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `kind` | string | yes | Resource kind, e.g. `"Pod"` or `"Deployment"`. |
| `group` | string | no | API group. Omit to match any group. |
| `api_version` | string | no | Specific version. Omit for the most recent version. |

**Output**:

```json
{
  "kind": "Pod",
  "group": "core",
  "api_version": "v1",
  "description": "Pod is a collection of containers that can run on a host.",
  "fields": [
    {
      "name": "spec",
      "field_type": "PodSpec",
      "required": false,
      "description": "Specification of the desired behavior of the pod.",
      "sub_fields": [],
      "type_ref": "PodSpec"
    }
  ],
  "spec_fields": [ "..." ],
  "status_fields": [ "..." ],
  "list_fields": [ "..." ]
}
```

Each field includes:

| Key | Description |
|-----|-------------|
| `name` | Field name |
| `field_type` | Type string: `"string"`, `"integer"`, `"[]Container"`, `"map[string]string"`, etc. |
| `required` | Whether the field is required |
| `description` | OpenAPI description |
| `sub_fields` | Inline expansion for simple composite types (one level deep) |
| `type_ref` | Schema name when the type is complex; use `get_type` to drill in |

When `type_ref` is set and `sub_fields` is empty, call `get_type` with that name
to retrieve the full field list for that type.

---

### `get_type`

Drill into a single composite type referenced via `type_ref` in `get_resource`
output.

**Input**:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `type_name` | string | yes | Schema short name, e.g. `"Container"` or `"PodFailurePolicy"`. |

**Output**:

```json
{
  "name": "Container",
  "description": "A single application container that you want to run within a pod.",
  "fields": [
    {
      "name": "image",
      "field_type": "string",
      "required": false,
      "description": "Container image name.",
      "sub_fields": [],
      "type_ref": null
    }
  ]
}
```

## Typical query flow

```
list_resources                          → discover kind names and groups
  └─ get_resource(kind="Deployment")   → see all top-level fields + spec/status
       └─ get_type(type_name="...")    → drill into any complex type_ref
```

## Health probe

```
GET /healthz
```

Returns `200 OK` with body `ok` once all Kubernetes versions have finished
loading. Returns `503 Service Unavailable` with body `loading` while startup
is still in progress.

This endpoint bypasses authentication and rate limiting so Kubernetes can poll
it freely during startup.

Use it for both the **startup probe** (wait until ready) and the **readiness
probe** (stop traffic if the pod restarts):

```yaml
startupProbe:
  httpGet:
    path: /healthz
    port: 3000
  failureThreshold: 30   # allow up to 5 min for version loading
  periodSeconds: 10
readinessProbe:
  httpGet:
    path: /healthz
    port: 3000
livenessProbe:
  httpGet:
    path: /healthz
    port: 3000
  initialDelaySeconds: 10
```

## Error responses

| HTTP status | Cause |
|-------------|-------|
| `307 Temporary Redirect` | Browser GET (no `Accept: text/event-stream`) when `--browser-redirect` is set |
| `400 Bad Request` | Browser GET when `--browser-redirect` is not set, or `version` parameter names a version not loaded at startup |
| `401 Unauthorized` | Missing or invalid `Authorization: Bearer <key>` header |
| `429 Too Many Requests` | Rate limit exceeded for the key's tier |

MCP-level errors (unknown tool name, missing required argument, kind not found)
are returned as MCP error content inside a normal `200` response.
