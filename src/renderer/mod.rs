mod copy;
mod pages;
mod sitemap;

use anyhow::Result;
use copy::UiCopy;
use docgen::model::{CommonDefinition, FieldType, Resource};
use minijinja::Environment;
use pages::*;
use pulldown_cmark::{html as cm_html, Options, Parser};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

type TypeData = std::collections::HashMap<String, (String, Vec<docgen::model::Field>)>;

pub struct TypeMaps<'a> {
    pub classifications: &'a std::collections::HashMap<String, bool>,
    pub simple_types: &'a TypeData,
    pub complex_types: &'a TypeData,
}

pub fn render(
    resources: &[Resource],
    common_defs: &[CommonDefinition],
    out: &Path,
    base_url: &str,
    is_latest: bool,
    type_maps: &TypeMaps<'_>,
) -> Result<()> {
    let TypeMaps {
        classifications,
        simple_types,
        complex_types,
    } = type_maps;
    fs::create_dir_all(out.join("docs"))?;
    fs::write(
        out.join("docs/style.css"),
        include_str!("../../templates/style.css"),
    )?;

    let mut env = Environment::new();
    env.add_template("base.html", include_str!("../../templates/base.html"))?;
    env.add_template(
        "resource.html",
        include_str!("../../templates/resource.html"),
    )?;
    env.add_template(
        "group_index.html",
        include_str!("../../templates/group_index.html"),
    )?;
    env.add_template(
        "version_index.html",
        include_str!("../../templates/version_index.html"),
    )?;
    env.add_template(
        "common_def.html",
        include_str!("../../templates/common_def.html"),
    )?;
    env.add_template(
        "common_defs_index.html",
        include_str!("../../templates/common_defs_index.html"),
    )?;

    let nav_prefix_top = if is_latest {
        "latest"
    } else {
        resources
            .first()
            .map(|r| r.k8s_version.as_str())
            .unwrap_or("")
    };
    // Root-relative hrefs used for field type cross-links.
    let kind_paths: std::collections::HashMap<String, String> = resources
        .iter()
        .map(|r| (r.kind.clone(), resource_path(r, nav_prefix_top)))
        .collect();

    // common_defs is pre-filtered by the parser to only referenced definitions.
    let common_def_paths: std::collections::HashMap<String, String> = common_defs
        .iter()
        .map(|cd| {
            (
                cd.name.clone(),
                format!(
                    "/docs/{}/common-definitions/{}/",
                    nav_prefix_top,
                    cd.name.to_lowercase()
                ),
            )
        })
        .collect();
    let mut common_defs_by_version: BTreeMap<String, Vec<&CommonDefinition>> = BTreeMap::new();
    for cd in common_defs {
        common_defs_by_version
            .entry(cd.k8s_version.clone())
            .or_default()
            .push(cd);
    }

    // All versions per (group, kind), sorted most-recent first — used for "other versions" links.
    let mut versions_by_kind: std::collections::HashMap<(String, String), Vec<VersionLink>> =
        std::collections::HashMap::new();
    for r in resources {
        versions_by_kind
            .entry((r.group.clone(), r.kind.clone()))
            .or_default()
            .push(VersionLink {
                api_version: r.api_version.clone(),
                href: resource_path(r, nav_prefix_top),
            });
    }
    for vs in versions_by_kind.values_mut() {
        vs.sort_by_key(|v| std::cmp::Reverse(version_rank(&v.api_version)));
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
        let nav_prefix = if is_latest {
            "latest"
        } else {
            k8s_version.as_str()
        };
        let version_label = if is_latest {
            format!("{k8s_version} (latest)")
        } else {
            k8s_version.clone()
        };

        let version_canonical_path = "/docs/latest/".to_string();
        sitemap_urls.push(format!("{base_url}{version_canonical_path}"));

        let group_links: Vec<GroupLink> = groups
            .keys()
            .map(|g| GroupLink {
                display: g.clone(),
                href: format!("/docs/{nav_prefix}/{g}/"),
            })
            .collect();

        let mut definition_links: Vec<GroupLink> = Vec::new();
        if common_defs_by_version.contains_key(k8s_version.as_str()) {
            definition_links.push(GroupLink {
                display: copy::BREADCRUMB_COMMON_DEFS.to_string(),
                href: format!("/docs/{nav_prefix}/common-definitions/"),
            });
        }

        let version_ctx = VersionIndexCtx {
            k8s_version: k8s_version.clone(),
            k8s_version_display: version_label.clone(),
            groups: group_links,
            definitions: definition_links,
            canonical_url: format!("{base_url}{version_canonical_path}"),
            canonical_path: version_canonical_path,
            breadcrumbs: vec![
                Crumb {
                    label: copy::BREADCRUMB_HOME.into(),
                    href: "/".into(),
                },
                Crumb {
                    label: version_label.clone(),
                    href: format!("/docs/{nav_prefix}/"),
                },
            ],
            meta_description: copy::meta_version_index(k8s_version),
            page_title: copy::title_version_index(&version_label),
            copy: UiCopy::new(),
        };
        write_html(
            &env,
            "version_index.html",
            &version_ctx,
            &out.join(format!("docs/{nav_prefix}/index.html")),
        )?;

        for (group, group_resources) in groups {
            let group_canonical_path = format!("/docs/latest/{group}/");
            sitemap_urls.push(format!("{base_url}{group_canonical_path}"));

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
                        .map(|r| VersionLink {
                            api_version: r.api_version.clone(),
                            href: resource_path(r, nav_prefix),
                        })
                        .collect();
                    ResourceLink { kind, versions }
                })
                .collect();
            resource_links.sort_by(|a, b| a.kind.cmp(&b.kind));

            let group_ctx = GroupIndexCtx {
                group_display: group.clone(),
                k8s_version: k8s_version.clone(),
                k8s_version_display: version_label.clone(),
                resources: resource_links,
                canonical_url: format!("{base_url}{group_canonical_path}"),
                canonical_path: group_canonical_path,
                breadcrumbs: vec![
                    Crumb {
                        label: copy::BREADCRUMB_HOME.into(),
                        href: "/".into(),
                    },
                    Crumb {
                        label: version_label.clone(),
                        href: format!("/docs/{nav_prefix}/"),
                    },
                    Crumb {
                        label: group.clone(),
                        href: format!("/docs/{nav_prefix}/{group}/"),
                    },
                ],
                meta_description: copy::meta_group_index(group, k8s_version),
                page_title: copy::title_group_index(group, &version_label),
                copy: UiCopy::new(),
            };
            write_html(
                &env,
                "group_index.html",
                &group_ctx,
                &out.join(format!("docs/{nav_prefix}/{group}/index.html")),
            )?;

            for resource in group_resources {
                let path = resource_path(resource, nav_prefix);
                let kind_lower = resource.kind.to_lowercase();
                let canonical_path = format!(
                    "/docs/latest/{group}/{}/{kind_lower}/",
                    resource.api_version
                );
                let canonical_url = format!("{base_url}{canonical_path}");
                sitemap_urls.push(canonical_url.clone());

                let mut fields = order_fields(build_fields_ctx(
                    &resource.fields,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                ));
                // Link spec/status type labels to their in-page section anchors.
                for f in &mut fields {
                    if f.name == "spec" && !resource.spec_fields.is_empty() {
                        f.type_href = Some(format!("#{}", resource.spec_name.to_lowercase()));
                        f.type_classification = None;
                        f.type_ref = None;
                        f.type_description = None;
                        f.sub_fields = vec![];
                    } else if f.name == "status" && !resource.status_fields.is_empty() {
                        f.type_href = Some(format!("#{}", resource.status_name.to_lowercase()));
                        f.type_classification = None;
                        f.type_ref = None;
                        f.type_description = None;
                        f.sub_fields = vec![];
                    }
                }
                let list_fields = order_fields(build_fields_ctx(
                    &resource.list_fields,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                ));
                let spec_fields = order_fields(build_fields_ctx(
                    &resource.spec_fields,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                ));
                let status_fields = order_fields(build_fields_ctx(
                    &resource.status_fields,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                ));

                // Collect type sections per section. A shared visited set prevents
                // the same type being rendered more than once on the page.
                let mut visited = std::collections::HashSet::new();
                let field_type_sections = collect_type_sections(
                    &fields,
                    complex_types,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                    &mut visited,
                );
                let spec_type_sections = collect_type_sections(
                    &spec_fields,
                    complex_types,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                    &mut visited,
                );
                let status_type_sections = collect_type_sections(
                    &status_fields,
                    complex_types,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                    &mut visited,
                );
                let list_type_sections = collect_type_sections(
                    &list_fields,
                    complex_types,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                    &mut visited,
                );

                let meta_description =
                    copy::meta_resource(&resource.kind, k8s_version, &resource.description);

                let json_ld = json!({
                    "@context": "https://schema.org",
                    "@type": copy::JSON_LD_TYPE,
                    "name": copy::json_ld_name(&resource.kind, k8s_version),
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
                    kind_lower: kind_lower.clone(),
                    group_display: group.clone(),
                    api_version: resource.api_version.clone(),
                    k8s_version: k8s_version.clone(),
                    k8s_version_display: version_label.clone(),
                    description: md_to_html(&resource.description),
                    fields,
                    field_type_sections,
                    list_description: md_to_html(&resource.list_description),
                    list_fields,
                    list_type_sections,
                    spec_name: resource.spec_name.clone(),
                    spec_description: md_to_html(&resource.spec_description),
                    spec_fields,
                    spec_type_sections,
                    status_name: resource.status_name.clone(),
                    status_description: md_to_html(&resource.status_description),
                    status_fields,
                    status_type_sections,
                    other_versions,
                    canonical_url,
                    canonical_path,
                    json_ld,
                    breadcrumbs: vec![
                        Crumb {
                            label: copy::BREADCRUMB_HOME.into(),
                            href: "/".into(),
                        },
                        Crumb {
                            label: version_label.clone(),
                            href: format!("/docs/{nav_prefix}/"),
                        },
                        Crumb {
                            label: group.clone(),
                            href: format!("/docs/{nav_prefix}/{group}/"),
                        },
                        Crumb {
                            label: resource.kind.clone(),
                            href: path,
                        },
                    ],
                    meta_description,
                    page_title: copy::title_resource(
                        &resource.kind,
                        &resource.api_version,
                        group,
                        &version_label,
                    ),
                    copy: UiCopy::new(),
                };
                write_html(
                    &env,
                    "resource.html",
                    &ctx,
                    &out.join(format!(
                        "docs/{nav_prefix}/{group}/{}/{kind_lower}/index.html",
                        resource.api_version
                    )),
                )?;
            }
        }

        // Render common definition pages and index for this k8s version.
        let version_common_defs = common_defs_by_version
            .get(k8s_version.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();

        if !version_common_defs.is_empty() {
            let idx_canonical_path = "/docs/latest/common-definitions/".to_string();
            sitemap_urls.push(format!("{base_url}{idx_canonical_path}"));

            let categories = group_common_defs_by_category(version_common_defs, nav_prefix);

            let idx_ctx = CommonDefsIndexCtx {
                k8s_version: k8s_version.clone(),
                k8s_version_display: version_label.clone(),
                categories,
                canonical_url: format!("{base_url}{idx_canonical_path}"),
                canonical_path: idx_canonical_path,
                breadcrumbs: vec![
                    Crumb {
                        label: copy::BREADCRUMB_HOME.into(),
                        href: "/".into(),
                    },
                    Crumb {
                        label: version_label.clone(),
                        href: format!("/docs/{nav_prefix}/"),
                    },
                    Crumb {
                        label: copy::BREADCRUMB_COMMON_DEFS.into(),
                        href: format!("/docs/{nav_prefix}/common-definitions/"),
                    },
                ],
                meta_description: copy::meta_common_defs_index(k8s_version),
                page_title: copy::title_common_defs_index(&version_label),
                copy: UiCopy::new(),
            };
            write_html(
                &env,
                "common_defs_index.html",
                &idx_ctx,
                &out.join(format!("docs/{nav_prefix}/common-definitions/index.html")),
            )?;

            for cd in version_common_defs {
                let name_lower = cd.name.to_lowercase();
                let canonical_path = format!("/docs/latest/common-definitions/{name_lower}/");
                let canonical_url = format!("{base_url}{canonical_path}");
                sitemap_urls.push(canonical_url.clone());

                let fields = order_fields(build_fields_ctx(
                    &cd.fields,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                ));
                let mut cd_visited = std::collections::HashSet::new();
                let type_sections = collect_type_sections(
                    &fields,
                    complex_types,
                    &kind_paths,
                    &common_def_paths,
                    classifications,
                    simple_types,
                    &mut cd_visited,
                );
                let ctx = CommonDefPageCtx {
                    name: cd.name.clone(),
                    description: md_to_html(&cd.description),
                    fields,
                    type_sections,
                    k8s_version: k8s_version.clone(),
                    k8s_version_display: version_label.clone(),
                    canonical_url,
                    canonical_path,
                    breadcrumbs: vec![
                        Crumb {
                            label: copy::BREADCRUMB_HOME.into(),
                            href: "/".into(),
                        },
                        Crumb {
                            label: version_label.clone(),
                            href: format!("/docs/{nav_prefix}/"),
                        },
                        Crumb {
                            label: copy::BREADCRUMB_COMMON_DEFS.into(),
                            href: format!("/docs/{nav_prefix}/common-definitions/"),
                        },
                        Crumb {
                            label: cd.name.clone(),
                            href: format!("/docs/{nav_prefix}/common-definitions/{name_lower}/"),
                        },
                    ],
                    meta_description: copy::meta_common_def(&cd.name, k8s_version, &cd.description),
                    page_title: copy::title_common_def(&cd.name, &version_label),
                    copy: UiCopy::new(),
                };
                write_html(
                    &env,
                    "common_def.html",
                    &ctx,
                    &out.join(format!(
                        "docs/{nav_prefix}/common-definitions/{name_lower}/index.html"
                    )),
                )?;
            }
        }
    }

    if is_latest {
        sitemap_urls.push(format!("{base_url}/"));
        // Evict all previous /docs/latest/ entries; each render fully replaces them.
        let evict_prefixes = vec![format!("{base_url}/docs/latest/")];
        sitemap::generate(&sitemap_urls, &out.join("sitemap.xml"), &evict_prefixes)?;

        fs::write(
            out.join("robots.txt"),
            format!(
                "User-agent: *\nAllow: /\nDisallow: /docs/v\n\nSitemap: {base_url}/sitemap.xml\n"
            ),
        )?;
    }

    println!(
        "Generated {} resource pages + index pages{}",
        resources.len(),
        if is_latest {
            " + sitemap.xml + robots.txt"
        } else {
            ""
        }
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
    top.sort_by_key(|f| {
        PINNED
            .iter()
            .position(|&p| p == f.name)
            .unwrap_or(usize::MAX)
    });
    required.sort_by(|a, b| a.name.cmp(&b.name));
    optional.sort_by(|a, b| a.name.cmp(&b.name));
    top.into_iter().chain(required).chain(optional).collect()
}

/// Returns a sortable rank for a Kubernetes API version string.
/// Higher = more recent. v1alpha1 < v1beta1 < v1 < v2.
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

fn common_def_category(name: &str) -> &'static str {
    match name {
        "ObjectMeta" | "ListMeta" => copy::CATEGORY_METADATA,
        "ObjectReference"
        | "LocalObjectReference"
        | "TypedLocalObjectReference"
        | "TypedObjectReference"
        | "CrossVersionObjectReference"
        | "BoundObjectReference" => copy::CATEGORY_REFERENCES,
        "LabelSelector"
        | "NodeSelector"
        | "NodeSelectorRequirement"
        | "ObjectFieldSelector"
        | "ResourceFieldSelector" => copy::CATEGORY_SELECTORS,
        "ResourceRequirements"
        | "Toleration"
        | "ContainerRestartRule"
        | "FileKeySelector"
        | "PodCertificateProjection"
        | "PodSchedulingGroup"
        | "PodSpec"
        | "PodTemplateSpec" => copy::CATEGORY_WORKLOAD,
        "Condition" | "Status" | "DeleteOptions" | "Patch" => copy::CATEGORY_STATUS_OPS,
        "Quantity" | "IntOrString" | "Time" => copy::CATEGORY_TYPES,
        _ => copy::CATEGORY_OTHER,
    }
}

