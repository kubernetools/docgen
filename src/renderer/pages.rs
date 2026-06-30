use super::copy::UiCopy;
use serde::Serialize;

#[derive(Serialize)]
pub struct TocEntry {
    pub label: String,
    pub href: String,
}

#[derive(Serialize)]
pub struct SameGroupItem {
    pub kind: String,
    pub href: String,
    pub is_current: bool,
}

#[derive(Serialize)]
pub struct CommonDefLink {
    pub name: String,
    pub href: String,
}

#[derive(Serialize)]
pub struct CommonDefPageCtx {
    pub name: String,
    pub description: String,
    pub fields: Vec<FieldCtx>,
    pub type_sections: Vec<TypeSectionCtx>,
    pub k8s_version: String,
    pub k8s_version_display: String,
    pub canonical_url: String,
    pub canonical_path: String,
    pub breadcrumbs: Vec<Crumb>,
    pub meta_description: String,
    pub page_title: String,
    pub copy: UiCopy,
    pub toc: Vec<TocEntry>,
    pub same_group: Vec<SameGroupItem>,
}

#[derive(Serialize)]
pub struct CommonDefCategory {
    pub label: String,
    pub definitions: Vec<CommonDefLink>,
}

#[derive(Serialize)]
pub struct CommonDefsIndexCtx {
    pub k8s_version: String,
    pub k8s_version_display: String,
    pub categories: Vec<CommonDefCategory>,
    pub canonical_url: String,
    pub canonical_path: String,
    pub breadcrumbs: Vec<Crumb>,
    pub meta_description: String,
    pub page_title: String,
    pub copy: UiCopy,
    pub toc: Vec<TocEntry>,
    pub same_group: Vec<SameGroupItem>,
}

#[derive(Serialize)]
pub struct Crumb {
    pub label: String,
    pub href: String,
}

#[derive(Serialize)]
pub struct TypeSectionCtx {
    pub anchor: String,
    pub name: String,
    pub description: String,
    pub fields: Vec<FieldCtx>,
}

#[derive(Serialize)]
pub struct FieldCtx {
    pub name: String,
    pub required: bool,
    pub type_prefix: String,
    pub type_display: String,
    pub type_href: Option<String>,
    pub type_classification: Option<String>,
    pub type_ref: Option<String>,
    pub description: String,
    pub type_description: Option<String>,
    pub sub_fields: Vec<FieldCtx>,
}

#[derive(Serialize)]
pub struct ResourcePageCtx {
    pub kind: String,
    pub kind_lower: String,
    pub group_display: String,
    pub api_version: String,
    pub k8s_version: String,
    pub k8s_version_display: String,
    pub description: String,
    pub fields: Vec<FieldCtx>,
    pub list_description: String,
    pub list_fields: Vec<FieldCtx>,
    pub field_type_sections: Vec<TypeSectionCtx>,
    pub spec_name: String,
    pub spec_description: String,
    pub spec_fields: Vec<FieldCtx>,
    pub spec_type_sections: Vec<TypeSectionCtx>,
    pub status_name: String,
    pub status_description: String,
    pub status_fields: Vec<FieldCtx>,
    pub status_type_sections: Vec<TypeSectionCtx>,
    pub list_type_sections: Vec<TypeSectionCtx>,
    pub other_versions: Vec<VersionLink>,
    pub canonical_url: String,
    pub canonical_path: String,
    pub breadcrumbs: Vec<Crumb>,
    pub meta_description: String,
    pub json_ld: String,
    pub page_title: String,
    pub copy: UiCopy,
    pub toc: Vec<TocEntry>,
    pub same_group: Vec<SameGroupItem>,
}

#[derive(Serialize, Clone)]
pub struct VersionLink {
    pub api_version: String,
    pub href: String,
}

#[derive(Serialize)]
pub struct ResourceLink {
    pub kind: String,
    /// All versions sorted most-recent first; the first entry is the primary link.
    pub versions: Vec<VersionLink>,
}

#[derive(Serialize)]
pub struct GroupIndexCtx {
    pub group_display: String,
    pub k8s_version: String,
    pub k8s_version_display: String,
    pub resources: Vec<ResourceLink>,
    pub canonical_url: String,
    pub canonical_path: String,
    pub breadcrumbs: Vec<Crumb>,
    pub meta_description: String,
    pub page_title: String,
    pub copy: UiCopy,
    pub toc: Vec<TocEntry>,
    pub same_group: Vec<SameGroupItem>,
}

#[derive(Serialize, Clone)]
pub struct GroupLink {
    pub display: String,
    pub href: String,
}

#[derive(Serialize)]
pub struct VersionIndexCtx {
    pub k8s_version: String,
    pub k8s_version_display: String,
    pub groups: Vec<GroupLink>,
    pub definitions: Vec<GroupLink>,
    pub canonical_url: String,
    pub canonical_path: String,
    pub breadcrumbs: Vec<Crumb>,
    pub meta_description: String,
    pub page_title: String,
    pub copy: UiCopy,
    pub toc: Vec<TocEntry>,
    pub same_group: Vec<SameGroupItem>,
}
