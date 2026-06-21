# Skill: generate-site

Rebuild the static HTML site from Kubernetes OpenAPI specs.

## When to use
Run this after any change to `src/`, `templates/`, or `Cargo.toml`, or when
asked to "build", "regenerate", or "rebuild the site".

## Steps

1. **Check for a running preview server** that might hold open file handles
   inside `./site`. If one is running, stop it first, otherwise the directory
   may not clear cleanly.

   ```bash
   lsof +D ./site 2>/dev/null | grep -v COMMAND
   # If anything is listed, kill the process before proceeding.
   ```

2. **Wipe and regenerate**:

   ```bash
   rm -rf ./site && cargo run --release -- generate --k8s-version v1.36 --out ./site
   ```

3. **Verify**:
   - Output ends with `Generated N resource pages + index pages + sitemap.xml`
   - Spot-check: `./site/docs/v1.36/core/v1/pod/index.html` exists and contains
     `<h1>Pod` and a `<dl>` of fields.
   - Check `apiVersion`/`kind` rendering: the Pod page must contain
     `<code>v1</code>` (not `string`) for `apiVersion`, and `<code>Pod</code>`
     for `kind`. A named-group resource (e.g. Deployment) must show `<code>apps/v1</code>`.
   - Confirm no stale list pages: `find ./site -name "*list*"` should return nothing.

## Common pitfalls
- **Stale pages**: forgetting `rm -rf ./site` leaves pages from old runs. The
  generator never deletes output files.
- **Rate limiting**: without `GITHUB_TOKEN` the GitHub API allows ~60 requests/hour.
  Set `export GITHUB_TOKEN=...` or pass `--token`.
