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
        let spec: RawSpec =
            serde_json::from_value(value).with_context(|| format!("parsing {filename}"))?;
        parse_spec_file(
            spec,
            k8s_version,
            &file_group,
            &file_version,
            &mut emitted,
            &mut resources,
        );
    }

    // Separate List variants from root resources and attach them to their root.
    let (lists, mut roots): (Vec<Resource>, Vec<Resource>) = resources
        .into_iter()
        .partition(|r| r.kind.ends_with("List"));

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
        assert_eq!(
            parse_filename("api__v1_openapi.json"),
            Some(("".into(), "v1".into()))
        );
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
        assert_eq!(
            parse_filename(".well-known__openid-configuration_openapi.json"),
            None
        );
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
            ("api__v1_openapi.json".into(), delete_options_spec("", "v1")),
            (
                "apis__apps__v1_openapi.json".into(),
                delete_options_spec("apps", "v1"),
            ),
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
        let specs = vec![(
            "api__v1_openapi.json".into(),
            json!({
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
            }),
        )];
        let resources = parse_specs(specs, "v1.33").unwrap();
        let kinds: Vec<&str> = resources.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, ["Pod", "Service"]);
    }

    fn pod_spec_with_sub_schemas() -> serde_json::Value {
        json!({
            "components": {
                "schemas": {
                    "io.k8s.api.core.v1.Pod": {
                        "description": "Pod is a collection of containers.",
                        "x-kubernetes-group-version-kind": [
                            {"group": "", "version": "v1", "kind": "Pod"}
                        ],
                        "properties": {
                            "spec": {"$ref": "#/components/schemas/io.k8s.api.core.v1.PodSpec"},
                            "status": {"$ref": "#/components/schemas/io.k8s.api.core.v1.PodStatus"}
                        }
                    },
                    "io.k8s.api.core.v1.PodSpec": {
                        "description": "PodSpec is a description of a pod.",
                        "properties": {
                            "nodeName": {"type": "string", "description": "Name of the node."},
                            "restartPolicy": {"type": "string", "description": "Restart policy."}
                        },
                        "required": ["nodeName"]
                    },
                    "io.k8s.api.core.v1.PodStatus": {
                        "description": "PodStatus represents the status of a pod.",
                        "properties": {
                            "hostIP": {"type": "string", "description": "IP address of the host."},
                            "phase": {"type": "string", "description": "Phase of the pod."}
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn spec_fields_are_extracted_from_sub_schema() {
        let specs = vec![("api__v1_openapi.json".into(), pod_spec_with_sub_schemas())];
        let resources = parse_specs(specs, "v1.36").unwrap();
        let pod = resources.iter().find(|r| r.kind == "Pod").unwrap();
        assert_eq!(pod.spec_description, "PodSpec is a description of a pod.");
        assert_eq!(pod.spec_fields.len(), 2);
        let names: Vec<&str> = pod.spec_fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"nodeName"));
        assert!(names.contains(&"restartPolicy"));
    }

    #[test]
    fn status_fields_are_extracted_from_sub_schema() {
        let specs = vec![("api__v1_openapi.json".into(), pod_spec_with_sub_schemas())];
        let resources = parse_specs(specs, "v1.36").unwrap();
        let pod = resources.iter().find(|r| r.kind == "Pod").unwrap();
        assert_eq!(
            pod.status_description,
            "PodStatus represents the status of a pod."
        );
        assert_eq!(pod.status_fields.len(), 2);
        let names: Vec<&str> = pod.status_fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"hostIP"));
        assert!(names.contains(&"phase"));
    }

    #[test]
    fn spec_required_is_propagated_from_sub_schema() {
        let specs = vec![("api__v1_openapi.json".into(), pod_spec_with_sub_schemas())];
        let resources = parse_specs(specs, "v1.36").unwrap();
        let pod = resources.iter().find(|r| r.kind == "Pod").unwrap();
        let node_name = pod
            .spec_fields
            .iter()
            .find(|f| f.name == "nodeName")
            .unwrap();
        let restart_policy = pod
            .spec_fields
            .iter()
            .find(|f| f.name == "restartPolicy")
            .unwrap();
        assert!(node_name.required);
        assert!(!restart_policy.required);
    }

    #[test]
    fn spec_fields_empty_when_sub_schema_absent() {
        // pod_spec() references PodSpec via $ref but does not include its schema.
        let specs = vec![("api__v1_openapi.json".into(), pod_spec("", "v1"))];
        let resources = parse_specs(specs, "v1.36").unwrap();
        let pod = resources.iter().find(|r| r.kind == "Pod").unwrap();
        assert!(pod.spec_fields.is_empty());
        assert!(pod.spec_description.is_empty());
    }

    #[test]
    fn resource_without_spec_or_status_has_empty_sub_fields() {
        let specs = vec![(
            "api__v1_openapi.json".into(),
            json!({
                "components": {
                    "schemas": {
                        "io.k8s.api.core.v1.ConfigMap": {
                            "description": "ConfigMap holds configuration data.",
                            "x-kubernetes-group-version-kind": [
                                {"group": "", "version": "v1", "kind": "ConfigMap"}
                            ],
                            "properties": {
                                "data": {"type": "object", "description": "Data contains the configuration data."}
                            }
                        }
                    }
                }
            }),
        )];
        let resources = parse_specs(specs, "v1.36").unwrap();
        let cm = resources.iter().find(|r| r.kind == "ConfigMap").unwrap();
        assert!(cm.spec_fields.is_empty());
        assert!(cm.status_fields.is_empty());
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

    // Index schemas by short name for spec/status sub-schema lookups.
    let by_short_name: std::collections::HashMap<String, &schema::RawSchema> = schemas
        .iter()
        .map(|(full_name, s)| (resolve::short_name(full_name), s))
        .collect();

    for (schema_name, schema) in &schemas {
        // If another file has already emitted a resource for this schema, skip.
        // This prevents cross-cutting types (DeleteOptions, WatchEvent, …) from
        // generating a page per group; the first file that owns the schema wins.
        if emitted.contains(schema_name.as_str()) {
            continue;
        }

        let Some(gvks) = schema.gvk.as_ref() else {
            continue;
        };

        // Only claim the GVK entry that matches this file's group/version.
        // The x-kubernetes-group-version-kind list on meta types enumerates
        // every group in the cluster; each file only owns its own entry.
        let Some(gvk) = gvks
            .iter()
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

        let (spec_description, spec_fields) = sub_schema_fields("spec", schema, &by_short_name);
        let (status_description, status_fields) =
            sub_schema_fields("status", schema, &by_short_name);

        // Mark as emitted only after we've confirmed we'll produce a resource.
        emitted.insert(schema_name.clone());

        out.push(Resource {
            kind: gvk.kind.clone(),
            group: gvk.group.clone(),
            api_version: gvk.version.clone(),
            k8s_version: k8s_version.to_string(),
            description: schema.description.clone().unwrap_or_default(),
            fields,
            list_description: String::new(),
            list_fields: Vec::new(),
            spec_description,
            spec_fields,
            status_description,
            status_fields,
        });
    }
}

/// Returns the (description, fields) of the sub-schema referenced by `field_name`
/// (e.g. "spec" or "status") on the given schema, or empty defaults if absent.
fn sub_schema_fields(
    field_name: &str,
    schema: &schema::RawSchema,
    by_short_name: &std::collections::HashMap<String, &schema::RawSchema>,
) -> (String, Vec<Field>) {
    let Some(props) = schema.properties.as_ref() else {
        return (String::new(), Vec::new());
    };
    let Some(prop) = props.get(field_name) else {
        return (String::new(), Vec::new());
    };
    let Some(ref_short) = prop_ref_short_name(prop) else {
        return (String::new(), Vec::new());
    };
    let Some(sub) = by_short_name.get(&ref_short) else {
        return (String::new(), Vec::new());
    };

    let description = sub.description.clone().unwrap_or_default();
    let required_set: HashSet<String> = sub
        .required
        .as_deref()
        .unwrap_or_default()
        .iter()
        .cloned()
        .collect();
    let mut fields: Vec<Field> = sub
        .properties
        .as_ref()
        .map(|ps| {
            ps.iter()
                .map(|(name, p)| Field {
                    name: name.clone(),
                    description: p.description.clone().unwrap_or_default(),
                    required: required_set.contains(name),
                    field_type: resolve::resolve_field_type(p),
                })
                .collect()
        })
        .unwrap_or_default();
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    (description, fields)
}

fn prop_ref_short_name(prop: &schema::RawProperty) -> Option<String> {
    if let Some(r) = &prop.ref_ {
        return Some(resolve::short_name(r));
    }
    if let Some(all_of) = &prop.all_of {
        if let Some(first) = all_of.first() {
            if let Some(r) = &first.ref_ {
                return Some(resolve::short_name(r));
            }
        }
    }
    None
}
