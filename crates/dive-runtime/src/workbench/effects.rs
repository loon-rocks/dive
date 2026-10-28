//! The effect executor: everything the pure reducer can't do. Long-running
//! work is spawned; completion always arrives back as a [`WorkbenchEvent`].

use super::event::WorkbenchEvent;
use super::network::NetworkView;
use super::runner::{DebugControls, UiGate, UiInterlocutor};
use super::tools::DraftState;
use dive_core::agent::conversation::{Conversation, ConversationError};
use dive_core::pipeline::authoring;
use dive_core::pipeline::doc::PlanDoc;
use dive_core::pipeline::Pipeline;
use dive_core::{AgentError, NewEntry, Store, ToolRegistry};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

pub struct Workbench {
    pub conversation: Conversation,
    pub active: std::sync::Mutex<String>,
    /// The plan-run pipeline (its sink already feeds the UI channel);
    /// gated runs clone it and install a [`UiGate`].
    pub pipeline: Arc<Pipeline>,
    pub history: Arc<tokio::sync::Mutex<Vec<NewEntry>>>,
    /// The draft plan, shared with [`super::tools::WorkbenchTools`].
    pub draft: Arc<std::sync::Mutex<DraftState>>,
    /// The agent's full catalog — the context pane shows what the planner
    /// and agent can call.
    pub catalog: Arc<dyn ToolRegistry>,
    pub store: Arc<dyn Store>,
    /// Where unsaved drafts land on Ctrl+S (first configured plans dir).
    pub plans_dir: Option<PathBuf>,
    /// Shared debugger state (breakpoints, continue mode) read by the gate.
    pub debug: Arc<DebugControls>,
    pub tx: UnboundedSender<WorkbenchEvent>,
    pub root: PathBuf,
    pub thread: std::sync::Mutex<Option<String>>,
    pub project: Option<String>,
    pub attachment: Arc<super::attachment::NetworkAttachment>,
}

impl Workbench {
    pub fn plan(&self) -> Option<PlanDoc> {
        self.draft.lock().unwrap().doc.clone()
    }

    pub fn is_dirty(&self) -> bool {
        self.draft.lock().unwrap().dirty
    }

    pub fn network(&self) -> NetworkView {
        super::artifact::view(&self.draft)
    }

    pub fn active_agent(&self) -> String {
        self.active.lock().unwrap().clone()
    }

    pub async fn send_message(&self, message: impl Into<String>) -> Result<String, String> {
        let message = message.into();
        let ctx = self;
        tracing::debug!(
            target: "workbench",
            "agent turn started ({} chars)",
            message.len()
        );
        let turn_started = std::time::Instant::now();
        let events = ctx.conversation.events.clone();
        let active = ctx.active.lock().unwrap().clone();
        let mut history = ctx.history.lock().await;
        events.run_started(&dive_core::RunStart {
            name: "workbench".to_string(),
            session_id: None,
            input: Some(serde_json::Value::String(message.clone())),
        });
        let result = ctx.conversation.run_turn(&history, &active, &message).await;
        match &result {
            Ok(turn) => events.run_finished(&serde_json::Value::String(turn.text.clone()), false),
            Err(error) => {
                events.run_finished(&serde_json::json!({"error": error.to_string()}), true)
            }
        }
        let result = match result {
            Ok(mut turn) => {
                if turn.text.trim().is_empty() {
                    if let Some(reply) = super::replay::last_reply(&turn.entries) {
                        turn.text = reply;
                    }
                }
                ctx.persist(&message, &active, &turn).await;
                history.extend(turn.entries);
                if turn.active != active {
                    *ctx.active.lock().unwrap() = turn.active.clone();
                    let _ = ctx.tx.send(WorkbenchEvent::ActiveAgent {
                        name: turn.active.clone(),
                        note: None,
                    });
                }
                Ok(turn.text)
            }
            Err(ConversationError::Turn {
                source, partial, ..
            }) => {
                if keep_partial_history(&source) {
                    history.extend(partial.entries);
                }
                Err(turn_failure_message(source))
            }
            Err(other) => Err(other.to_string()),
        };
        // Per turn, matching `dive chat`. The ledger is shared with
        // the pipeline, so a turn that called a plan tool reports that
        // plan's spend too.
        let usage = ctx.pipeline.usage.take();
        if !usage.is_empty() {
            events.usage_summary(&usage);
        }
        tracing::debug!(
            target: "workbench",
            "agent turn took {:.1}s ({} entries in history)",
            turn_started.elapsed().as_secs_f64(),
            history.len()
        );
        let _ = ctx.tx.send(WorkbenchEvent::TurnFinished(result.clone()));
        result
    }

