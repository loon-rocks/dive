use super::artifact::{self, ArtifactKind};
use super::network::{Network, TestRun};
use graph_core::store::EntryBody;
use graph_core::NewEntry;
use graph_llm::types::{ChatMessage, ToolCall};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

type Results = HashMap<String, (Value, bool)>;

fn collect(messages: &[ChatMessage], results: &mut Results) {
    for message in messages {
        if let ChatMessage::ToolResult {
            tool_call_id,
            content,
            is_error,
        } = message
        {
            results.insert(tool_call_id.clone(), (content.clone(), *is_error));
        }
    }
}

#[derive(Default)]
struct Replay {
    network: Network,
    composed: Option<Value>,
}

fn yaml_name(yaml: &str) -> Option<String> {
    serde_yaml::from_str::<serde_yaml::Value>(yaml)
        .ok()?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

impl Replay {
    fn messages(&mut self, messages: &[ChatMessage], results: &Results, tool_dirs: &[PathBuf]) {
        for message in messages {
            let ChatMessage::Assistant { tool_calls, .. } = message else {
                continue;
            };
            for call in tool_calls {
                if let Some((result, false)) = results.get(call.id.as_str()) {
                    self.call(call, result, tool_dirs);
                }
            }
        }
    }

    fn call(&mut self, call: &ToolCall, result: &Value, tool_dirs: &[PathBuf]) {
        let input = &call.arguments;
        if result.get("error").is_some() {
            return;
        }
        let kind = input["kind"].as_str().and_then(ArtifactKind::parse);
        match call.name.as_str() {
            "plan__compose_plan" => {
                self.composed = result.get("plan").filter(|plan| plan.is_object()).cloned();
            }
            artifact::SET_SKETCH => {
                let _ = self.network.set_sketch(input);
            }
            artifact::SET_INTERVIEW => {
                let _ = self.network.set_interview(input);
            }
            artifact::PROPOSE_DESIGNS => {
                let _ = self.network.propose_designs(input);
            }
            artifact::SET_ARTIFACT => match kind {
                Some(ArtifactKind::Plan) if result["kind"] == "plan" => {
                    let yaml = input["yaml"].as_str().map(str::to_string).or_else(|| {
                        let doc = graph_core::pipeline::plan_doc(self.composed.as_ref()?).ok()?;
                        graph_core::pipeline::authoring::to_yaml(&doc).ok()
                    });
                    if let (Some(yaml), Some(name)) = (yaml, result["name"].as_str()) {
                        let _ = self.network.put_draft(
                            ArtifactKind::Plan,
                            name,
                            yaml,
                            strings(&result["problems"]),
                        );
                    }
                }
                Some(kind @ (ArtifactKind::Agent | ArtifactKind::Tool)) => {
                    if let Some(yaml) = input["yaml"].as_str() {
                        if let Some(name) = yaml_name(yaml) {
                            let _ = self.network.put_draft(
                                kind,
                                &name,
                                yaml.to_string(),
                                strings(&result["problems"]),
                            );
                        }
                    }
                }
                _ => {}
            },
            artifact::LOAD_ARTIFACT => {
                if let (Some(kind @ (ArtifactKind::Agent | ArtifactKind::Tool)), Some(name)) =
                    (kind, result["opened"].as_str())
                {
                    if let Ok(found) = artifact::existing(kind, name, tool_dirs) {
                        self.network.open(
                            kind,
                            &found.name,
                            found.yaml,
                            found.editing,
                            Some(found.name.clone()),
                            Vec::new(),
                        );
                    }
                }
            }
            artifact::DISCARD_ARTIFACT => {
                if let Some(kind) = kind {
                    let name = input["name"].as_str().map(str::to_string).or_else(|| {
                        let drafted: Vec<String> = self
                            .network
                            .nodes
                            .iter()
                            .filter(|node| node.kind == kind && node.drafted())
                            .map(|node| node.name.clone())
                            .collect();
                        (drafted.len() == 1).then(|| drafted[0].clone())
                    });
                    if let Some(name) = name {
                        self.network.discard(kind, &name);
                    }
                }
            }
            artifact::DISCARD_NETWORK | artifact::SAVE_NETWORK => {
                self.network.clear();
            }
            artifact::TEST_NETWORK => {
                self.network.record_test(TestRun {
                    entry: input["entry"].as_str().unwrap_or_default().to_string(),
                    message: input["message"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| input["input"].to_string()),
                    passed: result["passed"].as_bool().unwrap_or(false),
                    ran: strings(&result["ran"]),
                    answer: result["answer"].as_str().unwrap_or_default().to_string(),
                });
            }
            artifact::FAIL_TEST => {
                self.network.fail_last_test(
                    input["reason"].as_str().unwrap_or_default(),
                    &strings(&input["nodes"]),
                );
            }
            _ => {}
        }
    }
}

pub fn network(entries: &[NewEntry], tool_dirs: &[PathBuf]) -> Network {
    let mut results = Results::new();
    for entry in entries {
        match &entry.body {
            EntryBody::Message { message } => collect(std::slice::from_ref(message), &mut results),
            EntryBody::SubagentRun { messages, .. } => collect(messages, &mut results),
            EntryBody::Handoff { .. } => {}
        }
    }
    let mut replay = Replay::default();
    for entry in entries {
        match &entry.body {
            EntryBody::Message { message } => {
                replay.messages(std::slice::from_ref(message), &results, tool_dirs);
            }
            EntryBody::SubagentRun {
                agent,
                input,
                messages,
                output,
                final_,
                ..
            } => {
                let node = input["node"].as_str().map(str::to_string);
                if let Some(node) = &node {
                    replay.network.builder_started(node, agent);
                }
                replay.messages(messages, &results, tool_dirs);
                if let Some(node) = &node {
                    replay
                        .network
                        .builder_finished(node, agent, output, !final_);
                }
            }
            EntryBody::Handoff { .. } => {}
        }
    }
    replay.network
}

pub fn last_reply(entries: &[NewEntry]) -> Option<String> {
    entries.iter().rev().find_map(|entry| match &entry.body {
        EntryBody::Message {
            message: ChatMessage::Assistant { tool_calls, .. },
        } => tool_calls
            .iter()
            .rev()
            .find(|call| {
                call.name == artifact::SET_INTERVIEW || call.name == artifact::PROPOSE_DESIGNS
            })
            .and_then(|call| call.arguments["reply"].as_str())
            .map(str::to_string)
            .filter(|reply| !reply.trim().is_empty()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str, arguments: Value) -> NewEntry {
        NewEntry::message(
            "network_drafter",
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments,
                }],
                thinking: Vec::new(),
            },
        )
    }

    fn result(id: &str, content: Value) -> NewEntry {
        NewEntry::message(
            "network_drafter",
            ChatMessage::ToolResult {
                tool_call_id: id.to_string(),
                content,
                is_error: false,
            },
        )
    }

    #[test]
    fn a_thread_replays_into_the_network_it_built() {
        let sketch = json!({
            "restate": {"outcome": "Words counted", "success": "A number", "constraint": "No model", "out_of_scope": []},
            "nodes": [{"name": "word_count", "kind": "tool", "job": "Counts words"}]
        });
        let tool = "name: word_count\ndescription: Counts words\nkind: exec\ncommand: wc\n";
        let entries = vec![
            call(
                "1",
                artifact::SET_INTERVIEW,
                json!({"reply": "Question: words?", "confidence": 60, "facts": []}),
            ),
            result("1", json!({"confidence": 60, "shown": true})),
            call("2", artifact::SET_SKETCH, sketch),
            result("2", json!({"nodes": 1})),
            NewEntry::new(
                "network_drafter",
                EntryBody::SubagentRun {
                    agent: "tool_drafter".into(),
                    caller: "network_drafter".into(),
                    input: json!({"node": "word_count"}),
                    messages: vec![
                        ChatMessage::Assistant {
                            content: None,
                            tool_calls: vec![ToolCall {
                                id: "3".into(),
                                name: artifact::SET_ARTIFACT.into(),
                                arguments: json!({"kind": "tool", "yaml": tool}),
                            }],
                            thinking: Vec::new(),
                        },
                        ChatMessage::ToolResult {
                            tool_call_id: "3".into(),
                            content: json!({"name": "word_count", "kind": "tool", "problems": []}),
                            is_error: false,
                        },
                    ],
                    output: json!({"name": "word_count", "status": "drafted", "summary": "counts"}),
                    final_: true,
                },
            ),
            call(
                "4",
                artifact::TEST_NETWORK,
                json!({"entry": "word_count", "input": {"text": "a b"}}),
            ),
            result(
                "4",
                json!({"passed": true, "ran": ["user__word_count"], "answer": "2"}),
            ),
        ];
        let network = network(&entries, &[]);
        assert_eq!(network.interview.as_ref().unwrap().confidence, 60);
        let node = network.node(ArtifactKind::Tool, "word_count").unwrap();
        assert_eq!(node.yaml.as_deref(), Some(tool));
        assert_eq!(node.builder.as_deref(), Some("tool_drafter"));
        assert!(network.tested());
        assert_eq!(
            last_reply(&entries[..2]).as_deref(),
            Some("Question: words?")
        );
    }

    #[test]
    fn a_saved_network_replays_as_empty() {
        let entries = vec![
            call(
                "1",
                artifact::SET_SKETCH,
                json!({"restate": {"outcome": "x"}, "nodes": [{"name": "a", "kind": "tool", "job": "j"}]}),
            ),
            result("1", json!({})),
            call("2", artifact::SAVE_NETWORK, json!({})),
            result("2", json!({"saved": "saved a"})),
        ];
        assert!(network(&entries, &[]).is_empty());
    }
}
