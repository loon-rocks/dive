use crate::cancel::CancelHandle;
use crate::runtime::{
    load_history, persist_turn, resolve_thread, starting_agent, title_from, PipelineHooks, Runtime,
};
use anyhow::Result;
use graph_core::agent::conversation::{Conversation, ConversationTurn};
use graph_core::usage::{UsageLedger, UsageReport};
use graph_core::{EventSink, NewEntry, RunStart, Store, ThreadMeta};
use std::sync::Arc;

pub struct ChatStart {
    store: Arc<dyn Store>,
    thread: Option<ThreadMeta>,
    fresh_thread: bool,
    owner: String,
    active: String,
}

impl ChatStart {
    pub async fn resolve(
        runtime: &Runtime,
        store: Arc<dyn Store>,
        agent: Option<&str>,
        thread: Option<Option<String>>,
    ) -> Result<Self> {
        let thread = resolve_thread(store.as_ref(), thread).await?;
        let agents = runtime.agent_set();
        let active = starting_agent(thread.as_ref(), agent, &|name| agents.get(name).is_some())?;
        let owner = thread
            .as_ref()
            .map(|meta| meta.owner.clone())
            .unwrap_or_else(|| active.clone());
        Ok(Self {
            store,
            thread,
            fresh_thread: false,
            owner,
            active,
        })
    }

    pub fn thread(&self) -> Option<&ThreadMeta> {
        self.thread.as_ref()
    }

    pub fn active(&self) -> &str {
        &self.active
    }

    pub async fn create_thread(&mut self, first_message: &str) -> Result<&ThreadMeta> {
        let meta = create_thread(
            self.store.as_ref(),
            first_message,
            &self.owner,
            &self.active,
        )
        .await?;
        self.fresh_thread = true;
        Ok(self.thread.insert(meta))
    }

    pub async fn open(
        self,
        runtime: &Runtime,
        events: Arc<dyn EventSink>,
        hooks: PipelineHooks,
        run_name: Option<String>,
    ) -> Result<ChatSession> {
        runtime.usage.attach_events(events.clone());
        let hooks_cancel = hooks.cancel.clone();
        let conversation = runtime
            .conversation(&self.store, events.clone(), hooks)
            .await?;
        let cancel = hooks_cancel;
        self.over(
            conversation,
            runtime.usage.clone(),
            events,
            run_name,
            cancel,
        )
        .await
    }

    pub async fn over(
        self,
        conversation: Conversation,
        usage: Arc<UsageLedger>,
        events: Arc<dyn EventSink>,
        run_name: Option<String>,
        cancel: Option<CancelHandle>,
    ) -> Result<ChatSession> {
        conversation.agent(&self.active)?;
        let history = match &self.thread {
            Some(meta) => load_history(self.store.as_ref(), &meta.id).await?,
            None => Vec::new(),
        };
        Ok(ChatSession {
            conversation,
            store: self.store,
            usage,
            events,
            run_name,
            cancel,
            thread: self.thread,
            fresh_thread: self.fresh_thread,
            owner: self.owner,
            active: self.active,
            history,
        })
    }
}

pub struct ChatSession {
    conversation: Conversation,
    store: Arc<dyn Store>,
    usage: Arc<UsageLedger>,
    events: Arc<dyn EventSink>,
    run_name: Option<String>,
    cancel: Option<CancelHandle>,
    thread: Option<ThreadMeta>,
    fresh_thread: bool,
    owner: String,
    active: String,
    history: Vec<NewEntry>,
}

pub struct TurnReport {
    pub turn: ConversationTurn,
    pub usage: UsageReport,
    pub thread_error: Option<anyhow::Error>,
    pub persist_error: Option<anyhow::Error>,
}

impl ChatSession {
    pub fn thread(&self) -> Option<&ThreadMeta> {
        self.thread.as_ref()
    }

    pub fn active(&self) -> &str {
        &self.active
    }

