use crate::cancel::CancelHandle;
use crate::runtime::{PipelineHooks, Runtime};
use crate::writer::RunWriter;
use dive_core::packs::Origin;
use dive_core::pipeline::doc::{apply_schema_defaults, validate_input, PlanDoc};
use dive_core::pipeline::{catalog, ExitStatus, PipelineError, PipelineOutcome};
use dive_core::run::{Identity, Run, RunId, RunToken};
use dive_core::store::{
    Entry, EntryBody, EntryHook, NewEntry, NewSession, RunEnded, RunMeta, RunStarted, RunStatus,
    SessionKind, StartError, StartRun, StoreError, RUN_AUTHOR,
};
use dive_core::usage::UsageReport;
use dive_core::{EventSink, Store, TeeSink};
use futures::Stream;
use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const FOLLOW_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanInfo {
    pub identifier: String,
    pub name: String,
    pub description: String,
    pub input_schema: Option<Value>,
    pub result_schema: Option<Value>,
}

impl From<&PlanDoc> for PlanInfo {
    fn from(doc: &PlanDoc) -> Self {
        Self {
            identifier: doc.identifier.clone(),
            name: doc.name.clone(),
            description: doc.description.clone(),
            input_schema: doc.input_schema.clone(),
            result_schema: doc.output_schema.clone(),
        }
    }
}

fn callable(runtime: &Runtime) -> Vec<PlanDoc> {
    let packs = runtime.packs.pack_set();
    runtime
        .plan_docs()
        .docs
        .into_iter()
        .filter(|doc| doc.path.is_some() || doc.pack.is_some())
        .filter(|doc| packs.shows(&Origin::Outside, &doc.identifier))
        .collect()
}

pub fn plans(runtime: &Runtime) -> Vec<PlanInfo> {
    callable(runtime).iter().map(PlanInfo::from).collect()
}

pub fn plan(runtime: &Runtime, identifier: &str) -> Option<PlanInfo> {
    callable(runtime)
        .iter()
        .find(|doc| doc.identifier == identifier)
        .map(PlanInfo::from)
}

