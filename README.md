# docgen

Generates static HTML Kubernetes API reference documentation from the OpenAPI
specs in [kubernetools/specs](https://github.com/kubernetools/specs).

## Usage

```bash
cargo build --release

# Generate the canonical site under /docs/latest/ (with sitemap + robots.txt)
./target/release/docgen generate --k8s-version v1.36 --out ./site --is-latest

# Also generate version-pinned copies for older versions
for v in v1.33 v1.34 v1.35; do
  ./target/release/docgen generate --k8s-version $v --out ./site
done
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-k`, `--k8s-version` | *(required)* | Kubernetes minor version, e.g. `v1.36` |
| `-o`, `--out` | `./site` | Output directory |
| `--base-url` | `https://www.kubernetools.com` | Base URL for canonical links and sitemap |
| `--token` | `$GITHUB_TOKEN` | GitHub token (raises API rate limit) |
| `--is-latest` | `false` | Generate site under `/docs/latest/` with all links rooted there, plus `sitemap.xml` and `robots.txt` |

### Output layout

Without `--is-latest` (version-pinned, no sitemap):

```
site/
  docs/
    v1.36/
      index.html               # version index (list of API groups)
      core/
        index.html             # group index (list of resources)
        v1/
          pod/index.html       # resource page
          ...
      apps/
        ...
```

With `--is-latest` (canonical site, includes sitemap):

```
site/
  docs/
    latest/
      index.html               # version index — title shows "v1.36 (latest)"
      core/
        index.html             # group index
        v1/
          pod/index.html       # resource page — all hrefs use /docs/latest/
          ...
      apps/
        ...
  sitemap.xml
  robots.txt
```

All canonical `<link>` tags and JSON-LD URLs always point to `/docs/latest/...`.
Pages generated with `--is-latest` are self-canonical; pages generated without it
carry a canonical pointing to their `/docs/latest/` counterpart.

## How it works

1. **Fetch** — lists `specs/{version}/` in the specs repository via the GitHub
   Contents API and downloads each `*_openapi.json` file.
2. **Parse** — extracts schemas with `x-kubernetes-group-version-kind`,
   deduplicating cross-cutting types (e.g. `DeleteOptions`) that appear in every
   spec file. `*List` kinds are attached to their root resource and not rendered
   as standalone pages.
3. **Render** — writes HTML pages via embedded [minijinja](https://docs.rs/minijinja)
   templates. Fields are ordered: `apiVersion` / `kind` / `metadata` first, then
   required fields alphabetically, then optional fields alphabetically.
   `apiVersion` and `kind` are rendered specially: instead of their OpenAPI type
   and description, the second column shows the actual value (`v1` / `apps/v1`
   for `apiVersion`; the resource or list kind name for `kind`).

## Development

```bash
cargo test       # run all unit tests (58 tests, no network required)
cargo clippy     # lint
```

Before regenerating the site, ensure no process (e.g. a local preview server)
holds open files in the output directory:

```bash
lsof +D ./site 2>/dev/null | grep -v COMMAND
rm -rf ./site && ./target/release/docgen generate --k8s-version v1.36 --out ./site
```
