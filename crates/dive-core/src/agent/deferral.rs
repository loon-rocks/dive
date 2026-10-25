use crate::pipeline::catalog::glob_matches;
use crate::tools::ToolDef;
use dive_llm::types::{ChatMessage, ToolSpec};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const SEARCH_TOOL: &str = "plan__search_tools";

pub const SEARCH_PLAN: &str = "search_tools";

pub fn is_standing(standing: &[String], name: &str) -> bool {
    name == SEARCH_TOOL || standing.iter().any(|pattern| glob_matches(pattern, name))
}

pub fn deferrable<'a>(defs: &'a [ToolDef], standing: &'a [String]) -> Vec<&'a ToolDef> {
    defs.iter()
        .filter(|def| !is_standing(standing, &def.name))
        .collect()
}

pub fn named(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter(|pattern| !pattern.contains(['*', '?', '[']))
        .cloned()
        .collect()
}

pub fn lists_search(patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| pattern == SEARCH_TOOL)
}

pub fn with_search(defs: &mut Vec<ToolDef>, standing: &[String], search: Option<ToolDef>) {
    if defs.iter().any(|def| def.name == SEARCH_TOOL) || deferrable(defs, standing).is_empty() {
        return;
    }
    if let Some(search) = search {
        defs.push(search);
    }
}

pub fn specs(defs: Vec<ToolDef>, standing: &[String]) -> Vec<ToolSpec> {
    let searching = defs.iter().any(|def| def.name == SEARCH_TOOL);
    defs.into_iter()
        .map(|def| ToolSpec {
            deferred: searching && !is_standing(standing, &def.name),
            name: def.name,
            description: def.description,
            input_schema: def.input_schema,
        })
        .collect()
}

pub fn search_input(mut input: Value, listed: bool) -> Value {
    if !listed {
        if let Some(map) = input.as_object_mut() {
            map.insert("schemas".to_string(), Value::Bool(false));
        }
    }
    input
}

pub fn references(messages: &[ChatMessage]) -> BTreeMap<String, Vec<String>> {
    let searches: BTreeSet<&str> = messages
        .iter()
        .flat_map(|message| match message {
            ChatMessage::Assistant { tool_calls, .. } => tool_calls.as_slice(),
            _ => &[],
        })
        .filter(|call| call.name == SEARCH_TOOL)
        .map(|call| call.id.as_str())
        .collect();
    messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::ToolResult {
                tool_call_id,
                content,
                is_error: false,
            } if searches.contains(tool_call_id.as_str()) => {
                let names: Vec<String> = content["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                    .collect();
                (!names.is_empty()).then(|| (tool_call_id.clone(), names))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_llm::types::ToolCall;
    use serde_json::json;

    fn def(name: &str) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            output_example: None,
            read_only: None,
        }
    }

    #[test]
    fn everything_but_standing_tools_is_deferred_once_search_is_offered() {
        let standing = vec!["workbench__*".to_string(), "builtin__infer".to_string()];
        let mut defs = vec![
            def("linear__list_issues"),
            def("workbench__set_sketch"),
            def("builtin__infer"),
        ];
        with_search(&mut defs, &standing, Some(def(SEARCH_TOOL)));
        let deferred: Vec<(String, bool)> = specs(defs, &standing)
            .into_iter()
            .map(|spec| (spec.name, spec.deferred))
            .collect();
        assert_eq!(
            deferred,
            [
                ("linear__list_issues".to_string(), true),
                ("workbench__set_sketch".to_string(), false),
                ("builtin__infer".to_string(), false),
                (SEARCH_TOOL.to_string(), false),
            ]
        );
    }

    #[test]
    fn nothing_is_deferred_when_every_tool_is_standing_or_search_is_unavailable() {
        let standing = vec!["workbench__*".to_string()];
        let mut defs = vec![def("workbench__set_sketch")];
        with_search(&mut defs, &standing, Some(def(SEARCH_TOOL)));
        assert_eq!(defs.len(), 1);
        let mut defs = vec![def("linear__list_issues")];
        with_search(&mut defs, &standing, None);
        assert!(specs(defs, &standing).iter().all(|spec| !spec.deferred));
    }

    #[test]
    fn a_search_result_references_the_tools_it_names_and_failures_reference_none() {
        let call = |id: &str| ChatMessage::Assistant {
            content: None,
            thinking: Vec::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: SEARCH_TOOL.to_string(),
                arguments: json!({"query": "issues"}),
            }],
        };
        let messages = vec![
            call("s1"),
            ChatMessage::ToolResult {
                tool_call_id: "s1".to_string(),
                content: json!({"tools": [{"name": "linear__list_issues"}, {"name": "linear__get_issue"}]}),
                is_error: false,
            },
            call("s2"),
            ChatMessage::ToolResult {
                tool_call_id: "s2".to_string(),
                content: json!({"error": "no decider"}),
                is_error: true,
            },
        ];
        assert_eq!(
            references(&messages),
            BTreeMap::from([(
                "s1".to_string(),
                vec![
                    "linear__list_issues".to_string(),
                    "linear__get_issue".to_string()
                ]
            )])
        );
    }
}
