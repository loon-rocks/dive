//! Persistence abstraction: sessions, their runs and entries, and the
//! observed-shape cache. Implemented by dive-store; behind a trait so
//! backends (file, memory, future remote stores) can be swapped.

pub use crate::trace::Span;
use async_trait::async_trait;
use dive_llm::types::{ChatMessage, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const USER_AUTHOR: &str = "user";

pub const NETWORK_ATTACHMENT: &str = "network";

pub const PLAN_ATTACHMENT: &str = "plan";

pub const EVIDENCE_ATTACHMENT: &str = "evidence";

pub const ATTACHMENT_KINDS: &[&str] = &[NETWORK_ATTACHMENT, PLAN_ATTACHMENT, EVIDENCE_ATTACHMENT];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: String,
    #[serde(flatten)]
    pub fields: serde_json::Map<String, Value>,
}

impl Attachment {
    pub fn new(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            fields: serde_json::Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(key.to_string(), value.into());
        self
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub output_image_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

impl From<&EntryUsage> for dive_config::TokenCounts {
    fn from(usage: &EntryUsage) -> Self {
        Self {
            input: usage.input_tokens,
            output: usage.output_tokens,
            output_image: usage.output_image_tokens,
            cache_write: usage.cache_creation_input_tokens,
            cache_read: usage.cache_read_input_tokens,
        }
    }
}

impl EntryUsage {
    pub fn new(model: &str, usage: &Usage) -> Self {
        Self {
            model: model.to_string(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            output_image_tokens: usage.output_image_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stamp {
    pub ms: Option<u64>,
    pub usage: Option<EntryUsage>,
    pub span: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryBody {
    RunStarted(RunStarted),
    RunEnded(RunEnded),
    Message {
        message: ChatMessage,
    },
    Handoff {
        from: String,
        to: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
    },
    AgentStarted {
        agent: String,
        caller: String,
        input: Value,
    },
    AgentFinished {
        agent: String,
        output: Value,
        #[serde(rename = "final")]
        final_: bool,
    },
    PlanStarted {
        plan: String,
        #[serde(default)]
        input: Value,
    },
    PlanFinished {
        plan: String,
        #[serde(default)]
        output: Value,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    StepStarted {
        step: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        #[serde(default)]
        input: Value,
    },
    StepFinished {
        step: String,
        #[serde(default)]
        output: Value,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    ItemStarted {
        index: u64,
        #[serde(default)]
        item: Value,
    },
    ItemFinished {
        index: u64,
        #[serde(default)]
        output: Value,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    ToolCalled {
        tool: String,
        #[serde(default)]
        input: Value,
        #[serde(default)]
        output: Value,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    Ask {
        question: Value,
    },
    Answer {
        answer: Value,
    },
    Compaction {
        summary: String,
    },
    RunResumed {
        generation: u64,
    },
    CancelRequested {
        by: String,
    },
}

impl EntryBody {
    pub fn is_conversation(&self) -> bool {
        matches!(
            self,
            Self::Message { .. } | Self::Handoff { .. } | Self::Compaction { .. }
        )
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub const RUN_AUTHOR: &str = "run";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunStarted {
    #[serde(default)]
    pub input: Value,
    pub identity: crate::run::Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub synthetic: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEnded {
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_usage: Option<Value>,
}

impl RunEnded {
    pub fn new(status: RunStatus) -> Self {
        Self {
            status,
            outcome: None,
            error: None,
            run_usage: None,
        }
    }

    pub fn completed(outcome: Option<Value>) -> Self {
        Self {
            outcome,
            ..Self::new(RunStatus::Completed)
        }
    }

    pub fn failed(error: &str) -> Self {
        Self {
            error: Some(error.to_string()),
            ..Self::new(RunStatus::Failed)
        }
    }

    pub fn with_usage(self, run_usage: Option<Value>) -> Self {
        Self { run_usage, ..self }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    Chat,
    Plan,
}

impl SessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Plan => "plan",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "chat" => Some(Self::Chat),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    pub at: i64,
    pub author: String,
    #[serde(flatten)]
    pub body: EntryBody,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<EntryUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub kind: SessionKind,
    pub title: String,
    pub tenant: String,
    pub owner: String,
    pub deployment: String,
    pub environment: String,
    pub agent: String,
    pub active_agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_holder: Option<String>,
    #[serde(default)]
    pub lease_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<i64>,
    #[serde(default)]
    pub message_count: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SessionMeta {
    pub fn in_project(&self, project: Option<&str>) -> bool {
        self.project.as_deref() == project
    }

    pub fn fences(&self, lease: &Lease) -> bool {
        self.active_run.as_deref() == Some(lease.run.as_str())
            && self.lease_generation == lease.generation
            && self.lease_holder.as_deref() == Some(lease.holder.as_str())
    }

    pub fn held(&self, now: i64) -> bool {
        self.lease_holder.is_some() && self.lease_expires_at.is_some_and(|at| at > now)
    }

    pub fn lease_expired(&self, now: i64) -> bool {
        self.active_run.is_some()
            && self.lease_holder.is_some()
            && self.lease_expires_at.is_none_or(|at| at <= now)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunMeta {
    pub id: String,
    pub session: String,
    pub kind: SessionKind,
    pub status: RunStatus,
    pub first_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cancel_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_by: Option<String>,
    pub tenant: String,
    pub owner: String,
    pub deployment: String,
    pub environment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_expires_at: Option<i64>,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewSession {
    pub kind: SessionKind,
    pub title: String,
    pub identity: crate::run::Identity,
    pub agent: String,
    pub plan: Option<String>,
    pub project: Option<String>,
    pub origin_session: Option<String>,
    pub origin_run: Option<String>,
}

impl NewSession {
    pub fn chat(title: &str, agent: &str, identity: &crate::run::Identity) -> Self {
        Self {
            kind: SessionKind::Chat,
            title: title.to_string(),
            identity: identity.clone(),
            agent: agent.to_string(),
            plan: None,
            project: None,
            origin_session: None,
            origin_run: None,
        }
    }

    pub fn plan(title: &str, plan: &str, identity: &crate::run::Identity) -> Self {
        Self {
            kind: SessionKind::Plan,
            plan: Some(plan.to_string()),
            ..Self::chat(title, "", identity)
        }
    }

    pub fn in_project(self, project: Option<&str>) -> Self {
        Self {
            project: project.map(str::to_string),
            ..self
        }
    }

    pub fn meta(self, id: String, now: i64) -> SessionMeta {
        SessionMeta {
            id,
            kind: self.kind,
            title: self.title,
            tenant: self.identity.tenant,
            owner: self.identity.user,
            deployment: self.identity.deployment,
            environment: self.identity.environment,
            active_agent: self.agent.clone(),
            agent: self.agent,
            plan: self.plan,
            attachments: Vec::new(),
            project: self.project,
            origin_session: self.origin_session,
            origin_run: self.origin_run,
            active_run: None,
            lease_holder: None,
            lease_generation: 0,
            lease_expires_at: None,
            message_count: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartRun {
    pub run_id: String,
    pub session: String,
    pub kind: SessionKind,
    pub holder: String,
    pub token_expires_at: Option<i64>,
    pub started: RunStarted,
}

impl StartRun {
    pub fn run_meta(&self, first_seq: u64, now: i64) -> RunMeta {
        let identity = &self.started.identity;
        RunMeta {
            id: self.run_id.clone(),
            session: self.session.clone(),
            kind: self.kind,
            status: RunStatus::Running,
            first_seq,
            last_seq: None,
            trace_id: self.started.trace_id.clone(),
            cancel_requested: false,
            cancel_by: None,
            tenant: identity.tenant.clone(),
            owner: identity.user.clone(),
            deployment: identity.deployment.clone(),
            environment: identity.environment.clone(),
            token_expires_at: self.token_expires_at,
            started_at: now,
            ended_at: None,
        }
    }

    pub fn first_entry(&self, seq: u64, now: i64) -> Entry {
        NewEntry::new(RUN_AUTHOR, EntryBody::RunStarted(self.started.clone())).into_entry(
            seq,
            &self.run_id,
            now,
        )
    }

    pub fn lease(&self, generation: u64, first_seq: u64) -> Lease {
        Lease {
            session: self.session.clone(),
            run: self.run_id.clone(),
            generation,
            holder: self.holder.clone(),
            next_seq: first_seq + 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub session: String,
    pub run: String,
    pub generation: u64,
    pub holder: String,
    pub next_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renewal {
    pub cancel_requested: bool,
    pub cancel_by: Option<String>,
    pub expires_at: i64,
}

pub type EntryHook = std::sync::Arc<dyn Fn(NewEntry) -> Option<u64> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    Signalled,
    Ended,
    NotRunning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reaped {
    pub run: String,
    pub session: String,
    pub kind: SessionKind,
    pub status: RunStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFilter {
    pub tenant: Option<String>,
    pub owner: Option<String>,
    pub kind: Option<SessionKind>,
    pub deployment: Option<String>,
    pub environment: Option<String>,
    pub project: Option<Option<String>>,
}

impl SessionFilter {
    pub fn matches(&self, meta: &SessionMeta) -> bool {
        let same =
            |want: &Option<String>, have: &str| want.as_deref().is_none_or(|want| want == have);
        same(&self.tenant, &meta.tenant)
            && same(&self.owner, &meta.owner)
            && same(&self.deployment, &meta.deployment)
            && same(&self.environment, &meta.environment)
            && self.kind.is_none_or(|kind| kind == meta.kind)
            && self
                .project
                .as_ref()
                .is_none_or(|project| meta.in_project(project.as_deref()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub offset: usize,
    pub limit: usize,
}

impl Page {
    pub const ALL: Page = Page {
        offset: 0,
        limit: usize::MAX,
    };

    pub fn first(limit: usize) -> Self {
        Self { offset: 0, limit }
    }

    pub fn of(&self, mut sessions: Vec<SessionMeta>) -> SessionPage {
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        let total = sessions.len();
        let sessions: Vec<SessionMeta> = sessions
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .collect();
        let more = self.offset.saturating_add(sessions.len()) < total;
        SessionPage { sessions, more }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionPage {
    pub sessions: Vec<SessionMeta>,
    pub more: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("session is busy: run {active_run} is in progress")]
    Busy { active_run: String },
    #[error("run {run} already exists in session {session}")]
    Duplicate { run: String, session: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, thiserror::Error)]
pub enum FenceError {
    #[error("the run's lease was lost")]
    LeaseLost,
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub const LEASE_RENEW_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

pub const LEASE_EXPIRES_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

pub const LEASE_SELF_STOP_AFTER: std::time::Duration = std::time::Duration::from_secs(25);

pub const REAPED_ERROR: &str = "the process running this turn stopped renewing its lease";

pub fn lease_expiry(now: i64) -> i64 {
    now.saturating_add(LEASE_EXPIRES_AFTER.as_millis() as i64)
}

pub fn check_batch(lease: &Lease, last_seq: u64, entries: &[Entry]) -> Result<(), StoreError> {
    let mut after = last_seq;
    for entry in entries {
        if entry.run_id != lease.run {
            return Err(StoreError(format!(
                "entry {} belongs to run {}, not {}",
                entry.seq, entry.run_id, lease.run
            )));
        }
        if entry.seq <= after {
            return Err(StoreError(format!(
                "duplicate seq {} in session {} (last is {after})",
                entry.seq, lease.session
            )));
        }
        after = entry.seq;
    }
    Ok(())
}

pub fn reaped_ending(kind: SessionKind) -> RunEnded {
    match kind {
        SessionKind::Chat => RunEnded::failed(REAPED_ERROR),
        SessionKind::Plan => RunEnded {
            error: Some(REAPED_ERROR.to_string()),
            ..RunEnded::new(RunStatus::Interrupted)
        },
    }
}

pub fn ended_entry(run: &str, seq: u64, now: i64, ended: RunEnded) -> Entry {
    NewEntry::new(RUN_AUTHOR, EntryBody::RunEnded(ended)).into_entry(seq, run, now)
}

pub fn cancel_entry(run: &str, seq: u64, now: i64, by: &str) -> Entry {
    NewEntry::new(
        RUN_AUTHOR,
        EntryBody::CancelRequested { by: by.to_string() },
    )
    .into_entry(seq, run, now)
}

pub fn messages_in<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> u64 {
    entries
        .into_iter()
        .filter(|entry| entry.path.is_empty() && matches!(entry.body, EntryBody::Message { .. }))
        .count() as u64
}

#[derive(Debug, Clone)]
pub struct NewEntry {
    pub author: String,
    pub body: EntryBody,
    pub path: String,
    pub parent: Option<u64>,
    pub ms: Option<u64>,
    pub usage: Option<EntryUsage>,
    pub span: Option<String>,
}

impl NewEntry {
    pub fn new(author: &str, body: EntryBody) -> Self {
        Self {
            author: author.to_string(),
            body,
            path: String::new(),
            parent: None,
            ms: None,
            usage: None,
            span: None,
        }
    }

    pub fn at_path(self, path: &str, parent: Option<u64>) -> Self {
        Self {
            path: path.to_string(),
            parent,
            ..self
        }
    }

    pub fn into_entry(self, seq: u64, run_id: &str, at: i64) -> Entry {
        Entry {
            seq,
            run_id: run_id.to_string(),
            path: self.path,
            parent: self.parent,
            span_id: self.span,
            at,
            author: self.author,
            body: self.body,
            ms: self.ms,
            usage: self.usage,
        }
    }

    pub fn message(author: &str, message: ChatMessage) -> Self {
        Self::new(author, EntryBody::Message { message })
    }

    pub fn in_span(self, span: Option<String>) -> Self {
        Self { span, ..self }
    }

    pub fn stamped(self, stamp: Stamp) -> Self {
        Self {
            ms: stamp.ms,
            usage: stamp.usage,
            span: stamp.span,
            ..self
        }
    }
}

impl From<Entry> for NewEntry {
    fn from(entry: Entry) -> Self {
        Self {
            author: entry.author,
            body: entry.body,
            path: entry.path,
            parent: entry.parent,
            ms: entry.ms,
            usage: entry.usage,
            span: entry.span_id,
        }
    }
}

/// An observed (or declared) output shape for a tool.
#[derive(Debug, Clone)]
pub struct ToolShape {
    pub tool: String,
    pub schema: Value,
    pub example: Value,
    pub seen_count: i64,
}

#[derive(Debug, thiserror::Error)]
#[error("store error: {0}")]
pub struct StoreError(pub String);

#[async_trait]
pub trait Store: Send + Sync {
    async fn create_session(&self, new: NewSession) -> Result<SessionMeta, StoreError>;
    async fn get_session(&self, id: &str) -> Result<Option<SessionMeta>, StoreError>;
    async fn list_sessions(
        &self,
        filter: &SessionFilter,
        page: Page,
    ) -> Result<SessionPage, StoreError>;
    async fn delete_session(&self, id: &str) -> Result<bool, StoreError>;
    async fn set_active_agent(&self, lease: &Lease, agent: &str) -> Result<(), FenceError>;
    async fn set_attachments(&self, id: &str, attachments: &[Attachment])
        -> Result<(), StoreError>;

    async fn start_run(&self, start: StartRun) -> Result<Lease, StartError>;
    async fn append(&self, lease: &Lease, entries: &[Entry]) -> Result<(), FenceError>;
    async fn renew(&self, lease: &Lease) -> Result<Renewal, FenceError>;
    async fn end_run(&self, lease: &Lease, ended: RunEnded) -> Result<(), FenceError>;
    async fn request_cancel(&self, run: &str, by: &str) -> Result<CancelOutcome, StoreError>;
    async fn reap_expired(&self, limit: usize) -> Result<Vec<Reaped>, StoreError>;

    async fn get_run(&self, id: &str) -> Result<Option<RunMeta>, StoreError>;
    async fn load_entries(
        &self,
        session: &str,
        after: Option<u64>,
    ) -> Result<Vec<Entry>, StoreError>;
    async fn append_spans(&self, session: &str, spans: &[Span]) -> Result<(), StoreError>;
    async fn load_spans(&self, session: &str) -> Result<Vec<Span>, StoreError>;

    /// Record an observed output shape for a tool (upsert; bumps seen_count).
    async fn record_tool_shape(
        &self,
        tool: &str,
        schema: &Value,
        example: &Value,
    ) -> Result<(), StoreError>;
    async fn tool_shapes(&self) -> Result<Vec<ToolShape>, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entries_serialize_flat_with_their_kind_and_envelope() {
        let entry = NewEntry::new(
            "orchestrator",
            EntryBody::Handoff {
                from: "orchestrator".to_string(),
                to: "plan_author".to_string(),
                message: "draft it".to_string(),
                via: None,
            },
        )
        .into_entry(3, "run-1", 10);
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            value,
            json!({"seq": 3, "run_id": "run-1", "at": 10, "author": "orchestrator",
                   "kind": "handoff", "from": "orchestrator", "to": "plan_author",
                   "message": "draft it"})
        );
        let back: Entry = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(back).unwrap(), value);

        let finished = NewEntry::new(
            "plan_author",
            EntryBody::AgentFinished {
                agent: "plan_refiner".to_string(),
                output: json!({"changes": []}),
                final_: true,
            },
        )
        .at_path("", Some(4))
        .into_entry(6, "run-1", 11);
        let value = serde_json::to_value(&finished).unwrap();
        assert_eq!(value["kind"], json!("agent_finished"));
        assert_eq!(value["final"], json!(true));
        assert_eq!(value["parent"], json!(4));
        assert!(value.get("path").is_none());
        let back: Entry = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(back).unwrap(), value);
    }

    #[test]
    fn run_bookends_round_trip() {
        let ended = NewEntry::new(
            RUN_AUTHOR,
            EntryBody::RunEnded(RunEnded::failed("boom").with_usage(Some(json!({"cost": 1})))),
        )
        .into_entry(9, "run-1", 12);
        let value = serde_json::to_value(&ended).unwrap();
        assert_eq!(value["kind"], "run_ended");
        assert_eq!(value["status"], "failed");
        assert_eq!(value["error"], "boom");
        assert_eq!(value["run_usage"], json!({"cost": 1}));
        let back: Entry = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(
            back.body,
            EntryBody::RunEnded(RunEnded {
                status: RunStatus::Failed,
                ..
            })
        ));
    }

    #[test]
    fn a_session_belongs_only_to_the_project_it_records() {
        let identity = crate::run::Identity::default();
        let meta = NewSession::chat("t", "chat", &identity)
            .in_project(Some("/work/site"))
            .meta("a1b2c3d4e5f6".into(), 1);
        assert!(meta.in_project(Some("/work/site")));
        assert!(!meta.in_project(Some("/work/other")));
        assert!(!meta.in_project(None));
        let global = NewSession::chat("t", "chat", &identity).meta("b".into(), 1);
        assert!(global.in_project(None));
        assert!(serde_json::to_value(&global)
            .unwrap()
            .get("project")
            .is_none());
    }

    #[test]
    fn an_attachment_keeps_its_kind_specific_fields_flat() {
        let attachment = Attachment::new("network").with("state", "saved");
        let value = serde_json::to_value(&attachment).unwrap();
        assert_eq!(value, json!({"kind": "network", "state": "saved"}));
        let back: Attachment = serde_json::from_value(value).unwrap();
        assert_eq!(back, attachment);
        assert_eq!(back.field("state"), Some("saved"));
    }

    #[test]
    fn a_batch_must_continue_the_log_in_order_within_its_run() {
        let lease = Lease {
            session: "s".into(),
            run: "r".into(),
            generation: 1,
            holder: "h".into(),
            next_seq: 3,
        };
        let entry = |seq: u64, run: &str| {
            NewEntry::message(USER_AUTHOR, ChatMessage::user("x")).into_entry(seq, run, 0)
        };
        assert!(check_batch(&lease, 2, &[entry(3, "r"), entry(4, "r")]).is_ok());
        assert!(check_batch(&lease, 3, &[entry(3, "r")]).is_err());
        assert!(check_batch(&lease, 2, &[entry(4, "r"), entry(4, "r")]).is_err());
        assert!(check_batch(&lease, 2, &[entry(3, "other")]).is_err());
    }
}
