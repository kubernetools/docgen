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
  resource.html    extends base — field_dl macro, fields / spec / status / list sections
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
   - For each resource, if its `spec` or `status` field is a `$ref`, the referenced
     schema's fields are extracted into `Resource.spec_fields` / `Resource.status_fields`
     (see [Spec/Status sub-schemas](#specstatus-sub-schemas)).
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

### Spec/Status sub-schemas

Resources whose `spec` or `status` field is a `$ref` to a sibling schema in the
same file (e.g. `PodSpec`, `PodStatus`) get those fields extracted at parse time.

**How it works in the parser** (`parser/mod.rs`):
- Before iterating over schemas, a `by_short_name: HashMap<String, &RawSchema>` index
  is built from the full schema map. The key is the last dotted component of the
  schema name (`io.k8s.api.core.v1.PodSpec` → `PodSpec`), which matches what
  `resolve::short_name()` returns from a `$ref` value.
- For each resource, `sub_schema_fields("spec", …)` and `sub_schema_fields("status", …)`
  look up the referenced schema and extract its fields and description into
  `Resource.spec_fields` / `Resource.spec_description` (and status equivalents).
- Resources with no `spec`/`status` field, or whose referenced schema is absent,
  simply have empty `spec_fields`/`status_fields` — no special-casing needed.

**How it appears in the renderer** (`renderer/mod.rs`):
- `spec_fields` and `status_fields` are built into `FieldCtx` slices using the same
  `build_fields` closure as the main and list fields.
- The `spec` / `status` entries in the main resource fields get `type_href` overridden
  to `#{kind_lower}spec` / `#{kind_lower}status` (in-page anchors) when the
  corresponding sub-field slice is non-empty.

**How it appears in the template** (`templates/resource.html`):
- Two new `<section>` elements are rendered between the main fields section and the
  list section, each with `id="{kind_lower}spec"` / `id="{kind_lower}status"`.
- Both sections are conditional on `spec_fields` / `status_fields` being non-empty,
  so resources like ConfigMap that have neither are unaffected.

### Field ordering on resource pages
1. `apiVersion`, `kind`, `metadata` — always first, in that order
2. Required fields — alphabetical
3. Optional fields — alphabetical

Required fields get `class="req"` on their `<dt>`, which CSS renders with a
trailing `*` via `dt.req::after { content: " *"; }`.

### Special rendering for `apiVersion` and `kind`
These two fields are not rendered like ordinary fields. Instead of showing the
type (`string`) and the OpenAPI description, the template shows:

| Field        | `<dt>` (first column) | `<dd>` (second column)                            |
|--------------|------------------------|---------------------------------------------------|
| `apiVersion` | `apiVersion`           | actual value: `v1` (core) or `group/version` (named group) |
| `kind`       | `kind`                 | actual kind name (e.g. `Pod`); `KindList` in the list section |

This is implemented in `templates/resource.html` by checking `field.name` inside
the field loop and branching before the normal `<dt>/<dd>` rendering.

### URL scheme
- Navigation hrefs are root-relative: `/docs/{k8s_version}/{group}/{api_version}/{kind_lower}/`
- Core group maps to `"core"` in the URL segment.
- `--base-url` is used only for canonical `<link>` tags, JSON-LD, and sitemap.

### Templates
- Engine: **minijinja 2** — templates embedded via `include_str!` (binary is self-contained).
- All URL values in templates need `| safe` to prevent `/` being escaped to `&#x2f;`.
- **Do not use `~` (string concatenation) to build paths containing `/`.**
  The concatenated string is treated as a template variable and gets auto-escaped,
  turning `/` into `&#x2f;`. Use a block `{% if %}…{% else %}…{% endif %}` with a
  literal `/` in the template body instead — literal characters are never escaped.
  Example: `{% if group == "core" %}{{ v }}{% else %}{{ group }}/{{ v }}{% endif %}`
  `~` is safe for non-path strings (e.g. `kind ~ "List"` for section headings).
- `resource.html` defines a `field_dl(fields, copy, group_display, api_version, kind_value)` macro
  that renders a `<dl>` for any field list. `kind_value` controls apiVersion/kind
  special-casing: pass `none` (default) for plain sections (spec, status), pass
  `kind` for the resource section, or `kind ~ "List"` for the list section. This
  avoids repeating the `<dt>`/`<dd>` rendering logic across four sections.
- `ResourcePageCtx` passes `kind_lower` (the lowercased kind) to the template so
  spec/status section anchor IDs (`id="podspec"`, `id="podstatus"`) can be built
  without string manipulation in the template.
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