    pub fn thread_id(&self) -> Option<String> {
        self.thread.lock().unwrap().clone()
    }

    async fn persist(
        &self,
        first_message: &str,
        active: &str,
        turn: &dive_core::agent::conversation::ConversationTurn,
    ) {
        let existing = self.thread.lock().unwrap().clone();
        let id = match existing {
            Some(id) => id,
            None => match self
                .store
                .create_thread(
                    &crate::runtime::title_from(first_message),
                    active,
                    self.project.as_deref(),
                )
                .await
            {
                Ok(meta) => {
                    *self.thread.lock().unwrap() = Some(meta.id.clone());
                    let record =
                        crate::attachment::AttachmentHost::record(self.attachment.as_ref());
                    if let Err(error) = self.store.set_attachments(&meta.id, &[record]).await {
                        tracing::warn!(
                            "couldn't record the draft's network on its thread: {error}"
                        );
                    }
                    meta.id
                }
                Err(error) => {
                    tracing::warn!("couldn't save the draft session as a thread: {error}");
                    return;
                }
            },
        };
        if let Err(error) = self.store.append_entries(&id, &turn.entries).await {
            tracing::warn!("couldn't save the draft turn: {error}");
        }
        if let Err(error) = self.store.append_spans(&id, &turn.spans).await {
            tracing::warn!("couldn't save the draft turn's trace: {error}");
        }
        if turn.active != active {
            let _ = self.store.set_active_agent(&id, &turn.active).await;
        }
    }

    pub fn select_agent(&self, name: Option<String>) {
        let ctx = self;
        let msg = match name {
            None => {
                let active = ctx.active.lock().unwrap().clone();
                let lines: Vec<String> = ctx
                    .conversation
                    .agents
                    .iter()
                    .map(|doc| {
                        let marker = if doc.name == active { "*" } else { " " };
                        format!("{marker} {} — {}", doc.name, doc.description)
                    })
                    .collect();
                WorkbenchEvent::AgentNotice(format!("talking with {active}\n{}", lines.join("\n")))
            }
            Some(name) => match ctx.conversation.agent(&name) {
                Ok(doc) => {
                    *ctx.active.lock().unwrap() = doc.name.clone();
                    WorkbenchEvent::ActiveAgent {
                        name: doc.name.clone(),
                        note: Some(format!("now talking with {}", doc.name)),
                    }
                }
                Err(error) => WorkbenchEvent::AgentNotice(error.to_string()),
            },
        };
        let _ = ctx.tx.send(msg);
    }

    pub async fn start_run(&self, gated: bool, input: Value) {
        let ctx = self;
        let doc = { ctx.draft.lock().unwrap().doc.clone() };
        let Some(doc) = doc else {
            let _ = ctx.tx.send(WorkbenchEvent::RunFinished {
                headline: "no plan to run".to_string(),
                is_error: true,
                exited: false,
                results: Map::new(),
            });
            return;
        };
        let mut pipeline = (*ctx.pipeline)
            .clone()
            .with_interlocutor(Arc::new(UiInterlocutor::new(ctx.tx.clone())));
        if gated {
            ctx.debug.arm();
            pipeline = pipeline.with_gate(Arc::new(UiGate::new(ctx.tx.clone(), ctx.debug.clone())));
        }
        tracing::debug!(
            target: "workbench",
            "run started: '{}' ({} steps, gated={gated})",
            doc.identifier,
            doc.steps.len()
        );
        let run_started = std::time::Instant::now();
        pipeline.events.run_started(&dive_core::RunStart {
            name: doc.identifier.clone(),
            session_id: None,
            input: Some(input.clone()),
        });
        let query = format!("Run the '{}' plan", doc.name);
        let result = pipeline
            .run_explicit_in(
                &query,
                doc.steps.clone(),
                doc.finish.clone(),
                Some(input),
                doc.pack.as_ref(),
            )
            .await;
        crate::telemetry::report_plan_result(pipeline.events.as_ref(), &result);
        tracing::debug!(
            target: "workbench",
            "run took {:.1}s",
            run_started.elapsed().as_secs_f64()
        );
        // Before the finished message, so the run log reads
        // step → usage → verdict. Drained even on a failed run: the
        // tokens were still spent.
        let usage = pipeline.usage.take();
        if !usage.is_empty() {
            pipeline.events.usage_summary(&usage);
        }
        let msg = super::runner::report(result).finished_msg();
        let _ = ctx.tx.send(msg);
    }

