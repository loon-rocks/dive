use crate::attachment::{AttachmentHost, EVIDENCE_ATTACHMENT, NETWORK_ATTACHMENT};
use crate::cancel::CancelHandle;
use crate::evidence::{Evidence, EvidenceAttachment, Item};
use crate::runtime::{
    load_history, persist_turn, resolve_thread, starting_agent, title_from, PipelineHooks, Runtime,
};
use crate::workbench::agents::Start;
use crate::workbench::attachment::{NetworkAttachment, NetworkSetup};
use crate::workbench::ArtifactKind;
use anyhow::{Context, Result};
use dive_core::agent::conversation::{Conversation, ConversationTurn};
use dive_core::pipeline::doc::PlanDoc;
use dive_core::usage::{UsageLedger, UsageReport};
use dive_core::{Attachment, EventSink, NewEntry, RunStart, Store, TeeSink, ThreadMeta};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct ThreadOpen {
    pub thread: Option<String>,
    pub agent: Option<String>,
    pub draft: Option<Start>,
    pub root: Option<PathBuf>,
}

impl Runtime {
    pub async fn open_thread(
        &self,
        open: ThreadOpen,
        events: Arc<dyn EventSink>,
        hooks: PipelineHooks,
        run_name: Option<String>,
    ) -> Result<ChatSession> {
        let store = self.store()?;
        let start = match open.draft {
            Some(draft) => ChatStart::draft(self, store, draft).await?,
            None => {
                ChatStart::resolve(self, store, open.agent.as_deref(), open.thread.map(Some))
                    .await?
            }
        };
        let start = match open.root {
            Some(root) => start.with_root(root),
            None => start,
        };
        start.open(self, events, hooks, run_name).await
    }
}

#[derive(Default)]
struct Seed {
    doc: Option<PlanDoc>,
    opening: Vec<(ArtifactKind, String)>,
    evidence: Option<Evidence>,
}

pub struct ChatStart {
    store: Arc<dyn Store>,
    thread: Option<ThreadMeta>,
    fresh_thread: bool,
    owner: String,
    active: String,
    attachments: Vec<String>,
    seed: Seed,
    root: Option<PathBuf>,
    project: Option<String>,
    title: Option<String>,
}

const UNRECORDED_DRAFTERS: [&str; 5] = [
    "network_drafter",
    "network_editor",
    "tool_drafter",
    "plan_drafter",
    "agent_drafter",
];

