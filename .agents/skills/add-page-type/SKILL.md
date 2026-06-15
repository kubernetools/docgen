# Skill: add-page-type

Add a new category of generated HTML page (e.g. a cross-version index, a
search page, a changelog page).

## Pattern to follow

Every page type in docgen follows the same three-file pattern:

### 1. Context struct — `src/renderer/pages.rs`

Add a `#[derive(Serialize)]` struct with all data the template needs.
URL fields should be `String` (root-relative for nav, absolute for canonical).

```rust
#[derive(Serialize)]
pub struct MyPageCtx {
    pub canonical_url: String,
    pub breadcrumbs: Vec<Crumb>,
    pub meta_description: String,
    // ... page-specific fields
}
```

### 2. Template — `templates/my-page.html`

Extend `base.html`. Always pipe URL values through `| safe` to prevent
minijinja from escaping `/` as `&#x2f;`.

```html
{% extends "base.html" %}
{% block title %}...{% endblock %}
{% block meta_description %}{{ meta_description }}{% endblock %}
{% block content %}
...{{ some_href | safe }}...
{% endblock %}
```

Register it in `renderer/mod.rs`:
```rust
env.add_template("my-page.html", include_str!("../../templates/my-page.html"))?;
```

### 3. Render call — `src/renderer/mod.rs`

Build the context, then call `write_html`:

```rust
let ctx = MyPageCtx { ... };
write_html(&env, "my-page.html", &ctx, &out.join("path/to/index.html"))?;
```

Add the page URL to `sitemap_urls` if it should appear in the sitemap:
```rust
sitemap_urls.push(format!("{base_url}/path/to/"));
```

## Notes
- JSON-LD must be pre-serialised in Rust as a `String` and passed in the
  context — minijinja 2 has no `tojson` filter. Use `serde_json::json!(...).to_string()`.
- Breadcrumbs: last crumb has no href (rendered as `<span aria-current="page">`
  by `base.html`). Always provide at least `[Docs → /docs/]`.
- The `write_html` helper creates parent directories automatically.
