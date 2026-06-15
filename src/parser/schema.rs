use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize)]
pub struct RawSpec {
    pub components: Option<RawComponents>,
}

#[derive(Deserialize)]
pub struct RawComponents {
    pub schemas: Option<HashMap<String, RawSchema>>,
}

#[derive(Deserialize)]
pub struct RawSchema {
    pub description: Option<String>,
    pub properties: Option<HashMap<String, RawProperty>>,
    pub required: Option<Vec<String>>,
    #[serde(rename = "x-kubernetes-group-version-kind")]
    pub gvk: Option<Vec<RawGVK>>,
}

#[derive(Deserialize)]
pub struct RawProperty {
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    #[serde(rename = "$ref")]
    pub ref_: Option<String>,
    #[serde(rename = "allOf")]
    pub all_of: Option<Vec<RawRef>>,
    pub items: Option<Box<RawProperty>>,
    #[serde(rename = "additionalProperties")]
    pub additional_properties: Option<Box<RawProperty>>,
}

#[derive(Deserialize)]
pub struct RawGVK {
    pub group: String,
    pub version: String,
    pub kind: String,
}

#[derive(Deserialize)]
pub struct RawRef {
    #[serde(rename = "$ref")]
    pub ref_: Option<String>,
}
