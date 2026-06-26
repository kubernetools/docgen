use serde::Serialize;

// ── Site identity ─────────────────────────────────────────────────────────────
pub const SITE_NAME: &str = "Kubernetools";
pub const SITE_TAGLINE: &str = "Kubernetes API Reference";
/// Used where "Kubernetes" is already present in the surrounding text.
pub const API_REF_LABEL: &str = "API Reference";
pub const DEFAULT_META_DESCRIPTION: &str = "Kubernetes API Reference documentation";
pub const FOOTER_TEXT: &str = "Kubernetools &mdash; Kubernetes API Reference";

// ── Navigation ────────────────────────────────────────────────────────────────
pub const NAV_LABEL_BREADCRUMB: &str = "Breadcrumb";
pub const NAV_LABEL_API_GROUPS: &str = "API Groups";
pub const BREADCRUMB_HOME: &str = "Home";
pub const BREADCRUMB_COMMON_DEFS: &str = "Common Definitions";
pub const SECTION_BY_GROUP: &str = "API Resources by Group";
pub const SECTION_DEFINITIONS: &str = "Definitions";

// ── Resource page labels ──────────────────────────────────────────────────────
pub const LABEL_OTHER_VERSIONS: &str = "Other versions:";
pub const HEADING_FIELDS: &str = "Fields";
pub const MSG_NO_DESCRIPTION: &str = "No description available.";
pub const MSG_NO_FIELDS: &str = "No fields documented.";
pub const LIST_SUFFIX: &str = "List";

// ── SEO: page titles ──────────────────────────────────────────────────────────
pub fn title_version_index(k8s_version_display: &str) -> String {
    format!("Kubernetes {k8s_version_display} {API_REF_LABEL} | {SITE_TAGLINE}")
}

pub fn title_group_index(group: &str, k8s_version_display: &str) -> String {
    format!("{group} &mdash; Kubernetes {k8s_version_display} {API_REF_LABEL} | {SITE_TAGLINE}")
}

pub fn title_resource(
    kind: &str,
    api_version: &str,
    group: &str,
    k8s_version_display: &str,
) -> String {
    format!(
        "{kind} {api_version} ({group}) &mdash; Kubernetes {k8s_version_display} | {SITE_TAGLINE}"
    )
}

// ── Common definition categories ──────────────────────────────────────────────
pub const CATEGORY_METADATA: &str = "Metadata";
pub const CATEGORY_REFERENCES: &str = "Object References";
pub const CATEGORY_SELECTORS: &str = "Selectors";
pub const CATEGORY_WORKLOAD: &str = "Workload";
pub const CATEGORY_STATUS_OPS: &str = "Status & Operations";
pub const CATEGORY_TYPES: &str = "Types";
pub const CATEGORY_OTHER: &str = "Other";

// ── Common definitions ────────────────────────────────────────────────────────
pub fn title_common_defs_index(k8s_version_display: &str) -> String {
    format!("Common Definitions &mdash; Kubernetes {k8s_version_display} | {SITE_TAGLINE}")
}

pub fn title_common_def(name: &str, k8s_version_display: &str) -> String {
    format!("{name} &mdash; Kubernetes {k8s_version_display} | {SITE_TAGLINE}")
}

pub fn meta_common_defs_index(k8s_version: &str) -> String {
    format!("Common type definitions shared across Kubernetes {k8s_version} API resources")
}

pub fn meta_common_def(name: &str, k8s_version: &str, description: &str) -> String {
    format!(
        "Kubernetes {name} common definition for {k8s_version}. {}",
        description.chars().take(120).collect::<String>()
    )
}

// ── SEO: meta descriptions ────────────────────────────────────────────────────
pub fn meta_version_index(k8s_version: &str) -> String {
    format!("Complete Kubernetes {k8s_version} API reference documentation")
}

pub fn meta_group_index(group: &str, k8s_version: &str) -> String {
    format!("{group} API resources for Kubernetes {k8s_version}")
}

pub fn meta_resource(kind: &str, k8s_version: &str, description: &str) -> String {
    format!(
        "Kubernetes {kind} API reference for {k8s_version}. {}",
        description.chars().take(120).collect::<String>()
    )
}

// ── SEO: JSON-LD ──────────────────────────────────────────────────────────────
pub const JSON_LD_TYPE: &str = "TechArticle";

