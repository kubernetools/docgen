mod resolve;
mod schema;

use crate::model::{Field, Resource};
use anyhow::{Context, Result};
use schema::RawSpec;
use serde_json::Value;
use std::collections::HashSet;

pub fn parse_specs(specs: Vec<(String, Value)>, k8s_version: &str) -> Result<Vec<Resource>> {
    let mut resources = Vec::new();
    // Track which schema names have already produced a resource.  Each schema
    // definition appears in every spec file that references it (the files are
    // self-contained), so without this guard types like DeleteOptions — which
    // carry a GVK entry for every API group — would generate one page per file.
    let mut emitted: HashSet<String> = HashSet::new();

    for (filename, value) in specs {
        // Only process versioned group files: api__v1 or apis__<group>__<version>.
        let Some((file_group, file_version)) = parse_filename(&filename) else {
            continue;
        };
        let spec: RawSpec = serde_json::from_value(value)
            .with_context(|| format!("parsing {filename}"))?;
        parse_spec_file(spec, k8s_version, &file_group, &file_version, &mut emitted, &mut resources);
    }

    // Separate List variants from root resources and attach them to their root.
    let (lists, mut roots): (Vec<Resource>, Vec<Resource>) =
        resources.into_iter().partition(|r| r.kind.ends_with("List"));

    for list in lists {
        let root_kind = list.kind.strip_suffix("List").unwrap();
        if let Some(root) = roots.iter_mut().find(|r| {
            r.kind == root_kind && r.group == list.group && r.api_version == list.api_version
        }) {
            root.list_description = list.description;
            root.list_fields = list.fields;
        }
        // Lists with no matching root (e.g. APIResourceList) are simply dropped.
    }

    roots.sort_by(|a, b| a.kind.cmp(&b.kind));
    Ok(roots)
}