impl ChatStart {
    pub async fn resolve(
        runtime: &Runtime,
        store: Arc<dyn Store>,
        agent: Option<&str>,
        thread: Option<Option<String>>,
    ) -> Result<Self> {
        let thread = resolve_thread(store.as_ref(), thread).await?;
        let attachments = match &thread {
            Some(meta) if !meta.attachments.is_empty() => {
                let kinds: Vec<String> = meta
                    .attachments
                    .iter()
                    .map(|recorded| recorded.kind.clone())
                    .collect();
                crate::attachment::thread_kinds(&kinds)
            }
            Some(meta) if UNRECORDED_DRAFTERS.contains(&meta.owner.as_str()) => {
                vec![NETWORK_ATTACHMENT.to_string()]
            }
            _ => Vec::new(),
        };
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
            attachments,
            seed: Seed::default(),
            root: None,
            project: runtime.project().map(str::to_string),
            title: None,
        })
    }

    pub async fn draft(runtime: &Runtime, store: Arc<dyn Store>, start: Start) -> Result<Self> {
        if let Start::Resume(id) = &start {
            let mut resumed = Self::resolve(runtime, store, None, Some(Some(id.clone()))).await?;
            if !resumed
                .attachments
                .iter()
                .any(|kind| kind == NETWORK_ATTACHMENT)
            {
                resumed
                    .attachments
                    .insert(0, NETWORK_ATTACHMENT.to_string());
                let agents = runtime.agent_set();
                resumed.active = starting_agent(resumed.thread.as_ref(), None, &|name| {
                    agents.get(name).is_some()
                })?;
            }
            return Ok(resumed);
        }
        let doc = match &start {
            Start::Plan(Some(arg)) => Some(crate::workbench::resolve_doc(runtime, arg)?),
            _ => None,
        };
        let mut attachments = vec![NETWORK_ATTACHMENT.to_string()];
        let mut title = None;
        let (opening, evidence) = match &start {
            Start::Agent(Some(name)) => (vec![(ArtifactKind::Agent, name.clone())], None),
            Start::Tool(Some(name)) => (vec![(ArtifactKind::Tool, name.clone())], None),
            Start::Edit { items, evidence } => {
                let items = edited(items, evidence.as_deref())?;
                if evidence.is_some() {
                    attachments.push(EVIDENCE_ATTACHMENT.to_string());
                }
                title = Some(edit_title(&items, evidence.as_deref()));
                (
                    items
                        .into_iter()
                        .map(|item| (item.kind, item.name))
                        .collect(),
                    evidence.as_deref().cloned(),
                )
            }
            _ => (Vec::new(), None),
        };
        let active = crate::workbench::agents::starting_agent(&start, doc.as_ref()).to_string();
        Ok(Self {
            store,
            thread: None,
            fresh_thread: false,
            owner: active.clone(),
            active,
            attachments,
            seed: Seed {
                doc,
                opening,
                evidence,
            },
            root: None,
            project: runtime.project().map(str::to_string),
            title,
        })
    }

    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    pub fn thread(&self) -> Option<&ThreadMeta> {
        self.thread.as_ref()
    }

    pub fn active(&self) -> &str {
        &self.active
    }

    pub fn attachments(&self) -> &[String] {
        &self.attachments
    }

    pub async fn create_thread(&mut self, first_message: &str) -> Result<&ThreadMeta> {
        let meta = create_thread(
            self.store.as_ref(),
            first_message,
            &self.owner,
            &self.active,
            self.project.as_deref(),
        )
        .await?;
        self.fresh_thread = true;
        Ok(self.thread.insert(meta))
    }

    pub async fn open(
        mut self,
        runtime: &Runtime,
        events: Arc<dyn EventSink>,
        mut hooks: PipelineHooks,
        run_name: Option<String>,
    ) -> Result<ChatSession> {
        runtime.usage.attach_events(events.clone());
        let cancel = hooks.cancel.get_or_insert_with(CancelHandle::new).clone();
        let kinds = self.attachments.clone();
        let pipeline = runtime
            .pipeline_with(&self.store, events.clone(), hooks)
            .await?;
        let toolbox = runtime.toolbox_over(&self.store, &pipeline, Some(&cancel))?;
        let seed = std::mem::take(&mut self.seed);
        let recorded: Vec<Attachment> = self
            .thread
            .as_ref()
            .map(|meta| meta.attachments.clone())
            .unwrap_or_default();
        let root = match self.root.take() {
            Some(root) => root,
            None => std::env::current_dir().context("failed to read the current directory")?,
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let network = NetworkAttachment::new(NetworkSetup {
            pipeline: pipeline.clone(),
            tx,
            plans_dir: runtime.plans_dir(),
            root,
            doc: seed.doc,
        })?;
        let network = Arc::new(if kinds.iter().any(|kind| kind == NETWORK_ATTACHMENT) {
            network
        } else {
            network.unloaded()
        });
        network.forward(rx, events.clone());
        let evidence = seed
            .evidence
            .filter(|_| kinds.iter().any(|kind| kind == EVIDENCE_ATTACHMENT))
            .or_else(|| {
                recorded
                    .iter()
                    .find(|recorded| recorded.kind == EVIDENCE_ATTACHMENT)
                    .and_then(Evidence::recall)
            });
        let evidence = Arc::new(match evidence {
            Some(evidence) => EvidenceAttachment::new(evidence),
            None => EvidenceAttachment::empty(),
        });
        let hosts: Vec<Arc<dyn AttachmentHost>> = vec![network.clone(), evidence.clone()];
        let mut sinks = vec![events.clone()];
        sinks.extend(hosts.iter().filter_map(|host| host.observer()));
        let agents = Arc::new(runtime.agent_set());
        let sink: Arc<dyn EventSink> = Arc::new(TeeSink::new(sinks));
        let attach: Arc<dyn dive_core::ToolRegistry> = Arc::new(crate::attach::AttachTools {
            network: network.clone(),
            evidence: evidence.clone(),
            store: self.store.clone(),
            agents: agents.clone(),
            pipeline: pipeline.clone(),
            events: sink.clone(),
        });
        let catalog = hosts.iter().fold(
            Arc::new(dive_core::CompositeRegistry::new(vec![
                toolbox as Arc<dyn dive_core::ToolRegistry>,
                attach,
            ])) as Arc<dyn dive_core::ToolRegistry>,
            |catalog, host| host.tools(catalog),
        );
        let mut conversation = runtime.conversation_over(agents, catalog, pipeline, sink);
        conversation.context = Some(crate::attachment::context_hook(hosts.clone()));
        for host in &hosts {
            host.wire(&mut conversation);
        }
        let mut session = self
            .over(
                conversation,
                runtime.usage.clone(),
                events,
                run_name,
                Some(cancel),
            )
            .await?;
        if let Some(recorded) = session
            .recorded
            .iter()
            .find(|recorded| recorded.kind == NETWORK_ATTACHMENT)
        {
            network.recall(recorded);
        }
        for host in &hosts {
            host.restore(&session.history);
        }
        for (kind, name) in seed.opening {
            network
                .open_item(kind, &name)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        session.attachments = hosts;
        Ok(session)
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
        let recorded = self
            .thread
            .as_ref()
            .map(|meta| meta.attachments.clone())
            .unwrap_or_default();
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
            attachments: Vec::new(),
            recorded,
            project: self.project,
            title: self.title,
        })
    }
}

fn edited(items: &[Item], evidence: Option<&Evidence>) -> Result<Vec<Item>> {
    let mut edited: Vec<Item> = Vec::new();
    let found = evidence.into_iter().flat_map(|evidence| {
        std::iter::once(&evidence.item).chain(
            evidence
                .focus
                .as_ref()
                .and_then(|focus| focus.item.as_ref()),
        )
    });
    for item in items.iter().chain(found) {
        if !edited.contains(item) {
            edited.push(item.clone());
        }
    }
    if edited.is_empty() {
        anyhow::bail!("an edit session needs an item to edit: name one in items or evidence");
    }
    Ok(edited)
}