fn group_common_defs_by_category(
    defs: &[&CommonDefinition],
    nav_prefix: &str,
) -> Vec<CommonDefCategory> {
    const ORDER: &[&str] = &[
        copy::CATEGORY_METADATA,
        copy::CATEGORY_REFERENCES,
        copy::CATEGORY_SELECTORS,
        copy::CATEGORY_WORKLOAD,
        copy::CATEGORY_STATUS_OPS,
        copy::CATEGORY_TYPES,
        copy::CATEGORY_OTHER,
    ];

    let mut buckets: std::collections::HashMap<&str, Vec<CommonDefLink>> =
        std::collections::HashMap::new();
    for cd in defs {
        let link = CommonDefLink {
            name: cd.name.clone(),
            href: format!(
                "/docs/{nav_prefix}/common-definitions/{}/",
                cd.name.to_lowercase()
            ),
        };
        buckets
            .entry(common_def_category(&cd.name))
            .or_default()
            .push(link);
    }

    ORDER
        .iter()
        .filter_map(|&cat| {
            buckets.remove(cat).map(|definitions| CommonDefCategory {
                label: cat.to_string(),
                definitions,
            })
        })
        .collect()
}

fn group_seg(group: &str) -> String {
    if group.is_empty() {
        "core".to_string()
    } else {
        group.to_string()
    }
}

