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

const ECHO_TOOL: &str = "name: shout\ndescription: Repeats a word back\nkind: exec\ncommand: printf\nargs: ['{\"said\":\"%s\"}', '{{input.word}}']\ninput_schema:\n  type: object\n  required: [word]\n  properties:\n    word: {type: string, description: The word to repeat}\n";

struct Scripted(Mutex<VecDeque<ChatResponse>>);

impl Scripted {
    fn new() -> Self {
        let call = |id: &str, name: &str, arguments: Value| ChatResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments,
            }],
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        };
        Self(Mutex::new(VecDeque::from([
            call(
                "set",
                SET_ARTIFACT,
                json!({"kind": "tool", "yaml": ECHO_TOOL}),
            ),
            call(
                "try",
                TRY_ARTIFACT,
                json!({"kind": "tool", "input": {"word": "hello"}}),
            ),
            ChatResponse {
                content: Some("The shout tool is drafted and passed its test run.".to_string()),
                tool_calls: Vec::new(),
                thinking: Vec::new(),
                structured: None,
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ])))
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = graph_config::Config::default();
    config.settings.data_dir = workspace.path().join("data");
    config.tools.paths = vec![workspace.path().join("tools")];
    config.plans.paths = Vec::new();

    let provider: Arc<dyn ChatProvider> = Arc::new(Scripted::new());
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
    let runtime = Runtime::with_router(
        config,
        InstalledPacks::default(),
        RuntimeOptions::default(),
        router,
    )?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkbenchEvent>();
    let events = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            seen.push(event.log_line().1);
            if let WorkbenchEvent::GateAsk {
                kind: GateKind::Ask { default, .. },
                reply,
                ..
            } = event
            {
                let _ = reply.send(UiDecision::Skip {
                    result: default.unwrap_or_else(|| json!({})),
                });
            }
        }
        seen
    });

    let workbench = Workbench::open(&runtime, Start::Tool(None), workspace.path(), tx).await?;
    let reply = workbench
        .send_message("Draft a tool that repeats a word back, then test it")
        .await
        .map_err(anyhow::Error::msg)?;
    let tried = workbench
        .artifact()
        .and_then(|view| view.last_run)
        .unwrap_or_default();
    anyhow::ensure!(
        tried["ran"] == json!(true),
        "the test run did not run: {tried}"
    );
    let saved = workbench
        .save_artifact()
        .await
        .map_err(anyhow::Error::msg)?;
    drop(workbench);
    runtime.shutdown().await;
    drop(runtime);
    let seen = events.await?;

    println!("{reply}");
    println!("{saved}");
    println!("test run result: {}", tried["result"]);
    println!("{} events:", seen.len());
    for line in seen {
        println!("  {line}");
    }
    Ok(())
}
