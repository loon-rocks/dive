use async_trait::async_trait;
use futures::StreamExt;
use graph_config::{ModelChoice, ModelRoles};
use graph_llm::types::{
    ChatRequest, ChatResponse, EventStream, StopReason, StreamEvent, ToolCall, Usage,
};
use graph_llm::{ChatProvider, LlmError, ModelRouter};
use graph_runtime::packs::InstalledPacks;
use graph_runtime::runtime::{Runtime, RuntimeOptions};
use graph_runtime::workbench::artifact::{SET_ARTIFACT, TRY_ARTIFACT};
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
            Some((SET_ARTIFACT, json!({"kind": "tool", "yaml": TOOL}))),
        ),
        response(
            None,
            Some((
                TRY_ARTIFACT,
                json!({"kind": "tool", "input": {"word": "hello"}}),
            )),
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
    Shown(bool),
    Asked,
    Turn(Result<String, String>),
    Status(String),
}

#[tokio::test]
async fn drafting_trying_and_saving_a_tool_reports_each_step_as_an_event() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime(root.path());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkbenchEvent>();
    let collector = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                WorkbenchEvent::ArtifactChanged(view) => seen.push(Seen::Shown(view.is_some())),
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
    assert_eq!(workbench.active_agent(), "tool_drafter");
    let reply = workbench.send_message("make a shout tool").await;
    assert_eq!(reply, Ok("drafted and tested".to_string()));
    let view = workbench.artifact().expect("the draft is in the pane");
    assert_eq!(view.yaml, TOOL);
    assert!(view.tested);
    assert_eq!(view.last_run.unwrap()["result"], json!({"said": "hello"}));

    let saved = workbench.save_artifact().await.unwrap();
    let path = root.path().join("tools/shout.yaml");
    assert!(saved.starts_with("saved user__shout to "), "{saved}");
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("name: shout"));
    assert!(workbench.artifact().is_none());

    drop(workbench);
    runtime.shutdown().await;
    drop(runtime);
    assert_eq!(
        collector.await.unwrap(),
        vec![
            Seen::Shown(true),
            Seen::Asked,
            Seen::Shown(true),
            Seen::Turn(Ok("drafted and tested".to_string())),
            Seen::Shown(false),
            Seen::Status(saved),
        ]
    );
}
