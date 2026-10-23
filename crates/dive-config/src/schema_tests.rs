use crate::pack::PackManifest;
use crate::{Config, PACK_SCHEMA, SCHEMA};
use serde_json::Value;
use std::collections::BTreeSet;

fn resolve<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or_default();
        return resolve(&root["definitions"][name], root);
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(options) = schema.get(key).and_then(Value::as_array) {
            if let Some(object) =
                options
                    .iter()
                    .map(|option| resolve(option, root))
                    .find(|option| {
                        option.get("properties").is_some()
                            || option
                                .get("additionalProperties")
                                .is_some_and(Value::is_object)
                            || option.get("items").is_some()
                    })
            {
                return object;
            }
        }
    }
    schema
}

fn paths(schema: &Value, root: &Value, prefix: &str, found: &mut BTreeSet<String>) {
    let schema = resolve(schema, root);
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            let path = format!("{prefix}{name}");
            found.insert(path.clone());
            paths(property, root, &format!("{path}."), found);
        }
    }
    if let Some(values) = schema
        .get("additionalProperties")
        .filter(|values| values.is_object())
    {
        paths(values, root, &format!("{prefix}<name>."), found);
    }
    if let Some(items) = schema.get("items") {
        paths(items, root, &format!("{prefix}[]."), found);
    }
}

#[test]
fn the_config_schema_documents_exactly_the_keys_the_loader_accepts() {
    let documented: Value = serde_yaml::from_str(SCHEMA).unwrap();
    let mut documented_paths = BTreeSet::new();
    paths(&documented, &documented, "", &mut documented_paths);
    documented_paths.remove("version");
    for leaf in ["output_schema", "output_example"] {
        documented_paths.retain(|path| !path.contains(&format!("{leaf}.")));
    }
    let actual = serde_json::to_value(schemars::schema_for!(Config)).unwrap();
    let mut actual_paths = BTreeSet::new();
    paths(&actual, &actual, "", &mut actual_paths);
    assert_eq!(documented_paths, actual_paths);
}

#[test]
fn the_config_schema_example_loads() {
    let documented: Value = serde_yaml::from_str(SCHEMA).unwrap();
    for example in documented["examples"].as_array().unwrap() {
        let mut example = example.clone();
        example.as_object_mut().unwrap().remove("version");
        let toml = toml::to_string(&example).unwrap();
        toml::from_str::<Config>(&toml).unwrap();
    }
}

#[test]
fn the_pack_schema_documents_exactly_the_keys_the_manifest_accepts() {
    let documented: Value = serde_yaml::from_str(PACK_SCHEMA).unwrap();
    let mut documented_paths = BTreeSet::new();
    paths(&documented, &documented, "", &mut documented_paths);
    let actual = serde_json::to_value(schemars::schema_for!(PackManifest)).unwrap();
    let mut actual_paths = BTreeSet::new();
    paths(&actual, &actual, "", &mut actual_paths);
    assert_eq!(documented_paths, actual_paths);
}

#[test]
fn the_pack_schema_example_passes_the_manifest_checks() {
    let documented: Value = serde_yaml::from_str(PACK_SCHEMA).unwrap();
    for example in documented["examples"].as_array().unwrap() {
        let toml = toml::to_string(example).unwrap();
        crate::pack::parse_manifest(&toml).unwrap();
    }
}