    pub fn history(&self) -> &[NewEntry] {
        &self.history
    }

    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    pub async fn set_active(&mut self, name: &str) -> Result<()> {
        let doc = self.conversation.agent(name)?;
        if let Some(meta) = &self.thread {
            self.store.set_active_agent(&meta.id, &doc.name).await?;
        }
        self.active = doc.name;
        Ok(())
    }

    pub fn cancel_handle(&self) -> Option<&CancelHandle> {
        self.cancel.as_ref()
    }

    pub async fn send(&mut self, message: &str) -> Result<TurnReport> {
        let mut thread_error = None;
        if self.thread.is_none() {
            match create_thread(self.store.as_ref(), message, &self.owner, &self.active).await {
                Ok(meta) => {
                    self.thread = Some(meta);
                    self.fresh_thread = true;
                }
                Err(error) => thread_error = Some(error),
            }
        }
        if let Some(cancel) = &self.cancel {
            cancel.rearm();
        }
        if let Some(name) = &self.run_name {
            self.events.run_started(&RunStart {
                name: name.clone(),
                session_id: self.thread.as_ref().map(|meta| meta.id.clone()),
                input: Some(serde_json::Value::String(message.to_string())),
            });
        }
        let turn = self
            .conversation
            .run_turn(&self.history, &self.active, message);
        let result: Result<ConversationTurn> = match &self.cancel {
            Some(cancel) => match cancel.run(turn).await {
                Ok(result) => result.map_err(Into::into),
                Err(cancelled) => Err(cancelled.into()),
            },
            None => turn.await.map_err(Into::into),
        };
        let usage = self.usage.take();
        let outcome = match result {
            Ok(turn) => {
                self.events
                    .run_finished(&serde_json::Value::String(turn.text.clone()), false);
                let persist_error = match &self.thread {
                    Some(meta) => persist_turn(self.store.as_ref(), meta, &self.active, &turn)
                        .await
                        .err(),
                    None => None,
                };
                self.active = turn.active.clone();
                self.history.extend(turn.entries.iter().cloned());
                self.fresh_thread = false;
                Ok(TurnReport {
                    turn,
                    usage: usage.clone(),
                    thread_error,
                    persist_error,
                })
            }
            Err(error) => {
                self.events
                    .run_finished(&serde_json::json!({"error": error.to_string()}), true);
                if self.fresh_thread {
                    if let Some(meta) = self.thread.take() {
                        let _ = self.store.delete_thread(&meta.id).await;
                    }
                    self.fresh_thread = false;
                }
                Err(error)
            }
        };
        if !usage.is_empty() {
            self.events.usage_summary(&usage);
        }
        outcome
    }
}