    pub fn validate(&self) {
        let ctx = self;
        let problems = match &ctx.draft.lock().unwrap().doc {
            Some(doc) => super::tools::plan_problems(&ctx.pipeline, doc),
            None => vec!["no draft to validate".to_string()],
        };
        let _ = ctx.tx.send(WorkbenchEvent::Validated(problems));
    }

    pub async fn load_context(&self) {
        let ctx = self;
        // The registry alone under-reports what the planner sees:
        // control steps are executor-evaluated, never registered, so
        // their descriptions are appended here just like the `tools`
        // command surface does.
        let mut tools = ctx.catalog.tools().await.unwrap_or_default();
        tools.extend(dive_core::pipeline::control_step_defs());
        let shapes = ctx.store.tool_shapes().await.unwrap_or_default();
        let _ = ctx.tx.send(WorkbenchEvent::ContextLoaded { tools, shapes });
    }

    pub fn sync_debug(&self, breakpoints: HashSet<String>) {
        self.debug.set_breakpoints(breakpoints);
    }

    pub fn apply_edit(&self, commit: bool, edit: impl Fn(&mut PlanDoc) -> Result<Value, Value>) {
        let ctx = self;
        let current = { ctx.draft.lock().unwrap().doc.clone() };
        let msg = match current {
            None => WorkbenchEvent::EditOutcome {
                committed: false,
                introduced: vec!["no draft to edit".to_string()],
                pre_existing: Vec::new(),
            },
            Some(doc) if commit => match authoring::apply_edit(&doc, &edit) {
                Ok(accepted) => {
                    super::tools::publish_draft(&ctx.draft, &ctx.tx, accepted.doc, true);
                    WorkbenchEvent::EditOutcome {
                        committed: true,
                        introduced: Vec::new(),
                        pre_existing: accepted.pre_existing,
                    }
                }
                Err(rejected) => WorkbenchEvent::EditOutcome {
                    committed: false,
                    introduced: rejection_problems(&rejected),
                    pre_existing: rejected.pre_existing,
                },
            },
            Some(doc) => check_edit(ctx, &doc, &edit),
        };
        let _ = ctx.tx.send(msg);
    }

    pub fn save_plan(&self) -> Result<String, String> {
        let result = save_draft(&self.draft, self.plans_dir.as_deref());
        let _ = self.tx.send(WorkbenchEvent::Saved(result.clone()));
        result
    }

    pub async fn save_network(&self) -> Result<String, String> {
        self.save_network_to(None, Vec::new()).await
    }

    pub async fn save_network_to(
        &self,
        scope: Option<&str>,
        skip: Vec<String>,
    ) -> Result<String, String> {
        let ctx = self;
        let saved = super::artifact::save_network(
            &ctx.draft,
            &ctx.pipeline,
            &ctx.tx,
            super::artifact::SaveRequest {
                plans_dir: ctx.plans_dir.as_deref(),
                root: Some(&ctx.root),
                scope: super::artifact::Scope::parse(scope),
                skip,
                untested: true,
                confirm: false,
            },
        )
        .await
        .map(|saved| saved.saved);
        let status = match &saved {
            Ok(message) => message.clone(),
            Err(error) => format!("not saved: {error}"),
        };
        let _ = ctx.tx.send(WorkbenchEvent::Status(status));
        saved
    }

    pub fn discard_network(&self) -> Option<String> {
        let ctx = self;
        let discarded = super::artifact::discard_network(&ctx.draft, &ctx.tx);
        let status = discarded
            .clone()
            .unwrap_or_else(|| "nothing to discard".to_string());
        let _ = ctx.tx.send(WorkbenchEvent::Status(status));
        discarded
    }

