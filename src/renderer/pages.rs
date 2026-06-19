use serde::Serialize;

#[derive(Serialize)]
pub struct Crumb {
    pub label: String,
    pub href: String,
}

#[derive(Serialize)]
pub struct FieldCtx {
    pub name: String,
    pub required: bool,
    pub type_display: String,
    pub type_href: Option<String>,
    pub description: String,
}

#[derive(Serialize)]
pub struct ResourcePageCtx {
    pub kind: String,
    pub group_display: String,
    pub api_version: String,
    pub k8s_version: String,
    pub k8s_version_display: String,
    pub description: String,
    pub fields: Vec<FieldCtx>,
    pub list_description: String,
    pub list_fields: Vec<FieldCtx>,
    pub other_versions: Vec<VersionLink>,
    pub canonical_url: String,
    pub canonical_path: String,
    pub breadcrumbs: Vec<Crumb>,
    pub meta_description: String,
    pub json_ld: String,
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
}

#[derive(Serialize)]
pub struct GroupLink {
    pub display: String,
    pub href: String,
}

#[derive(Serialize)]
pub struct VersionIndexCtx {
    pub k8s_version: String,
    pub k8s_version_display: String,
    pub groups: Vec<GroupLink>,
    pub canonical_url: String,
    pub canonical_path: String,
    pub breadcrumbs: Vec<Crumb>,
    pub meta_description: String,
}
