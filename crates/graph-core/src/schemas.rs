pub const AGENT: &str = include_str!("schemas/agent.yaml");
pub const TOOL: &str = include_str!("schemas/tool.yaml");
pub const PLAN: &str = include_str!("schemas/plan.yaml");

pub fn parse(source: &str) -> serde_yaml::Value {
    serde_yaml::from_str(source).expect("file schemas are valid YAML")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value;
    use std::collections::BTreeSet;

    fn keys(schema: &Value) -> BTreeSet<String> {
        schema
            .get("properties")
            .and_then(Value::as_mapping)
            .map(|props| {
                props
                    .keys()
                    .filter_map(Value::as_str)
                    .filter(|key| *key != "version" && *key != "kind")
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items
            .iter()
            .filter(|key| **key != "kind" && **key != "toolName")
            .map(|key| key.to_string())
            .collect()
    }

    fn examples(schema: &Value) -> Vec<String> {
        let branches = schema
            .get("oneOf")
            .and_then(Value::as_sequence)
            .cloned()
            .unwrap_or_default();
        let found: Vec<String> = std::iter::once(schema)
            .chain(branches.iter())
            .filter_map(|node| node.get("examples").and_then(Value::as_sequence))
            .flatten()
            .map(|example| serde_yaml::to_string(example).unwrap())
            .collect();
        assert!(!found.is_empty(), "every file schema carries an example");
        found
    }

    #[test]
    fn the_agent_schema_documents_exactly_the_keys_the_loader_accepts() {
        let schema = parse(AGENT);
        assert_eq!(keys(&schema), set(crate::agent::doc::AGENT_KEYS));
        for example in examples(&schema) {
            crate::agent::doc::parse_agent_source(&example).unwrap();
        }
    }

    #[test]
    fn the_tool_schema_documents_exactly_the_keys_each_kind_accepts() {
        let schema = parse(TOOL);
        assert_eq!(keys(&schema), set(crate::user_tools::TOOL_KEYS));
        let kinds: Vec<(&str, &[&str])> = vec![
            ("exec", crate::user_tools::EXEC_KEYS),
            ("prompt", crate::user_tools::PROMPT_KEYS),
            ("reshape", crate::user_tools::RESHAPE_KEYS),
            ("decision", crate::user_tools::DECISION_KEYS),
        ];
        let branches = schema.get("oneOf").and_then(Value::as_sequence).unwrap();
        assert_eq!(branches.len(), kinds.len());
        for (kind, accepted) in kinds {
            let branch = branches
                .iter()
                .find(|branch| branch.get("title").and_then(Value::as_str) == Some(kind))
                .unwrap_or_else(|| panic!("the tool schema has no `{kind}` branch"));
            assert_eq!(keys(branch), set(accepted), "`{kind}` tool keys");
        }
        for example in examples(&schema) {
            crate::user_tools::parse_tool_source(&example).unwrap();
        }
    }

    #[test]
    fn the_plan_schema_documents_exactly_the_keys_the_loader_accepts() {
        let schema = parse(PLAN);
        let step = schema
            .get("properties")
            .and_then(|props| props.get("steps"))
            .and_then(|steps| steps.get("items"))
            .unwrap();
        assert_eq!(keys(step), set(crate::pipeline::doc::STEP_KEYS));
        let full = crate::pipeline::doc::PlanDoc {
            identifier: "a".into(),
            name: "a".into(),
            description: "a".into(),
            exemplars: vec!["a".into()],
            requires_servers: vec!["a".into()],
            input_schema: Some(serde_json::json!({})),
            output_schema: Some(serde_json::json!({})),
            steps: Vec::new(),
            finish: crate::pipeline::Finish::Silent,
            path: None,
            pack: None,
        };
        let written = serde_yaml::to_value(&full).unwrap();
        assert_eq!(
            keys(&schema),
            keys(&Value::Mapping({
                let mut map = serde_yaml::Mapping::new();
                map.insert("properties".into(), written);
                map
            }))
        );
        for example in examples(&schema) {
            crate::pipeline::doc::parse_plan_source(&example, "example.yaml").unwrap();
        }
    }
}
