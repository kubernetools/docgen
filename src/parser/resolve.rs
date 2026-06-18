use super::schema::RawProperty;
use crate::model::FieldType;

pub fn resolve_field_type(prop: &RawProperty) -> FieldType {
    if let Some(r) = &prop.ref_ {
        return FieldType::Ref(short_name(r));
    }

    if let Some(all_of) = &prop.all_of {
        if let Some(first) = all_of.first() {
            if let Some(r) = &first.ref_ {
                return FieldType::Ref(short_name(r));
            }
        }
    }

    if prop.ty.as_deref() == Some("array") {
        if let Some(items) = &prop.items {
            return FieldType::Array(Box::new(resolve_field_type(items)));
        }
    }

    if let Some(add_props) = &prop.additional_properties {
        return FieldType::Map(Box::new(resolve_field_type(add_props)));
    }

    if let Some(ty) = &prop.ty {
        return FieldType::Scalar(ty.clone());
    }

    FieldType::Object
}

fn short_name(ref_: &str) -> String {
    // "#/components/schemas/io.k8s.api.core.v1.Pod" → "Pod"
    ref_.rsplit('/')
        .next()
        .and_then(|s| s.rsplit('.').next())
        .unwrap_or(ref_)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FieldType;

    fn prop(ty: Option<&str>, ref_: Option<&str>) -> RawProperty {
        RawProperty {
            description: None,
            ty: ty.map(str::to_string),
            ref_: ref_.map(str::to_string),
            all_of: None,
            items: None,
            additional_properties: None,
        }
    }

    #[test]
    fn short_name_strips_prefix_and_package() {
        assert_eq!(
            short_name("#/components/schemas/io.k8s.api.core.v1.Pod"),
            "Pod"
        );
        assert_eq!(
            short_name("#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta"),
            "ObjectMeta"
        );
        assert_eq!(short_name("Pod"), "Pod");
    }

    #[test]
    fn resolve_direct_ref() {
        let p = prop(
            None,
            Some("#/components/schemas/io.k8s.api.core.v1.PodSpec"),
        );
        assert!(matches!(resolve_field_type(&p), FieldType::Ref(n) if n == "PodSpec"));
    }

    #[test]
    fn resolve_all_of_ref() {
        let p = RawProperty {
            description: None,
            ty: None,
            ref_: None,
            all_of: Some(vec![super::super::schema::RawRef {
                ref_: Some("#/components/schemas/io.k8s.api.core.v1.ObjectMeta".to_string()),
            }]),
            items: None,
            additional_properties: None,
        };
        assert!(matches!(resolve_field_type(&p), FieldType::Ref(n) if n == "ObjectMeta"));
    }

    #[test]
    fn resolve_array() {
        let p = RawProperty {
            description: None,
            ty: Some("array".to_string()),
            ref_: None,
            all_of: None,
            items: Some(Box::new(prop(
                None,
                Some("#/components/schemas/io.k8s.api.core.v1.Pod"),
            ))),
            additional_properties: None,
        };
        assert!(
            matches!(resolve_field_type(&p), FieldType::Array(inner) if matches!(*inner, FieldType::Ref(ref n) if n == "Pod"))
        );
    }

    #[test]
    fn resolve_map() {
        let p = RawProperty {
            description: None,
            ty: None,
            ref_: None,
            all_of: None,
            items: None,
            additional_properties: Some(Box::new(prop(Some("string"), None))),
        };
        assert!(
            matches!(resolve_field_type(&p), FieldType::Map(inner) if matches!(*inner, FieldType::Scalar(ref s) if s == "string"))
        );
    }

    #[test]
    fn resolve_scalar() {
        assert!(
            matches!(resolve_field_type(&prop(Some("string"), None)), FieldType::Scalar(s) if s == "string")
        );
        assert!(
            matches!(resolve_field_type(&prop(Some("integer"), None)), FieldType::Scalar(s) if s == "integer")
        );
        assert!(
            matches!(resolve_field_type(&prop(Some("boolean"), None)), FieldType::Scalar(s) if s == "boolean")
        );
    }

    #[test]
    fn resolve_object_fallback() {
        assert!(matches!(
            resolve_field_type(&prop(None, None)),
            FieldType::Object
        ));
    }
}
