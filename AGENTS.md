# docgen — agent knowledge base

`docgen` is a Rust CLI that fetches Kubernetes OpenAPI v3 specs from
`github.com/kubernetools/specs` and generates a static HTML documentation site.

## Build & run

```bash
# Always wipe the output first — the generator only writes, never deletes.
# Stale pages from old runs will persist and cause confusion otherwise.
#
# A Python (or other) HTTP server may be running to preview the site.
# If so, kill it first — a live server retains file handles and the
# directory may not be fully cleared, leaving stale pages behind.
# Check with: lsof +D ./site   or   fuser -m ./site
rm -rf ./site && cargo run --release -- generate --k8s-version v1.33 --out ./site
```

Optional flags:
- `--base-url https://www.kubernetools.com` — absolute prefix for canonical URLs and sitemap (default value)
- `--token` / env `GITHUB_TOKEN` — GitHub PAT for higher rate limits

## Module map

```
src/
  main.rs          Entry: parse CLI, drive async runtime
  cli.rs           clap Cli / Commands (Generate subcommand)
  fetcher.rs       GitHub Contents API → download *_openapi.json files
  model.rs         Public data types: Resource, Field, FieldType
  parser/
    mod.rs         Orchestrate: per-file parse → post-process → Vec<Resource>
    schema.rs      serde structs for raw OpenAPI (RawSpec, RawSchema, RawProperty, RawGVK)
    resolve.rs     $ref / allOf resolution → FieldType; short_name() strips schema prefix
  renderer/
    mod.rs         minijinja env setup, page dispatch, helper functions
    pages.rs       Context structs passed to templates (ResourcePageCtx, GroupIndexCtx, …)
    sitemap.rs     Pure-Rust sitemap.xml generation
templates/
  base.html        DOCTYPE, <head>, breadcrumb nav, CSS, {% block content %}
  resource.html    extends base — description, fields dl, list section, other-versions
  group_index.html extends base — one row per kind, all versions on same line
  version_index.html extends base — list of groups for a k8s version
```

## Data flow

1. **Fetch**: `fetcher::fetch_specs` lists `specs/{version}/` on GitHub, filters
   `*_openapi.json`, downloads each as `serde_json::Value`.
2. **Parse**: `parser::parse_specs` iterates files, derives `(group, version)` from
   the filename, deserialises into `RawSpec`, emits one `Resource` per schema that
   carries a matching `x-kubernetes-group-version-kind` entry.
3. **Post-process** (still in parser):
   - `*List` resources are partitioned out, attached to their root (`PodList → Pod`),
     and removed from the top-level list.
   - Remaining resources are sorted by `kind`.
4. **Render**: `renderer::render` writes index pages + one page per resource.

## Key design decisions

### Filename → (group, version)
- `api__v1_openapi.json` → group `""`, version `v1` (core group)
- `apis__apps__v1_openapi.json` → group `apps`, version `v1`
- Files that don't match these patterns (discovery endpoints) are skipped.

### Deduplication via `emitted: HashSet<String>`
Cross-cutting schemas like `DeleteOptions` and `WatchEvent` carry a
`x-kubernetes-group-version-kind` array listing all 62 API groups. Each spec
file is self-contained and includes its own copy. Without deduplication each
file would emit its own page. The `emitted` set tracks schema names; the first
file to claim a schema wins, subsequent files skip it.

### GVK matching per file
For each schema, only the GVK entry where `group == file_group && version ==
file_version` is used. This prevents a file from generating pages for groups
it doesn't own.

### List resources
`*List` kinds (e.g. `PodList`) are never rendered as standalone pages. After
parsing, they are matched to their root by `(kind.strip_suffix("List"), group,
api_version)` and their description + fields are stored on `Resource.list_fields`
/ `Resource.list_description`. The resource page renders a dedicated `<section>`
for the list. Lists with no matching root (e.g. `APIResourceList`) are dropped.

### Version ordering
`version_rank(v) -> (major, stability, minor)` where stability: 0=alpha, 1=beta, 2=stable.
Higher rank = more recent. Used to sort version badges in group index (most
recent first) and to determine the primary link per kind.

### Group index: one row per kind
Resources with multiple API versions (e.g. `VolumeAttributesClass v1beta1` and
`v1alpha1`) appear as a single row. The kind name links to the most recent
version. Version badges are shown as plain text for older versions — no links —
to avoid duplicate-content SEO issues.

### Resource page: other versions
Each resource page shows a "Other versions:" line with linked badges for
alternate API versions of the same kind + group. Older version pages exist and
are fully rendered; they just aren't linked from the group index.

### Field ordering on resource pages
1. `apiVersion`, `kind`, `metadata` — always first, in that order
2. Required fields — alphabetical
3. Optional fields — alphabetical

Required fields get `class="req"` on their `<dt>`, which CSS renders with a
trailing `*` via `dt.req::after { content: " *"; }`.

### URL scheme
- Navigation hrefs are root-relative: `/docs/{k8s_version}/{group}/{api_version}/{kind_lower}/`
- Core group maps to `"core"` in the URL segment.
- `--base-url` is used only for canonical `<link>` tags, JSON-LD, and sitemap.

### Templates
- Engine: **minijinja 2** — templates embedded via `include_str!` (binary is self-contained).
- All URL values in templates need `| safe` to prevent `/` being escaped to `&#x2f;`.
- JSON-LD is pre-serialised in Rust (`serde_json::json!(...).to_string()`) and
  passed as a `String` field (`json_ld`) — minijinja 2 has no built-in `tojson` filter.
- `components` and `schemas` are `Option<…>` because many spec files are
  discovery endpoints with no schemas.

## Output layout

| Page            | Path                                                               |
|-----------------|--------------------------------------------------------------------|
| Version index   | `{out}/docs/{k8s_version}/index.html`                             |
| Group index     | `{out}/docs/{k8s_version}/{group}/index.html`                     |
| Resource page   | `{out}/docs/{k8s_version}/{group}/{api_version}/{kind}/index.html`|
| Sitemap         | `{out}/sitemap.xml`                                               |