    pub fn restore_draft(&self) {
        let ctx = self;
        let restored = ctx.draft.lock().unwrap().restore();
        match restored {
            Some((doc, dirty)) => {
                let _ = ctx.tx.send(WorkbenchEvent::DraftReplaced {
                    doc: Box::new(doc),
                    dirty,
                });
            }
            None => {
                let _ = ctx.tx.send(WorkbenchEvent::Status(
                    "nothing to restore — the draft has not been replaced yet".to_string(),
                ));
            }
        }
    }
}

fn rejection_problems(rejected: &authoring::EditRejected) -> Vec<String> {
    if !rejected.introduced.is_empty() {
        return rejected.introduced.clone();
    }
    rejected
        .body
        .get("error")
        .and_then(serde_json::Value::as_str)
        .map(|error| vec![error.to_string()])
        .unwrap_or_default()
}

fn check_edit(
    ctx: &Workbench,
    doc: &PlanDoc,
    edit: impl Fn(&mut PlanDoc) -> Result<Value, Value>,
) -> WorkbenchEvent {
    let before = super::tools::plan_problems(&ctx.pipeline, doc);
    let mut edited = doc.clone();
    if let Err(body) = edit(&mut edited) {
        return WorkbenchEvent::EditOutcome {
            committed: false,
            introduced: body
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(|error| vec![error.to_string()])
                .unwrap_or_default(),
            pre_existing: before,
        };
    }
    let after = super::tools::plan_problems(&ctx.pipeline, &edited);
    let blocking = authoring::static_problems(&edited);
    let (pre_existing, introduced): (Vec<String>, Vec<String>) = after
        .into_iter()
        .partition(|problem| before.contains(problem));
    let introduced = introduced
        .into_iter()
        .map(|problem| {
            if blocking.contains(&problem) {
                problem
            } else {
                format!("non-blocking (submit still allowed): {problem}")
            }
        })
        .collect();
    WorkbenchEvent::EditOutcome {
        committed: false,
        introduced,
        pre_existing,
    }
}

/// MaxIterations is real progress — the history ends cleanly in tool
/// results, so the next message can build on it. Every other turn error
/// leaves the exchange broken mid-turn and must be rolled back.
fn keep_partial_history(error: &AgentError) -> bool {
    matches!(error, AgentError::MaxIterations(_))
}

fn turn_failure_message(error: AgentError) -> String {
    match error {
        AgentError::MaxIterations(n) => format!(
            "stopped after {n} tool iterations with no successful edit — \
             progress is kept, send \"continue\" to resume (or raise \
             max_agent_iterations in config)"
        ),
        other => other.to_string(),
    }
}

/// Write the draft to disk: back to its source file, or into the plans
/// directory for new drafts. Shared by Ctrl+S and `workbench__save_artifact`.
pub fn save_draft(
    draft: &std::sync::Mutex<DraftState>,
    plans_dir: Option<&std::path::Path>,
) -> Result<String, String> {
    let mut guard = draft.lock().unwrap();
    let Some(doc) = guard.doc.as_mut() else {
        return Err("no draft".to_string());
    };
    // Where it lands, and the guards against clobbering another plan, are
    // shared with `dive plan` via authoring::target_path / write_doc. The
    // draft's on-disk identity and dirty flag are this surface's own
    // bookkeeping, so they stay here.
    let path = authoring::target_path(doc, plans_dir).map_err(|e| e.to_string())?;
    authoring::write_doc(doc, &path, false).map_err(|e| e.to_string())?;
    doc.path = Some(path.clone());
    guard.dirty = false;
    Ok(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_iterations_keeps_history_and_says_how_to_continue() {
        assert!(keep_partial_history(&AgentError::MaxIterations(15)));
        assert!(!keep_partial_history(&AgentError::IncompleteStream));

        let message = turn_failure_message(AgentError::MaxIterations(15));
        assert!(message.contains("15"), "{message}");
        assert!(message.contains("continue"), "{message}");
        assert!(message.contains("max_agent_iterations"), "{message}");

        let other = turn_failure_message(AgentError::IncompleteStream);
        assert!(other.contains("stream ended"), "{other}");
    }
}
