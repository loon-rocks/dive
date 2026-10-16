use super::network::NetworkView;
use super::runner::UiDecision;
use graph_core::pipeline::doc::PlanDoc;
use graph_core::{ToolDef, ToolShape};
use serde_json::{Map, Value};
use tokio::sync::oneshot;

/// Why the debugger paused.
#[derive(Debug, Clone)]
pub enum GateKind {
    /// Before a tool call: step, continue, skip, or abort.
    BeforeCall,
    /// The call failed (break-on-exception): replace, let it fail, or abort.
    OnError { error: Value },
    /// An `ask` step is putting a question to the user. Not a debugger
    /// pause: the run is waiting on an answer it declared it needs, so the
    /// only two replies are an answer (the inject editor) and a decline.
    Ask {
        schema: Value,
        default: Option<Value>,
    },
}

pub enum WorkbenchEvent {
    // Chat / agent turn
    AgentDelta(String),
    AgentToolStarted(String),
    AgentToolFinished {
        name: String,
        is_error: bool,
    },
    /// Ok carries the turn's final assistant text — the completed
    /// response is canonical; deltas are just its live preview.
    TurnFinished(Result<String, String>),
    ActiveAgent {
        name: String,
        note: Option<String>,
    },
    AgentNotice(String),
    // Draft changes published by the workbench tools; `dirty` is false
    // when the draft came straight from disk (load), true for edits.
    DraftReplaced {
        doc: Box<PlanDoc>,
        dirty: bool,
    },
    // Plan run
    /// A run was launched by the agent's `workbench__try_artifact` tool (the
    /// keyboard path resets the pane directly). `breakpoints` is Some when
    /// the agent replaced the set.
    RunStarted {
        gated: bool,
        breakpoints: Option<Vec<String>>,
    },
    Planning,
    Synthesizing,
    /// End-of-run token/cost summary, already formatted.
    RunUsage(String),
    StepStarted {
        path: String,
        tool: String,
        input: Value,
        /// True when the event belongs to the plan being run (empty call
        /// stack) — body sub-steps included; nested plans excluded.
        in_plan: bool,
    },
    StepFinished {
        path: String,
        result: Value,
        is_error: bool,
        in_plan: bool,
    },
    SolverDelta(String),
    GateAsk {
        kind: GateKind,
        path: String,
        tool: String,
        input: Value,
        call_stack: Vec<String>,
        scope: Map<String, Value>,
        reply: oneshot::Sender<UiDecision>,
    },
    RunFinished {
        headline: String,
        is_error: bool,
        /// A fired `exit` ended the run early — never-run steps and the
        /// finish were skipped, not completed.
        exited: bool,
        results: Map<String, Value>,
    },
    // Effect completions
    Validated(Vec<String>),
    ContextLoaded {
        tools: Vec<ToolDef>,
        shapes: Vec<ToolShape>,
    },
    Saved(Result<String, String>),
    NetworkChanged(Box<NetworkView>),
    /// A one-line status from the effect executor (e.g. an undo miss).
    Status(String),
    EditOutcome {
        committed: bool,
        introduced: Vec<String>,
        pre_existing: Vec<String>,
    },
}

impl WorkbenchEvent {
    pub fn log_line(&self) -> (tracing::Level, String) {
        use tracing::Level;
        let line = match self {
            WorkbenchEvent::AgentDelta(text) => {
                return (Level::TRACE, format!("agent delta ({} chars)", text.len()))
            }
            WorkbenchEvent::SolverDelta(text) => {
                return (Level::TRACE, format!("solver delta ({} chars)", text.len()))
            }
            WorkbenchEvent::AgentToolStarted(name) => format!("agent tool started: {name}"),
            WorkbenchEvent::AgentToolFinished { name, is_error } => {
                format!(
                    "agent tool finished: {name}{}",
                    if *is_error { " (error)" } else { "" }
                )
            }
            WorkbenchEvent::TurnFinished(result) => match result {
                Ok(text) => format!("agent turn finished ({} chars)", text.len()),
                Err(error) => format!("agent turn failed: {error}"),
            },
            WorkbenchEvent::ActiveAgent { name, .. } => format!("active agent: {name}"),
            WorkbenchEvent::AgentNotice(text) => format!("agent notice: {text}"),
            WorkbenchEvent::DraftReplaced { doc, dirty } => format!(
                "draft replaced: '{}' ({} steps, dirty={dirty})",
                doc.identifier,
                doc.steps.len()
            ),
            WorkbenchEvent::RunStarted { gated, breakpoints } => {
                format!("agent run started (gated={gated}, breakpoints={breakpoints:?})")
            }
            WorkbenchEvent::Planning => "planning…".to_string(),
            WorkbenchEvent::Synthesizing => "synthesizing…".to_string(),
            WorkbenchEvent::RunUsage(summary) => format!("usage: {summary}"),
            WorkbenchEvent::StepStarted { path, tool, .. } => {
                format!("step started: {path} {tool}")
            }
            WorkbenchEvent::StepFinished {
                path,
                result,
                is_error,
                ..
            } => format!(
                "step finished: {path}{} ({} result bytes)",
                if *is_error { " (error)" } else { "" },
                result.to_string().len()
            ),
            WorkbenchEvent::GateAsk {
                kind,
                path,
                tool,
                call_stack,
                ..
            } => format!(
                "gate ask: {} at {path} ({tool}), call-stack depth {}",
                match kind {
                    GateKind::BeforeCall => "before-call",
                    GateKind::OnError { .. } => "on-error",
                    GateKind::Ask { .. } => "ask",
                },
                call_stack.len()
            ),
            WorkbenchEvent::RunFinished {
                headline, is_error, ..
            } => format!("run finished (is_error={is_error}): {headline}"),
            WorkbenchEvent::Validated(problems) => {
                format!("validated: {} problem(s)", problems.len())
            }
            WorkbenchEvent::ContextLoaded { tools, shapes } => format!(
                "context loaded: {} tools, {} shapes",
                tools.len(),
                shapes.len()
            ),
            WorkbenchEvent::Saved(result) => match result {
                Ok(path) => format!("saved to {path}"),
                Err(error) => format!("save failed: {error}"),
            },
            WorkbenchEvent::Status(text) => format!("status: {text}"),
            WorkbenchEvent::NetworkChanged(view) => format!(
                "network: {} nodes, {} drafted",
                view.nodes.len(),
                view.unsaved()
            ),
            WorkbenchEvent::EditOutcome {
                committed,
                introduced,
                pre_existing,
            } => format!(
                "edit outcome: committed={committed}, {} introduced, {} pre-existing",
                introduced.len(),
                pre_existing.len()
            ),
        };
        (Level::DEBUG, line)
    }
}