fn resource_path(r: &Resource, prefix: &str) -> String {
    let g = group_seg(&r.group);
    format!(
        "/docs/{prefix}/{g}/{}/{}/",
        r.api_version,
        r.kind.to_lowercase()
    )
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
        FieldType::Map(inner) => ref_name(inner),
        _ => None,
    }
}

// Returns (prefix, ref_name) where prefix is the non-linked leading text.
// e.g. Map(Ref("Quantity")) → ("map[string]", "Quantity")
//      Array(Ref("Quantity")) → ("[]", "Quantity")
//      Ref("Quantity")        → ("", "Quantity")
fn ref_type_parts(ft: &FieldType) -> Option<(String, String)> {
    let name = ref_name(ft)?;
    let full = fmt_field_type(ft);
    let prefix = full.strip_suffix(&name).unwrap_or("").to_string();
    Some((prefix, name))
}

fn md_to_html(md: &str) -> String {
    if md.is_empty() {
        return String::new();
    }
    let parser = Parser::new_ext(md, Options::empty());
    let mut out = String::new();
    cm_html::push_html(&mut out, parser);
    linkify_html(out)
}

/// Annotates HTTP(S) links with external-link attributes and wraps bare URLs in `<a>` tags.
///
/// Pulldown-cmark does not auto-link bare URLs; they end up as plain text. This function
/// handles both the markdown-generated anchors (adding attributes) and bare URL text nodes
/// (wrapping them). Text inside existing `<a>` elements is skipped to avoid nesting.
fn linkify_html(html: String) -> String {
    // Annotate <a href="http…"> already generated from markdown link syntax.
    let html = html.replace(
        r#"<a href="http"#,
        r#"<a target="_blank" rel="noopener noreferrer" href="http"#,
    );

    let mut out = String::with_capacity(html.len() + 256);
    let mut rest: &str = &html;

    while !rest.is_empty() {
        let tag_pos = rest.find('<');
        let url_pos = find_bare_url_pos(rest);

        match (tag_pos, url_pos) {
            (None, None) => {
                out.push_str(rest);
                break;
            }
            // Tag comes before (or at the same position as) any bare URL.
            (Some(t), url) if url.is_none_or(|u| t <= u) => {
                out.push_str(&rest[..t]);
                rest = &rest[t..];
                let tag_end = rest.find('>').map(|p| p + 1).unwrap_or(rest.len());
                let tag = &rest[..tag_end];
                out.push_str(tag);
                rest = &rest[tag_end..];
                // Skip inner text of <a> tags verbatim to avoid double-wrapping.
                if tag.starts_with("<a ") || tag == "<a>" {
                    if let Some(close) = rest.find("</a>") {
                        out.push_str(&rest[..close + 4]);
                        rest = &rest[close + 4..];
                    } else {
                        out.push_str(rest);
                        break;
                    }
                }
            }
            // Bare URL comes before the next tag.
            (_, Some(u)) => {
                out.push_str(&rest[..u]);
                rest = &rest[u..];
                let raw_end = rest
                    .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\''))
                    .unwrap_or(rest.len());
                let url = rest[..raw_end].trim_end_matches(['.', ',', ';', ':', ')']);
                out.push_str(&format!(
                    r#"<a href="{url}" target="_blank" rel="noopener noreferrer">{url}</a>"#
                ));
                rest = &rest[url.len()..];
            }
            _ => unreachable!(),
        }
    }

    out
}

/// Returns the byte offset of the first bare `http://` or `https://` that is not already
/// the value of an `href` attribute (i.e. not preceded by `href="`).
fn find_bare_url_pos(s: &str) -> Option<usize> {
    let mut from = 0;
    loop {
        let a = s[from..].find("https://").map(|p| p + from);
        let b = s[from..].find("http://").map(|p| p + from);
        let pos = match (a, b) {
            (None, None) => return None,
            (Some(x), None) | (None, Some(x)) => x,
            (Some(x), Some(y)) => x.min(y),
        };
        if pos >= 6 && (s[pos - 6..pos] == *r#"href=""# || s[pos - 6..pos] == *"href='") {
            from = pos + 7;
            continue;
        }
        return Some(pos);
    }
}