async fn create_thread(
    store: &dyn Store,
    first_message: &str,
    owner: &str,
    active: &str,
) -> Result<ThreadMeta> {
    let mut meta = store
        .create_thread(&title_from(first_message), owner)
        .await?;
    if meta.active != active && store.set_active_agent(&meta.id, active).await.is_ok() {
        meta.active = active.to_string();
    }
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::InstalledPacks;
    use crate::runtime::RuntimeOptions;
    use async_trait::async_trait;
    use futures::StreamExt;
    use graph_config::{ModelChoice, ModelRoles};
    use graph_core::agent::doc::{builtin_agents, AgentSet};
    use graph_core::NullSink;
    use graph_llm::types::{
        ChatRequest, ChatResponse, EventStream, StopReason, StreamEvent, ToolCall, Usage,
    };
    use graph_llm::{ChatProvider, LlmError, ModelRouter};
    use serde_json::{json, Value};
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    const AGENTS: &[&str] = &[
        "name: front\ndescription: routes\nmodel: chat\ntools: []\nhandoffs: [back]\nsystem_prompt: You are front.\n",
        "name: back\ndescription: works\nmodel: chat\ntools: []\nsystem_prompt: You are back.\n",
    ];

    struct Scripted(Mutex<VecDeque<ChatResponse>>);

    impl Scripted {
        fn next(&self) -> Result<ChatResponse, LlmError> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| LlmError::Unsupported("script exhausted".to_string()))
        }
    }

    #[async_trait]
    impl ChatProvider for Scripted {
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.next()
        }

        async fn chat_stream(&self, _: ChatRequest) -> Result<EventStream, LlmError> {
            let response = self.next()?;
            Ok(futures::stream::iter(vec![Ok(StreamEvent::Completed(response))]).boxed())
        }
    }

    fn text(content: &str) -> ChatResponse {
        ChatResponse {
            content: Some(content.to_string()),
            tool_calls: Vec::new(),
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn call(name: &str, arguments: Value) -> ChatResponse {
        ChatResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: "c1".to_string(),
                name: name.to_string(),
                arguments,
            }],
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        }
    }

    fn runtime(responses: Vec<ChatResponse>) -> Runtime {
        let mut config = graph_config::Config::default();
        config.tools.paths = Vec::new();
        config.plans.paths = Vec::new();
        let provider: Arc<dyn ChatProvider> = Arc::new(Scripted(Mutex::new(responses.into())));
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
        let options = RuntimeOptions {
            storage_backend: Some("memory".to_string()),
            capture_content: false,
        };
        Runtime::with_router(config, InstalledPacks::default(), options, router).unwrap()
    }

    async fn session(
        runtime: &Runtime,
        store: &Arc<dyn Store>,
        thread: Option<Option<String>>,
    ) -> ChatSession {
        let events: Arc<dyn EventSink> = Arc::new(NullSink);
        let mut conversation = runtime
            .conversation(store, events.clone(), PipelineHooks::default())
            .await
            .unwrap();
        conversation.agents = Arc::new(AgentSet::layered(builtin_agents(AGENTS), Vec::new()));
        ChatStart::resolve(runtime, store.clone(), Some("front"), thread)
            .await
            .unwrap()
            .over(conversation, runtime.usage.clone(), events, None, None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_first_message_creates_the_thread_and_each_turn_persists() {
        let runtime = runtime(vec![text("hello"), text("again")]);
        let store = runtime.store().unwrap();
        let mut chat = session(&runtime, &store, None).await;
        assert!(chat.thread().is_none());

        let report = chat.send("Plan the release notes").await.unwrap();
        assert_eq!(report.turn.text, "hello");
        assert!(report.thread_error.is_none() && report.persist_error.is_none());
        let meta = chat.thread().cloned().unwrap();
        assert_eq!(meta.title, "Plan the release notes");
        assert_eq!(meta.owner, "front");

        chat.send("and again").await.unwrap();
        let threads = store.list_threads().await.unwrap();
        assert_eq!(threads.len(), 1);
        let persisted = load_history(store.as_ref(), &meta.id).await.unwrap();
        assert_eq!(persisted.len(), chat.history().len());
        assert_eq!(persisted.len(), 4, "two user messages and two replies");
    }

    #[tokio::test]
    async fn a_failed_first_turn_deletes_the_thread_it_created() {
        let runtime = runtime(Vec::new());
        let store = runtime.store().unwrap();
        let mut chat = session(&runtime, &store, None).await;

        assert!(chat.send("hi").await.is_err());
        assert!(chat.thread().is_none());
        assert!(store.list_threads().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_turn_on_a_resumed_thread_keeps_it() {
        let runtime = runtime(vec![text("hello")]);
        let store = runtime.store().unwrap();
        let mut first = session(&runtime, &store, None).await;
        first.send("hi").await.unwrap();
        let id = first.thread().unwrap().id.clone();

        let mut resumed = session(&runtime, &store, Some(Some(id.clone()))).await;
        assert_eq!(resumed.history().len(), 2);
        assert!(resumed.send("again").await.is_err());
        assert!(store.get_thread(&id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_handoff_changes_the_active_agent_and_the_thread_records_it() {
        let runtime = runtime(vec![
            call("transfer_to_back", json!({"message": "yours"})),
            text("back here"),
        ]);
        let store = runtime.store().unwrap();
        let mut chat = session(&runtime, &store, None).await;

        let report = chat.send("hand this over").await.unwrap();
        assert_eq!(report.turn.active, "back");
        assert_eq!(chat.active(), "back");
        let meta = store
            .get_thread(&chat.thread().unwrap().id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.active, "back");
        assert_eq!(meta.owner, "front");
    }

    struct Slow {
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl graph_core::ToolRegistry for Slow {
        async fn tools(&self) -> Result<Vec<graph_core::ToolDef>, graph_core::ToolError> {
            Ok(vec![graph_core::ToolDef {
                name: "slow__wait".to_string(),
                description: "waits".to_string(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
            }])
        }

        async fn invoke(
            &self,
            _: &str,
            _: Value,
        ) -> Result<graph_core::ToolOutcome, graph_core::ToolError> {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(graph_core::ToolOutcome {
                result: json!({}),
                is_error: false,
            })
        }
    }

    struct Delayed(Mutex<VecDeque<ChatResponse>>);

    #[async_trait]
    impl ChatProvider for Delayed {
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, LlmError> {
            Err(LlmError::Unsupported("stream only".to_string()))
        }

        async fn chat_stream(&self, _: ChatRequest) -> Result<EventStream, LlmError> {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let response = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| LlmError::Unsupported("script exhausted".to_string()))?;
            Ok(futures::stream::iter(vec![Ok(StreamEvent::Completed(response))]).boxed())
        }
    }

    #[tokio::test]
    async fn a_cancelled_turn_finishes_its_tool_call_then_stops_and_persists_nothing() {
        let mut config = graph_config::Config::default();
        config.tools.paths = Vec::new();
        config.plans.paths = Vec::new();
        let provider: Arc<dyn ChatProvider> = Arc::new(Delayed(Mutex::new(
            vec![call("slow__wait", json!({})), text("after")].into(),
        )));
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
        let options = RuntimeOptions {
            storage_backend: Some("memory".to_string()),
            capture_content: false,
        };
        let runtime =
            Runtime::with_router(config, InstalledPacks::default(), options, router).unwrap();
        let store = runtime.store().unwrap();
        let cancel = CancelHandle::new();
        let events: Arc<dyn EventSink> = Arc::new(NullSink);
        let hooks = PipelineHooks {
            cancel: Some(cancel.clone()),
            ..Default::default()
        };
        let mut conversation = runtime
            .conversation(&store, events.clone(), hooks)
            .await
            .unwrap();
        conversation.agents = Arc::new(AgentSet::layered(
            builtin_agents(&["name: waiter\ndescription: waits\nmodel: chat\ntools: [slow__wait]\nsystem_prompt: Wait.\n"]),
            Vec::new(),
        ));
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        conversation.catalog = cancel.track(Arc::new(Slow {
            finished: finished.clone(),
        }));
        let mut chat = ChatStart::resolve(&runtime, store.clone(), Some("waiter"), None)
            .await
            .unwrap()
            .over(
                conversation,
                runtime.usage.clone(),
                events,
                None,
                Some(cancel.clone()),
            )
            .await
            .unwrap();

        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            canceller.cancel();
        });
        let started = std::time::Instant::now();
        let Err(error) = chat.send("wait for it").await else {
            panic!("the turn should be cancelled");
        };
        let elapsed = started.elapsed();
        assert!(
            error.downcast_ref::<crate::cancel::Cancelled>().is_some(),
            "{error}"
        );
        assert!(
            finished.load(std::sync::atomic::Ordering::SeqCst),
            "the tool call finished"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(1300),
            "{elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1600),
            "{elapsed:?}"
        );
        assert!(chat.thread().is_none());
        assert!(chat.history().is_empty());
        assert!(store.list_threads().await.unwrap().is_empty());
        assert_eq!(cancel.tools_in_flight(), 0);
    }

    #[tokio::test]
    async fn switching_agents_validates_and_persists_the_choice() {
        let runtime = runtime(vec![text("hello")]);
        let store = runtime.store().unwrap();
        let mut chat = session(&runtime, &store, None).await;
        chat.send("hi").await.unwrap();

        assert!(chat.set_active("nobody").await.is_err());
        chat.set_active("back").await.unwrap();
        let meta = store
            .get_thread(&chat.thread().unwrap().id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.active, "back");
    }
}
