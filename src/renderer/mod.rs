mod pages;
mod sitemap;

use crate::model::{FieldType, Resource};
use anyhow::Result;
use minijinja::Environment;
use pages::*;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub fn render(resources: &[Resource], out: &Path, base_url: &str) -> Result<()> {
    let mut env = Environment::new();
    env.add_template("base.html", include_str!("../../templates/base.html"))?;
    env.add_template("resource.html", include_str!("../../templates/resource.html"))?;
    env.add_template("group_index.html", include_str!("../../templates/group_index.html"))?;
    env.add_template("version_index.html", include_str!("../../templates/version_index.html"))?;

    // Root-relative hrefs used for field type cross-links.
    let kind_paths: std::collections::HashMap<String, String> = resources
        .iter()
        .map(|r| (r.kind.clone(), resource_path(r)))
        .collect();

    // All versions per (group, kind), sorted most-recent first — used for "other versions" links.
    let mut versions_by_kind: std::collections::HashMap<(String, String), Vec<VersionLink>> =
        std::collections::HashMap::new();
    for r in resources {
        versions_by_kind
            .entry((r.group.clone(), r.kind.clone()))
            .or_default()
            .push(VersionLink { api_version: r.api_version.clone(), href: resource_path(r) });
    }
    for vs in versions_by_kind.values_mut() {
        vs.sort_by(|a, b| version_rank(&b.api_version).cmp(&version_rank(&a.api_version)));
    }

    let mut by_version: BTreeMap<String, BTreeMap<String, Vec<&Resource>>> = BTreeMap::new();
    for resource in resources {
        by_version
            .entry(resource.k8s_version.clone())
            .or_default()
            .entry(group_seg(&resource.group))
            .or_default()
            .push(resource);
    }

    // Sitemap needs absolute URLs.
    let mut sitemap_urls: Vec<String> = Vec::new();

    for (k8s_version, groups) in &by_version {
        let version_path = format!("/docs/{k8s_version}/");
        sitemap_urls.push(format!("{base_url}{version_path}"));

        let group_links: Vec<GroupLink> = groups
            .keys()
            .map(|g| GroupLink {
                display: g.clone(),
                href: format!("/docs/{k8s_version}/{g}/"),
            })
            .collect();

        let version_ctx = VersionIndexCtx {
            k8s_version: k8s_version.clone(),
            groups: group_links,
            canonical_url: format!("{base_url}{version_path}"),
            breadcrumbs: vec![
                Crumb { label: "Docs".into(), href: "/docs/".into() },
                Crumb { label: k8s_version.clone(), href: version_path },
            ],
            meta_description: format!(
                "Complete Kubernetes {k8s_version} API reference documentation"
            ),
        };
        write_html(
            &env,
            "version_index.html",
            &version_ctx,
            &out.join(format!("docs/{k8s_version}/index.html")),
        )?;

        for (group, group_resources) in groups {
            let group_path = format!("/docs/{k8s_version}/{group}/");
            sitemap_urls.push(format!("{base_url}{group_path}"));

            // Group resources by kind; each kind may have multiple API versions.
            let mut by_kind: BTreeMap<String, Vec<&Resource>> = BTreeMap::new();
            for r in group_resources {
                by_kind.entry(r.kind.clone()).or_default().push(r);
            }
            let mut resource_links: Vec<ResourceLink> = by_kind
                .into_iter()
                .map(|(kind, mut rs)| {
                    rs.sort_by(|a, b| {
                        version_rank(&b.api_version).cmp(&version_rank(&a.api_version))
                    });
                    let versions = rs
                        .iter()
                        .map(|r| VersionLink { api_version: r.api_version.clone(), href: resource_path(r) })
                        .collect();
                    ResourceLink { kind, versions }
                })
                .collect();
            resource_links.sort_by(|a, b| a.kind.cmp(&b.kind));

            let group_ctx = GroupIndexCtx {
                group_display: group.clone(),
                k8s_version: k8s_version.clone(),
                resources: resource_links,
                canonical_url: format!("{base_url}{group_path}"),
                breadcrumbs: vec![
                    Crumb { label: "Docs".into(), href: "/docs/".into() },
                    Crumb { label: k8s_version.clone(), href: format!("/docs/{k8s_version}/") },
                    Crumb { label: group.clone(), href: group_path },
                ],
                meta_description: format!(
                    "{group} API resources for Kubernetes {k8s_version}"
                ),
            };
            write_html(
                &env,
                "group_index.html",
                &group_ctx,
                &out.join(format!("docs/{k8s_version}/{group}/index.html")),
            )?;

            for resource in group_resources {
                let path = resource_path(resource);
                let canonical_url = format!("{base_url}{path}");
                sitemap_urls.push(canonical_url.clone());

                let build_fields = |fields: &[crate::model::Field]| -> Vec<FieldCtx> {
                    fields
                        .iter()
                        .map(|f| {
                            let type_display = fmt_field_type(&f.field_type);
                            let type_href = ref_name(&f.field_type)
                                .and_then(|name| kind_paths.get(&name))
                                .cloned();
                            FieldCtx {
                                name: f.name.clone(),
                                required: f.required,
                                type_display,
                                type_href,
                                description: f.description.clone(),
                            }
                        })
                        .collect()
                };
                let fields = order_fields(build_fields(&resource.fields));
                let list_fields = order_fields(build_fields(&resource.list_fields));

                let meta_description = format!(
                    "Kubernetes {} API reference for {}. {}",
                    resource.kind,
                    k8s_version,
                    resource.description.chars().take(120).collect::<String>()
                );

                let json_ld = json!({
                    "@context": "https://schema.org",
                    "@type": "TechArticle",
                    "name": format!("{} — Kubernetes {} API Reference", resource.kind, k8s_version),
                    "description": resource.description,
                    "url": canonical_url,
                })
                .to_string();

                let other_versions: Vec<VersionLink> = versions_by_kind
                    .get(&(resource.group.clone(), resource.kind.clone()))
                    .map(|vs| {
                        vs.iter()
                            .filter(|v| v.api_version != resource.api_version)
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();

                let ctx = ResourcePageCtx {
                    kind: resource.kind.clone(),
                    group_display: group.clone(),
                    api_version: resource.api_version.clone(),
                    k8s_version: k8s_version.clone(),
                    description: resource.description.clone(),
                    fields,
                    list_description: resource.list_description.clone(),
                    list_fields,
                    other_versions,
                    canonical_url,
                    json_ld,
                    breadcrumbs: vec![
                        Crumb { label: "Docs".into(), href: "/docs/".into() },
                        Crumb { label: k8s_version.clone(), href: format!("/docs/{k8s_version}/") },
                        Crumb { label: group.clone(), href: format!("/docs/{k8s_version}/{group}/") },
                        Crumb { label: resource.kind.clone(), href: path },
                    ],
                    meta_description,
                };
                let kind_lower = resource.kind.to_lowercase();
                write_html(
                    &env,
                    "resource.html",
                    &ctx,
                    &out.join(format!(
                        "docs/{k8s_version}/{group}/{}/{kind_lower}/index.html",
                        resource.api_version
                    )),
                )?;
            }
        }
    }

    // One eviction prefix per k8s version: replaces that version's entries in the
    // sitemap so removed resources don't linger across regenerations.
    let evict_prefixes: Vec<String> = by_version
        .keys()
        .map(|v| format!("{base_url}/docs/{v}/"))
        .collect();
    sitemap::generate(&sitemap_urls, &out.join("sitemap.xml"), &evict_prefixes)?;
    println!(
        "Generated {} resource pages + index pages + sitemap.xml",
        resources.len()
    );
    Ok(())
}

fn write_html<T: Serialize>(
    env: &Environment<'_>,
    template: &str,
    ctx: &T,
    path: &Path,
) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let html = env.get_template(template)?.render(ctx)?;
    fs::write(path, html)?;
    Ok(())
}

/// Orders fields: apiVersion / kind / metadata first, then required alpha, then optional alpha.
fn order_fields(fields: Vec<FieldCtx>) -> Vec<FieldCtx> {
    const PINNED: &[&str] = &["apiVersion", "kind", "metadata"];
    let mut top: Vec<FieldCtx> = Vec::new();
    let mut required: Vec<FieldCtx> = Vec::new();
    let mut optional: Vec<FieldCtx> = Vec::new();
    for f in fields {
        if PINNED.contains(&f.name.as_str()) {
            top.push(f);
        } else if f.required {
            required.push(f);
        } else {
            optional.push(f);
        }
    }
    top.sort_by_key(|f| PINNED.iter().position(|&p| p == f.name).unwrap_or(usize::MAX));
    required.sort_by(|a, b| a.name.cmp(&b.name));
    optional.sort_by(|a, b| a.name.cmp(&b.name));
    top.into_iter().chain(required).chain(optional).collect()
}

/// Returns a sortable rank for a Kubernetes API version string.
/// Higher = more recent. v1alpha1 < v1beta1 < v1 < v2.
fn version_rank(v: &str) -> (u32, u32, u32) {
    let s = v.strip_prefix('v').unwrap_or(v);
    if let Some(idx) = s.find("alpha") {
        (s[..idx].parse().unwrap_or(0), 0, s[idx + 5..].parse().unwrap_or(0))
    } else if let Some(idx) = s.find("beta") {
        (s[..idx].parse().unwrap_or(0), 1, s[idx + 4..].parse().unwrap_or(0))
    } else {
        (s.parse().unwrap_or(0), 2, 0)
    }
}

fn group_seg(group: &str) -> String {
    if group.is_empty() {
        "core".to_string()
    } else {
        group.to_string()
    }
}

fn resource_path(r: &Resource) -> String {
    let g = group_seg(&r.group);
    format!("/docs/{}/{g}/{}/{}/", r.k8s_version, r.api_version, r.kind.to_lowercase())
}

fn fmt_field_type(ft: &FieldType) -> String {
    match ft {
        FieldType::Scalar(s) => s.clone(),
        FieldType::Ref(name) => name.clone(),
        FieldType::Array(inner) => format!("[]{}", fmt_field_type(inner)),
        FieldType::Map(inner) => format!("map[string]{}", fmt_field_type(inner)),
        FieldType::Object => "object".to_string(),
    }
}

fn ref_name(ft: &FieldType) -> Option<String> {
    match ft {
        FieldType::Ref(name) => Some(name.clone()),
        FieldType::Array(inner) => ref_name(inner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, required: bool) -> FieldCtx {
        FieldCtx {
            name: name.to_string(),
            required,
            type_display: "string".to_string(),
            type_href: None,
            description: String::new(),
        }
    }

    #[test]
    fn version_rank_ordering() {
        assert!(version_rank("v1alpha1") < version_rank("v1alpha2"));
        assert!(version_rank("v1alpha2") < version_rank("v1beta1"));
        assert!(version_rank("v1beta1") < version_rank("v1beta2"));
        assert!(version_rank("v1beta2") < version_rank("v1"));
        assert!(version_rank("v1")      < version_rank("v2"));
        assert!(version_rank("v2")      > version_rank("v2beta1"));
    }

    #[test]
    fn version_rank_stable_beats_beta_same_major() {
        assert!(version_rank("v1") > version_rank("v1beta1"));
        assert!(version_rank("v1") > version_rank("v1alpha1"));
        assert!(version_rank("v1beta1") > version_rank("v1alpha1"));
    }

    #[test]
    fn order_fields_pinned_first() {
        let fields = vec![
            field("status", false),
            field("metadata", false),
            field("kind", false),
            field("spec", false),
            field("apiVersion", false),
        ];
        let ordered = order_fields(fields);
        assert_eq!(ordered[0].name, "apiVersion");
        assert_eq!(ordered[1].name, "kind");
        assert_eq!(ordered[2].name, "metadata");
    }

    #[test]
    fn order_fields_required_before_optional() {
        let fields = vec![
            field("zoo", false),
            field("alpha", true),
            field("beta", false),
            field("gamma", true),
        ];
        let ordered = order_fields(fields);
        let names: Vec<&str> = ordered.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["alpha", "gamma", "beta", "zoo"]);
    }

    #[test]
    fn order_fields_pinned_not_duplicated_in_required() {
        let fields = vec![
            field("apiVersion", true),
            field("kind", true),
            field("metadata", false),
            field("name", true),
        ];
        let ordered = order_fields(fields);
        let names: Vec<&str> = ordered.iter().map(|f| f.name.as_str()).collect();
        // apiVersion/kind/metadata first, then required non-pinned, no duplicates
        assert_eq!(names, ["apiVersion", "kind", "metadata", "name"]);
    }

    #[test]
    fn group_seg_empty_is_core() {
        assert_eq!(group_seg(""), "core");
        assert_eq!(group_seg("apps"), "apps");
    }

    #[test]
    fn resource_path_core() {
        use crate::model::Resource;
        let r = Resource {
            kind: "Pod".into(),
            group: "".into(),
            api_version: "v1".into(),
            k8s_version: "v1.33".into(),
            description: String::new(),
            fields: vec![],
            list_description: String::new(),
            list_fields: vec![],
        };
        assert_eq!(resource_path(&r), "/docs/v1.33/core/v1/pod/");
    }

    #[test]
    fn resource_path_named_group() {
        use crate::model::Resource;
        let r = Resource {
            kind: "Deployment".into(),
            group: "apps".into(),
            api_version: "v1".into(),
            k8s_version: "v1.33".into(),
            description: String::new(),
            fields: vec![],
            list_description: String::new(),
            list_fields: vec![],
        };
        assert_eq!(resource_path(&r), "/docs/v1.33/apps/v1/deployment/");
    }

    fn make_resource(kind: &str) -> crate::model::Resource {
        crate::model::Resource {
            kind: kind.into(),
            group: "".into(),
            api_version: "v1".into(),
            k8s_version: "v1.33".into(),
            description: String::new(),
            fields: vec![],
            list_description: String::new(),
            list_fields: vec![],
        }
    }

    #[test]
    fn render_evicts_stale_sitemap_entries_on_regeneration() {
        let dir = tempfile::tempdir().unwrap();
        let base = "https://example.com";

        // First render: two resources
        render(&[make_resource("Foo"), make_resource("Bar")], dir.path(), base).unwrap();
        let sitemap = std::fs::read_to_string(dir.path().join("sitemap.xml")).unwrap();
        assert!(sitemap.contains("/docs/v1.33/core/v1/foo/"), "foo must be in sitemap after first render");
        assert!(sitemap.contains("/docs/v1.33/core/v1/bar/"), "bar must be in sitemap after first render");

        // Second render: Bar removed from the spec
        render(&[make_resource("Foo")], dir.path(), base).unwrap();
        let sitemap = std::fs::read_to_string(dir.path().join("sitemap.xml")).unwrap();
        assert!(sitemap.contains("/docs/v1.33/core/v1/foo/"), "foo must still be present");
        assert!(!sitemap.contains("/docs/v1.33/core/v1/bar/"), "stale bar entry must be evicted");
    }

    #[test]
    fn render_preserves_other_version_sitemap_entries() {
        let dir = tempfile::tempdir().unwrap();
        let base = "https://example.com";

        // Render v1.33
        render(&[make_resource("Pod")], dir.path(), base).unwrap();

        // Render v1.34 — must not evict v1.33 entries
        let mut r = make_resource("Pod");
        r.k8s_version = "v1.34".into();
        render(&[r], dir.path(), base).unwrap();

        let sitemap = std::fs::read_to_string(dir.path().join("sitemap.xml")).unwrap();
        assert!(sitemap.contains("/docs/v1.33/"), "v1.33 entries must survive v1.34 render");
        assert!(sitemap.contains("/docs/v1.34/"), "v1.34 entries must be present");
    }
}
