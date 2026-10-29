use super::{ItemKind, KIND_SEPARATOR};
use crate::agent::doc::AgentDoc;
use crate::pipeline::plan::Plan;
use crate::pipeline::{AGENT_TOOL, MAP_TOOL, REDUCE_TOOL, ROUTE_TOOL};
use serde_json::{Map, Value};

pub const SELF_PREFIX: &str = "$pack__";

pub fn resolve_name(name: &str, pack: &str) -> Result<String, String> {
    let Some(rest) = name.strip_prefix(SELF_PREFIX) else {
        if name.starts_with('$') {
            return Err(format!(
                "'{name}' is not a pack reference; write $pack__<tool|plan|agent>__<name>"
            ));
        }
        return Ok(name.to_string());
    };
    if rest.contains('*') {
        return Ok(format!("{pack}{KIND_SEPARATOR}{rest}"));
    }
    match rest.split_once(KIND_SEPARATOR) {
        Some((kind, item)) if ItemKind::parse(kind).is_some() && !item.is_empty() => {
            Ok(format!("{pack}{KIND_SEPARATOR}{rest}"))
        }
        _ => Err(format!(
            "'{name}' is not a pack reference; write $pack__<tool|plan|agent>__<name>"
        )),
    }
}

pub fn resolve_plan_steps(steps: &mut Plan, pack: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for step in steps.iter_mut() {
        resolve_field(&mut step.tool_name, pack, &step.id, &mut problems);
        let tool = step.tool_name.clone();
        resolve_input(&tool, &mut step.input, pack, &step.id, &mut problems);
    }
    problems
}

pub fn resolve_agent(doc: &mut AgentDoc, pack: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let tools = doc.tools.iter_mut().map(|entry| ("tools", &mut entry.name));
    let subagents = doc.subagents.iter_mut().map(|name| ("subagents", name));
    let handoffs = doc.handoffs.iter_mut().map(|name| ("handoffs", name));
    for (field, name) in tools.chain(subagents).chain(handoffs) {
        match resolve_name(name, pack) {
            Ok(resolved) => *name = resolved,
            Err(problem) => problems.push(format!("{field}: {problem}")),
        }
    }
    problems
}

fn resolve_field(name: &mut String, pack: &str, at: &str, problems: &mut Vec<String>) {
    match resolve_name(name, pack) {
        Ok(resolved) => *name = resolved,
        Err(problem) => problems.push(format!("step {at}: {problem}")),
    }
}

fn resolve_input(
    tool: &str,
    input: &mut Map<String, Value>,
    pack: &str,
    at: &str,
    problems: &mut Vec<String>,
) {
    match tool {
        ROUTE_TOOL => {
            for side in ["then", "else"] {
                if let Some(body) = input.get_mut(side) {
                    resolve_body(body, pack, &format!("{at}/{side}"), problems);
                }
            }
            if let Some(Value::Object(cases)) = input.get_mut("cases") {
                for (key, body) in cases.iter_mut() {
                    resolve_body(body, pack, &format!("{at}/{key}"), problems);
                }
            }
        }
        MAP_TOOL | REDUCE_TOOL => {
            if let Some(body) = input.get_mut("do") {
                resolve_body(body, pack, &format!("{at}/do"), problems);
            }
        }
        AGENT_TOOL => {
            if let Some(Value::Array(tools)) = input.get_mut("tools") {
                for entry in tools.iter_mut() {
                    if let Value::String(name) = entry {
                        resolve_field(name, pack, at, problems);
                    }
                }
            }
        }
        _ => {}
    }
}

fn resolve_body(body: &mut Value, pack: &str, at: &str, problems: &mut Vec<String>) {
    match body {
        Value::Array(steps) => {
            for step in steps.iter_mut() {
                resolve_body_step(step, pack, at, problems);
            }
        }
        Value::Object(_) => resolve_body_step(body, pack, at, problems),
        _ => {}
    }
}