pub fn json_ld_name(kind: &str, k8s_version: &str) -> String {
    format!("{kind} \u{2014} Kubernetes {k8s_version} {API_REF_LABEL}")
}

// ── Template context ──────────────────────────────────────────────────────────
/// All static copy strings passed to every template.
#[derive(Serialize, Clone)]
pub struct UiCopy {
    pub site_name: &'static str,
    pub site_tagline: &'static str,
    pub api_ref_label: &'static str,
    pub default_meta_description: &'static str,
    pub footer_text: &'static str,
    pub nav_label_breadcrumb: &'static str,
    pub nav_label_api_groups: &'static str,
    pub breadcrumb_home: &'static str,
    pub breadcrumb_common_defs: &'static str,
    pub section_by_group: &'static str,
    pub section_definitions: &'static str,
    pub label_other_versions: &'static str,
    pub heading_fields: &'static str,
    pub msg_no_description: &'static str,
    pub msg_no_fields: &'static str,
    pub list_suffix: &'static str,
}

impl UiCopy {
    pub fn new() -> Self {
        Self {
            site_name: SITE_NAME,
            site_tagline: SITE_TAGLINE,
            api_ref_label: API_REF_LABEL,
            default_meta_description: DEFAULT_META_DESCRIPTION,
            footer_text: FOOTER_TEXT,
            nav_label_breadcrumb: NAV_LABEL_BREADCRUMB,
            nav_label_api_groups: NAV_LABEL_API_GROUPS,
            breadcrumb_home: BREADCRUMB_HOME,
            breadcrumb_common_defs: BREADCRUMB_COMMON_DEFS,
            section_by_group: SECTION_BY_GROUP,
            section_definitions: SECTION_DEFINITIONS,
            label_other_versions: LABEL_OTHER_VERSIONS,
            heading_fields: HEADING_FIELDS,
            msg_no_description: MSG_NO_DESCRIPTION,
            msg_no_fields: MSG_NO_FIELDS,
            list_suffix: LIST_SUFFIX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_version_index_format() {
        assert_eq!(
            title_version_index("v1.33"),
            "Kubernetes v1.33 API Reference | Kubernetes API Reference"
        );
    }

    #[test]
    fn title_group_index_format() {
        assert_eq!(
            title_group_index("apps", "v1.33"),
            "apps &mdash; Kubernetes v1.33 API Reference | Kubernetes API Reference"
        );
    }

    #[test]
    fn title_group_index_uses_html_entity_not_unicode() {
        let title = title_group_index("apps", "v1.33");
        assert!(title.contains("&mdash;"));
        assert!(!title.contains('\u{2014}'));
    }

    #[test]
    fn title_resource_format() {
        assert_eq!(
            title_resource("Pod", "v1", "core", "v1.33"),
            "Pod v1 (core) &mdash; Kubernetes v1.33 | Kubernetes API Reference"
        );
    }

    #[test]
    fn title_resource_uses_html_entity_not_unicode() {
        let title = title_resource("Pod", "v1", "core", "v1.33");
        assert!(title.contains("&mdash;"));
        assert!(!title.contains('\u{2014}'));
    }

    #[test]
    fn meta_version_index_format() {
        assert_eq!(
            meta_version_index("v1.33"),
            "Complete Kubernetes v1.33 API reference documentation"
        );
    }

    #[test]
    fn meta_group_index_format() {
        assert_eq!(
            meta_group_index("apps", "v1.33"),
            "apps API resources for Kubernetes v1.33"
        );
    }

    #[test]
    fn meta_resource_format() {
        assert_eq!(
            meta_resource("Pod", "v1.33", "Runs containers."),
            "Kubernetes Pod API reference for v1.33. Runs containers."
        );
    }

    #[test]
    fn meta_resource_truncates_description_at_120_chars() {
        let long_desc = "x".repeat(200);
        let meta = meta_resource("Pod", "v1.33", &long_desc);
        let desc_part = meta.split(". ").nth(1).unwrap_or("");
        assert_eq!(desc_part.len(), 120);
    }

    #[test]
    fn json_ld_name_format() {
        assert_eq!(
            json_ld_name("Pod", "v1.33"),
            "Pod \u{2014} Kubernetes v1.33 API Reference"
        );
    }

    #[test]
    fn json_ld_name_uses_unicode_not_html_entity() {
        let name = json_ld_name("Pod", "v1.33");
        assert!(name.contains('\u{2014}'));
        assert!(!name.contains("&mdash;"));
    }
}
