use super::artifact::{self, ArtifactKind, Existing};
use super::network::{Network, TestRun};
use dive_core::agent::conversation::starts_run;
use dive_core::store::EntryBody;
use dive_core::NewEntry;
use dive_llm::types::{ChatMessage, ToolCall};
use serde_json::Value;
use std::collections::HashMap;

type Results = HashMap<String, (Value, bool)>;

pub type Items<'a> = &'a dyn Fn(ArtifactKind, &str) -> Option<Existing>;

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
struct Replay<'a> {
    network: Network,
    composed: Option<Value>,
    runs: Vec<(&'a NewEntry, bool)>,
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

impl<'a> Replay<'a> {
    fn messages(&mut self, messages: &[ChatMessage], results: &Results, items: Items) {
        for message in messages {
            let ChatMessage::Assistant { tool_calls, .. } = message else {
                continue;
            };
            for call in tool_calls {
                if let Some(run) = self.started(call) {
                    self.run(run, results, items);
                    continue;
                }
                if let Some((result, false)) = results.get(call.id.as_str()) {
                    self.call(call, result, items);
                }
            }
        }
    }

    fn started(&mut self, call: &ToolCall) -> Option<&'a NewEntry> {
        let (run, done) = self.runs.iter_mut().find(|(run, done)| {
            !*done
                && matches!(&run.body, EntryBody::SubagentRun { agent, input, .. } if starts_run(call, agent, input))
        })?;
        *done = true;
        Some(*run)
    }

    fn run(&mut self, entry: &'a NewEntry, results: &Results, items: Items) {
        let EntryBody::SubagentRun {
            agent,
            input,
            messages,
            output,
            final_,
            ..
        } = &entry.body
        else {
            return;
        };
        let node = input["node"].as_str().map(str::to_string);
        if let Some(node) = &node {
            self.network.builder_started(node, agent);
        }
        self.messages(messages, results, items);
        if let Some(node) = &node {
            self.network.builder_finished(node, agent, output, !final_);
        }
    }

    fn call(&mut self, call: &ToolCall, result: &Value, items: Items) {
        let input = &call.arguments;
        if result.get("error").is_some() {
            return;
        }
        let kind = input["kind"].as_str().and_then(ArtifactKind::parse);
        if let Some((needed, _)) = super::secrets::parse_report(result) {
            if matches!(
                call.name.as_str(),
                artifact::TEST_NETWORK | artifact::TRY_ARTIFACT
            ) {
                self.network.wait_on_secrets(needed);
            }
            return;
        }
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
                        let doc = dive_core::pipeline::plan_doc(self.composed.as_ref()?).ok()?;
                        dive_core::pipeline::authoring::to_yaml(&doc).ok()
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
                if let (Some(kind), Some(_)) = (kind, result["opened"].as_str()) {
                    if let Some(found) = items(kind, input["name"].as_str().unwrap_or_default()) {
                        self.network.open(kind, found, Vec::new());
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
            artifact::DISCARD_NETWORK => {
                self.network.discard_all();
            }
            artifact::SAVE_NETWORK => match result["files"].as_array() {
                Some(files) => {
                    let written: Vec<(ArtifactKind, String, std::path::PathBuf)> = files
                        .iter()
                        .filter_map(|file| {
                            Some((
                                ArtifactKind::parse(file["kind"].as_str()?)?,
                                file["name"].as_str()?.to_string(),
                                std::path::PathBuf::from(file["path"].as_str()?),
                            ))
                        })
                        .collect();
                    self.network.saved(&written);
                }
                None => {
                    self.network.clear();
                }
            },
            artifact::TEST_NETWORK => {
                self.network.waiting_on_secrets = false;
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

pub fn network(entries: &[NewEntry], opened: &[(ArtifactKind, String)], items: Items) -> Network {
    let mut results = Results::new();
    for entry in entries {
        match &entry.body {
            EntryBody::Message { message } => collect(std::slice::from_ref(message), &mut results),
            EntryBody::SubagentRun { messages, .. } => collect(messages, &mut results),
            EntryBody::Handoff { .. } => {}
        }
    }
    let mut replay = Replay {
        runs: entries
            .iter()
            .filter(|entry| matches!(entry.body, EntryBody::SubagentRun { .. }))
            .map(|entry| (entry, false))
            .collect(),
        ..Replay::default()
    };
    for (kind, name) in opened {
        if let Some(found) = items(*kind, name) {
            replay.network.open(*kind, found, Vec::new());
        }
    }
    for entry in entries {
        match &entry.body {
            EntryBody::Message { message } => {
                replay.messages(std::slice::from_ref(message), &results, items);
            }
            EntryBody::SubagentRun { .. } => {
                let pending = replay
                    .runs
                    .iter_mut()
                    .find(|(run, done)| !*done && std::ptr::eq(*run, entry));
                if let Some((_, done)) = pending {
                    *done = true;
                    replay.run(entry, &results, items);
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
        let network = network(&entries, &[], &|_, _| None);
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
    fn subagent_runs_written_after_the_turn_replay_at_the_call_that_started_them() {
        let sketch = json!({
            "restate": {"outcome": "Words counted", "success": "A number", "constraint": "No model", "out_of_scope": []},
            "nodes": [{"name": "word_count", "kind": "tool", "job": "Counts words"}]
        });
        let tool = "name: word_count\ndescription: Counts words\nkind: exec\ncommand: wc\n";
        let input = json!({"node": "word_count", "notes": "count them"});
        let entries = vec![
            call("1", artifact::SET_SKETCH, sketch),
            result("1", json!({"nodes": 1})),
            call("2", "agent__tool_drafter", input.clone()),
            result("2", json!({"name": "word_count", "status": "drafted"})),
            call(
                "4",
                artifact::TEST_NETWORK,
                json!({"entry": "word_count", "input": {"text": "a b"}}),
            ),
            result(
                "4",
                json!({"passed": true, "ran": ["user__word_count"], "answer": "2"}),
            ),
            NewEntry::new(
                "network_drafter",
                EntryBody::SubagentRun {
                    agent: "tool_drafter".into(),
                    caller: "network_drafter".into(),
                    input,
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
        ];
        let network = network(&entries, &[], &|_, _| None);
        let node = network.node(ArtifactKind::Tool, "word_count").unwrap();
        assert_eq!(node.yaml.as_deref(), Some(tool));
        assert!(network.tested());
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
        assert!(network(&entries, &[], &|_, _| None).is_empty());
    }

    fn opened_file(name: &str, yaml: &str) -> Existing {
        Existing {
            yaml: yaml.to_string(),
            editing: Some(std::path::PathBuf::from(format!("/work/tools/{name}.yaml"))),
            name: name.to_string(),
            origin: super::super::network::Origin::File,
        }
    }

    #[test]
    fn items_opened_at_the_start_replay_before_the_edits_made_to_them() {
        let saved = "name: word_count\ndescription: Counts words\nkind: exec\ncommand: wc\n";
        let edited =
            "name: word_count\ndescription: Counts words\nkind: exec\ncommand: wc\nargs: [-w]\n";
        let digest = "identifier: digest\nname: Digest\ndescription: d\nsteps: []\n";
        let items = |kind: ArtifactKind, name: &str| match (kind, name) {
            (ArtifactKind::Tool, "word_count") => Some(opened_file("word_count", saved)),
            (ArtifactKind::Plan, "digest") => Some(Existing {
                yaml: digest.to_string(),
                editing: None,
                name: "digest".to_string(),
                origin: super::super::network::Origin::Builtin,
            }),
            _ => None,
        };
        let entries = vec![
            NewEntry::new(
                "network_editor",
                EntryBody::SubagentRun {
                    agent: "tool_drafter".into(),
                    caller: "network_editor".into(),
                    input: json!({"node": "word_count", "notes": "count words, not lines"}),
                    messages: vec![
                        ChatMessage::Assistant {
                            content: None,
                            tool_calls: vec![ToolCall {
                                id: "1".into(),
                                name: artifact::SET_ARTIFACT.into(),
                                arguments: json!({"kind": "tool", "yaml": edited}),
                            }],
                            thinking: Vec::new(),
                        },
                        ChatMessage::ToolResult {
                            tool_call_id: "1".into(),
                            content: json!({"name": "word_count", "kind": "tool", "problems": []}),
                            is_error: false,
                        },
                    ],
                    output: json!({"name": "word_count", "status": "drafted", "summary": "counts words"}),
                    final_: true,
                },
            ),
            call(
                "2",
                artifact::LOAD_ARTIFACT,
                json!({"kind": "plan", "name": "digest", "reason": "it calls word_count"}),
            ),
            result("2", json!({"opened": "digest", "kind": "plan"})),
        ];
        let opened = [(ArtifactKind::Tool, "word_count".to_string())];

        let network = network(&entries, &opened, &items);
        let tool = network.node(ArtifactKind::Tool, "word_count").unwrap();
        assert_eq!(tool.yaml.as_deref(), Some(edited));
        assert_eq!(tool.base.as_deref(), Some(saved));
        assert!(tool.changed());
        assert_eq!(
            tool.editing.as_deref(),
            Some(std::path::Path::new("/work/tools/word_count.yaml"))
        );
        let plan = network.node(ArtifactKind::Plan, "digest").unwrap();
        assert!(!plan.changed());
        assert!(network.is_edit());

        let lost = super::network(&entries, &[], &items);
        let tool = lost.node(ArtifactKind::Tool, "word_count").unwrap();
        assert!(
            tool.base.is_none(),
            "without the opening the edit has no file to go back to"
        );
    }

    #[test]
    fn a_saved_edit_keeps_its_items_and_a_discarded_one_reverts_them() {
        let saved = "name: word_count\ndescription: Counts words\nkind: exec\ncommand: wc\n";
        let edited =
            "name: word_count\ndescription: Counts words\nkind: exec\ncommand: wc\nargs: [-w]\n";
        let items = |_: ArtifactKind, _: &str| Some(opened_file("word_count", saved));
        let opened = [(ArtifactKind::Tool, "word_count".to_string())];
        let edit = |id: &str| {
            vec![
                call(
                    id,
                    artifact::SET_ARTIFACT,
                    json!({"kind": "tool", "yaml": edited}),
                ),
                result(
                    id,
                    json!({"name": "word_count", "kind": "tool", "problems": []}),
                ),
            ]
        };
        let mut entries = edit("1");
        entries.push(call("2", artifact::SAVE_NETWORK, json!({"untested": true})));
        entries.push(result(
            "2",
            json!({"saved": "saved /work/tools/word_count.yaml", "untested": true, "files": [{"kind": "tool", "name": "word_count", "path": "/work/tools/word_count.yaml", "destination": "file"}]}),
        ));
        let network = network(&entries, &opened, &items);
        let tool = network.node(ArtifactKind::Tool, "word_count").unwrap();
        assert_eq!(tool.base.as_deref(), Some(edited));
        assert!(!network.has_edits());

        let mut entries = edit("1");
        entries.push(call("2", artifact::DISCARD_NETWORK, json!({})));
        entries.push(result("2", json!({"discarded": "discarded 1"})));
        let network = super::network(&entries, &opened, &items);
        let tool = network.node(ArtifactKind::Tool, "word_count").unwrap();
        assert_eq!(tool.yaml.as_deref(), Some(saved));
        assert!(!network.has_edits());
    }
}
