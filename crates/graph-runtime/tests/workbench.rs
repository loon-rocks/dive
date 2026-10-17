use async_trait::async_trait;
use futures::StreamExt;
use graph_config::{ModelChoice, ModelRoles};
use graph_llm::types::{
    ChatRequest, ChatResponse, EventStream, StopReason, StreamEvent, ToolCall, Usage,
};
use graph_llm::{ChatProvider, LlmError, ModelRouter};
use graph_runtime::packs::InstalledPacks;
use graph_runtime::runtime::{Runtime, RuntimeOptions};
use graph_runtime::workbench::artifact::{SET_ARTIFACT, SET_SKETCH, TRY_ARTIFACT};
use graph_runtime::workbench::{GateKind, Start, UiDecision, Workbench, WorkbenchEvent};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

const TOOL: &str = "name: shout\ndescription: Repeats a word back\nkind: exec\ncommand: printf\nargs: ['{\"said\":\"%s\"}', '{{input.word}}']\ninput_schema:\n  type: object\n  required: [word]\n  properties:\n    word: {type: string, description: The word to repeat}\n";

struct Scripted(Mutex<VecDeque<ChatResponse>>);

fn response(content: Option<&str>, call: Option<(&str, Value)>) -> ChatResponse {
    ChatResponse {
        content: content.map(str::to_string),
        stop_reason: if call.is_some() {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        },
        tool_calls: call
            .into_iter()
            .map(|(name, arguments)| ToolCall {
                id: name.to_string(),
                name: name.to_string(),
                arguments,
            })
            .collect(),
        thinking: Vec::new(),
        structured: None,
        usage: Usage::default(),
    }
}

#[async_trait]
impl ChatProvider for Scripted {
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| LlmError::Parse("script exhausted".to_string()))
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let response = self.chat(request).await?;
        Ok(futures::stream::iter(vec![Ok(StreamEvent::Completed(response))]).boxed())
    }
}

fn runtime(root: &std::path::Path) -> Runtime {
    let mut config = graph_config::Config::default();
    config.settings.data_dir = root.join("data");
    config.tools.paths = vec![root.join("tools")];
    config.plans.paths = Vec::new();
    let provider: Arc<dyn ChatProvider> = Arc::new(Scripted(Mutex::new(VecDeque::from([
        response(
            None,
            Some((
                SET_SKETCH,
                json!({
                    "restate": {
                        "outcome": "A tool that repeats a word back",
                        "success": "It returns the word",
                        "constraint": "No model",
                        "out_of_scope": ["Anything else"]
                    },
                    "nodes": [{"name": "shout", "kind": "tool", "job": "Repeats a word back", "principles": ["No model needed"]}]
                }),
            )),
        ),
        response(
            None,
            Some(("agent__tool_drafter", json!({"node": "shout"}))),
        ),
        response(
            None,
            Some((SET_ARTIFACT, json!({"kind": "tool", "yaml": TOOL}))),
        ),
        response(
            None,
            Some((
                TRY_ARTIFACT,
                json!({"kind": "tool", "input": {"word": "hello"}}),
            )),
        ),
        response(
            Some(
                r#"{"name": "shout", "status": "drafted", "summary": "Takes a word and returns it as said"}"#,
            ),
            None,
        ),
        response(Some("drafted and tested"), None),
    ]))));
    let router = ModelRouter::with_providers(
        HashMap::from([("scripted".to_string(), provider)]),
        ModelRoles::from([(
            "default",
            ModelChoice {
                provider: "scripted".to_string(),
                model: "scripted".to_string(),
                temperature: None,
                description: None,
                fallbacks: Vec::new(),
                context_window: None,
            },
        )]),
    );
    Runtime::with_router(
        config,
        InstalledPacks::default(),
        RuntimeOptions::default(),
        router,
    )
    .unwrap()
}

#[derive(Debug, PartialEq)]
enum Seen {
    Shown(usize),
    Asked,
    Turn(Result<String, String>),
    Status(String),
}

#[tokio::test]
async fn the_architect_sketches_a_builder_drafts_and_tries_and_the_network_saves() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkbenchEvent>();
    let collector = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                WorkbenchEvent::NetworkChanged(view) => seen.push(Seen::Shown(view.unsaved())),
                WorkbenchEvent::GateAsk {
                    kind: GateKind::Ask { default, .. },
                    reply,
                    ..
                } => {
                    seen.push(Seen::Asked);
                    let _ = reply.send(UiDecision::Skip {
                        result: default.unwrap_or_default(),
                    });
                }
                WorkbenchEvent::TurnFinished(result) => seen.push(Seen::Turn(result)),
                WorkbenchEvent::Status(status) => seen.push(Seen::Status(status)),
                _ => {}
            }
        }
        seen
    });

    let workbench = Workbench::open(&runtime, Start::Tool(None), root.path(), tx)
        .await
        .unwrap();
    assert_eq!(workbench.active_agent(), "network_drafter");
    let reply = workbench.send_message("make a shout tool").await;
    assert_eq!(reply, Ok("drafted and tested".to_string()));
    let view = workbench.network();
    assert_eq!(
        view.restate.as_ref().unwrap().out_of_scope,
        ["Anything else"]
    );
    let node = &view.nodes[0];
    assert_eq!(node.yaml.as_deref(), Some(TOOL));
    assert_eq!(
        node.last_run.as_ref().unwrap()["result"],
        json!({"said": "hello"})
    );

    let saved = workbench.save_network().await.unwrap();
    let path = root.path().join("tools/shout.yaml");
    assert!(saved.ends_with("tools/shout.yaml (untested)"), "{saved}");
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("name: shout"));
    assert!(workbench.network().is_empty());

    drop(workbench);
    runtime.shutdown().await;
    drop(runtime);
    assert_eq!(
        collector.await.unwrap(),
        vec![
            Seen::Shown(0),
            Seen::Shown(0),
            Seen::Shown(1),
            Seen::Asked,
            Seen::Shown(1),
            Seen::Shown(1),
            Seen::Turn(Ok("drafted and tested".to_string())),
            Seen::Shown(0),
            Seen::Status(saved),
        ]
    );
}