/// Derives (group, version) from a spec filename.
/// `api__v1_openapi.json`          → ("", "v1")
/// `apis__apps__v1_openapi.json`   → ("apps", "v1")
/// Returns None for discovery/meta files that carry no schemas.
fn parse_filename(filename: &str) -> Option<(String, String)> {
    let base = filename.strip_suffix("_openapi.json")?;
    let parts: Vec<&str> = base.split("__").collect();
    match parts.as_slice() {
        ["api", version] => Some(("".to_string(), version.to_string())),
        ["apis", group, version] => Some((group.to_string(), version.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_filename_core() {
        assert_eq!(parse_filename("api__v1_openapi.json"), Some(("".into(), "v1".into())));
    }

    #[test]
    fn parse_filename_group() {
        assert_eq!(
            parse_filename("apis__apps__v1_openapi.json"),
            Some(("apps".into(), "v1".into()))
        );
        assert_eq!(
            parse_filename("apis__admissionregistration.k8s.io__v1beta1_openapi.json"),
            Some(("admissionregistration.k8s.io".into(), "v1beta1".into()))
        );
    }

    #[test]
    fn parse_filename_rejects_discovery_files() {
        assert_eq!(parse_filename("api_openapi.json"), None);
        assert_eq!(parse_filename("apis_openapi.json"), None);
        assert_eq!(parse_filename("version_openapi.json"), None);
        assert_eq!(parse_filename(".well-known__openid-configuration_openapi.json"), None);
    }

    fn pod_spec(group: &str, version: &str) -> serde_json::Value {
        json!({
            "components": {
                "schemas": {
                    "io.k8s.api.core.v1.Pod": {
                        "description": "Pod is a collection of containers.",
                        "x-kubernetes-group-version-kind": [
                            {"group": group, "version": version, "kind": "Pod"}
                        ],
                        "properties": {
                            "apiVersion": {"type": "string"},
                            "spec": {"$ref": "#/components/schemas/io.k8s.api.core.v1.PodSpec"}
                        },
                        "required": ["spec"]
                    },
                    "io.k8s.api.core.v1.PodList": {
                        "description": "PodList is a list of Pods.",
                        "x-kubernetes-group-version-kind": [
                            {"group": group, "version": version, "kind": "PodList"}
                        ],
                        "properties": {
                            "items": {
                                "type": "array",
                                "items": {"$ref": "#/components/schemas/io.k8s.api.core.v1.Pod"}
                            }
                        },
                        "required": ["items"]
                    }
                }
            }
        })
    }

    fn delete_options_spec(_group: &str, _version: &str) -> serde_json::Value {
        json!({
            "components": {
                "schemas": {
                    "io.k8s.apimachinery.pkg.apis.meta.v1.DeleteOptions": {
                        "description": "DeleteOptions may be provided when deleting an API object.",
                        "x-kubernetes-group-version-kind": [
                            {"group": "",     "version": "v1", "kind": "DeleteOptions"},
                            {"group": "apps", "version": "v1", "kind": "DeleteOptions"}
                        ],
                        "properties": {
                            "gracePeriodSeconds": {"type": "integer"}
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn list_is_attached_to_root_and_removed() {
        let specs = vec![("api__v1_openapi.json".into(), pod_spec("", "v1"))];
        let resources = parse_specs(specs, "v1.33").unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].kind, "Pod");
        assert_eq!(resources[0].list_description, "PodList is a list of Pods.");
        assert!(!resources[0].list_fields.is_empty());
    }

    #[test]
    fn emitted_guard_deduplicates_cross_cutting_schemas() {
        let specs = vec![
            ("api__v1_openapi.json".into(),      delete_options_spec("", "v1")),
            ("apis__apps__v1_openapi.json".into(), delete_options_spec("apps", "v1")),
        ];
        let resources = parse_specs(specs, "v1.33").unwrap();
        // DeleteOptions appears in both files under the same schema name;
        // only the first file's GVK entry should be used.
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].kind, "DeleteOptions");
        assert_eq!(resources[0].group, "");
    }

    #[test]
    fn resources_are_sorted_by_kind() {
        let specs = vec![("api__v1_openapi.json".into(), json!({
            "components": { "schemas": {
                "io.k8s.api.core.v1.Service": {
                    "x-kubernetes-group-version-kind": [{"group": "", "version": "v1", "kind": "Service"}],
                    "properties": {}
                },
                "io.k8s.api.core.v1.Pod": {
                    "x-kubernetes-group-version-kind": [{"group": "", "version": "v1", "kind": "Pod"}],
                    "properties": {}
                }
            }}
        }))];
        let resources = parse_specs(specs, "v1.33").unwrap();
        let kinds: Vec<&str> = resources.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, ["Pod", "Service"]);
    }
}

fn parse_spec_file(
    spec: RawSpec,
    k8s_version: &str,
    file_group: &str,
    file_version: &str,
    emitted: &mut HashSet<String>,
    out: &mut Vec<Resource>,
) {
    let Some(schemas) = spec.components.and_then(|c| c.schemas) else {
        return;
    };
    for (schema_name, schema) in schemas {
        // If another file has already emitted a resource for this schema, skip.
        // This prevents cross-cutting types (DeleteOptions, WatchEvent, …) from
        // generating a page per group; the first file that owns the schema wins.
        if emitted.contains(&schema_name) {
            continue;
        }

        let Some(gvks) = schema.gvk else { continue };

        // Only claim the GVK entry that matches this file's group/version.
        // The x-kubernetes-group-version-kind list on meta types enumerates
        // every group in the cluster; each file only owns its own entry.
        let Some(gvk) = gvks
            .into_iter()
            .find(|g| g.group == file_group && g.version == file_version)
        else {
            continue;
        };

        let required_set: HashSet<String> = schema
            .required
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect();

        let mut fields: Vec<Field> = schema
            .properties
            .as_ref()
            .map(|props| {
                props
                    .iter()
                    .map(|(name, prop)| Field {
                        name: name.clone(),
                        description: prop.description.clone().unwrap_or_default(),
                        required: required_set.contains(name),
                        field_type: resolve::resolve_field_type(prop),
                    })
                    .collect()
            })
            .unwrap_or_default();
        fields.sort_by(|a, b| a.name.cmp(&b.name));

        // Mark as emitted only after we've confirmed we'll produce a resource.
        emitted.insert(schema_name);

        out.push(Resource {
            kind: gvk.kind,
            group: gvk.group,
            api_version: gvk.version,
            k8s_version: k8s_version.to_string(),
            description: schema.description.unwrap_or_default(),
            fields,
            list_description: String::new(),
            list_fields: Vec::new(),
        });
    }
}
