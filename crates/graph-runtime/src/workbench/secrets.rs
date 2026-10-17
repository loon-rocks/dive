use graph_core::pipeline::catalog::glob_matches;
use graph_core::pipeline::Pipeline;
use graph_core::secrets::{is_set, SecretSource};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSecret {
    pub name: String,
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretState {
    pub name: String,
    pub tools: Vec<String>,
    pub description: Option<String>,
    pub set: bool,
}

pub fn with_state(needed: &[TestSecret], source: Option<&dyn SecretSource>) -> Vec<SecretState> {
    needed
        .iter()
        .map(|secret| SecretState {
            name: secret.name.clone(),
            tools: secret.tools.clone(),
            description: secret.description.clone(),
            set: source.is_some_and(|source| is_set(source, &secret.name)),
        })
        .collect()
}

pub fn missing(needed: &[TestSecret], source: &dyn SecretSource) -> Vec<TestSecret> {
    needed
        .iter()
        .filter(|secret| !is_set(source, &secret.name))
        .cloned()
        .collect()
}

fn called_tools(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for key in ["tool_name", "toolName"] {
                if let Some(Value::String(name)) = map.get(key) {
                    found.push(name.clone());
                }
            }
            for item in map.values() {
                called_tools(item, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                called_tools(item, found);
            }
        }
        _ => {}
    }
}

fn plan_identifier<'a>(pipeline: &Pipeline, name: &'a str) -> Option<&'a str> {
    pipeline
        .packs
        .plan_target(name)
        .or_else(|| name.strip_prefix("plan__"))
}

pub async fn reachable_tools(pipeline: &Pipeline, entry: &str, own: &[String]) -> Vec<String> {
    let catalog: Vec<String> = pipeline
        .registry
        .tools()
        .await
        .map(|defs| defs.into_iter().map(|def| def.name).collect())
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([entry.to_string()]);
    let mut tools = Vec::new();
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(identifier) = plan_identifier(pipeline, &name) {
            if let Some(doc) = pipeline
                .plans
                .iter()
                .find(|doc| doc.identifier == identifier)
            {
                let mut called = Vec::new();
                called_tools(&json!(doc.steps), &mut called);
                queue.extend(called);
            }
            continue;
        }
        if let Some(agent) = name.strip_prefix("agent__") {
            if let Some(doc) = pipeline.agents.get(agent) {
                for pattern in &doc.tools {
                    if !pattern.contains('*') {
                        queue.push_back(pattern.clone());
                        continue;
                    }
                    let pool = if pattern == "*" { own } else { &catalog };
                    queue.extend(
                        pool.iter()
                            .filter(|tool| glob_matches(pattern, tool))
                            .cloned(),
                    );
                }
                for other in doc.subagents.iter().chain(&doc.handoffs) {
                    queue.push_back(format!("agent__{other}"));
                }
            }
            continue;
        }
        tools.push(name);
    }
    tools
}

pub async fn needed(pipeline: &Pipeline, entry: &str, own: &[String]) -> Vec<TestSecret> {
    let mut needed: Vec<TestSecret> = Vec::new();
    for tool in reachable_tools(pipeline, entry, own).await {
        let Some(secrets) = pipeline.registry.needed_secrets(&tool).await else {
            continue;
        };
        for secret in secrets {
            match needed.iter_mut().find(|seen| seen.name == secret.name) {
                Some(seen) => {
                    if !seen.tools.contains(&tool) {
                        seen.tools.push(tool.clone());
                    }
                    if seen.description.is_none() {
                        seen.description = secret.description;
                    }
                }
                None => needed.push(TestSecret {
                    name: secret.name,
                    tools: vec![tool.clone()],
                    description: secret.description,
                }),
            }
        }
    }
    needed
}

pub fn missing_report(needed: &[TestSecret], missing: &[TestSecret]) -> Value {
    let names: Vec<&str> = missing.iter().map(|secret| secret.name.as_str()).collect();
    json!({
        "ran": false,
        "status": "missing_secrets",
        "missing_secrets": missing,
        "needed_secrets": needed,
        "note": format!(
            "Nothing ran: the test needs {} set first. Stop here and tell the user which secrets to set, the tool that needs each and what it is (from its description), then wait. Don't work around it: no other test, no fake values, no changes to the tools. When the user says they're set, run the same test again; when they say to skip testing, offer to save the build untested.",
            names.join(", ")
        ),
    })
}

pub fn parse_report(result: &Value) -> Option<(Vec<TestSecret>, Vec<TestSecret>)> {
    if result["status"] != json!("missing_secrets") {
        return None;
    }
    let list = |key: &str| -> Vec<TestSecret> {
        serde_json::from_value(result[key].clone()).unwrap_or_default()
    };
    Some((list("needed_secrets"), list("missing_secrets")))
}