#[derive(Debug)]
pub struct StartPlan {
    pub plan: String,
    pub input: Value,
    pub title: Option<String>,
    pub run_id: String,
    pub identity: Identity,
    pub token: Option<RunToken>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartFailure {
    #[error("no plan named '{0}'")]
    NoPlan(String),
    #[error("plan '{plan}' can't run here: {reason}")]
    Unrunnable { plan: String, reason: String },
    #[error("the input doesn't match the input schema of plan '{plan}'")]
    InvalidInput { plan: String, problems: Vec<String> },
    #[error("run {run} already started")]
    Duplicate { run: String, session: String },
    #[error("this token's jti was already used")]
    JtiTaken,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<StoreError> for StartFailure {
    fn from(error: StoreError) -> Self {
        Self::Internal(anyhow::Error::new(error))
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunControl {
    cancel: CancelHandle,
    interrupted: Arc<AtomicBool>,
    cancelled_by: Arc<std::sync::Mutex<Option<String>>>,
    forced: tokio_util::sync::CancellationToken,
}

impl RunControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::SeqCst);
        self.cancel.cancel();
    }

    pub fn force(&self) {
        self.interrupt();
        self.forced.cancel();
    }

    pub fn interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    pub fn cancel(&self, by: &str) {
        self.cancelled_by
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert_with(|| by.to_string());
        self.cancel.cancel();
    }

    fn cancelled_by(&self) -> Option<String> {
        self.cancelled_by
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

pub type Execution = Pin<Box<dyn Future<Output = RunStatus> + Send>>;

pub struct StartedRun {
    pub run_id: String,
    pub session_id: String,
    pub execution: Execution,
}

pub fn check_plan(
    runtime: &Runtime,
    plan: &str,
    input: &mut Value,
) -> Result<PlanDoc, StartFailure> {
    let loaded = runtime.plan_docs();
    let packs = runtime.packs.pack_set();
    let doc = loaded
        .docs
        .iter()
        .filter(|doc| doc.path.is_some() || doc.pack.is_some())
        .find(|doc| doc.identifier == plan && packs.shows(&Origin::Outside, &doc.identifier))
        .cloned()
        .ok_or_else(|| StartFailure::NoPlan(plan.to_string()))?;
    let tools = runtime.tool_catalog(&loaded.docs)?;
    let requirements = runtime.requirements_over(&loaded.docs, &runtime.agent_set(), &tools);
    if let Some(refusal) =
        requirements.refusal(&dive_core::requirements::Item::Plan(plan.to_string()))
    {
        return Err(StartFailure::Unrunnable {
            plan: plan.to_string(),
            reason: refusal.to_string(),
        });
    }
    let mut check = catalog::resolve_plan_tools_deep(&doc, &loaded.docs, &tools);
    check.errors.append(&mut check.notes);
    if !check.errors.is_empty() {
        return Err(StartFailure::Unrunnable {
            plan: plan.to_string(),
            reason: format!("unresolvable tools: {}", check.errors.join("; ")),
        });
    }
    if let Some(schema) = &doc.input_schema {
        apply_schema_defaults(schema, input);
    }
    validate_input(&doc, input).map_err(|problems| StartFailure::InvalidInput {
        plan: plan.to_string(),
        problems,
    })?;
    Ok(doc)
}

pub async fn start_plan(
    runtime: &Arc<Runtime>,
    store: &Arc<dyn Store>,
    request: StartPlan,
    control: RunControl,
    events: Arc<dyn EventSink>,
) -> Result<StartedRun, StartFailure> {
    let StartPlan {
        plan,
        mut input,
        title,
        run_id,
        identity,
        token,
    } = request;
    let doc = check_plan(runtime, &plan, &mut input)?;
    if let Some(existing) = store.get_run(&run_id).await? {
        return Err(duplicate(existing, &identity.tenant));
    }
    let steps = Arc::new(StepEntries::default());
    let events: Arc<dyn EventSink> = Arc::new(TeeSink::new(vec![steps.clone(), events]));
    let mut run = Run::new(identity, events.clone()).with_id(RunId::from(run_id.as_str()));
    if let Some(token) = token {
        run = run.with_token(token);
    }
    let run = Arc::new(run);
    let writer = begin(store, &run, &doc, &input, title.as_deref(), &control).await?;
    steps.attach(writer.hook());
    let pipeline = match runtime
        .pipeline_with(
            store,
            events,
            PipelineHooks {
                gate: None,
                interlocutor: None,
                cancel: Some(control.cancel.clone()),
            },
        )
        .await
    {
        Ok(pipeline) => pipeline,
        Err(error) => {
            let message = error.to_string();
            if let Err(fence) = writer.finish(RunEnded::failed(&message)).await {
                tracing::warn!(error = %fence, "couldn't close the plan run");
            }
            return Err(StartFailure::Internal(error));
        }
    };
    let session_id = writer.session().to_string();
    let execution = Box::pin(async move {
        let query = format!("Run the '{}' plan", doc.name);
        let work = run.scope(pipeline.run_explicit_in(
            &query,
            doc.steps.clone(),
            doc.finish.clone(),
            Some(input),
            doc.pack.as_ref(),
        ));
        let result = tokio::select! {
            result = control.cancel.run(work) => result.ok(),
            _ = control.forced.cancelled() => None,
        };
        let mut cancelled = writer.cancel_requested();
        if !cancelled {
            if let Some(by) = control.cancelled_by() {
                writer.push(NewEntry::new(RUN_AUTHOR, EntryBody::CancelRequested { by }));
                cancelled = true;
            }
        }
        let ended = ending(result, &run.usage(), cancelled || !control.interrupted());
        let status = ended.status;
        if let Err(error) = writer.finish(ended).await {
            tracing::warn!(error = %error, run = %run.id, "couldn't close the plan run");
        }
        status
    });
    Ok(StartedRun {
        run_id,
        session_id,
        execution,
    })
}

async fn begin(
    store: &Arc<dyn Store>,
    run: &Run,
    doc: &PlanDoc,
    input: &Value,
    title: Option<&str>,
    control: &RunControl,
) -> Result<RunWriter, StartFailure> {
    let title = title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(&doc.name);
    let session = store
        .create_session(NewSession::plan(title, &doc.identifier, &run.identity))
        .await?;
    let start = StartRun {
        run_id: run.id.to_string(),
        session: session.id.clone(),
        kind: SessionKind::Plan,
        holder: crate::writer::holder().to_string(),
        token_expires_at: None,
        started: RunStarted {
            input: input.clone(),
            identity: run.identity.clone(),
            trace_id: run.trace.as_ref().map(|trace| trace.trace_id.clone()),
            plan: serde_json::to_value(doc).ok(),
            synthetic: false,
        },
    };
    match RunWriter::start(Arc::clone(store), start, Some(control.cancel.clone())).await {
        Ok(writer) => Ok(writer),
        Err(error) => {
            if let Err(cleanup) = store.delete_session(&session.id).await {
                tracing::warn!(error = %cleanup, session = %session.id, "couldn't remove an unused session");
            }
            Err(match error {
                StartError::Duplicate { .. } => match store.get_run(run.id.as_str()).await? {
                    Some(existing) => duplicate(existing, &run.identity.tenant),
                    None => StartFailure::Internal(anyhow::Error::new(error)),
                },
                other => StartFailure::Internal(anyhow::Error::new(other)),
            })
        }
    }
}

fn duplicate(existing: RunMeta, tenant: &str) -> StartFailure {
    if existing.tenant == tenant {
        StartFailure::Duplicate {
            run: existing.id,
            session: existing.session,
        }
    } else {
        StartFailure::JtiTaken
    }
}

pub fn ending(
    result: Option<Result<PipelineOutcome, PipelineError>>,
    usage: &UsageReport,
    cancelled: bool,
) -> RunEnded {
    let stopped = if cancelled {
        RunStatus::Cancelled
    } else {
        RunStatus::Interrupted
    };
    let ended = match result {
        None | Some(Err(PipelineError::Aborted { .. })) => RunEnded::new(stopped),
        Some(Ok(outcome)) => {
            let output = Some(crate::telemetry::plan_output(&outcome));
            match &outcome.exit {
                Some(exit) if exit.status == ExitStatus::Error => RunEnded {
                    error: Some(exit.message.clone()),
                    ..RunEnded::completed(output)
                },
                _ => RunEnded::completed(output),
            }
        }
        Some(Err(error)) => RunEnded::failed(&error.to_string()),
    };
    ended.with_usage(serde_json::to_value(usage).ok())
}

#[derive(Default)]
struct StepEntries {
    hook: OnceLock<EntryHook>,
}

impl StepEntries {
    fn attach(&self, hook: EntryHook) {
        let _ = self.hook.set(hook);
    }

    fn push(&self, call_stack: &[String], path: &str, body: EntryBody) {
        if let Some(hook) = self.hook.get() {
            hook(NewEntry::new(RUN_AUTHOR, body).at_path(&step_path(call_stack, path), None));
        }
    }
}

pub use dive_core::run::step_path;

impl EventSink for StepEntries {
    fn step_started(&self, call_stack: &[String], path: &str, tool: &str, input: &Value) {
        self.push(
            call_stack,
            path,
            EntryBody::StepStarted {
                step: path.to_string(),
                tool: Some(tool.to_string()),
                input: input.clone(),
            },
        );
    }

    fn step_finished(
        &self,
        call_stack: &[String],
        path: &str,
        _tool: &str,
        result: &Value,
        is_error: bool,
        _elapsed: Duration,
    ) {
        self.push(
            call_stack,
            path,
            EntryBody::StepFinished {
                step: path.to_string(),
                output: result.clone(),
                is_error,
            },
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    ExitGate,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunView {
    pub id: String,
    pub session_id: String,
    pub plan: Option<String>,
    pub status: RunStatus,
    pub outcome: Option<Outcome>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub first_seq: u64,
    pub last_seq: Option<u64>,
    pub cancel_requested: bool,
    pub usage: Option<Value>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

fn timestamp(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

pub async fn view(store: &dyn Store, run: &RunMeta) -> Result<RunView, StoreError> {
    let plan = store
        .get_session(&run.session)
        .await?
        .and_then(|session| session.plan);
    let ended = match run.last_seq {
        Some(last) if run.status != RunStatus::Running => store
            .load_entries(&run.session, Some(last.saturating_sub(1)))
            .await?
            .into_iter()
            .filter(|entry| entry.run_id == run.id)
            .find_map(|entry| match entry.body {
                EntryBody::RunEnded(ended) => Some(ended),
                _ => None,
            }),
        _ => None,
    };
    let outcome = (run.status == RunStatus::Completed).then(|| {
        match ended.as_ref().and_then(|ended| ended.error.as_ref()) {
            Some(_) => Outcome::ExitGate,
            None => Outcome::Ok,
        }
    });
    let (result, error, usage) = match ended {
        Some(ended) => (ended.outcome, ended.error, ended.run_usage),
        None => (None, None, None),
    };
    Ok(RunView {
        id: run.id.clone(),
        session_id: run.session.clone(),
        plan,
        status: run.status,
        outcome,
        result,
        error,
        first_seq: run.first_seq,
        last_seq: run.last_seq,
        cancel_requested: run.cancel_requested,
        usage,
        started_at: timestamp(run.started_at),
        ended_at: run.ended_at.map(timestamp),
    })
}

struct Follow {
    store: Arc<dyn Store>,
    session: String,
    run: String,
    cursor: u64,
    pending: VecDeque<Entry>,
    done: bool,
    poll: Duration,
    first: bool,
}

pub fn follow(
    store: Arc<dyn Store>,
    run: &RunMeta,
    after: Option<u64>,
    poll: Duration,
) -> impl Stream<Item = Result<Entry, StoreError>> + Send + 'static {
    let state = Follow {
        store,
        session: run.session.clone(),
        run: run.id.clone(),
        cursor: after.unwrap_or(0).max(run.first_seq.saturating_sub(1)),
        pending: VecDeque::new(),
        done: false,
        poll,
        first: true,
    };
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(entry) = state.pending.pop_front() {
                if matches!(entry.body, EntryBody::RunEnded(_)) {
                    state.done = true;
                }
                return Some((Ok(entry), state));
            }
            if state.done {
                return None;
            }
            if !state.first {
                tokio::time::sleep(state.poll).await;
            }
            state.first = false;
            match state
                .store
                .load_entries(&state.session, Some(state.cursor))
                .await
            {
                Ok(entries) => {
                    for entry in entries {
                        state.cursor = state.cursor.max(entry.seq);
                        if entry.run_id == state.run {
                            state.pending.push_back(entry);
                        }
                    }
                }
                Err(error) => {
                    state.done = true;
                    return Some((Err(error), state));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn identity() -> Identity {
        crate::ops::session::tests::identity()
    }

    async fn ended_run(store: &Arc<dyn Store>) -> RunMeta {
        let run = Arc::new(Run::new(identity(), Arc::new(dive_core::NullSink)));
        let doc: PlanDoc =
            serde_yaml::from_str("identifier: digest\nname: Digest\ndescription: d\nsteps: []\n")
                .unwrap();
        let writer = begin(
            store,
            &run,
            &doc,
            &serde_json::json!({"day": 1}),
            Some("Digest for Monday"),
            &RunControl::new(),
        )
        .await
        .unwrap();
        let steps = StepEntries::default();
        steps.attach(writer.hook());
        steps.step_started(&[], "E0", "builtin__reshape", &serde_json::json!({}));
        steps.step_finished(
            &["inner".to_string()],
            "E1",
            "builtin__reshape",
            &serde_json::json!({"ok": true}),
            false,
            Duration::ZERO,
        );
        writer
            .finish(RunEnded::completed(Some(
                serde_json::json!({"day": "monday"}),
            )))
            .await
            .unwrap();
        store.get_run(run.id.as_str()).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_finished_run_reads_back_with_its_plan_result_and_ok_outcome() {
        let store: Arc<dyn Store> = Arc::new(dive_store::MemoryStore::new());
        let meta = ended_run(&store).await;
        let view = view(store.as_ref(), &meta).await.unwrap();
        assert_eq!(view.plan.as_deref(), Some("digest"));
        assert_eq!(view.status, RunStatus::Completed);
        assert_eq!(view.outcome, Some(Outcome::Ok));
        assert_eq!(view.result, Some(serde_json::json!({"day": "monday"})));
        let session = store.get_session(&meta.session).await.unwrap().unwrap();
        assert_eq!(session.title, "Digest for Monday");
    }

    #[tokio::test]
    async fn following_a_finished_run_replays_its_entries_after_the_cursor_and_ends() {
        let store: Arc<dyn Store> = Arc::new(dive_store::MemoryStore::new());
        let meta = ended_run(&store).await;
        let all: Vec<Entry> = follow(store.clone(), &meta, None, Duration::from_millis(1))
            .map(Result::unwrap)
            .collect()
            .await;
        let kinds: Vec<&str> = all
            .iter()
            .map(|entry| match &entry.body {
                EntryBody::RunStarted(_) => "run_started",
                EntryBody::StepStarted { .. } => "step_started",
                EntryBody::StepFinished { .. } => "step_finished",
                EntryBody::RunEnded(_) => "run_ended",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            ["run_started", "step_started", "step_finished", "run_ended"]
        );
        assert_eq!(all[1].path, "step:E0");
        assert_eq!(all[2].path, "plan:inner/step:E1");
        let rest: Vec<u64> = follow(
            store.clone(),
            &meta,
            Some(all[1].seq),
            Duration::from_millis(1),
        )
        .map(|entry| entry.unwrap().seq)
        .collect()
        .await;
        assert_eq!(rest, vec![all[2].seq, all[3].seq]);
    }

    #[test]
    fn an_exit_gate_completes_the_run_with_its_message_and_a_stop_without_a_cancel_interrupts() {
        let interrupted = ending(None, &UsageReport::default(), false);
        assert_eq!(interrupted.status, RunStatus::Interrupted);
        let cancelled = ending(None, &UsageReport::default(), true);
        assert_eq!(cancelled.status, RunStatus::Cancelled);
        let failed = ending(
            Some(Err(PipelineError::OutputRender("boom".into()))),
            &UsageReport::default(),
            true,
        );
        assert_eq!(failed.status, RunStatus::Failed);
    }
}
