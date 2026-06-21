#[derive(Clone)]
pub struct CommonDefinition {
    pub name: String,
    pub description: String,
    pub fields: Vec<Field>,
    pub k8s_version: String,
}

#[derive(Clone)]
pub struct Resource {
    pub kind: String,
    pub group: String,
    pub api_version: String,
    pub k8s_version: String,
    pub description: String,
    pub fields: Vec<Field>,
    pub list_description: String,
    pub list_fields: Vec<Field>,
    pub spec_description: String,
    pub spec_fields: Vec<Field>,
    pub status_description: String,
    pub status_fields: Vec<Field>,
}

#[derive(Clone)]
pub struct Field {
    pub name: String,
    pub description: String,
    pub required: bool,
    pub field_type: FieldType,
}

#[derive(Clone)]
pub enum FieldType {
    Scalar(String),
    Ref(String),
    Array(Box<FieldType>),
    Map(Box<FieldType>),
    Object,
}