fn edit_title(items: &[Item], evidence: Option<&Evidence>) -> String {
    let failed = evidence.is_some_and(|evidence| {
        evidence.error.is_some()
            || evidence
                .focus
                .as_ref()
                .is_some_and(|focus| focus.error.is_some())
    });
    let verb = match evidence {
        Some(_) if failed => "Fix",
        Some(_) => "Improve",
        None => "Edit",
    };
    if let Some(evidence) = evidence {
        let within = format!("{} {}", evidence.item.kind.label(), evidence.item.name);
        if let Some(focus) = &evidence.focus {
            let what = match (&focus.model, &focus.step, &focus.item) {
                (Some(_), _, _) => "a model call".to_string(),
                (_, Some(path), _) => path.rsplit('/').next().unwrap_or(path).to_string(),
                (_, _, Some(item)) => item.name.clone(),
                _ => focus.name.clone(),
            };
            return format!("{verb} {what} in {within}");
        }
        if evidence.turn {
            return format!("{verb} a turn of {within}");
        }
    }
    let named: Vec<String> = items
        .iter()
        .map(|item| format!("{} {}", item.kind.label(), item.name))
        .collect();
    format!("{verb} {}", named.join(", "))
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
    attachments: Vec<Arc<dyn AttachmentHost>>,
    recorded: Vec<Attachment>,
    project: Option<String>,
    title: Option<String>,
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

    pub fn attachments(&self) -> Vec<Arc<dyn AttachmentHost>> {
        crate::attachment::loaded(&self.attachments)
    }

    pub fn hosts(&self) -> &[Arc<dyn AttachmentHost>] {
        &self.attachments
    }

    pub fn attachment(&self, kind: &str) -> Option<&Arc<dyn AttachmentHost>> {
        self.attachments
            .iter()
            .find(|host| host.kind() == kind && host.loaded())
    }

    pub fn network(&self) -> Option<&NetworkAttachment> {
        self.attachment(NETWORK_ATTACHMENT)
            .and_then(|host| host.as_any().downcast_ref::<NetworkAttachment>())
    }

    pub fn recorded(&self) -> &[Attachment] {
        &self.recorded
    }

    pub async fn act(&mut self, kind: &str, action: &str, input: Value) -> Result<Value> {
        let host = self
            .attachment(kind)
            .cloned()
            .with_context(|| format!("this thread has no {kind} attachment"))?;
        let result = host.act(action, input).await.map_err(anyhow::Error::msg);
        self.events.attachment_changed(host.kind(), &host.view());
        self.record_attachments().await?;
        result
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

    pub fn cancel(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }

    async fn record_attachments(&mut self) -> Result<()> {
        let Some(meta) = &self.thread else {
            return Ok(());
        };
        let records = crate::attachment::records(&self.attachments, &self.recorded);
        if records == self.recorded {
            return Ok(());
        }
        crate::attachment::record(self.store.as_ref(), &meta.id, &records).await?;
        self.recorded = records;
        Ok(())
    }

    pub async fn send(&mut self, message: &str) -> Result<TurnReport> {
        self.send_with(message, Vec::new()).await
    }

    pub async fn send_with(&mut self, message: &str, images: Vec<String>) -> Result<TurnReport> {
        self.turn(Some(dive_llm::types::ChatMessage::User {
            content: message.to_string(),
            images,
        }))
        .await
    }

    pub async fn begin(&mut self) -> Result<TurnReport> {
        self.turn(None).await
    }

    async fn turn(&mut self, user: Option<dive_llm::types::ChatMessage>) -> Result<TurnReport> {
        let message = match &user {
            Some(dive_llm::types::ChatMessage::User { content, .. }) => Some(content.as_str()),
            _ => None,
        };
        let mut thread_error = None;
        if self.thread.is_none() {
            let title = message
                .filter(|message| !message.trim().is_empty())
                .map(str::to_string)
                .or_else(|| self.title.clone())
                .unwrap_or_else(|| self.active.clone());
            match create_thread(
                self.store.as_ref(),
                &title,
                &self.owner,
                &self.active,
                self.project.as_deref(),
            )
            .await
            {
                Ok(meta) => {
                    self.thread = Some(meta);
                    self.fresh_thread = true;
                    if let Err(error) = self.record_attachments().await {
                        thread_error = Some(error);
                    }
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
                input: message.map(|message| serde_json::Value::String(message.to_string())),
            });
        }
        let turn = self
            .conversation
            .run_turn_with(&self.history, &self.active, user.clone());
        let result: Result<ConversationTurn> = match &self.cancel {
            Some(cancel) => match cancel.run(turn).await {
                Ok(result) => result.map_err(Into::into),
                Err(cancelled) => Err(cancelled.into()),
            },
            None => turn.await.map_err(Into::into),
        };
        let usage = self.usage.take();
        let outcome = match result {
            Ok(mut turn) => {
                if turn.text.trim().is_empty() {
                    if let Some(reply) = crate::workbench::replay::last_reply(&turn.entries) {
                        turn.text = reply;
                    }
                }
                self.events
                    .run_finished(&serde_json::Value::String(turn.text.clone()), false);
                let mut persist_error = match &self.thread {
                    Some(meta) => persist_turn(self.store.as_ref(), meta, &self.active, &turn)
                        .await
                        .err(),
                    None => None,
                };
                if let Err(error) = self.record_attachments().await {
                    persist_error.get_or_insert(error);
                }
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
                    self.recorded.clear();
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
    project: Option<&str>,
) -> Result<ThreadMeta> {
    let mut meta = store
        .create_thread(&title_from(first_message), owner, project)
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
    use dive_config::{ModelChoice, ModelRoles};
    use dive_core::agent::doc::{builtin_agents, AgentSet};
    use dive_core::NullSink;
    use dive_llm::types::{
        ChatRequest, ChatResponse, EventStream, StopReason, StreamEvent, ToolCall, Usage,
    };
    use dive_llm::{ChatProvider, LlmError, ModelRouter};
    use futures::StreamExt;
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
        let mut config = dive_config::Config::default();
        config.tools.paths = Vec::new();
        config.plans.paths = Vec::new();
        let provider: Arc<dyn ChatProvider> = Arc::new(Scripted(Mutex::new(responses.into())));
        let router = ModelRouter::with_providers(
            HashMap::from([("scripted".to_string(), provider)]),
            ModelRoles::from([(
                "default",
                ModelChoice {
                    kind: None,
                    provider: "scripted".to_string(),
                    model: "scripted".to_string(),
                    temperature: None,
                    description: None,
                    fallbacks: Vec::new(),
                    context_window: None,
                    images: None,
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
    async fn a_project_runtime_records_its_root_on_the_threads_it_creates() {
        let runtime = runtime(vec![text("hello")]).in_project(Some("/work/site".to_string()));
        let store = runtime.store().unwrap();
        let mut chat = session(&runtime, &store, None).await;
        chat.send("hi").await.unwrap();
        let meta = chat.thread().cloned().unwrap();
        assert_eq!(meta.project.as_deref(), Some("/work/site"));
        let stored = store.get_thread(&meta.id).await.unwrap().unwrap();
        assert_eq!(stored.project.as_deref(), Some("/work/site"));
    }

    #[tokio::test]
    async fn a_global_runtime_creates_global_threads() {
        let runtime = runtime(vec![text("hello")]);
        assert_eq!(runtime.project(), None);
        let store = runtime.store().unwrap();
        let mut chat = session(&runtime, &store, None).await;
        chat.send("hi").await.unwrap();
        let id = chat.thread().unwrap().id.clone();
        assert_eq!(store.get_thread(&id).await.unwrap().unwrap().project, None);
    }

    #[tokio::test]
    async fn a_thread_created_before_the_first_turn_records_the_project_too() {
        let runtime = runtime(Vec::new()).in_project(Some("/work/site".to_string()));
        let store = runtime.store().unwrap();
        let mut start = ChatStart::resolve(&runtime, store.clone(), None, None)
            .await
            .unwrap();
        let id = start.create_thread("hi").await.unwrap().id.clone();
        let stored = store.get_thread(&id).await.unwrap().unwrap();
        assert_eq!(stored.project.as_deref(), Some("/work/site"));
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
    impl dive_core::ToolRegistry for Slow {
        async fn tools(&self) -> Result<Vec<dive_core::ToolDef>, dive_core::ToolError> {
            Ok(vec![dive_core::ToolDef {
                name: "slow__wait".to_string(),
                description: "waits".to_string(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
                file_input: Default::default(),
            }])
        }

        async fn invoke(
            &self,
            _: &str,
            _: Value,
        ) -> Result<dive_core::ToolOutcome, dive_core::ToolError> {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(dive_core::ToolOutcome {
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
        let mut config = dive_config::Config::default();
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
                    kind: None,
                    provider: "scripted".to_string(),
                    model: "scripted".to_string(),
                    temperature: None,
                    description: None,
                    fallbacks: Vec::new(),
                    context_window: None,
                    images: None,
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

    fn file_runtime(dir: &std::path::Path, responses: Vec<ChatResponse>) -> Runtime {
        let mut config = dive_config::Config::default();
        config.tools.paths = Vec::new();
        config.plans.paths = Vec::new();
        config.settings.data_dir = dir.join("data");
        let provider: Arc<dyn ChatProvider> = Arc::new(Scripted(Mutex::new(responses.into())));
        let router = ModelRouter::with_providers(
            HashMap::from([("scripted".to_string(), provider)]),
            ModelRoles::from([(
                "default",
                ModelChoice {
                    kind: None,
                    provider: "scripted".to_string(),
                    model: "scripted".to_string(),
                    temperature: None,
                    description: None,
                    fallbacks: Vec::new(),
                    context_window: None,
                    images: None,
                },
            )]),
        );
        let options = RuntimeOptions {
            storage_backend: Some("file".to_string()),
            capture_content: false,
        };
        Runtime::with_router(config, InstalledPacks::default(), options, router).unwrap()
    }

    #[derive(Default)]
    struct Changes(Mutex<Vec<(String, Value)>>);

    impl EventSink for Changes {
        fn attachment_changed(&self, kind: &str, view: &Value) {
            self.0
                .lock()
                .unwrap()
                .push((kind.to_string(), view.clone()));
        }
    }

    async fn open(runtime: &Runtime, root: &std::path::Path, open: ThreadOpen) -> ChatSession {
        runtime
            .open_thread(
                ThreadOpen {
                    root: Some(root.to_path_buf()),
                    ..open
                },
                Arc::new(NullSink),
                PipelineHooks::default(),
                None,
            )
            .await
            .unwrap()
    }

    fn sketch_call(id: &str) -> Vec<NewEntry> {
        vec![
            NewEntry::message(
                "user",
                dive_llm::types::ChatMessage::user("a tool that counts words".to_string()),
            ),
            NewEntry::message(
                "network_drafter",
                dive_llm::types::ChatMessage::Assistant {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: id.to_string(),
                        name: crate::workbench::artifact::SET_SKETCH.to_string(),
                        arguments: json!({
                            "restate": {"outcome": "Words counted", "success": "A number", "constraint": "No model", "out_of_scope": []},
                            "nodes": [{"name": "word_count", "kind": "tool", "job": "Counts words"}]
                        }),
                    }],
                    thinking: Vec::new(),
                },
            ),
            NewEntry::message(
                "network_drafter",
                dive_llm::types::ChatMessage::ToolResult {
                    tool_call_id: id.to_string(),
                    content: json!({"nodes": 1}),
                    is_error: false,
                },
            ),
        ]
    }

    #[tokio::test]
    async fn a_plain_thread_opens_as_a_plain_chat() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(dir.path(), vec![text("hello")]);
        let mut chat = open(&runtime, dir.path(), ThreadOpen::default()).await;
        assert!(chat.attachments().is_empty());
        chat.send("hi").await.unwrap();
        let id = chat.thread().unwrap().id.clone();

        let reopened = open(
            &runtime,
            dir.path(),
            ThreadOpen {
                thread: Some(id.clone()),
                ..Default::default()
            },
        )
        .await;
        assert!(reopened.attachments().is_empty());
        assert!(reopened.network().is_none());
        assert!(reopened.recorded().is_empty());
        assert_eq!(reopened.active(), "chat");
        assert_eq!(reopened.history().len(), 2);
        assert!(reopened
            .conversation()
            .agents
            .get("network_drafter")
            .is_some());
        let meta = runtime
            .store()
            .unwrap()
            .get_thread(&id)
            .await
            .unwrap()
            .unwrap();
        assert!(meta.attachments.is_empty());
    }

    #[tokio::test]
    async fn a_network_thread_reopens_with_its_network_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(dir.path(), Vec::new());
        let store = runtime.store().unwrap();
        let meta = store
            .create_thread("a tool that counts words", "network_drafter", None)
            .await
            .unwrap();
        store
            .append_entries(&meta.id, &sketch_call("1"))
            .await
            .unwrap();
        store
            .set_attachments(&meta.id, &[Attachment::new(NETWORK_ATTACHMENT)])
            .await
            .unwrap();

        let chat = open(
            &runtime,
            dir.path(),
            ThreadOpen {
                thread: Some(meta.id.clone()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(chat.active(), "network_drafter");
        let network = chat.network().expect("a network attachment");
        let view = network.network();
        assert_eq!(view.nodes.len(), 1);
        assert_eq!(view.nodes[0].name, "word_count");
        let host = chat.attachment(NETWORK_ATTACHMENT).unwrap();
        assert_eq!(host.view()["state"], json!("open"));
        assert_eq!(host.view()["network"]["nodes"][0]["kind"], json!("tool"));
        let sections = (chat.conversation().context.as_ref().unwrap())("network_drafter");
        assert!(sections[1].contains("word_count"), "{sections:?}");
    }

    #[tokio::test]
    async fn a_draft_thread_from_before_attachments_reopens_with_its_network() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(dir.path(), Vec::new());
        let store = runtime.store().unwrap();
        let meta = store
            .create_thread("old draft", "network_drafter", None)
            .await
            .unwrap();
        store
            .append_entries(&meta.id, &sketch_call("1"))
            .await
            .unwrap();

        let chat = open(
            &runtime,
            dir.path(),
            ThreadOpen {
                thread: Some(meta.id.clone()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(chat.network().unwrap().network().nodes.len(), 1);
        assert!(chat.recorded().is_empty());
    }

    #[tokio::test]
    async fn a_thread_gets_a_network_only_when_an_agent_creates_one() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(
            dir.path(),
            vec![
                call(crate::workbench::artifact::SET_SKETCH, json!({"nodes": []})),
                call(crate::attach::CREATE_NETWORK, json!({})),
                text("Who should it greet?"),
            ],
        );
        let changes = Arc::new(Changes::default());
        let mut chat = runtime
            .open_thread(
                ThreadOpen {
                    root: Some(dir.path().to_path_buf()),
                    agent: Some("network_drafter".to_string()),
                    ..Default::default()
                },
                changes.clone(),
                PipelineHooks::default(),
                None,
            )
            .await
            .unwrap();
        assert!(chat.network().is_none());
        assert!(chat.attachments().is_empty());
        assert!((chat.conversation().context.as_ref().unwrap())("network_drafter").is_empty());
        let report = chat.send("a tool that greets").await.unwrap();
        let refused = report
            .turn
            .entries
            .iter()
            .find_map(|entry| match &entry.body {
                dive_core::store::EntryBody::Message {
                    message:
                        dive_llm::types::ChatMessage::ToolResult {
                            content,
                            is_error: true,
                            ..
                        },
                } => Some(content.to_string()),
                _ => None,
            })
            .expect("the sketch before a draft fails");
        assert!(refused.contains("builtin__create_network"), "{refused}");
        assert!(chat.network().is_some());
        assert!(changes
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, _)| kind == NETWORK_ATTACHMENT));
        let meta = runtime
            .store()
            .unwrap()
            .get_thread(&chat.thread().unwrap().id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            meta.attachments,
            [Attachment::new(NETWORK_ATTACHMENT).with("state", "open")]
        );
        let sections = (chat.conversation().context.as_ref().unwrap())("chat");
        assert!(sections
            .iter()
            .any(|section| section.starts_with("## The network draft")));
    }

    #[test]
    fn every_workbench_and_attach_tool_a_built_in_agent_names_is_pre_loaded() {
        for source in crate::workbench::agents::builtin_sources() {
            let doc = dive_core::agent::doc::parse_agent_source(source).unwrap();
            for entry in &doc.tools {
                if entry.name.starts_with("workbench__")
                    || crate::attach::ATTACH_TOOLS.contains(&entry.name.as_str())
                {
                    assert!(entry.pre_load, "{}: {}", doc.name, entry.name);
                }
            }
        }
    }

    #[tokio::test]
    async fn an_agent_brings_in_evidence_from_another_thread() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(dir.path(), Vec::new());
        let store = runtime.store().unwrap();
        let source = store.create_thread("digest", "chat", None).await.unwrap();
        store
            .append_entries(
                &source.id,
                &[
                    NewEntry::message(
                        "chat",
                        dive_llm::types::ChatMessage::Assistant {
                            content: None,
                            tool_calls: vec![ToolCall {
                                id: "c1".to_string(),
                                name: "agent__front_desk".to_string(),
                                arguments: json!({"prompt": "hi"}),
                            }],
                            thinking: Vec::new(),
                        },
                    ),
                    NewEntry::message(
                        "chat",
                        dive_llm::types::ChatMessage::ToolResult {
                            tool_call_id: "c1".to_string(),
                            content: json!({"error": "boom"}),
                            is_error: true,
                        },
                    ),
                ],
            )
            .await
            .unwrap();
        let span = |id: &str, parent: &str, name: &str, kind: dive_core::trace::SpanKind| {
            dive_core::trace::Span {
                trace_id: "t".into(),
                span_id: id.into(),
                parent_span_id: Some(parent.into()),
                name: name.into(),
                kind,
                start_time_unix_nano: 1,
                end_time_unix_nano: Some(2),
                status: Default::default(),
                attributes: Default::default(),
                input: None,
                output: None,
            }
        };
        let mut called = span(
            "s1",
            "turn",
            "agent__front_desk",
            dive_core::trace::SpanKind::Tool,
        );
        called
            .attributes
            .insert("gen_ai.tool.call.id".into(), json!("c1"));
        let mut inner = span("s2", "s1", "plan__digest", dive_core::trace::SpanKind::Plan);
        inner.output = Some(json!({"error": "boom"}));
        store
            .append_spans(&source.id, &[called, inner])
            .await
            .unwrap();
        let chat = open(&runtime, dir.path(), ThreadOpen::default()).await;
        let registry = chat.conversation().catalog.clone();
        let outcome = registry
            .invoke(
                crate::attach::ATTACH_EVIDENCE,
                json!({"thread": source.id, "call": "c1"}),
            )
            .await
            .unwrap();
        assert!(!outcome.is_error, "{}", outcome.result);
        let evidence = chat
            .attachment(EVIDENCE_ATTACHMENT)
            .expect("evidence is loaded");
        assert_eq!(evidence.view()["item"]["name"], json!("front_desk"));
        assert_eq!(evidence.view()["error"], json!("boom"));
        assert!(
            evidence.view()["trace"][0]
                .as_str()
                .unwrap()
                .starts_with("plan__digest ok"),
            "{}",
            evidence.view()
        );
        let missing = registry
            .invoke(
                crate::attach::ATTACH_EVIDENCE,
                json!({"thread": source.id, "call": "nope"}),
            )
            .await
            .unwrap();
        assert!(missing.is_error);
    }

    #[tokio::test]
    async fn a_draft_starts_from_its_seed_and_discarding_marks_the_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(
            dir.path(),
            vec![
                call(
                    crate::workbench::artifact::SET_SKETCH,
                    json!({"restate": {"outcome": "x"}, "nodes": [{"name": "a", "kind": "tool", "job": "j"}]}),
                ),
                text("Sketched."),
            ],
        );
        let changes = Arc::new(Changes::default());
        let mut chat = runtime
            .open_thread(
                ThreadOpen {
                    draft: Some(Start::Tool(None)),
                    root: Some(dir.path().to_path_buf()),
                    ..Default::default()
                },
                changes.clone(),
                PipelineHooks::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(chat.active(), "network_drafter");
        assert!(chat.thread().is_none());
        chat.send("a tool").await.unwrap();
        assert_eq!(chat.network().unwrap().network().nodes.len(), 1);
        let discarded = chat
            .act(
                NETWORK_ATTACHMENT,
                crate::workbench::attachment::DISCARD,
                json!({}),
            )
            .await
            .unwrap();
        assert!(discarded.as_str().unwrap().starts_with("discarded"));
        let meta = runtime
            .store()
            .unwrap()
            .get_thread(&chat.thread().unwrap().id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            meta.attachments,
            [Attachment::new(NETWORK_ATTACHMENT).with("state", "discarded")]
        );
        let (kind, last) = changes.0.lock().unwrap().last().cloned().unwrap();
        assert_eq!(kind, NETWORK_ATTACHMENT);
        assert_eq!(last["state"], json!("discarded"));
        assert!(chat
            .act(NETWORK_ATTACHMENT, "publish", json!({}))
            .await
            .is_err());
    }

    fn edit_runtime(dir: &std::path::Path, responses: Vec<ChatResponse>) -> Runtime {
        edit_runtime_over(dir, Arc::new(Scripted(Mutex::new(responses.into()))))
    }

    fn edit_runtime_over(dir: &std::path::Path, provider: Arc<dyn ChatProvider>) -> Runtime {
        let mut config = dive_config::Config::default();
        config.tools.paths = vec![dir.join("tools")];
        config.plans.paths = Vec::new();
        config.settings.data_dir = dir.join("data");
        let router = ModelRouter::with_providers(
            HashMap::from([("scripted".to_string(), provider)]),
            ModelRoles::from([(
                "default",
                ModelChoice {
                    kind: None,
                    provider: "scripted".to_string(),
                    model: "scripted".to_string(),
                    temperature: None,
                    description: None,
                    fallbacks: Vec::new(),
                    context_window: None,
                    images: None,
                },
            )]),
        );
        let options = RuntimeOptions {
            storage_backend: Some("file".to_string()),
            capture_content: false,
        };
        Runtime::with_router(config, InstalledPacks::default(), options, router).unwrap()
    }

    const WORD_COUNT: &str =
        "name: word_count\ndescription: Counts words\nkind: exec\ncommand: wc\nargs: [-l]\n";

    fn word_count_evidence() -> Evidence {
        Evidence {
            thread: "a1b2c3d4e5f6".to_string(),
            call: Some("toolu_7".to_string()),
            item: Item {
                kind: ArtifactKind::Tool,
                name: "word_count".to_string(),
            },
            input: json!({"text": "one two three"}),
            output: Some(json!("1")),
            error: None,
            trace: Vec::new(),
            turn: false,
            focus: None,
        }
    }

    #[tokio::test]
    async fn an_edit_session_opens_its_items_with_the_evidence_and_proposes_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tools")).unwrap();
        std::fs::write(dir.path().join("tools/word_count.yaml"), WORD_COUNT).unwrap();
        let runtime = edit_runtime(
            dir.path(),
            vec![text(
                "It counts lines, not words: `-l` should be `-w`. Go, or tell me what to change?",
            )],
        );
        let mut chat = runtime
            .open_thread(
                ThreadOpen {
                    draft: Some(Start::Edit {
                        items: Vec::new(),
                        evidence: Some(Box::new(word_count_evidence())),
                    }),
                    root: Some(dir.path().to_path_buf()),
                    ..Default::default()
                },
                Arc::new(NullSink),
                PipelineHooks::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(chat.active(), "network_editor");
        let kinds: Vec<&str> = chat.attachments().iter().map(|host| host.kind()).collect();
        assert_eq!(kinds, ["network", "evidence"]);
        let node = chat.network().unwrap().network().nodes[0].clone();
        assert_eq!((node.name.as_str(), node.origin), ("word_count", "file"));
        assert!(!node.changed);

        let report = chat.begin().await.unwrap();
        assert!(report.turn.text.starts_with("It counts lines"));
        let meta = chat.thread().cloned().unwrap();
        assert_eq!(meta.title, "Improve tool word_count");
        let store = runtime.store().unwrap();
        let history = load_history(store.as_ref(), &meta.id).await.unwrap();
        assert!(history.iter().all(|entry| entry.author != "user"));
        let note = history
            .iter()
            .find_map(|entry| match &entry.body {
                dive_core::store::EntryBody::Message {
                    message: dive_llm::types::ChatMessage::User { content, .. },
                } if entry.author == "context:network_editor" => Some(content.clone()),
                _ => None,
            })
            .expect("network_editor's context note");
        assert!(note.contains("## The network draft"), "{note}");
        assert!(note.contains("### tool word_count draft"), "{note}");
        assert!(note.contains("## The evidence"), "{note}");
        assert!(note.contains("one two three"), "{note}");

        let stored = store.get_thread(&meta.id).await.unwrap().unwrap();
        assert_eq!(
            stored.attachments,
            [
                Attachment::new(NETWORK_ATTACHMENT)
                    .with("state", "open")
                    .with("opened", json!([{"kind": "tool", "name": "word_count"}])),
                word_count_evidence().record(),
            ]
        );

        let reopened = open(
            &runtime,
            dir.path(),
            ThreadOpen {
                thread: Some(meta.id.clone()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(reopened.active(), "network_editor");
        let nodes = reopened.network().unwrap().network().nodes;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "word_count");
        assert_eq!(nodes[0].base.as_deref(), Some(WORD_COUNT));
        let evidence = reopened.attachment(EVIDENCE_ATTACHMENT).unwrap().view();
        assert_eq!(
            evidence,
            serde_json::to_value(word_count_evidence()).unwrap()
        );
        assert_eq!(reopened.recorded(), stored.attachments.as_slice());
    }

    #[tokio::test]
    async fn the_editor_can_offer_a_rework_beside_the_patch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tools")).unwrap();
        std::fs::write(dir.path().join("tools/word_count.yaml"), WORD_COUNT).unwrap();
        let reply = "It counts lines because the tool shells out. Patch the flag, or rework it as a reshape?";
        let runtime = edit_runtime(
            dir.path(),
            vec![call(
                "workbench__propose_designs",
                json!({
                    "reply": reply,
                    "yours": {"title": "Patch the flag", "nodes": [{"kind": "tool", "name": "word_count"}], "lines": ["-l becomes -w", "0 model calls"]},
                    "suggested": {"title": "Rework the tool", "nodes": [{"kind": "tool", "name": "word_count"}], "lines": ["exec wc -w over stdin", "0 model calls"], "principles": ["No model at all"]}
                }),
            )],
        );
        let mut chat = open(
            &runtime,
            dir.path(),
            ThreadOpen {
                draft: Some(Start::Edit {
                    items: Vec::new(),
                    evidence: Some(Box::new(word_count_evidence())),
                }),
                ..Default::default()
            },
        )
        .await;
        let report = chat.begin().await.unwrap();
        assert_eq!(report.turn.text, reply);
        assert_eq!(chat.active(), "network_editor");
        let network = chat.network().unwrap().network();
        let designs = network.designs.clone().expect("both designs in the pane");
        assert_eq!(designs.yours.title, "Patch the flag");
        assert_eq!(designs.suggested.title, "Rework the tool");
        assert_eq!(network.nodes.len(), 1);
        assert!(network.is_edit());
        assert!(!network.nodes[0].changed);
    }

    struct Recording {
        script: Scripted,
        seen: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait]
    impl ChatProvider for Recording {
        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.seen.lock().unwrap().push(request);
            self.script.next()
        }

        async fn chat_stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
            self.seen.lock().unwrap().push(request);
            let response = self.script.next()?;
            Ok(futures::stream::iter(vec![Ok(StreamEvent::Completed(response))]).boxed())
        }
    }

    fn prefix(request: &ChatRequest) -> (Value, Value, Value) {
        (
            json!(request.system),
            serde_json::to_value(&request.tools).unwrap(),
            serde_json::to_value(&request.messages).unwrap(),
        )
    }

    #[tokio::test]
    async fn a_reopened_thread_resends_the_previous_turn_s_prompt_unchanged_so_the_cache_holds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tools")).unwrap();
        std::fs::write(dir.path().join("tools/word_count.yaml"), WORD_COUNT).unwrap();
        let provider = Arc::new(Recording {
            script: Scripted(Mutex::new(
                vec![
                    call("workbench__describe_tool", json!({"name": "word_count"})),
                    text("It counts lines. Go?"),
                    text("Done."),
                ]
                .into(),
            )),
            seen: Mutex::new(Vec::new()),
        });
        let runtime = edit_runtime_over(dir.path(), provider.clone());
        let mut chat = open(
            &runtime,
            dir.path(),
            ThreadOpen {
                draft: Some(Start::Edit {
                    items: Vec::new(),
                    evidence: Some(Box::new(word_count_evidence())),
                }),
                ..Default::default()
            },
        )
        .await;
        chat.begin().await.unwrap();
        let thread = chat.thread().unwrap().id.clone();
        drop(chat);

        let mut reopened = open(
            &runtime,
            dir.path(),
            ThreadOpen {
                thread: Some(thread),
                ..Default::default()
            },
        )
        .await;
        reopened.send("go").await.unwrap();

        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let (system, tools, messages) = prefix(&seen[1]);
        let (next_system, next_tools, next_messages) = prefix(&seen[2]);
        assert_eq!(next_system, system);
        assert_eq!(next_tools, tools);
        let earlier = messages.as_array().unwrap();
        let later = next_messages.as_array().unwrap();
        assert!(later.len() > earlier.len());
        assert_eq!(&later[..earlier.len()], earlier.as_slice());
    }

    #[test]
    fn a_focused_edit_opens_the_step_s_item_too_and_is_titled_by_the_step() {
        let plan = Item {
            kind: ArtifactKind::Plan,
            name: "research".to_string(),
        };
        let tool = Item {
            kind: ArtifactKind::Tool,
            name: "crawl4ai_read".to_string(),
        };
        let mut evidence = word_count_evidence();
        evidence.item = plan.clone();
        evidence.focus = Some(crate::evidence::Focus {
            span: "R".to_string(),
            name: "user__crawl4ai_read".to_string(),
            step: Some("depth/quick/read_pages".to_string()),
            model: None,
            item: Some(tool.clone()),
            input: json!({}),
            output: None,
            error: Some("timed out".to_string()),
        });
        assert_eq!(edited(&[], Some(&evidence)).unwrap(), vec![plan, tool]);
        assert_eq!(
            edit_title(&[], Some(&evidence)),
            "Fix read_pages in plan research"
        );
        evidence.focus = None;
        evidence.turn = true;
        evidence.item = Item {
            kind: ArtifactKind::Agent,
            name: "research_agent".to_string(),
        };
        assert_eq!(
            edit_title(&[], Some(&evidence)),
            "Improve a turn of agent research_agent"
        );
    }

    #[tokio::test]
    async fn an_edit_without_an_item_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = edit_runtime(dir.path(), Vec::new());
        let refused = ChatStart::draft(
            &runtime,
            runtime.store().unwrap(),
            Start::Edit {
                items: Vec::new(),
                evidence: None,
            },
        )
        .await;
        assert!(refused.is_err());
    }
}