fn build_fields_ctx(
    fields: &[docgen::model::Field],
    kind_paths: &std::collections::HashMap<String, String>,
    common_def_paths: &std::collections::HashMap<String, String>,
    classifications: &std::collections::HashMap<String, bool>,
    simple_types: &std::collections::HashMap<String, (String, Vec<docgen::model::Field>)>,
) -> Vec<FieldCtx> {
    fields
        .iter()
        .map(|f| {
            // Check for an existing cross-page link (common def or resource page).
            let (type_prefix, type_display, existing_href) = match ref_type_parts(&f.field_type)
                .and_then(|(prefix, name)| {
                    let href = common_def_paths
                        .get(&name)
                        .or_else(|| kind_paths.get(&name))
                        .cloned()?;
                    Some((prefix, name, href))
                }) {
                Some((prefix, label, href)) => (prefix, label, Some(href)),
                None => (String::new(), fmt_field_type(&f.field_type), None),
            };

            // When there's no existing cross-page link, classify the type.
            let (type_href, type_classification, type_ref) = if existing_href.is_some() {
                (existing_href, None, None)
            } else if let Some(rn) = ref_name(&f.field_type) {
                match classifications.get(&rn) {
                    Some(true) => {
                        // Complex: link to the in-page type section.
                        let anchor = format!("#type-{}", rn.to_lowercase());
                        (Some(anchor), None, Some(rn))
                    }
                    Some(false) => {
                        // Simple: inline-expand below.
                        (None, Some("simple".to_string()), None)
                    }
                    None => (None, None, None),
                }
            } else {
                (None, None, None)
            };

            // Inline-expand simple types: type description in italic + sub-fields.
            let (type_description, sub_fields) =
                if matches!(type_classification, Some(ref s) if s == "simple") {
                    if let Some(rn) = ref_name(&f.field_type) {
                        if let Some((desc, type_fields)) = simple_types.get(&rn) {
                            let prefix = format!("{}.", f.name);
                            let sfs = order_fields(
                                type_fields
                                    .iter()
                                    .map(|sf| FieldCtx {
                                        name: format!("{}{}", prefix, sf.name),
                                        required: sf.required,
                                        type_prefix: match &sf.field_type {
                                            FieldType::Array(_) => "[]".to_string(),
                                            FieldType::Map(_) => "map[string]".to_string(),
                                            _ => String::new(),
                                        },
                                        type_display: fmt_field_type(leaf_type(&sf.field_type)),
                                        type_href: None,
                                        type_classification: None,
                                        type_ref: None,
                                        description: md_to_html(&sf.description),
                                        type_description: None,
                                        sub_fields: vec![],
                                    })
                                    .collect(),
                            );
                            (Some(md_to_html(desc)), sfs)
                        } else {
                            (None, vec![])
                        }
                    } else {
                        (None, vec![])
                    }
                } else {
                    (None, vec![])
                };

            FieldCtx {
                name: f.name.clone(),
                required: f.required,
                type_prefix,
                type_display,
                type_href,
                type_classification,
                type_ref,
                description: md_to_html(&f.description),
                type_description,
                sub_fields,
            }
        })
        .collect()
}

/// Collects type sections for all complex types transitively reachable from
/// `fields`, in DFS order. `visited` is shared across calls to prevent
/// duplicates within a page.
fn collect_type_sections(
    fields: &[FieldCtx],
    complex_types: &std::collections::HashMap<String, (String, Vec<docgen::model::Field>)>,
    kind_paths: &std::collections::HashMap<String, String>,
    common_def_paths: &std::collections::HashMap<String, String>,
    classifications: &std::collections::HashMap<String, bool>,
    simple_types: &std::collections::HashMap<String, (String, Vec<docgen::model::Field>)>,
    visited: &mut std::collections::HashSet<String>,
) -> Vec<TypeSectionCtx> {
    let mut sections = Vec::new();
    for f in fields {
        let Some(ref type_name) = f.type_ref else {
            continue;
        };
        if !visited.insert(type_name.clone()) {
            continue;
        }
        let Some((desc, raw_fields)) = complex_types.get(type_name) else {
            continue;
        };
        let sub_fields = order_fields(build_fields_ctx(
            raw_fields,
            kind_paths,
            common_def_paths,
            classifications,
            simple_types,
        ));
        // Recurse into the sub-fields to collect their complex type sections.
        let mut sub_sections = collect_type_sections(
            &sub_fields,
            complex_types,
            kind_paths,
            common_def_paths,
            classifications,
            simple_types,
            visited,
        );
        sections.push(TypeSectionCtx {
            anchor: format!("type-{}", type_name.to_lowercase()),
            name: type_name.clone(),
            description: md_to_html(desc),
            fields: sub_fields,
        });
        sections.append(&mut sub_sections);
    }
    sections
}

