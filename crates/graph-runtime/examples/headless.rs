use async_trait::async_trait;
use futures::StreamExt;
use graph_config::{ModelChoice, ModelRoles};
use graph_core::NullSink;
use graph_llm::types::{ChatRequest, ChatResponse, EventStream, StopReason, StreamEvent, Usage};
use graph_llm::{ChatProvider, LlmError, ModelRouter};
use graph_runtime::chat::ChatStart;
use graph_runtime::packs::InstalledPacks;
use graph_runtime::runtime::{PipelineHooks, Runtime, RuntimeOptions};
use std::collections::HashMap;
use std::sync::Arc;

struct Canned;

impl Canned {
    fn reply() -> ChatResponse {
        ChatResponse {
            content: Some("Hello from a scripted model.".to_string()),
            tool_calls: Vec::new(),
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }
}

#[async_trait]
impl ChatProvider for Canned {
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, LlmError> {
        Ok(Self::reply())
    }

    async fn chat_stream(&self, _: ChatRequest) -> Result<EventStream, LlmError> {
        Ok(futures::stream::iter(vec![Ok(StreamEvent::Completed(Self::reply()))]).boxed())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let mut config = graph_config::Config::default();
    config.settings.data_dir = workspace.path().join("data");
    config.tools.paths = Vec::new();
    config.plans.paths = Vec::new();

    let provider: Arc<dyn ChatProvider> = Arc::new(Canned);
    let router = ModelRouter::with_providers(
        HashMap::from([("canned".to_string(), provider)]),
        ModelRoles::from([(
            "default",
            ModelChoice {
                provider: "canned".to_string(),
                model: "canned".to_string(),
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
    let store = runtime.store()?;

    let mut chat = ChatStart::resolve(&runtime, store.clone(), None, None)
        .await?
        .open(
            &runtime,
            Arc::new(NullSink),
            PipelineHooks::default(),
            Some("chat".to_string()),
        )
        .await?;
    let report = chat.send("Say hello").await?;
    runtime.shutdown().await;

    let thread = chat.thread().expect("the first message creates a thread");
    let persisted = store.load_entries(&thread.id).await?.len();
    println!(
        "{} ({}): {}",
        report.turn.active, thread.id, report.turn.text
    );
    println!(
        "{persisted} entries persisted under {}",
        workspace.path().display()
    );
    Ok(())
}