fn resolve_body_step(step: &mut Value, pack: &str, at: &str, problems: &mut Vec<String>) {
    let Value::Object(fields) = step else {
        return;
    };
    let at = match fields.get("id").and_then(Value::as_str) {
        Some(id) => format!("{at}/{id}"),
        None => at.to_string(),
    };
    let mut tool = None;
    for key in ["tool_name", "toolName"] {
        if let Some(Value::String(name)) = fields.get_mut(key) {
            resolve_field(name, pack, &at, problems);
            tool = Some(name.clone());
        }
    }
    let Some(tool) = tool else {
        return;
    };
    if let Some(Value::Object(input)) = fields.get_mut("input") {
        resolve_input(&tool, input, pack, &at, problems);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::plan::Step;

    #[test]
    fn names_resolve_only_with_the_self_prefix() {
        assert_eq!(
            resolve_name("$pack__tool__file_info", "example").unwrap(),
            "example__tool__file_info"
        );
        assert_eq!(
            resolve_name("$pack__tool__*", "example").unwrap(),
            "example__tool__*"
        );
        assert_eq!(
            resolve_name("builtin__infer", "example").unwrap(),
            "builtin__infer"
        );
        assert!(resolve_name("$pack__widget__x", "example").is_err());
        assert!(resolve_name("$pack__tool__", "example").is_err());
        assert!(resolve_name("$other__tool__x", "example").is_err());
    }

    fn steps(json: Value) -> Plan {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn every_structural_tool_name_resolves_at_any_depth() {
        let mut plan = steps(serde_json::json!([
            {"id": "a", "tool_name": "$pack__tool__file_info", "input": {"args": "$pack__tool__x $HOME"}},
            {"id": "b", "tool_name": "route", "input": {
                "if": {"value": "{{a.ok}}", "op": "truthy"},
                "then": {"tool_name": "$pack__plan__describe", "input": {}},
                "else": [
                    {"id": "e1", "toolName": "map", "input": {"over": "[]", "do": [
                        {"id": "m1", "tool_name": "$pack__agent__helper", "input": {"prompt": "use $pack__tool__x"}},
                        {"id": "m2", "tool_name": "route", "input": {
                            "if": {"value": "1", "op": "truthy"},
                            "cases": {"x": {"tool_name": "$pack__tool__deep", "input": {}}}
                        }}
                    ]}}
                ]
            }},
            {"id": "c", "tool_name": "reduce", "input": {"over": "[]", "initial": 0, "do": {"tool_name": "$pack__tool__fold", "input": {}}}},
            {"id": "d", "tool_name": "agent", "input": {"prompt": "$pack__tool__x stays", "tools": ["$pack__tool__*", "builtin__infer"]}}
        ]));
        let problems = resolve_plan_steps(&mut plan, "example");
        assert!(problems.is_empty(), "{problems:?}");
        let text = serde_json::to_value(&plan).unwrap();
        assert_eq!(text[0]["toolName"], "example__tool__file_info");
        assert_eq!(text[0]["input"]["args"], "$pack__tool__x $HOME");
        assert_eq!(
            text[1]["input"]["then"]["tool_name"],
            "example__plan__describe"
        );
        let body = &text[1]["input"]["else"][0]["input"]["do"];
        assert_eq!(body[0]["tool_name"], "example__agent__helper");
        assert_eq!(body[0]["input"]["prompt"], "use $pack__tool__x");
        assert_eq!(
            body[1]["input"]["cases"]["x"]["tool_name"],
            "example__tool__deep"
        );
        assert_eq!(text[2]["input"]["do"]["tool_name"], "example__tool__fold");
        assert_eq!(text[3]["input"]["tools"][0], "example__tool__*");
        assert_eq!(text[3]["input"]["prompt"], "$pack__tool__x stays");
    }

    #[test]
    fn a_malformed_reference_is_a_problem_naming_the_step() {
        let mut plan = vec![Step {
            id: "a".into(),
            tool_name: "$pack__widget__x".into(),
            input: Map::new(),
            reasoning: None,
            files: None,
        }];
        let problems = resolve_plan_steps(&mut plan, "example");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("step a:"), "{problems:?}");
    }

    #[test]
    fn agent_lists_resolve_and_the_prompt_does_not() {
        let mut doc = crate::agent::doc::parse_agent_source(
            "name: helper\ndescription: d\nmodel: chat\ntools: [$pack__tool__file_info, builtin__infer]\nsubagents: [$pack__agent__sub]\nhandoffs: [$pack__agent__other]\nsystem_prompt: call $pack__tool__file_info\n",
        )
        .unwrap();
        assert!(resolve_agent(&mut doc, "example").is_empty());
        assert_eq!(
            doc.tool_names(),
            ["example__tool__file_info", "builtin__infer"]
        );
        assert_eq!(doc.subagents, ["example__agent__sub"]);
        assert_eq!(doc.handoffs, ["example__agent__other"]);
        assert_eq!(doc.system_prompt, "call $pack__tool__file_info");
    }
}