/// Returns the leaf FieldType after stripping Array/Map wrappers.
fn leaf_type(ft: &FieldType) -> &FieldType {
    match ft {
        FieldType::Array(inner) | FieldType::Map(inner) => leaf_type(inner),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, required: bool) -> FieldCtx {
        FieldCtx {
            name: name.to_string(),
            required,
            type_prefix: String::new(),
            type_display: "string".to_string(),
            type_href: None,
            type_classification: None,
            type_ref: None,
            description: String::new(),
            type_description: None,
            sub_fields: vec![],
        }
    }

    #[test]
    fn version_rank_ordering() {
        assert!(version_rank("v1alpha1") < version_rank("v1alpha2"));
        assert!(version_rank("v1alpha2") < version_rank("v1beta1"));
        assert!(version_rank("v1beta1") < version_rank("v1beta2"));
        assert!(version_rank("v1beta2") < version_rank("v1"));
        assert!(version_rank("v1") < version_rank("v2"));
        assert!(version_rank("v2") > version_rank("v2beta1"));
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
        use docgen::model::Resource;
        let r = Resource {
            kind: "Pod".into(),
            group: "".into(),
            api_version: "v1".into(),
            k8s_version: "v1.33".into(),
            description: String::new(),
            fields: vec![],
            list_description: String::new(),
            list_fields: vec![],
            spec_name: String::new(),
            spec_description: String::new(),
            spec_fields: vec![],
            status_name: String::new(),
            status_description: String::new(),
            status_fields: vec![],
        };
        assert_eq!(resource_path(&r, "v1.33"), "/docs/v1.33/core/v1/pod/");
        assert_eq!(resource_path(&r, "latest"), "/docs/latest/core/v1/pod/");
    }

    #[test]
    fn resource_path_named_group() {
        use docgen::model::Resource;
        let r = Resource {
            kind: "Deployment".into(),
            group: "apps".into(),
            api_version: "v1".into(),
            k8s_version: "v1.33".into(),
            description: String::new(),
            fields: vec![],
            list_description: String::new(),
            list_fields: vec![],
            spec_name: String::new(),
            spec_description: String::new(),
            spec_fields: vec![],
            status_name: String::new(),
            status_description: String::new(),
            status_fields: vec![],
        };
        assert_eq!(
            resource_path(&r, "v1.33"),
            "/docs/v1.33/apps/v1/deployment/"
        );
        assert_eq!(
            resource_path(&r, "latest"),
            "/docs/latest/apps/v1/deployment/"
        );
    }

    fn make_common_def(name: &str) -> docgen::model::CommonDefinition {
        docgen::model::CommonDefinition {
            name: name.into(),
            description: format!("{name} is a common definition."),
            fields: vec![],
            k8s_version: "v1.33".into(),
        }
    }

    fn make_resource(kind: &str) -> docgen::model::Resource {
        docgen::model::Resource {
            kind: kind.into(),
            group: "".into(),
            api_version: "v1".into(),
            k8s_version: "v1.33".into(),
            description: String::new(),
            fields: vec![],
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

    fn model_field(name: &str, description: &str) -> docgen::model::Field {
        docgen::model::Field {
            name: name.into(),
            description: description.into(),
            required: false,
            field_type: docgen::model::FieldType::Scalar("string".into()),
        }
    }

    #[test]
    fn render_evicts_stale_sitemap_entries_on_regeneration() {
        let dir = tempfile::tempdir().unwrap();
        let base = "https://example.com";

        // First render: two resources
        render(
            &[make_resource("Foo"), make_resource("Bar")],
            &[],
            dir.path(),
            base,
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let sitemap = std::fs::read_to_string(dir.path().join("sitemap.xml")).unwrap();
        assert!(
            sitemap.contains("/docs/latest/core/v1/foo/"),
            "foo must be in sitemap after first render"
        );
        assert!(
            sitemap.contains("/docs/latest/core/v1/bar/"),
            "bar must be in sitemap after first render"
        );

        // Second render: Bar removed from the spec
        render(
            &[make_resource("Foo")],
            &[],
            dir.path(),
            base,
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let sitemap = std::fs::read_to_string(dir.path().join("sitemap.xml")).unwrap();
        assert!(
            sitemap.contains("/docs/latest/core/v1/foo/"),
            "foo must still be present"
        );
        assert!(
            !sitemap.contains("/docs/latest/core/v1/bar/"),
            "stale bar entry must be evicted"
        );
    }

    #[test]
    fn render_sitemap_uses_latest_urls_only() {
        let dir = tempfile::tempdir().unwrap();
        let base = "https://example.com";

        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            base,
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let sitemap = std::fs::read_to_string(dir.path().join("sitemap.xml")).unwrap();
        assert!(
            sitemap.contains("/docs/latest/"),
            "sitemap must use /docs/latest/ URLs"
        );
        assert!(
            !sitemap.contains("/docs/v1.33/"),
            "sitemap must not contain versioned URLs"
        );
    }

    #[test]
    fn render_sitemap_includes_homepage() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let sitemap = std::fs::read_to_string(dir.path().join("sitemap.xml")).unwrap();
        assert!(
            sitemap.contains("<loc>https://example.com/</loc>"),
            "sitemap must include the homepage URL"
        );
    }

    #[test]
    fn render_writes_robots_txt() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let robots = std::fs::read_to_string(dir.path().join("robots.txt")).unwrap();
        assert!(robots.contains("Allow: /"));
        assert!(robots.contains("Disallow: /docs/v"));
        assert!(robots.contains("Sitemap: https://example.com/sitemap.xml"));
    }

    #[test]
    fn render_robots_txt_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("robots.txt"), "old content").unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let robots = std::fs::read_to_string(dir.path().join("robots.txt")).unwrap();
        assert!(
            !robots.contains("old content"),
            "robots.txt must be overwritten"
        );
        assert!(robots.contains("Allow: /"));
    }

    #[test]
    fn render_json_ld_url_uses_latest() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("https://example.com/docs/latest/core/v1/pod/"),
            "JSON-LD url must use /docs/latest/ canonical URL"
        );
        assert!(
            !html.contains("\"url\":\"https://example.com/docs/v1.33/"),
            "JSON-LD url must not reference versioned URL"
        );
    }

    #[test]
    fn render_is_latest_writes_to_docs_latest() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        assert!(
            dir.path()
                .join("docs/latest/core/v1/pod/index.html")
                .exists(),
            "resource page must be written under docs/latest/ when is_latest"
        );
        assert!(
            dir.path().join("docs/latest/core/index.html").exists(),
            "group index must be written under docs/latest/ when is_latest"
        );
        assert!(
            dir.path().join("docs/latest/index.html").exists(),
            "version index must be written under docs/latest/ when is_latest"
        );
        assert!(
            !dir.path().join("docs/v1.33").exists(),
            "versioned directory must not be written when is_latest"
        );
    }

    #[test]
    fn render_is_latest_links_use_latest_prefix() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("/docs/latest/core/"),
            "breadcrumb hrefs must use /docs/latest/ prefix"
        );
        assert!(
            !html.contains("/docs/v1.33/"),
            "no versioned hrefs must appear in is_latest pages"
        );
    }

    #[test]
    fn render_canonical_link_points_to_latest() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();

        // Resource page written to /docs/latest/ — canonical is self-referential
        let resource_html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            resource_html.contains(
                r#"<link rel="canonical" href="https://example.com/docs/latest/core/v1/pod/">"#
            ),
            "resource page canonical must point to /docs/latest/"
        );

        // Group index: /docs/latest/core/index.html
        let group_html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/index.html")).unwrap();
        assert!(
            group_html
                .contains(r#"<link rel="canonical" href="https://example.com/docs/latest/core/">"#),
            "group index canonical must point to /docs/latest/"
        );

        // Version index: /docs/latest/index.html
        let version_html =
            std::fs::read_to_string(dir.path().join("docs/latest/index.html")).unwrap();
        assert!(
            version_html
                .contains(r#"<link rel="canonical" href="https://example.com/docs/latest/">"#),
            "version index canonical must point to /docs/latest/"
        );
    }

    #[test]
    fn render_without_is_latest_writes_to_versioned_dir() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        assert!(
            dir.path()
                .join("docs/v1.33/core/v1/pod/index.html")
                .exists(),
            "resource page must be written under docs/v1.33/ when not is_latest"
        );
        assert!(
            !dir.path().join("docs/latest").exists(),
            "docs/latest must not be written when not is_latest"
        );
    }

    #[test]
    fn render_version_label_shows_latest_suffix() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html = std::fs::read_to_string(dir.path().join("docs/latest/index.html")).unwrap();
        assert!(
            html.contains("v1.33 (latest)"),
            "version index title must show 'v1.33 (latest)' when is_latest"
        );
    }

    #[test]
    fn render_without_is_latest_skips_sitemap_and_robots() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        assert!(
            !dir.path().join("sitemap.xml").exists(),
            "sitemap.xml must not be written when not is_latest"
        );
        assert!(
            !dir.path().join("robots.txt").exists(),
            "robots.txt must not be written when not is_latest"
        );
    }

    #[test]
    fn render_home_breadcrumb_label_and_href() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        for path in [
            "docs/latest/index.html",
            "docs/latest/core/index.html",
            "docs/latest/core/v1/pod/index.html",
        ] {
            let html = std::fs::read_to_string(dir.path().join(path)).unwrap();
            assert!(
                html.contains(r#"href="/">"#),
                "{path}: Home breadcrumb must link to /"
            );
            assert!(
                html.contains(">Home<"),
                "{path}: Home breadcrumb label must be 'Home'"
            );
        }
    }

    #[test]
    fn render_page_title_resource() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains(
                "<title>Pod v1 (core) &mdash; Kubernetes v1.33 (latest) | Kubernetes API Reference</title>"
            ),
            "resource page title must use &mdash; and correct format"
        );
    }

    #[test]
    fn render_page_title_group_index() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html = std::fs::read_to_string(dir.path().join("docs/latest/core/index.html")).unwrap();
        assert!(
            html.contains(
                "<title>core &mdash; Kubernetes v1.33 (latest) API Reference | Kubernetes API Reference</title>"
            ),
            "group index title must use &mdash; and correct format"
        );
    }

    #[test]
    fn render_page_title_version_index() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html = std::fs::read_to_string(dir.path().join("docs/latest/index.html")).unwrap();
        assert!(
            html.contains(
                "<title>Kubernetes v1.33 (latest) API Reference | Kubernetes API Reference</title>"
            ),
            "version index title must have correct format"
        );
    }

    #[test]
    fn render_json_ld_name_uses_unicode_em_dash() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("\"name\":\"Pod \u{2014} Kubernetes v1.33 API Reference\""),
            "JSON-LD name must use Unicode em dash, not &mdash;"
        );
        assert!(
            !html.contains("\"name\":\"Pod &mdash;"),
            "JSON-LD name must not contain HTML entity &mdash;"
        );
    }

    #[test]
    fn apiversion_field_core_shows_version_value_not_type_or_description() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![model_field(
            "apiVersion",
            "APIVersion defines the versioned schema.",
        )];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("<code>v1</code>"),
            "apiVersion must show the api version value for core resources"
        );
        assert!(
            !html.contains(r#"apiVersion<span class="type">"#),
            "apiVersion must not render a type span"
        );
        assert!(
            !html.contains("APIVersion defines the versioned schema."),
            "apiVersion must not render its description"
        );
    }

    #[test]
    fn apiversion_field_named_group_shows_group_slash_version() {
        let dir = tempfile::tempdir().unwrap();
        let r = docgen::model::Resource {
            kind: "Deployment".into(),
            group: "apps".into(),
            api_version: "v1".into(),
            k8s_version: "v1.33".into(),
            description: String::new(),
            fields: vec![model_field(
                "apiVersion",
                "APIVersion defines the versioned schema.",
            )],
            list_description: String::new(),
            list_fields: vec![],
            spec_name: String::new(),
            spec_description: String::new(),
            spec_fields: vec![],
            status_name: String::new(),
            status_description: String::new(),
            status_fields: vec![],
        };
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/apps/v1/deployment/index.html"))
                .unwrap();
        assert!(
            html.contains("<code>apps/v1</code>"),
            "apiVersion must show group/version for named-group resources"
        );
    }

    #[test]
    fn kind_field_shows_resource_kind_name_not_type_or_description() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![model_field(
            "kind",
            "Kind is a string value representing the REST resource.",
        )];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("<code>Pod</code>"),
            "kind must show the resource kind name"
        );
        assert!(
            !html.contains(r#"kind<span class="type">"#),
            "kind must not render a type span"
        );
        assert!(
            !html.contains("Kind is a string value representing the REST resource."),
            "kind must not render its description"
        );
    }

    #[test]
    fn list_kind_field_shows_kind_list_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.list_fields = vec![
            model_field("apiVersion", "APIVersion defines the versioned schema."),
            model_field(
                "kind",
                "Kind is a string value representing the REST resource.",
            ),
        ];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("<code>PodList</code>"),
            "list kind must show the kind name suffixed with List"
        );
        assert!(
            !html.contains("Kind is a string value representing the REST resource."),
            "list kind must not render its description"
        );
    }

    fn ref_field(name: &str, ref_type: &str, description: &str) -> docgen::model::Field {
        docgen::model::Field {
            name: name.into(),
            description: description.into(),
            required: false,
            field_type: docgen::model::FieldType::Ref(ref_type.into()),
        }
    }

    #[test]
    fn spec_section_renders_when_spec_fields_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.spec_name = "PodSpec".into();
        r.spec_description = "PodSpec is a description of a pod.".into();
        r.spec_fields = vec![
            model_field("nodeName", "Name of the node."),
            model_field("restartPolicy", "Restart policy."),
        ];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains(r#"id="podspec""#),
            "spec section must have id=podspec"
        );
        assert!(html.contains("PodSpec"), "spec heading must say PodSpec");
        assert!(
            html.contains("PodSpec is a description of a pod."),
            "spec description must be rendered"
        );
        assert!(
            html.contains("nodeName"),
            "spec field nodeName must be rendered"
        );
        assert!(
            html.contains("restartPolicy"),
            "spec field restartPolicy must be rendered"
        );
    }

    #[test]
    fn status_section_renders_when_status_fields_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.status_name = "PodStatus".into();
        r.status_description = "PodStatus represents the status of a pod.".into();
        r.status_fields = vec![
            model_field("hostIP", "IP address of the host."),
            model_field("phase", "Phase of the pod."),
        ];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains(r#"id="podstatus""#),
            "status section must have id=podstatus"
        );
        assert!(
            html.contains("PodStatus"),
            "status heading must say PodStatus"
        );
        assert!(
            html.contains("PodStatus represents the status of a pod."),
            "status description must be rendered"
        );
        assert!(
            html.contains("phase"),
            "status field phase must be rendered"
        );
    }

    #[test]
    fn spec_type_href_links_to_in_page_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![ref_field("spec", "PodSpec", "Spec of the pod.")];
        r.spec_name = "PodSpec".into();
        r.spec_fields = vec![model_field("nodeName", "Name of the node.")];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("href=\"#podspec\""),
            "spec field type must link to #podspec"
        );
    }

    #[test]
    fn status_type_href_links_to_in_page_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![ref_field("status", "PodStatus", "Status of the pod.")];
        r.status_name = "PodStatus".into();
        r.status_fields = vec![model_field("phase", "Phase of the pod.")];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("href=\"#podstatus\""),
            "status field type must link to #podstatus"
        );
    }

    #[test]
    fn spec_type_href_not_set_when_spec_fields_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![ref_field("spec", "PodSpec", "Spec of the pod.")];
        // spec_fields left empty — no anchor should be generated
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            !html.contains("href=\"#podspec\""),
            "spec field must not link to anchor when spec_fields is empty"
        );
    }

    #[test]
    fn no_spec_or_status_section_for_resources_without_sub_fields() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("ConfigMap")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/configmap/index.html"))
                .unwrap();
        assert!(
            !html.contains("ConfigMapSpec"),
            "no spec section for resources without spec_fields"
        );
        assert!(
            !html.contains("ConfigMapStatus"),
            "no status section for resources without status_fields"
        );
    }

    #[test]
    fn spec_section_uses_referenced_schema_name_not_kind_name() {
        // LocalSubjectAccessReview has a spec field of type SubjectAccessReviewSpec,
        // so the section heading and anchor must say SubjectAccessReviewSpec, not
        // LocalSubjectAccessReviewSpec.
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("LocalSubjectAccessReview");
        r.spec_name = "SubjectAccessReviewSpec".into();
        r.spec_fields = vec![model_field("user", "User is the user you're testing for.")];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html = std::fs::read_to_string(
            dir.path()
                .join("docs/latest/core/v1/localsubjectaccessreview/index.html"),
        )
        .unwrap();
        assert!(
            html.contains(r#"id="subjectaccessreviewspec""#),
            "spec section id must use the referenced schema name"
        );
        assert!(
            html.contains("SubjectAccessReviewSpec"),
            "spec heading must say SubjectAccessReviewSpec"
        );
        assert!(
            !html.contains("LocalSubjectAccessReviewSpec"),
            "spec heading must not use the resource kind name"
        );
    }

    #[test]
    fn spec_section_anchor_uses_lowercase_spec_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("ReplicaSet");
        r.spec_name = "ReplicaSetSpec".into();
        r.spec_fields = vec![model_field("replicas", "Number of replicas.")];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/replicaset/index.html"))
                .unwrap();
        assert!(
            html.contains(r#"id="replicasetspec""#),
            "spec section id must be lowercase spec name"
        );
    }

    #[test]
    fn common_def_index_page_is_generated() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[make_common_def("ObjectMeta")],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        assert!(
            dir.path()
                .join("docs/v1.33/common-definitions/index.html")
                .exists(),
            "common definitions index must be generated"
        );
    }

    #[test]
    fn common_def_page_is_generated() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[make_common_def("ObjectMeta")],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        assert!(
            dir.path()
                .join("docs/v1.33/common-definitions/objectmeta/index.html")
                .exists(),
            "ObjectMeta page must be generated at lowercased path"
        );
    }

    #[test]
    fn common_def_pages_absent_when_no_common_defs() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        assert!(
            !dir.path().join("docs/v1.33/common-definitions").exists(),
            "common-definitions directory must not be created when there are no common defs"
        );
    }

    #[test]
    fn common_def_with_no_fields_omits_fields_section() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[make_common_def("IntOrString")],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html = std::fs::read_to_string(
            dir.path()
                .join("docs/v1.33/common-definitions/intorstring/index.html"),
        )
        .unwrap();
        assert!(
            !html.contains("Fields"),
            "common def page with no fields must not render a Fields section"
        );
        assert!(
            !html.contains("No fields documented"),
            "common def page with no fields must not show the no-fields message"
        );
    }

    #[test]
    fn common_def_with_fields_shows_fields_section() {
        let dir = tempfile::tempdir().unwrap();
        let mut cd = make_common_def("Condition");
        cd.fields = vec![model_field("type", "Type of the condition.")];
        render(
            &[make_resource("Pod")],
            &[cd],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html = std::fs::read_to_string(
            dir.path()
                .join("docs/v1.33/common-definitions/condition/index.html"),
        )
        .unwrap();
        assert!(
            html.contains("Fields"),
            "common def page with fields must render a Fields section"
        );
        assert!(
            html.contains("Type of the condition."),
            "common def page must render the field description"
        );
    }

    #[test]
    fn common_def_index_groups_by_category() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[
                make_common_def("ObjectMeta"), // Metadata
                make_common_def("Toleration"), // Workload
                make_common_def("Condition"),  // Status & Operations
            ],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/v1.33/common-definitions/index.html"))
                .unwrap();
        assert!(
            html.contains("Metadata"),
            "index must show Metadata category"
        );
        assert!(
            html.contains("Workload"),
            "index must show Workload category"
        );
        assert!(
            html.contains("Status &amp; Operations"),
            "index must show Status &amp; Operations category"
        );
    }

    #[test]
    fn common_def_index_metadata_before_workload() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[
                make_common_def("Toleration"), // Workload
                make_common_def("ObjectMeta"), // Metadata
            ],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/v1.33/common-definitions/index.html"))
                .unwrap();
        let metadata_pos = html.find("Metadata").unwrap();
        let workload_pos = html.find("Workload").unwrap();
        assert!(
            metadata_pos < workload_pos,
            "Metadata category must appear before Workload in the index"
        );
    }

    #[test]
    fn resource_field_ref_to_common_def_gets_href() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![docgen::model::Field {
            name: "metadata".into(),
            description: "Standard object metadata.".into(),
            required: false,
            field_type: docgen::model::FieldType::Ref("ObjectMeta".into()),
        }];
        render(
            &[r],
            &[make_common_def("ObjectMeta")],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/v1.33/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("/common-definitions/objectmeta/"),
            "metadata field must link to the ObjectMeta common definition page"
        );
    }

    #[test]
    fn version_index_links_to_common_definitions_when_defs_present() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[make_common_def("ObjectMeta")],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html = std::fs::read_to_string(dir.path().join("docs/v1.33/index.html")).unwrap();
        assert!(
            html.contains("common-definitions"),
            "version index must contain a link to common-definitions"
        );
    }

    #[test]
    fn version_index_no_common_definitions_link_when_no_defs() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html = std::fs::read_to_string(dir.path().join("docs/v1.33/index.html")).unwrap();
        assert!(
            !html.contains("common-definitions"),
            "version index must not mention common-definitions when none are present"
        );
    }

    #[test]
    fn render_writes_style_css() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        assert!(
            dir.path().join("docs/style.css").exists(),
            "render must write docs/style.css"
        );
        let css = std::fs::read_to_string(dir.path().join("docs/style.css")).unwrap();
        assert!(!css.is_empty(), "docs/style.css must not be empty");
    }

    #[test]
    fn resource_description_is_rendered_as_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.description = "A **bold** description with `code`.".into();
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("<strong>bold</strong>"),
            "resource description must render markdown bold"
        );
        assert!(
            html.contains("<code>code</code>"),
            "resource description must render markdown inline code"
        );
    }

    #[test]
    fn field_description_is_rendered_as_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![model_field("name", "A **required** field with `type`.")];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("<strong>required</strong>"),
            "field description must render markdown bold"
        );
        assert!(
            html.contains("<code>type</code>"),
            "field description must render markdown inline code"
        );
    }

    #[test]
    fn bare_urls_are_linkified() {
        let html =
            md_to_html("More info: https://kubernetes.io/docs/concepts/ and http://example.com.");
        assert!(html.contains(r#"<a href="https://kubernetes.io/docs/concepts/" target="_blank" rel="noopener noreferrer">https://kubernetes.io/docs/concepts/</a>"#));
        assert!(html.contains(r#"<a href="http://example.com" target="_blank" rel="noopener noreferrer">http://example.com</a>"#));
        // trailing period must not be part of the URL
        assert!(!html.contains("http://example.com.\""));
    }

    #[test]
    fn bare_url_at_end_of_string_is_linkified() {
        let html = md_to_html("See https://kubernetes.io/docs/");
        assert!(
            html.contains(
                r#"<a href="https://kubernetes.io/docs/" target="_blank" rel="noopener noreferrer">https://kubernetes.io/docs/</a>"#
            ),
            "URL at end of string with no trailing text must be linkified"
        );
    }

    #[test]
    fn description_without_urls_is_unchanged() {
        let html = md_to_html("A plain description with no links.");
        assert!(
            !html.contains("<a "),
            "description without URLs must not contain any anchor tags"
        );
        assert!(
            html.contains("A plain description with no links."),
            "description text must be preserved"
        );
    }

    #[test]
    fn url_in_href_attribute_is_not_double_wrapped() {
        let html = md_to_html("See [kubernetes.io](https://kubernetes.io/).");
        let count = html.matches(r#"href="https://kubernetes.io/""#).count();
        assert_eq!(
            count, 1,
            "href URL must appear exactly once, not be double-wrapped"
        );
    }

    #[test]
    fn markdown_links_get_external_attributes() {
        let html = md_to_html("See [the docs](https://kubernetes.io/docs/).");
        assert!(html.contains(
            r#"target="_blank" rel="noopener noreferrer" href="https://kubernetes.io/docs/""#
        ));
        // text inside <a> must not be re-wrapped
        assert!(!html.contains("<a href=\"the docs\""));
    }

    #[test]
    fn common_def_urls_in_sitemap_when_is_latest() {
        let dir = tempfile::tempdir().unwrap();
        render(
            &[make_resource("Pod")],
            &[make_common_def("ObjectMeta")],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let sitemap = std::fs::read_to_string(dir.path().join("sitemap.xml")).unwrap();
        assert!(
            sitemap.contains("/docs/latest/common-definitions/objectmeta/"),
            "sitemap must include individual common def URL"
        );
        assert!(
            sitemap.contains("/docs/latest/common-definitions/"),
            "sitemap must include common definitions index URL"
        );
    }

    // ref_type_parts unit tests

    #[test]
    fn ref_type_parts_direct_ref() {
        let ft = FieldType::Ref("Quantity".into());
        let (prefix, name) = ref_type_parts(&ft).unwrap();
        assert_eq!(prefix, "");
        assert_eq!(name, "Quantity");
    }

    #[test]
    fn ref_type_parts_array_of_ref() {
        let ft = FieldType::Array(Box::new(FieldType::Ref("Quantity".into())));
        let (prefix, name) = ref_type_parts(&ft).unwrap();
        assert_eq!(prefix, "[]");
        assert_eq!(name, "Quantity");
    }

    #[test]
    fn ref_type_parts_map_of_ref() {
        let ft = FieldType::Map(Box::new(FieldType::Ref("Quantity".into())));
        let (prefix, name) = ref_type_parts(&ft).unwrap();
        assert_eq!(prefix, "map[string]");
        assert_eq!(name, "Quantity");
    }

    #[test]
    fn ref_type_parts_scalar_returns_none() {
        let ft = FieldType::Scalar("string".into());
        assert!(ref_type_parts(&ft).is_none());
    }

    #[test]
    fn ref_type_parts_map_of_scalar_returns_none() {
        let ft = FieldType::Map(Box::new(FieldType::Scalar("string".into())));
        assert!(ref_type_parts(&ft).is_none());
    }

    // render tests — only the ref name is linked, prefix is plain text

    fn map_ref_field(name: &str, ref_type: &str) -> docgen::model::Field {
        docgen::model::Field {
            name: name.into(),
            description: String::new(),
            required: false,
            field_type: docgen::model::FieldType::Map(Box::new(docgen::model::FieldType::Ref(
                ref_type.into(),
            ))),
        }
    }

    fn array_ref_field(name: &str, ref_type: &str) -> docgen::model::Field {
        docgen::model::Field {
            name: name.into(),
            description: String::new(),
            required: false,
            field_type: docgen::model::FieldType::Array(Box::new(docgen::model::FieldType::Ref(
                ref_type.into(),
            ))),
        }
    }

    #[test]
    fn map_ref_field_links_only_ref_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![map_ref_field("overhead", "Quantity")];
        render(
            &[r],
            &[make_common_def("Quantity")],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/v1.33/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains(r#"map[string]<a href="/docs/v1.33/common-definitions/quantity/"#),
            "prefix map[string] must be plain text; only Quantity must be the link"
        );
        assert!(
            !html.contains(r#"><a href="/docs/v1.33/common-definitions/quantity/"#),
            "the link must not start immediately after '>': prefix must precede it"
        );
    }

    #[test]
    fn array_ref_field_links_only_ref_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![array_ref_field("tolerations", "Toleration")];
        render(
            &[r],
            &[make_common_def("Toleration")],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/v1.33/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains(r#"[]<a href="/docs/v1.33/common-definitions/toleration/"#),
            "prefix [] must be plain text; only Toleration must be the link"
        );
    }

    #[test]
    fn map_of_scalar_field_has_no_link() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![docgen::model::Field {
            name: "labels".into(),
            description: String::new(),
            required: false,
            field_type: docgen::model::FieldType::Map(Box::new(docgen::model::FieldType::Scalar(
                "string".into(),
            ))),
        }];
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            false,
            &TypeMaps {
                classifications: &std::collections::HashMap::new(),
                simple_types: &std::collections::HashMap::new(),
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/v1.33/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("map[string]string"),
            "map[string]string must be rendered as plain text"
        );
        assert!(
            !html.contains(r#"map[string]string</a>"#),
            "map[string]string must not be wrapped in a link"
        );
    }

    // --- simple type inline expansion ---

    #[test]
    fn simple_type_expands_sub_fields_inline() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.spec_fields = vec![ref_field("tolerations", "Toleration", "Pod tolerations.")];
        let mut simple_types = std::collections::HashMap::new();
        simple_types.insert(
            "Toleration".to_string(),
            (
                "Toleration allows a pod to tolerate a taint.".to_string(),
                vec![
                    model_field("key", "Taint key."),
                    model_field("value", "Taint value."),
                ],
            ),
        );
        let mut classifications = std::collections::HashMap::new();
        classifications.insert("Toleration".to_string(), false); // simple
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &classifications,
                simple_types: &simple_types,
                complex_types: &std::collections::HashMap::new(),
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("tolerations.key"),
            "sub-field tolerations.key must be rendered inline"
        );
        assert!(
            html.contains("tolerations.value"),
            "sub-field tolerations.value must be rendered inline"
        );
        assert!(
            html.contains("Toleration allows a pod to tolerate a taint."),
            "type description must appear inline"
        );
    }

    // --- complex type sections ---

    #[test]
    fn complex_type_field_links_to_in_page_type_section() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.spec_fields = vec![ref_field("containers", "Container", "List of containers.")];
        let mut classifications = std::collections::HashMap::new();
        classifications.insert("Container".to_string(), true); // complex
        let mut complex_types = std::collections::HashMap::new();
        complex_types.insert(
            "Container".to_string(),
            ("A container in a pod.".to_string(), vec![]),
        );
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &classifications,
                simple_types: &std::collections::HashMap::new(),
                complex_types: &complex_types,
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains("href=\"#type-container\""),
            "complex field must link to #type-container"
        );
        assert!(
            html.contains(r#"id="type-container""#),
            "type section must have id=type-container"
        );
    }

    #[test]
    fn complex_type_section_renders_after_its_parent_section() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.spec_name = "PodSpec".into();
        r.spec_fields = vec![ref_field("containers", "Container", "List of containers.")];
        let mut classifications = std::collections::HashMap::new();
        classifications.insert("Container".to_string(), true);
        let mut complex_types = std::collections::HashMap::new();
        complex_types.insert(
            "Container".to_string(),
            (
                "A container in a pod.".to_string(),
                vec![model_field("name", "Container name.")],
            ),
        );
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &classifications,
                simple_types: &std::collections::HashMap::new(),
                complex_types: &complex_types,
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        let spec_pos = html
            .find(r#"id="podspec""#)
            .expect("spec section not found");
        let type_pos = html
            .find(r#"id="type-container""#)
            .expect("type section not found");
        assert!(
            spec_pos < type_pos,
            "spec section must appear before the Container type section"
        );
    }

    #[test]
    fn complex_type_section_not_duplicated_when_referenced_twice() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.spec_fields = vec![
            ref_field("initContainers", "Container", "Init containers."),
            ref_field("containers", "Container", "Containers."),
        ];
        let mut classifications = std::collections::HashMap::new();
        classifications.insert("Container".to_string(), true);
        let mut complex_types = std::collections::HashMap::new();
        complex_types.insert(
            "Container".to_string(),
            ("A container in a pod.".to_string(), vec![]),
        );
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &classifications,
                simple_types: &std::collections::HashMap::new(),
                complex_types: &complex_types,
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        let count = html.matches(r#"id="type-container""#).count();
        assert_eq!(count, 1, "Container type section must appear exactly once");
    }

    #[test]
    fn complex_type_sections_are_rendered_recursively() {
        // Container (complex) has a field of type Probe (complex).
        // Both must get type sections; Probe must appear after Container.
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.spec_fields = vec![ref_field("containers", "Container", "Containers.")];
        let mut classifications = std::collections::HashMap::new();
        classifications.insert("Container".to_string(), true);
        classifications.insert("Probe".to_string(), true);
        let mut complex_types = std::collections::HashMap::new();
        complex_types.insert(
            "Container".to_string(),
            (
                "A container.".to_string(),
                vec![ref_field("livenessProbe", "Probe", "Liveness probe.")],
            ),
        );
        complex_types.insert(
            "Probe".to_string(),
            ("Probe describes a health check.".to_string(), vec![]),
        );
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &classifications,
                simple_types: &std::collections::HashMap::new(),
                complex_types: &complex_types,
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            html.contains(r#"id="type-container""#),
            "Container type section must be rendered"
        );
        assert!(
            html.contains(r#"id="type-probe""#),
            "Probe type section must be rendered"
        );
        let container_pos = html.find(r#"id="type-container""#).unwrap();
        let probe_pos = html.find(r#"id="type-probe""#).unwrap();
        assert!(
            container_pos < probe_pos,
            "Container section must appear before Probe section"
        );
    }

    #[test]
    fn spec_field_does_not_generate_type_section_when_spec_fields_expanded() {
        // The 'spec' field in main fields links to the spec sub-section, not a
        // type section — no #type-podspec should appear in the output.
        let dir = tempfile::tempdir().unwrap();
        let mut r = make_resource("Pod");
        r.fields = vec![ref_field("spec", "PodSpec", "Spec of the pod.")];
        r.spec_name = "PodSpec".into();
        r.spec_fields = vec![model_field("nodeName", "Node name.")];
        let mut classifications = std::collections::HashMap::new();
        classifications.insert("PodSpec".to_string(), true);
        let mut complex_types = std::collections::HashMap::new();
        complex_types.insert("PodSpec".to_string(), ("PodSpec desc.".to_string(), vec![]));
        render(
            &[r],
            &[],
            dir.path(),
            "https://example.com",
            true,
            &TypeMaps {
                classifications: &classifications,
                simple_types: &std::collections::HashMap::new(),
                complex_types: &complex_types,
            },
        )
        .unwrap();
        let html =
            std::fs::read_to_string(dir.path().join("docs/latest/core/v1/pod/index.html")).unwrap();
        assert!(
            !html.contains(r#"id="type-podspec""#),
            "no type section must appear for spec when spec_fields are present"
        );
        assert!(
            html.contains("href=\"#podspec\""),
            "spec field must link to the spec sub-section anchor"
        );
    }
}
