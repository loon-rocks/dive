//! Workbench-local agent tools: the chat agent builds and edits the draft
//! plan through these, so the plan pane is always the agent's source of
//! truth. Registered under `workbench__` alongside the normal catalog.

use super::event::WorkbenchEvent;
use super::runner::{DebugControls, UiGate};
use async_trait::async_trait;
use dive_core::pipeline::authoring;
use dive_core::pipeline::doc::{apply_schema_defaults, load_plan_doc, validate_input, PlanDoc};
use dive_core::pipeline::Pipeline;
use dive_core::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

pub const DESCRIBE_TOOL: &str = "workbench__describe_tool";
pub const UPDATE_METADATA: &str = "workbench__update_metadata";
pub const ADD_STEP: &str = "workbench__add_step";
pub const UPDATE_STEP: &str = "workbench__update_step";
pub const DELETE_STEP: &str = "workbench__delete_step";
pub const RESTORE_DRAFT: &str = "workbench__restore_draft";

/// The mutating edit tools — a successful call to one is genuine forward
/// progress on the draft. The agent loop resets its iteration budget on these
/// (see `Agent::progress_tools`) so a long fix-forward session (edit, validate,
/// run, repeat) isn't starved mid-repair.
pub fn progress_tools() -> Vec<String> {
    [
        super::artifact::SET_ARTIFACT,
        super::artifact::SET_SKETCH,
        UPDATE_METADATA,
        ADD_STEP,
        UPDATE_STEP,
        DELETE_STEP,
        RESTORE_DRAFT,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Appended to every workbench__ tool description surfaced to the chat
/// agent: these are agent-side tools, not runtime tools, so a plan step
/// referencing one only fails at run time. Descriptions are routing
/// signals — this keeps them out of drafted steps.
pub(crate) const WORKBENCH_ONLY_NOTE: &str =
    " Workbench-only: not available to plan steps at runtime.";

/// The shared draft: the doc, its unsaved-changes flag, and a one-deep undo
/// snapshot. One mutex holds all three so a tool can check `dirty`
/// atomically with replacing the doc — the agent runs a batch's tool calls
/// concurrently, so a check across two locks would race.
pub struct DraftState {
    pub doc: Option<PlanDoc>,
    pub dirty: bool,
    /// The (doc, dirty) displaced by the last replacement. `restore` swaps
    /// it with the current draft, so calling it twice is redo.
    pub undo: Option<(PlanDoc, bool)>,
    pub composed: Option<Value>,
    pub network: super::network::Network,
}

impl DraftState {
    pub fn new(doc: Option<PlanDoc>) -> Self {
        Self {
            doc,
            dirty: false,
            undo: None,
            composed: None,
            network: super::network::Network::default(),
        }
    }

    /// Swap the current draft with the undo snapshot; returns the restored
    /// (doc, dirty), or None when nothing has been replaced yet.
    pub fn restore(&mut self) -> Option<(PlanDoc, bool)> {
        let (doc, dirty) = self.undo.take()?;
        self.undo = self.doc.take().map(|old| (old, self.dirty));
        self.doc = Some(doc.clone());
        self.dirty = dirty;
        Some((doc, dirty))
    }
}

pub type SharedDraft = Arc<Mutex<DraftState>>;

pub fn publish_draft(
    draft: &Mutex<DraftState>,
    tx: &UnboundedSender<WorkbenchEvent>,
    doc: PlanDoc,
    dirty: bool,
) {
    {
        let mut state = draft.lock().unwrap();
        state.undo = state.doc.take().map(|old| (old, state.dirty));
        state.doc = Some(doc.clone());
        state.dirty = dirty;
    }
    let _ = tx.send(WorkbenchEvent::DraftReplaced {
        doc: Box::new(doc),
        dirty,
    });
}

pub struct WorkbenchTools {
    draft: SharedDraft,
    pipeline: Arc<Pipeline>,
    plans_dir: Option<PathBuf>,
    debug: Arc<DebugControls>,
    tx: UnboundedSender<WorkbenchEvent>,
    tester: super::artifact::Tester,
    root: Option<PathBuf>,
    loaded: Option<Arc<std::sync::atomic::AtomicBool>>,
}

pub const NO_DRAFT: &str = "this thread has no network draft; call builtin__create_network to start one, or builtin__open_network to open an existing agent, plan or tool";

fn needs_draft(name: &str) -> bool {
    !matches!(
        name,
        DESCRIBE_TOOL | super::artifact::LIST_ARTIFACTS | super::artifact::SHOW_ARTIFACT
    )
}

impl WorkbenchTools {
    pub fn new(
        draft: SharedDraft,
        pipeline: Arc<Pipeline>,
        plans_dir: Option<PathBuf>,
        debug: Arc<DebugControls>,
        tx: UnboundedSender<WorkbenchEvent>,
    ) -> Self {
        Self {
            draft,
            pipeline,
            plans_dir,
            debug,
            tx,
            tester: Arc::new(std::sync::OnceLock::new()),
            root: None,
            loaded: None,
        }
    }

    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root = Some(root);
        self
    }

    pub fn with_loaded(mut self, loaded: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.loaded = Some(loaded);
        self
    }

    pub fn with_tester(mut self, tester: super::artifact::Tester) -> Self {
        self.tester = tester;
        self
    }

    fn current(&self) -> Option<PlanDoc> {
        self.draft.lock().unwrap().doc.clone()
    }

    /// Replace the draft, stashing the displaced doc as the undo snapshot.
    /// Deliberately unguarded: the edit path (draft_plan, the
    /// precise tools) preserves unsaved work by setting dirty=true, and any
    /// bad replacement is one restore away. Only load_plan needs the dirty
    /// guard, and it does its check-and-replace under the same lock.
    fn publish(&self, doc: PlanDoc, dirty: bool) {
        publish_draft(&self.draft, &self.tx, doc, dirty);
    }

    /// Resolve a catalog identifier or YAML file path to a plan document.
    fn resolve_plan(&self, name_or_path: &str) -> Result<PlanDoc, ToolOutcome> {
        if let Some(doc) = self
            .pipeline
            .plans
            .iter()
            .find(|d| d.identifier == name_or_path)
        {
            return Ok(doc.clone());
        }
        let path = std::path::Path::new(name_or_path);
        if !path.exists() {
            let available: Vec<&str> = self
                .pipeline
                .plans
                .iter()
                .map(|d| d.identifier.as_str())
                .collect();
            return Err(ToolOutcome {
                result: json!({
                    "error": format!(
                        "'{name_or_path}' is neither a known plan identifier nor a file"
                    ),
                    "availablePlans": available,
                }),
                is_error: true,
            });
        }
        load_plan_doc(path).map_err(|error| {
            let mut message = format!("failed to load plan: {error}");
            if error.to_string().contains("unknown field") {
                message.push_str(
                    "\nhint: control flow is not a field — it is a step whose \
                     toolName is one of the bare control steps exit, agent, \
                     ask, route, filter, map, or reduce (there is no \
                     gate/assert tool); a \
                     plan ends with one `finish`: `silent`, {output: …} or {solver: …}",
                );
            }
            error_outcome(&message)
        })
    }

    /// Read a plan's YAML without touching the draft — the inspection
    /// counterpart to load_plan, so studying a plan never replaces work.
    fn show_plan(&self, input: &Value) -> ToolOutcome {
        let Some(name_or_path) = input.get("name").and_then(Value::as_str) else {
            return error_outcome(
                "show_artifact requires a 'name': a plan identifier or file path",
            );
        };
        let doc = match self.resolve_plan(name_or_path) {
            Ok(doc) => doc,
            Err(outcome) => return outcome,
        };
        match authoring::to_yaml(&doc) {
            Ok(yaml) => ToolOutcome {
                result: json!({"identifier": doc.identifier, "name": doc.name, "yaml": yaml}),
                is_error: false,
            },
            Err(error) => error_outcome(&error.to_string()),
        }
    }

    /// Load an existing plan into the workbench: an identifier from the
    /// configured plan catalog, or a YAML file path.
    fn load_plan(&self, input: &Value) -> ToolOutcome {
        let Some(name_or_path) = input.get("name").and_then(Value::as_str) else {
            return error_outcome(
                "load_artifact requires a 'name': a plan identifier or file path",
            );
        };
        let doc = match self.resolve_plan(name_or_path) {
            Ok(doc) => doc,
            Err(outcome) => return outcome,
        };
        let problems = plan_problems(&self.pipeline, &doc);
        let summary = json!({
            "identifier": doc.identifier,
            "name": doc.name,
            "steps": doc.steps.len(),
            "validation": if problems.is_empty() { json!("ok") } else { json!(problems) },
        });
        // Check-and-replace under one lock: concurrent same-batch loads
        // each see the true dirty state, so unsaved work is never lost
        // without an explicit overwrite.
        let overwrite = input
            .get("overwrite_draft")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        {
            let mut state = self.draft.lock().unwrap();
            if state.dirty && !overwrite {
                return ToolOutcome {
                    result: json!({
                        "error": "the draft has unsaved changes — save them with \
                                  workbench__save_artifact, or pass overwrite_draft: true \
                                  only after the user confirms discarding them",
                        "dirtyDraft": state.doc.as_ref().map(|d| d.identifier.clone()),
                    }),
                    is_error: true,
                };
            }
            state.undo = state.doc.take().map(|old| (old, state.dirty));
            state.doc = Some(doc.clone());
            state.dirty = false;
        }
        let _ = self.tx.send(WorkbenchEvent::DraftReplaced {
            doc: Box::new(doc),
            dirty: false,
        });
        ToolOutcome {
            result: summary,
            is_error: false,
        }
    }

    /// One-level undo of the last draft replacement; calling it again redoes.
    fn restore_draft(&self) -> ToolOutcome {
        let restored = self.draft.lock().unwrap().restore();
        match restored {
            Some((doc, dirty)) => {
                let result = json!({"restored": doc.identifier, "dirty": dirty});
                let _ = self.tx.send(WorkbenchEvent::DraftReplaced {
                    doc: Box::new(doc),
                    dirty,
                });
                ToolOutcome {
                    result,
                    is_error: false,
                }
            }
            None => error_outcome("nothing to restore — the draft has not been replaced yet"),
        }
    }

    /// The plan catalog, as the workspace context tab sees it.
    fn list_plans(&self) -> ToolOutcome {
        let plans: Vec<Value> = self
            .pipeline
            .plans
            .iter()
            .map(|doc| {
                json!({
                    "identifier": doc.identifier,
                    "name": doc.name,
                    "description": doc.description,
                    "steps": doc.steps.len(),
                })
            })
            .collect();
        ToolOutcome {
            result: json!({"count": plans.len(), "plans": plans}),
            is_error: false,
        }
    }

    /// Run the draft inside this agent turn. Step events stream to the
    /// workspace pane; gated runs pause on the USER's y/s/a decisions.
    async fn run_plan(&self, input: &Value) -> ToolOutcome {
        let Some(doc) = self.current() else {
            return error_outcome("no draft to run — load or draft one first");
        };
        let breakpoints: Vec<String> = input
            .get("breakpoints")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // Breakpoints imply debugging.
        let gated =
            input.get("gated").and_then(Value::as_bool).unwrap_or(false) || !breakpoints.is_empty();
        let unknown_breakpoints: Vec<&String> = breakpoints
            .iter()
            .filter(|id| !doc.steps.iter().any(|s| &s.id == *id))
            .collect();
        let mut run_input = input
            .get("input")
            .cloned()
            .unwrap_or(Value::Object(Map::new()));
        if !run_input.is_object() {
            return error_outcome("'input' must be a JSON object of plan inputs");
        }
        if let Some(schema) = &doc.input_schema {
            apply_schema_defaults(schema, &mut run_input);
            if let Err(problems) = validate_input(&doc, &run_input) {
                return ToolOutcome {
                    result: json!({
                        "error": "invalid or missing plan inputs",
                        "problems": problems,
                        "inputSchema": schema,
                    }),
                    is_error: true,
                };
            }
        }
        let provided = !breakpoints.is_empty();
        if gated {
            if provided {
                self.debug
                    .set_breakpoints(breakpoints.iter().cloned().collect());
            }
            self.debug.arm();
        }
        let _ = self.tx.send(WorkbenchEvent::RunStarted {
            gated,
            breakpoints: provided.then(|| breakpoints.clone()),
        });
        // The interlocutor rides every run, gated or not: an `ask` step's
        // question is the plan asking for input, not a debugger pause.
        let mut pipeline = (*self.pipeline)
            .clone()
            .with_interlocutor(super::runner::interlocutor_for(&self.pipeline, &self.tx));
        if gated {
            pipeline =
                pipeline.with_gate(Arc::new(UiGate::new(self.tx.clone(), self.debug.clone())));
        }
        let query = format!("Run the '{}' plan", doc.name);
        let result = pipeline
            .run_explicit_in(
                &query,
                doc.steps.clone(),
                doc.finish.clone(),
                Some(run_input),
                doc.pack.as_ref(),
            )
            .await;
        let report = super::runner::report(result);
        let is_error = report.is_error;
        let _ = self.tx.send(report.finished_msg());
        let mut summary = report.summary;
        if !unknown_breakpoints.is_empty() {
            summary["unknownBreakpoints"] = json!(unknown_breakpoints);
        }
        ToolOutcome {
            result: summary,
            is_error,
        }
    }

    /// Save the draft to disk and surface the result in the status bar.
    fn save_plan(&self) -> ToolOutcome {
        let result = super::effects::save_draft(&self.draft, self.plans_dir.as_deref());
        let _ = self.tx.send(WorkbenchEvent::Saved(result.clone()));
        match result {
            Ok(path) => ToolOutcome {
                result: json!({"savedTo": path}),
                is_error: false,
            },
            Err(error) => error_outcome(&error),
        }
    }

    async fn plan_into_network(&self, doc: &PlanDoc) -> Option<ToolOutcome> {
        let in_network = self.plans_open_as_nodes()
            || self
                .draft
                .lock()
                .unwrap()
                .network
                .node(super::artifact::ArtifactKind::Plan, &doc.identifier)
                .is_some();
        if !in_network {
            return None;
        }
        let yaml = match authoring::to_yaml(doc) {
            Ok(yaml) => yaml,
            Err(error) => return Some(error_outcome(&error.to_string())),
        };
        Some(
            super::artifact::put(
                &self.draft,
                &self.pipeline,
                &self.tx,
                super::artifact::ArtifactKind::Plan,
                &doc.identifier,
                &yaml,
            )
            .await,
        )
    }

    async fn set_plan(&self, input: &Value) -> ToolOutcome {
        let plan = match input["yaml"]
            .as_str()
            .filter(|yaml| !yaml.trim().is_empty())
        {
            Some(yaml) => match serde_yaml::from_str::<Value>(yaml) {
                Ok(plan) => plan,
                Err(error) => return error_outcome(&format!("the plan isn't valid YAML: {error}")),
            },
            None => match self.draft.lock().unwrap().composed.clone() {
                Some(plan) => plan,
                None => {
                    return error_outcome(
                        "there is no composed plan to publish: call plan__compose_plan first",
                    )
                }
            },
        };
        let doc = match dive_core::pipeline::plan_doc(&plan) {
            Ok(doc) => doc,
            Err(error) => return error_outcome(&format!("not a plan document: {error}")),
        };
        if let Some(outcome) = self.plan_into_network(&doc).await {
            return outcome;
        }
        let overwrite = input
            .get("overwrite_draft")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let problems = plan_problems(&self.pipeline, &doc);
        let summary = json!({
            "identifier": doc.identifier,
            "name": doc.name,
            "steps": doc.steps.len(),
            "validation": if problems.is_empty() { json!("ok") } else { json!(problems) },
        });
        {
            let mut state = self.draft.lock().unwrap();
            if state.dirty && !overwrite {
                return ToolOutcome {
                    result: json!({
                        "error": "the draft has unsaved changes — save them with \
                                  workbench__save_artifact, or pass overwrite_draft: true \
                                  only after the user confirms discarding them",
                        "dirtyDraft": state.doc.as_ref().map(|d| d.identifier.clone()),
                    }),
                    is_error: true,
                };
            }
            state.undo = state.doc.take().map(|old| (old, state.dirty));
            state.doc = Some(doc.clone());
            state.dirty = true;
        }
        let _ = self.tx.send(WorkbenchEvent::DraftReplaced {
            doc: Box::new(doc),
            dirty: true,
        });
        ToolOutcome {
            result: summary,
            is_error: false,
        }
    }

    async fn describe_tool(&self, input: &Value) -> ToolOutcome {
        let Some(name) = input.get("name").and_then(Value::as_str) else {
            return error_outcome("describe_tool requires a 'name' string");
        };
        let network = self.draft.lock().unwrap().network.clone();
        let pipeline = if network.nodes.is_empty() {
            (*self.pipeline).clone()
        } else {
            super::network::overlay(&self.pipeline, &network)
        };
        let observed = match &pipeline.store {
            Some(store) => store
                .tool_shapes()
                .await
                .unwrap_or_default()
                .into_iter()
                .find(|shape| shape.tool == name)
                .map(|shape| json!({"schema": shape.schema, "example": shape.example})),
            None => None,
        };
        if let Some(identifier) = pipeline
            .packs
            .plan_target(name)
            .filter(|_| pipeline.packs.shows(&pipeline.origin, name))
        {
            if let Some(doc) = pipeline.plans.iter().find(|d| d.identifier == identifier) {
                return ToolOutcome {
                    result: json!({
                        "name": name,
                        "description": doc.tool_description(),
                        "inputSchema": doc.tool_input_schema(),
                        "outputSchema": doc.result_schema(),
                        "observedOutput": observed,
                        "readOnly": null,
                    }),
                    is_error: false,
                };
            }
        }
        let mut defs = pipeline.planner_tool_defs().await;
        let natives: Vec<_> = dive_core::pipeline::native_tool_defs()
            .into_iter()
            .filter(|native| defs.iter().all(|def| def.name != native.name))
            .collect();
        defs.extend(natives);
        let Some(def) = defs.iter().find(|def| def.name == name).cloned() else {
            let servers = pipeline.registry.servers().await;
            return error_outcome(&unknown_tool_message(name, &defs, &servers));
        };
        ToolOutcome {
            result: json!({
                "name": def.name,
                "description": def.description,
                "inputSchema": def.input_schema,
                "outputSchema": def.output_schema,
                "outputExample": def.output_example,
                "observedOutput": observed,
                "readOnly": def.read_only,
            }),
            is_error: false,
        }
    }

    /// The choke point for the precise editing tools. The edit rules
    /// (reject only what introduces NEW problems) live in
    /// [`authoring::apply_edit`]; this wrapper adds the workbench's own
    /// concerns — there must *be* a draft, and an accepted edit publishes
    /// to the UI and marks the draft dirty.
    fn edit_draft(&self, mutate: impl FnOnce(&mut PlanDoc) -> Result<Value, Value>) -> ToolOutcome {
        let Some(doc) = self.current() else {
            return error_outcome("no draft — load or draft one first");
        };
        match authoring::apply_edit(&doc, mutate) {
            Ok(accepted) => {
                let summary = authoring::summarize_edit(&accepted);
                self.publish(accepted.doc, true);
                ToolOutcome {
                    result: summary,
                    is_error: false,
                }
            }
            Err(rejected) => ToolOutcome {
                result: rejected.body,
                is_error: true,
            },
        }
    }

    /// Patch the draft's plan-level fields: identifier, name, description,
    /// exemplars, and/or the finish type (solver ⇄ output).
    fn update_metadata(&self, input: &Value) -> ToolOutcome {
        self.edit_draft(|doc| authoring::patch_metadata(doc, input))
    }

    /// Insert a step: appended, or anchored before/after an existing id.
    fn add_step(&self, input: &Value) -> ToolOutcome {
        self.edit_draft(|doc| authoring::patch_add_step(doc, input))
    }

    /// Patch one step's fields; `newId` renames it and rewrites downstream
    /// `{{id.*}}` references so templates keep working.
    fn update_step(&self, input: &Value) -> ToolOutcome {
        self.edit_draft(|doc| authoring::patch_update_step(doc, input))
    }

    /// Remove a step. Validation rejects the edit if later steps still
    /// reference it — the problems say which templates dangle.
    fn delete_step(&self, input: &Value) -> ToolOutcome {
        self.edit_draft(|doc| authoring::patch_delete_step(doc, input))
    }

    async fn artifact_tool(&self, name: &str, target: Target, input: &Value) -> ToolOutcome {
        use super::artifact as file;
        match (name, target) {
            (file::SET_ARTIFACT, Target::Plan) => self.set_plan(input).await,
            (file::SET_ARTIFACT, Target::File(kind)) => {
                file::set_artifact(&self.draft, &self.pipeline, &self.tx, kind, input).await
            }
            (file::LOAD_ARTIFACT, Target::Plan) if !self.plans_open_as_nodes() => {
                self.load_plan(input)
            }
            (file::LOAD_ARTIFACT, Target::Plan) => {
                file::load_node(
                    &self.draft,
                    &self.pipeline,
                    &self.tx,
                    file::ArtifactKind::Plan,
                    input,
                )
                .await
            }
            (file::LOAD_ARTIFACT, Target::File(kind)) => {
                file::load_node(&self.draft, &self.pipeline, &self.tx, kind, input).await
            }
            (file::TRY_ARTIFACT, Target::Plan) => self.run_plan(input).await,
            (file::TRY_ARTIFACT, Target::File(file::ArtifactKind::Tool)) => {
                file::try_artifact(&self.draft, &self.pipeline, &self.tx, input).await
            }
            (file::TRY_ARTIFACT, Target::File(_)) => error_outcome(
                "an agent draft is tried with workbench__test_network, once every node of the network is drafted",
            ),
            (file::SAVE_ARTIFACT, Target::Plan) => self.save_plan(),
            (file::SAVE_ARTIFACT, Target::File(_)) => error_outcome(
                "agent and tool drafts are saved together with workbench__save_network",
            ),
            (file::DISCARD_ARTIFACT, Target::Plan) => {
                let named = input["name"].as_str();
                match file::discard_artifact(&self.draft, &self.tx, file::ArtifactKind::Plan, named) {
                    Ok(message) => ToolOutcome {
                        result: json!({ "discarded": message }),
                        is_error: false,
                    },
                    Err(_) => error_outcome(
                        "the plan pane's plan can't be discarded; it stays until another plan replaces it",
                    ),
                }
            }
            (file::DISCARD_ARTIFACT, Target::File(kind)) => {
                match file::discard_artifact(&self.draft, &self.tx, kind, input["name"].as_str()) {
                    Ok(message) => ToolOutcome {
                        result: json!({ "discarded": message }),
                        is_error: false,
                    },
                    Err(error) => error_outcome(&error),
                }
            }
            (file::LIST_ARTIFACTS, Target::Plan) => self.list_plans(),
            (file::LIST_ARTIFACTS, Target::File(file::ArtifactKind::Tool)) => {
                file::list_tools(&self.pipeline).await
            }
            (file::LIST_ARTIFACTS, Target::File(_)) => file::list_agents(&self.pipeline),
            (file::SHOW_ARTIFACT, Target::Plan) => self.show_plan(input),
            (file::SHOW_ARTIFACT, Target::File(kind)) => {
                let name = input["name"].as_str().unwrap_or_default();
                match file::existing(kind, name, &self.pipeline) {
                    Ok(found) => ToolOutcome {
                        result: json!({"name": found.name, "yaml": found.yaml}),
                        is_error: false,
                    },
                    Err(error) => error_outcome(&error),
                }
            }
            (other, _) => error_outcome(&format!("unknown artifact tool '{other}'")),
        }
    }

    fn plans_open_as_nodes(&self) -> bool {
        let network = &self.draft.lock().unwrap().network;
        network.restate.is_some() || network.is_edit()
    }

    async fn sketch_catalog(&self, sketch: &Value) -> Vec<(String, String)> {
        let text = sketch.to_string();
        let words: std::collections::BTreeSet<&str> = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| word.len() > 3)
            .collect();
        let defs = self.pipeline.planner_tool_defs().await;
        let mut found = Vec::new();
        for def in defs {
            let short = def.name.rsplit("__").next().unwrap_or(&def.name);
            if !(words.contains(def.name.as_str()) || words.contains(short)) {
                continue;
            }
            if def.name.starts_with("workbench__") || found.len() >= 12 {
                continue;
            }
            let output = def
                .output_schema
                .as_ref()
                .map(|schema| format!("; returns {schema}"))
                .unwrap_or_default();
            found.push((
                def.name.clone(),
                format!("takes {}{output}", def.input_schema),
            ));
        }
        found
    }

    async fn network_tool(&self, name: &str, input: &Value) -> ToolOutcome {
        use super::artifact as file;
        match name {
            file::SET_SKETCH => {
                let outcome = file::set_sketch(&self.draft, &self.tx, input);
                if !outcome.is_error {
                    let catalog = self.sketch_catalog(input).await;
                    self.draft.lock().unwrap().network.catalog = catalog;
                }
                outcome
            }
            file::TEST_NETWORK => {
                file::test_network(&self.draft, &self.pipeline, &self.tester, &self.tx, input).await
            }
            file::FAIL_TEST => file::fail_test(&self.draft, &self.tx, input),
            file::SAVE_NETWORK => match file::save_network(
                &self.draft,
                &self.pipeline,
                &self.tx,
                file::SaveRequest {
                    plans_dir: self.plans_dir.as_deref(),
                    root: self.root.as_deref(),
                    scope: file::Scope::parse(input["scope"].as_str()),
                    skip: input["skip"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                    untested: input["untested"].as_bool().unwrap_or(false),
                    confirm: true,
                },
            )
            .await
            {
                Ok(saved) => ToolOutcome {
                    result: serde_json::to_value(saved).unwrap_or_default(),
                    is_error: false,
                },
                Err(error) => error_outcome(&error),
            },
            file::SET_INTERVIEW => file::set_interview(&self.draft, &self.tx, input),
            file::PROPOSE_DESIGNS => file::propose_designs(&self.draft, &self.tx, input),
            file::DISCARD_NETWORK => match file::discard_network(&self.draft, &self.tx) {
                Some(message) => ToolOutcome {
                    result: json!({ "discarded": message }),
                    is_error: false,
                },
                None => error_outcome("there is no network draft to discard"),
            },
            other => error_outcome(&format!("unknown network tool '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Target {
    Plan,
    File(super::artifact::ArtifactKind),
}

impl Target {
    fn of(input: &Value) -> Result<Self, ToolOutcome> {
        match input["kind"].as_str() {
            Some("plan") => Ok(Self::Plan),
            Some(kind) => super::artifact::ArtifactKind::parse(kind)
                .map(Self::File)
                .ok_or(()),
            None => Err(()),
        }
        .map_err(|()| error_outcome("kind must be \"plan\", \"agent\" or \"tool\""))
    }
}

/// The workbench's full validation verdict for a draft: the document's own
/// problems plus catalog-aware tool resolution against the pipeline's
/// catalog. See [`authoring::plan_problems`] for why the catalog is
/// reporting-only and never gates an edit.
pub(super) fn plan_problems(pipeline: &Pipeline, doc: &PlanDoc) -> Vec<String> {
    authoring::plan_problems(doc, &pipeline.plans, pipeline.live_catalog().as_ref())
}

fn unknown_tool_message(
    name: &str,
    defs: &[dive_core::tools::ToolDef],
    servers: &[dive_core::tools::ToolServer],
) -> String {
    let mut message = format!("no tool named '{name}' in the plan catalog.");
    if let Some((prefix, _)) = name.split_once("__") {
        let known = ["builtin", "user", "plan", "agent"].contains(&prefix)
            || servers.iter().any(|server| server.name == prefix);
        if !known {
            let configured: Vec<&str> = servers.iter().map(|server| server.name.as_str()).collect();
            message.push_str(&format!(
                " No MCP server '{prefix}' is configured (configured: {}), so no {prefix}__ tool exists.",
                if configured.is_empty() { "none".to_string() } else { configured.join(", ") }
            ));
        }
    }
    let words: Vec<&str> = name
        .split(['_', '-'])
        .filter(|word| word.len() > 2)
        .collect();
    let mut close: Vec<(usize, &str)> = defs
        .iter()
        .map(|def| {
            let shared = words
                .iter()
                .filter(|word| {
                    def.name.contains(*word) || def.description.to_lowercase().contains(*word)
                })
                .count();
            (shared, def.name.as_str())
        })
        .filter(|(shared, _)| *shared > 0)
        .collect();
    close.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let close: Vec<&str> = close.into_iter().take(8).map(|(_, name)| name).collect();
    if close.is_empty() {
        message.push_str(" Nothing in the catalog resembles it; do not guess further names.");
    } else {
        message.push_str(&format!(
            " Closest tools that exist: {}. If none fits, the capability is missing from the catalog; tell the user instead of guessing further names.",
            close.join(", ")
        ));
    }
    message
}

fn error_outcome(message: &str) -> ToolOutcome {
    ToolOutcome {
        result: json!({"error": message}),
        is_error: true,
    }
}

pub fn tool_defs() -> Vec<ToolDef> {
    let mut defs = vec![
        ToolDef {
            name: DESCRIBE_TOOL.to_string(),
            description: "Describe one tool from the plan catalog, or one of Dive's native \
                          builtin__ tools, without calling it: its description, input schema, declared output schema, the output \
                          shape observed in earlier runs, and whether it's read-only. Use it \
                          to write a step's input and the templates that read its result. \
                          Plans drafted in this thread's network draft can be described too."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {"type": "string", "description": "The tool's full name, e.g. linear__list_issues or plan__project_status"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: UPDATE_METADATA.to_string(),
            description: "Update the draft's plan-level fields: identifier, name, \
                          description, exemplars, input_schema, output_schema, \
                          requires_servers, and/or the finish. `input_schema` declares \
                          the plan's inputs; `requires_servers` lists the MCP servers it \
                          needs. `finish` sets how the plan produces its result — \
                          {output: {<template map>}} for a structured templated result, \
                          {solver: {queryToAnswer, systemPrompt?}} for LLM synthesis of \
                          the step results, or \"silent\" for a side-effect plan. \
                          `output_schema` types an output plan's result; every success \
                          exit's `output` must have the same keys as `finish.output`. \
                          Changing the identifier makes it a new plan."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "identifier": {"type": "string", "description": "Tool-name-safe identifier (letters, digits, _, -). Changing it detaches the draft from its loaded file."},
                    "name": {"type": "string", "description": "Human-readable display name."},
                    "description": {"type": "string", "description": "What the plan does — shown in the catalog and used for routing."},
                    "exemplars": {"type": "array", "items": {"type": "string"}, "description": "Example requests the plan should handle; replaces the current list."},
                    "input_schema": {"type": "object", "description": "Replace the plan's input schema (a JSON Schema object describing the plan's inputs, e.g. {\"type\":\"object\",\"required\":[\"pr\"],\"properties\":{\"pr\":{\"type\":\"integer\"}}}). Pass null to clear it."},
                    "requires_servers": {"type": "array", "items": {"type": "string"}, "description": "Replace the list of MCP server names the plan requires to be configured (gates catalog visibility). Empty array clears it."},
                    "output_schema": {"type": "object", "description": "Replace the JSON Schema of an output plan's result; its properties are the `finish.output` keys. Pass null to clear it."},
                    "finish": {
                        "type": ["object", "string"],
                        "description": "Set the plan's finish: \"silent\", EITHER {output: {a map of key -> template string}} for a structured result, OR {solver: {queryToAnswer: string, systemPrompt?: string}} for LLM synthesis.",
                        "properties": {
                            "solver": {"type": "object", "properties": {"queryToAnswer": {"type": "string"}, "systemPrompt": {"type": "string"}}},
                            "output": {"type": "object", "description": "Map of output key to template string, e.g. {\"summary\": \"{{E3.text}}\"}."}
                        }
                    }
                }
            }),
            output_schema: None,
            output_example: Some(
                json!({"ok": true, "identifier": "sprint_report", "name": "Sprint report"}),
            ),
            read_only: None,
        },
        ToolDef {
            name: ADD_STEP.to_string(),
            description: "Insert one top-level step into the draft — appended, or \
                          anchored with `before`/`after` an existing step id. The \
                          edit is rejected (draft unchanged) only if it introduces \
                          NEW validation problems, e.g. a duplicate id or a \
                          reference to a later step; pre-existing problems never \
                          block it and are reported in the result. Steps inside \
                          route/map/reduce bodies live in the control step's \
                          input — use update_step on that step instead."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["step"],
                "properties": {
                    "step": {
                        "type": "object",
                        "required": ["id", "toolName", "input"],
                        "properties": {
                            "id": {"type": "string", "description": "Unique step id templates reference: E-style (E4) or descriptive (fetch_issues)."},
                            "toolName": {"type": "string", "description": "Exact tool name from the catalog."},
                            "input": {"type": "object", "description": "Tool input; string values may reference earlier steps with {{id.path}} templates."},
                            "reasoning": {"type": "string", "description": "Why this step exists and what it should produce."}
                        }
                    },
                    "before": {"type": "string", "description": "Insert before this existing step id."},
                    "after": {"type": "string", "description": "Insert after this existing step id. Omit both anchors to append."}
                }
            }),
            output_schema: None,
            output_example: Some(json!({"ok": true, "id": "E4", "index": 2, "steps": 5})),
            read_only: None,
        },
        ToolDef {
            name: UPDATE_STEP.to_string(),
            description: "Update fields of one top-level step: toolName, input \
                          (replaced whole, not merged), reasoning (empty string \
                          clears it), and/or newId to rename the step — renaming \
                          rewrites {{id.*}} references in later steps, the solver, \
                          and the output so templates keep working. Rejected only \
                          when the edit introduces NEW validation problems; \
                          pre-existing ones are reported, not blocking. For steps \
                          inside route/map/reduce bodies, update the owning \
                          control step's input."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": {"type": "string", "description": "The step to update."},
                    "newId": {"type": "string", "description": "Rename the step; downstream template references are rewritten."},
                    "toolName": {"type": "string", "description": "New tool name from the catalog."},
                    "input": {"type": "object", "description": "New tool input — replaces the step's entire input object."},
                    "reasoning": {"type": "string", "description": "New reasoning; an empty string clears it."}
                }
            }),
            output_schema: None,
            output_example: Some(json!({"ok": true, "id": "fetch_issues"})),
            read_only: None,
        },
        ToolDef {
            name: DELETE_STEP.to_string(),
            description: "Delete one top-level step from the draft. Rejected \
                          (draft unchanged) if later steps still reference it — \
                          update those steps first — or if it is the plan's only \
                          step."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": {"type": "string", "description": "The step to delete."}
                }
            }),
            output_schema: None,
            output_example: Some(json!({"ok": true, "id": "E2", "steps": 3})),
            read_only: None,
        },
        ToolDef {
            name: RESTORE_DRAFT.to_string(),
            description: "One-level undo: put the draft back to what it was before \
                          the last replacement (load, draft, or edit) — use it \
                          when you or the user replaced the draft by mistake. \
                          Calling it again redoes."
                .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: None,
            output_example: Some(json!({"restored": "sprint_report", "dirty": true})),
            read_only: None,
        },
    ];
    defs.extend(super::artifact::tool_defs());
    for def in &mut defs {
        def.description.push_str(WORKBENCH_ONLY_NOTE);
    }
    defs
}

#[async_trait]
impl ToolRegistry for WorkbenchTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(tool_defs())
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        match name {
            DESCRIBE_TOOL | UPDATE_METADATA | ADD_STEP | UPDATE_STEP | DELETE_STEP
            | RESTORE_DRAFT => {}
            name if super::artifact::ARTIFACT_TOOLS.contains(&name) => {}
            name if super::artifact::NETWORK_TOOLS.contains(&name) => {}
            // Not ours: stay silent, or the composite registry's fallthrough
            // (the fs tools are also workbench__*) double-logs the call.
            other => return Err(ToolError::Unknown(other.to_string())),
        }
        let unloaded = self
            .loaded
            .as_ref()
            .is_some_and(|loaded| !loaded.load(std::sync::atomic::Ordering::SeqCst));
        if unloaded && needs_draft(name) {
            return Ok(ToolOutcome {
                result: serde_json::json!({ "error": NO_DRAFT }),
                is_error: true,
            });
        }
        tracing::debug!(
            target: "workbench",
            "agent invoked {name}: {}",
            input.to_string()
        );
        let started = std::time::Instant::now();
        let outcome = match name {
            DESCRIBE_TOOL => Ok(self.describe_tool(&input).await),
            UPDATE_METADATA => Ok(self.update_metadata(&input)),
            ADD_STEP => Ok(self.add_step(&input)),
            UPDATE_STEP => Ok(self.update_step(&input)),
            DELETE_STEP => Ok(self.delete_step(&input)),
            RESTORE_DRAFT => Ok(self.restore_draft()),
            network_tool if super::artifact::NETWORK_TOOLS.contains(&network_tool) => {
                Ok(self.network_tool(network_tool, &input).await)
            }
            artifact_tool => Ok(match Target::of(&input) {
                Ok(target) => self.artifact_tool(artifact_tool, target, &input).await,
                Err(outcome) => outcome,
            }),
        };
        if let Ok(outcome) = &outcome {
            tracing::debug!(
                target: "workbench",
                "{name} finished in {:.1}s (is_error={}): {}",
                started.elapsed().as_secs_f64(),
                outcome.is_error,
                outcome.result.to_string()
            );
        }
        outcome
    }
}

pub struct ComposedCapture {
    inner: Arc<dyn ToolRegistry>,
    draft: SharedDraft,
}

impl ComposedCapture {
    pub fn new(inner: Arc<dyn ToolRegistry>, draft: SharedDraft) -> Self {
        Self { inner, draft }
    }
}

#[async_trait]
impl ToolRegistry for ComposedCapture {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        self.inner.tools().await
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let outcome = self.inner.invoke(name, input).await?;
        if name == COMPOSE_PLAN_TOOL && !outcome.is_error {
            let plan = outcome.result.get("plan").filter(|plan| plan.is_object());
            self.draft.lock().unwrap().composed = plan.cloned();
        }
        Ok(outcome)
    }

    async fn servers(&self) -> Vec<dive_core::ToolServer> {
        self.inner.servers().await
    }

    async fn server_identity(
        &self,
        server: &str,
    ) -> Option<Result<dive_core::ServerIdentity, String>> {
        self.inner.server_identity(server).await
    }
}

const COMPOSE_PLAN_TOOL: &str = "plan__compose_plan";

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::workbench::artifact::{
        DISCARD_ARTIFACT, LIST_ARTIFACTS, LOAD_ARTIFACT, SAVE_ARTIFACT, SET_ARTIFACT,
        SHOW_ARTIFACT, TRY_ARTIFACT,
    };

    fn call(tools: &WorkbenchTools, name: &str, input: Value) -> ToolOutcome {
        futures::executor::block_on(tools.invoke(name, input)).unwrap()
    }

    fn report_yaml() -> String {
        serde_yaml::to_string(&report_plan()).unwrap()
    }

    pub(in crate::workbench) fn test_pipeline(plans: Vec<PlanDoc>) -> Arc<Pipeline> {
        Arc::new(Pipeline {
            router: Arc::new(dive_llm::ModelRouter::with_providers(
                Default::default(),
                Default::default(),
            )),
            registry: Arc::new(dive_core::CompositeRegistry::new(vec![])),
            events: Arc::new(dive_core::NullSink),
            plans: Arc::new(plans),
            call_stack: Vec::new(),
            store: None,
            gate: None,
            interlocutor: None,
            catalog: None,
            user_context: String::new(),
            current_date: String::new(),
            max_attempts: 1,
            max_agent_iterations: 15,
            usage: std::sync::Arc::new(dive_core::usage::UsageLedger::unpriced()),
            agents: Arc::new(dive_core::agent::doc::AgentSet::default()),
            agent_depth: 0,
            always_loaded: Default::default(),
            search_pool: None,
            drafted: Default::default(),
            packs: Default::default(),
            origin: Default::default(),
        })
    }

    fn demo_doc() -> PlanDoc {
        str::parse::<dive_core::pipeline::doc::PlanDoc>(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
"#,
        )
        .unwrap()
    }

    #[test]
    fn load_plan_by_identifier_publishes_a_clean_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let draft: SharedDraft = Arc::new(Mutex::new(DraftState::new(None)));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![demo_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );

        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": "demo"}),
        );
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["identifier"], json!("demo"));
        assert_eq!(outcome.result["validation"], json!("ok"));
        assert!(draft.lock().unwrap().doc.is_some());
        match rx.try_recv().unwrap() {
            WorkbenchEvent::DraftReplaced { doc, dirty } => {
                assert_eq!(doc.identifier, "demo");
                assert!(!dirty, "a load is not an unsaved edit");
            }
            _ => panic!("expected DraftReplaced"),
        }
    }

    #[test]
    fn validate_plan_resolves_tools_against_the_runtime_catalog() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut doc = demo_doc();
        doc.steps[0].tool_name = "user__nope".to_string();
        // The catalog is the runtime-loadable one — empty here, so the
        // user tool cannot resolve. workbench__* never appears in it.
        let mut pipeline = (*test_pipeline(vec![])).clone();
        pipeline.catalog = Some(Arc::new(dive_core::pipeline::ToolCatalog::default()));
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(Some(doc)))),
            Arc::new(pipeline),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let doc = tools.current().unwrap();
        let problems = plan_problems(&tools.pipeline, &doc);
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("user__nope")),
            "{problems:?}"
        );
    }

    #[test]
    fn load_plan_unknown_name_lists_available_plans() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(None))),
            test_pipeline(vec![demo_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": "nope"}),
        );
        assert!(outcome.is_error);
        assert_eq!(outcome.result["availablePlans"], json!(["demo"]));
    }

    fn other_doc() -> PlanDoc {
        let mut other = demo_doc();
        other.identifier = "other_plan".to_string();
        other
    }

    #[test]
    fn show_plan_reads_yaml_without_touching_the_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = DraftState::new(Some(demo_doc()));
        state.dirty = true;
        let draft: SharedDraft = Arc::new(Mutex::new(state));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![other_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );

        let outcome = call(
            &tools,
            SHOW_ARTIFACT,
            json!({"kind": "plan", "name": "other_plan"}),
        );
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["identifier"], json!("other_plan"));
        assert!(outcome.result["yaml"]
            .as_str()
            .unwrap()
            .contains("identifier: other_plan"));
        assert!(rx.try_recv().is_err(), "a peek must not publish");
        let state = draft.lock().unwrap();
        assert_eq!(
            state.doc.as_ref().unwrap().identifier,
            "demo",
            "the draft is untouched"
        );
        assert!(state.dirty, "the dirty flag is untouched");

        let unknown = call(
            &tools,
            SHOW_ARTIFACT,
            json!({"kind": "plan", "name": "nope"}),
        );
        assert!(unknown.is_error);
        assert_eq!(unknown.result["availablePlans"], json!(["other_plan"]));
    }

    #[test]
    fn load_plan_refuses_to_replace_a_dirty_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = DraftState::new(Some(demo_doc()));
        state.dirty = true;
        let draft: SharedDraft = Arc::new(Mutex::new(state));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![other_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );

        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": "other_plan"}),
        );
        assert!(outcome.is_error);
        assert!(
            outcome.result["error"]
                .as_str()
                .unwrap()
                .contains("unsaved changes"),
            "{:?}",
            outcome.result
        );
        assert_eq!(outcome.result["dirtyDraft"], json!("demo"));
        assert!(rx.try_recv().is_err(), "a refused load must not publish");
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().identifier,
            "demo",
            "the dirty draft survives"
        );
    }

    #[test]
    fn load_plan_overwrite_flag_replaces_a_dirty_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = DraftState::new(Some(demo_doc()));
        state.dirty = true;
        let draft: SharedDraft = Arc::new(Mutex::new(state));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![other_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );

        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": "other_plan", "overwrite_draft": true}),
        );
        assert!(!outcome.is_error, "{:?}", outcome.result);
        match rx.try_recv().unwrap() {
            WorkbenchEvent::DraftReplaced { doc, dirty } => {
                assert_eq!(doc.identifier, "other_plan");
                assert!(!dirty);
            }
            _ => panic!("expected DraftReplaced"),
        }
        let state = draft.lock().unwrap();
        assert_eq!(state.doc.as_ref().unwrap().identifier, "other_plan");
        let (undone, was_dirty) = state.undo.as_ref().unwrap();
        assert_eq!(
            undone.identifier, "demo",
            "the overwritten draft is one undo away"
        );
        assert!(*was_dirty);
    }

    #[test]
    fn list_plans_enumerates_the_catalog() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(None))),
            test_pipeline(vec![demo_doc()]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = call(&tools, LIST_ARTIFACTS, json!({"kind": "plan"}));
        assert!(!outcome.is_error);
        assert_eq!(outcome.result["count"], json!(1));
        assert_eq!(outcome.result["plans"][0]["identifier"], json!("demo"));
        assert_eq!(outcome.result["plans"][0]["steps"], json!(1));
    }

    #[test]
    fn save_plan_writes_yaml_and_notifies() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(Some(demo_doc())))),
            test_pipeline(vec![]),
            Some(dir.path().to_path_buf()),
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = call(&tools, SAVE_ARTIFACT, json!({"kind": "plan"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert!(dir.path().join("demo.yaml").exists());
        assert!(matches!(
            rx.try_recv().unwrap(),
            WorkbenchEvent::Saved(Ok(_))
        ));
    }

    #[test]
    fn save_refuses_to_overwrite_a_file_holding_another_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.yaml");
        std::fs::write(&path, authoring::to_yaml(&demo_doc()).unwrap()).unwrap();

        // A draft whose identity drifted from the file its path points at
        // (the pre-fix bug state) must not clobber that file on save.
        let mut drifted = demo_doc();
        drifted.identifier = "other_plan".to_string();
        drifted.path = Some(path.clone());
        let draft = Mutex::new(DraftState::new(Some(drifted)));
        let error = super::super::effects::save_draft(&draft, Some(dir.path())).unwrap_err();
        assert!(error.contains("refusing to overwrite"), "{error}");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("identifier: demo"), "file was clobbered");
    }

    #[test]
    fn run_plan_missing_required_input_errors_with_schema() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut doc = demo_doc();
        doc.input_schema = Some(json!({
            "type": "object",
            "required": ["team"],
            "properties": {"team": {"type": "string"}}
        }));
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(Some(doc)))),
            test_pipeline(vec![]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        let outcome = call(&tools, TRY_ARTIFACT, json!({"kind": "plan"}));
        assert!(outcome.is_error);
        assert!(outcome.result["inputSchema"].is_object());
        assert!(rx.try_recv().is_err(), "no run should have started");
    }

    /// Three steps with cross-references plus solver templates — the
    /// fixture for the precise editing tools.
    fn referencing_doc() -> PlanDoc {
        str::parse::<dive_core::pipeline::doc::PlanDoc>(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
  - id: E1
    tool_name: t__fetch
    input: { id: "{{E0.values.0.id}}" }
  - id: E2
    tool_name: t__report
    input: { rows: "{{#E1.values}}x{{/E1.values}} of {{E1.values.length}}" }
solver:
  queryToAnswer: "Summarize {{E1.values.length}} items"
"#,
        )
        .unwrap()
    }

    fn editing_tools(
        doc: Option<PlanDoc>,
    ) -> (
        WorkbenchTools,
        SharedDraft,
        tokio::sync::mpsc::UnboundedReceiver<WorkbenchEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let draft = Arc::new(Mutex::new(DraftState::new(doc)));
        let tools = WorkbenchTools::new(
            draft.clone(),
            test_pipeline(vec![]),
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        (tools, draft, rx)
    }

    fn assert_dirty_publish(rx: &mut tokio::sync::mpsc::UnboundedReceiver<WorkbenchEvent>) {
        match rx.try_recv().unwrap() {
            WorkbenchEvent::DraftReplaced { dirty, .. } => {
                assert!(dirty, "edits are unsaved changes")
            }
            _ => panic!("expected DraftReplaced"),
        }
    }

    #[test]
    fn update_metadata_patches_fields_and_publishes_dirty() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome =
            tools.update_metadata(&json!({"name": "Better name", "description": "clearer"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["name"], json!("Better name"));
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert_eq!(doc.name, "Better name");
        assert_eq!(doc.description, "clearer");
        assert_dirty_publish(&mut rx);
    }

    #[test]
    fn update_metadata_identifier_change_drops_the_loaded_path() {
        let mut loaded = referencing_doc();
        loaded.path = Some(PathBuf::from("/plans/demo.yaml"));
        let (tools, draft, _rx) = editing_tools(Some(loaded));

        // Renaming (display name) keeps the on-disk identity…
        assert!(!tools.update_metadata(&json!({"name": "Renamed"})).is_error);
        assert!(draft.lock().unwrap().doc.as_ref().unwrap().path.is_some());

        // …but a new identifier is a different plan: the path is dropped
        // so the next save cannot overwrite the old file.
        assert!(
            !tools
                .update_metadata(&json!({"identifier": "other_plan"}))
                .is_error
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().path, None);
    }

    #[test]
    fn update_metadata_with_nothing_to_change_errors() {
        let (tools, _draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_metadata(&json!({}));
        assert!(outcome.is_error);
        assert!(rx.try_recv().is_err(), "nothing should have been published");
    }

    #[test]
    fn update_metadata_switches_finish_type_both_directions() {
        // referencing_doc finishes with solver; switch it to output.
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome =
            tools.update_metadata(&json!({"finish": {"output": {"summary": "{{E0.text}}"}}}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert!(doc.solver().is_none(), "solver should be cleared");
        let output = doc.output().cloned().expect("output should be set");
        assert!(output.contains_key("summary"));
        assert_dirty_publish(&mut rx);

        // Reverse: from an output finish back to a solver.
        let outcome =
            tools.update_metadata(&json!({"finish": {"solver": {"queryToAnswer": "answer it"}}}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert!(doc.output().is_none(), "output should be cleared");
        let solver = doc.solver().cloned().expect("solver should be set");
        assert_eq!(solver.query_to_answer, "answer it");
    }

    #[test]
    fn update_metadata_finish_rejects_both_and_neither() {
        let (tools, _draft, _rx) = editing_tools(Some(referencing_doc()));

        let both = tools.update_metadata(&json!({
            "finish": {"solver": {"queryToAnswer": "x"}, "output": {"k": "v"}}
        }));
        assert!(both.is_error);
        assert!(
            both.result["error"].as_str().unwrap().contains("not both"),
            "{:?}",
            both.result
        );

        // A non-empty finish object lacking both keys is malformed.
        let malformed = tools.update_metadata(&json!({"finish": {"bogus": 1}}));
        assert!(malformed.is_error);
        let message = malformed.result["error"].as_str().unwrap();
        assert!(
            message.contains("solver") && message.contains("output"),
            "{message}"
        );
    }

    #[test]
    fn update_metadata_clears_finish_to_silent() {
        // referencing_doc finishes with solver; clear it to a silent plan.
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_metadata(&json!({"finish": {}}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert!(doc.solver().is_none(), "solver should be cleared");
        assert!(doc.output().is_none(), "output should be cleared");
        assert_dirty_publish(&mut rx);

        // null clears too — reset a solver first, then clear via null.
        tools.update_metadata(&json!({"finish": {"solver": {"queryToAnswer": "answer it"}}}));
        let outcome = tools.update_metadata(&json!({"finish": null}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert!(doc.solver().is_none() && doc.output().is_none());
    }

    #[test]
    fn update_metadata_sets_and_clears_input_schema() {
        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        let schema = json!({
            "type": "object",
            "required": ["pr"],
            "properties": {"pr": {"type": "integer"}}
        });
        let outcome = tools.update_metadata(&json!({"input_schema": schema.clone()}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().input_schema,
            Some(schema)
        );

        let outcome = tools.update_metadata(&json!({"input_schema": null}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().input_schema,
            None
        );

        let bad = tools.update_metadata(&json!({"input_schema": "foo"}));
        assert!(bad.is_error);
    }

    #[test]
    fn update_metadata_edits_requires_servers() {
        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_metadata(&json!({"requires_servers": ["linear", "github"]}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().requires_servers,
            vec!["linear".to_string(), "github".to_string()]
        );

        let outcome = tools.update_metadata(&json!({"requires_servers": []}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert!(draft
            .lock()
            .unwrap()
            .doc
            .as_ref()
            .unwrap()
            .requires_servers
            .is_empty());

        let bad = tools.update_metadata(&json!({"requires_servers": "linear"}));
        assert!(bad.is_error);
    }

    #[test]
    fn add_step_appends_by_default_and_anchors_on_request() {
        let step = json!({"id": "E3", "toolName": "t__extra", "input": {"q": "{{E0.values}}"}});

        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.add_step(&json!({ "step": step }));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["index"], json!(3));
        assert_eq!(outcome.result["steps"], json!(4));
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[3].id,
            "E3"
        );
        assert_dirty_publish(&mut rx);

        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.add_step(&json!({"step": step, "after": "E0"}));
        assert_eq!(outcome.result["index"], json!(1), "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[1].id,
            "E3"
        );

        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        // Anchored before E0 the step may not reference E0 anymore.
        let independent = json!({"id": "E3", "toolName": "t__extra", "input": {"q": "fixed"}});
        let outcome = tools.add_step(&json!({"step": independent, "before": "E0"}));
        assert_eq!(outcome.result["index"], json!(0), "{:?}", outcome.result);
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[0].id,
            "E3"
        );
    }

    #[test]
    fn add_step_anchor_and_shape_errors() {
        let (tools, _draft, _rx) = editing_tools(Some(referencing_doc()));
        let step = json!({"id": "E3", "toolName": "t__extra", "input": {}});

        let outcome = tools.add_step(&json!({"step": step, "after": "E9"}));
        assert!(outcome.is_error);
        assert_eq!(outcome.result["availableSteps"], json!(["E0", "E1", "E2"]));

        let outcome = tools.add_step(&json!({"step": step, "before": "E0", "after": "E1"}));
        assert!(outcome.is_error);

        let outcome = tools.add_step(&json!({"step": {"id": "E3"}}));
        assert!(outcome.is_error, "missing toolName/input must not parse");
    }

    #[test]
    fn add_step_rejects_an_invalid_result_and_keeps_the_draft() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        // Inserted at the front, this step references E1 — a forward
        // reference the validator must reject.
        let outcome = tools.add_step(&json!({
            "step": {"id": "E3", "toolName": "t__extra", "input": {"q": "{{E1.values}}"}},
            "before": "E0",
        }));
        assert!(outcome.is_error);
        assert!(
            outcome.result["problemsIntroduced"].is_array(),
            "{:?}",
            outcome.result
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 3);
        assert!(rx.try_recv().is_err(), "rejected edits must not publish");

        let duplicate = json!({"id": "E0", "toolName": "t__extra", "input": {}});
        assert!(tools.add_step(&json!({ "step": duplicate })).is_error);
    }

    #[test]
    fn update_step_patches_fields() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_step(&json!({
            "id": "E0",
            "toolName": "t__better_search",
            "input": {"query": "y", "limit": 5},
            "reasoning": "narrower query",
        }));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert_eq!(doc.steps[0].tool_name, "t__better_search");
        assert_eq!(doc.steps[0].input["limit"], json!(5));
        assert_eq!(doc.steps[0].reasoning.as_deref(), Some("narrower query"));
        assert_dirty_publish(&mut rx);

        // An empty reasoning clears it; an empty patch is an error.
        assert!(
            !tools
                .update_step(&json!({"id": "E0", "reasoning": ""}))
                .is_error
        );
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[0].reasoning,
            None
        );
        assert!(tools.update_step(&json!({"id": "E0"})).is_error);
        assert!(
            tools
                .update_step(&json!({"id": "E9", "toolName": "t__x"}))
                .is_error
        );
    }

    #[test]
    fn update_step_rename_rewrites_downstream_references() {
        let (tools, draft, _rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_step(&json!({"id": "E1", "newId": "issues"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["id"], json!("issues"));
        let doc = draft.lock().unwrap().doc.clone().unwrap();
        assert_eq!(doc.steps[1].id, "issues");
        assert_eq!(
            doc.steps[2].input["rows"],
            json!("{{#issues.values}}x{{/issues.values}} of {{issues.values.length}}")
        );
        assert_eq!(
            doc.solver().cloned().unwrap().query_to_answer,
            "Summarize {{issues.values.length}} items"
        );
    }

    #[test]
    fn update_step_rename_collision_is_rejected() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.update_step(&json!({"id": "E0", "newId": "E1"}));
        assert!(outcome.is_error);
        assert!(
            outcome.result["problemsIntroduced"].is_array(),
            "{:?}",
            outcome.result
        );
        assert_eq!(
            draft.lock().unwrap().doc.as_ref().unwrap().steps[0].id,
            "E0"
        );
        assert!(rx.try_recv().is_err(), "rejected edits must not publish");
    }

    #[test]
    fn delete_step_removes_and_publishes() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.delete_step(&json!({"id": "E2"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["steps"], json!(2));
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 2);
        assert_dirty_publish(&mut rx);
    }

    #[test]
    fn delete_step_rejects_dangling_references_and_empty_plans() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        // E1 still reads {{E0.values.0.id}} — deleting E0 must be refused.
        let outcome = tools.delete_step(&json!({"id": "E0"}));
        assert!(outcome.is_error);
        assert!(
            outcome.result["problemsIntroduced"]
                .to_string()
                .contains("E0"),
            "{:?}",
            outcome.result
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 3);
        assert!(rx.try_recv().is_err(), "rejected edits must not publish");

        // The last remaining step cannot be deleted either.
        let (tools, _draft, _rx) = editing_tools(Some(demo_doc()));
        assert!(tools.delete_step(&json!({"id": "E0"})).is_error);
    }

    /// A draft with a pre-existing validation problem: E10's input has a
    /// template parse error — the state draft_plan handed over in the
    /// 2026-07-15 incident.
    fn invalid_doc() -> PlanDoc {
        str::parse::<dive_core::pipeline::doc::PlanDoc>(
            r#"
identifier: demo
name: Demo
description: demo plan
steps:
  - id: E0
    tool_name: t__search
    input: { query: x }
  - id: E10
    tool_name: t__report
    input: { rows: "{{.}}" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn edits_on_an_already_invalid_plan_are_accepted_when_they_break_nothing() {
        let (tools, draft, mut rx) = editing_tools(Some(invalid_doc()));
        // The new step is valid; the plan stays invalid, but only from the
        // pre-existing E10 problem — the edit must land.
        let outcome = tools.add_step(&json!({
            "step": {"id": "E5", "toolName": "t__extra", "input": {"q": "{{E0.values}}"}},
            "after": "E0",
        }));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        let pre_existing = outcome.result["preExistingProblems"].to_string();
        assert!(pre_existing.contains("E10"), "{:?}", outcome.result);
        assert!(
            outcome.result["note"]
                .as_str()
                .unwrap()
                .contains("not caused by this edit"),
            "{:?}",
            outcome.result
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 3);
        assert_dirty_publish(&mut rx);
    }

    #[test]
    fn edit_introducing_a_new_problem_is_rejected_citing_only_that_problem() {
        let (tools, draft, mut rx) = editing_tools(Some(invalid_doc()));
        // Forward reference to E99: a NEW problem on top of E10's.
        let outcome = tools.add_step(&json!({
            "step": {"id": "E5", "toolName": "t__extra", "input": {"q": "{{E99.values}}"}},
        }));
        assert!(outcome.is_error);
        let introduced = outcome.result["problemsIntroduced"].as_array().unwrap();
        assert_eq!(introduced.len(), 1, "{:?}", outcome.result);
        assert!(introduced[0].as_str().unwrap().contains("E99"));
        let pre_existing = outcome.result["preExistingProblems"].to_string();
        assert!(
            pre_existing.contains("E10") && !introduced[0].as_str().unwrap().contains("E10"),
            "pre-existing problems must be reported separately, not as the cause: {:?}",
            outcome.result
        );
        assert_eq!(draft.lock().unwrap().doc.as_ref().unwrap().steps.len(), 2);
        assert!(rx.try_recv().is_err(), "rejected edits must not publish");
    }

    /// The 2026-07-15 incident replayed: draft_plan handed over a draft
    /// whose E10 had a template parse error. The fix was add_step E9b then
    /// update_step E10 to read from it — both edits must land in order,
    /// without the delete/re-add workaround.
    #[test]
    fn incident_e9b_then_e10_fix_sequence_is_accepted_in_order() {
        let (tools, draft, _rx) = editing_tools(Some(invalid_doc()));

        // 1. Add the new valid step E9b (previously rejected with E10's
        //    pre-existing error).
        let outcome = tools.add_step(&json!({
            "step": {"id": "E9b", "toolName": "t__extra", "input": {"q": "{{E0.values}}"}},
            "after": "E0",
        }));
        assert!(!outcome.is_error, "{:?}", outcome.result);

        // 2. Point E10 at E9b (previously rejected because E9b was never
        //    admitted).
        let outcome = tools.update_step(&json!({
            "id": "E10",
            "input": {"rows": "{{E9b.values}}"},
        }));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert!(
            outcome.result.get("preExistingProblems").is_none(),
            "the plan is now fully valid: {:?}",
            outcome.result
        );

        let doc = draft.lock().unwrap().doc.clone().unwrap();
        let ids: Vec<&str> = doc.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["E0", "E9b", "E10"]);
        assert!(plan_problems(&tools.pipeline, &doc).is_empty());
    }

    #[test]
    fn load_plan_unknown_field_error_includes_a_control_flow_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        let yaml = "identifier: demo\nname: Demo\ndescription: d\nsteps:\n\
                    - id: E0\n  tool_name: t__x\n  input: {}\ngate:\n  condition: true\n";
        std::fs::write(&path, yaml).unwrap();
        let (tools, _draft, _rx) = editing_tools(None);
        let outcome = call(
            &tools,
            LOAD_ARTIFACT,
            json!({"kind": "plan", "name": path.to_str().unwrap() }),
        );
        assert!(outcome.is_error);
        let message = outcome.result["error"].as_str().unwrap();
        assert!(message.contains("unknown field"), "{message}");
        assert!(message.contains("hint:"), "{message}");
        assert!(
            message.contains("exit, agent, ask, route, filter, map"),
            "{message}"
        );
    }

    #[test]
    fn edits_capture_an_undo_snapshot_and_restore_swaps_back() {
        let (tools, draft, mut rx) = editing_tools(Some(referencing_doc()));
        assert!(!tools.update_metadata(&json!({"name": "Renamed"})).is_error);
        assert_dirty_publish(&mut rx);
        {
            let state = draft.lock().unwrap();
            let (undone, was_dirty) = state.undo.as_ref().unwrap();
            assert_eq!(undone.name, "Demo");
            assert!(!*was_dirty, "the displaced draft was clean");
        }

        // Restore puts the original back with its clean flag…
        let outcome = tools.restore_draft();
        assert!(!outcome.is_error, "{:?}", outcome.result);
        match rx.try_recv().unwrap() {
            WorkbenchEvent::DraftReplaced { doc, dirty } => {
                assert_eq!(doc.name, "Demo");
                assert!(!dirty);
            }
            _ => panic!("expected DraftReplaced"),
        }
        assert!(!draft.lock().unwrap().dirty);

        // …and restoring again redoes the edit.
        assert!(!tools.restore_draft().is_error);
        let state = draft.lock().unwrap();
        assert_eq!(state.doc.as_ref().unwrap().name, "Renamed");
        assert!(state.dirty);
    }

    #[test]
    fn restore_with_no_snapshot_errors() {
        let (tools, _draft, mut rx) = editing_tools(Some(referencing_doc()));
        let outcome = tools.restore_draft();
        assert!(outcome.is_error);
        assert!(rx.try_recv().is_err(), "nothing should have been published");
    }

    #[test]
    fn editing_tools_require_a_draft() {
        let (tools, _draft, _rx) = editing_tools(None);
        assert!(tools.update_metadata(&json!({"name": "x"})).is_error);
        assert!(
            tools
                .add_step(&json!({"step": {"id": "E0", "toolName": "t__x", "input": {}}}))
                .is_error
        );
        assert!(
            tools
                .update_step(&json!({"id": "E0", "toolName": "t__x"}))
                .is_error
        );
        assert!(tools.delete_step(&json!({"id": "E0"})).is_error);
    }

    #[tokio::test]
    async fn workbench_tool_descriptions_carry_the_not_plan_legal_note() {
        let (tools, _draft, _rx) = editing_tools(None);
        for def in ToolRegistry::tools(&tools).await.unwrap() {
            assert!(
                def.description.ends_with(WORKBENCH_ONLY_NOTE),
                "{} is missing the workbench-only note",
                def.name
            );
        }
    }

    // ── draft_plan (scripted LLM) ──────────────────────────────────────

    struct ScriptedProvider {
        responses: Mutex<Vec<dive_llm::types::ChatResponse>>,
        requests: Mutex<Vec<dive_llm::types::ChatRequest>>,
    }

    #[async_trait]
    impl dive_llm::ChatProvider for ScriptedProvider {
        async fn chat(
            &self,
            req: dive_llm::types::ChatRequest,
        ) -> Result<dive_llm::types::ChatResponse, dive_llm::LlmError> {
            self.requests.lock().unwrap().push(req);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(dive_llm::LlmError::Parse("script exhausted".into()));
            }
            Ok(responses.remove(0))
        }

        async fn chat_stream(
            &self,
            req: dive_llm::types::ChatRequest,
        ) -> Result<dive_llm::types::EventStream, dive_llm::LlmError> {
            use futures::StreamExt;
            let response = self.chat(req).await?;
            Ok(
                futures::stream::iter(vec![Ok(dive_llm::types::StreamEvent::Completed(response))])
                    .boxed(),
            )
        }
    }

    /// A pipeline whose planner answers from a script of structured
    /// planner outputs — the same pattern as dive-core's pipeline tests.
    fn scripted_pipeline(outputs: Vec<Value>) -> (Arc<Pipeline>, Arc<ScriptedProvider>) {
        let provider = Arc::new(ScriptedProvider {
            responses: Mutex::new(
                outputs
                    .into_iter()
                    .map(|value| dive_llm::types::ChatResponse {
                        content: None,
                        tool_calls: vec![],
                        thinking: Vec::new(),
                        structured: Some(value),
                        stop_reason: dive_llm::types::StopReason::EndTurn,
                        usage: dive_llm::types::Usage::default(),
                    })
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
        });
        let mut providers: std::collections::HashMap<String, Arc<dyn dive_llm::ChatProvider>> =
            std::collections::HashMap::new();
        providers.insert("mock".to_string(), provider.clone());
        let roles = dive_config::ModelRoles::from([(
            "default",
            dive_config::ModelChoice {
                kind: None,
                provider: "mock".to_string(),
                model: "test".to_string(),
                temperature: None,
                description: None,
                fallbacks: Vec::new(),
                context_window: None,
            },
        )]);
        let router = Arc::new(dive_llm::ModelRouter::with_providers(providers, roles));
        let llm = dive_core::user_tools::load_builtin_tools().unwrap();
        let pipeline = Arc::new(Pipeline {
            registry: Arc::new(dive_core::user_tools::UserToolRegistry::builtins(
                llm,
                router.clone(),
            )),
            router,
            events: Arc::new(dive_core::NullSink),
            plans: Arc::new(dive_core::pipeline::doc::builtin_plan_docs()),
            call_stack: Vec::new(),
            store: None,
            gate: None,
            interlocutor: None,
            catalog: None,
            user_context: String::new(),
            current_date: String::new(),
            max_attempts: 1,
            max_agent_iterations: 15,
            usage: std::sync::Arc::new(dive_core::usage::UsageLedger::unpriced()),
            agents: Arc::new(dive_core::agent::doc::AgentSet::default()),
            agent_depth: 0,
            always_loaded: Default::default(),
            search_pool: None,
            drafted: Default::default(),
            packs: Default::default(),
            origin: Default::default(),
        });
        (pipeline, provider)
    }

    fn draft_tools(
        pipeline: Arc<Pipeline>,
    ) -> (
        WorkbenchTools,
        tokio::sync::mpsc::UnboundedReceiver<WorkbenchEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let tools = WorkbenchTools::new(
            Arc::new(Mutex::new(DraftState::new(None))),
            pipeline,
            None,
            Arc::new(DebugControls::default()),
            tx,
        );
        (tools, rx)
    }

    fn report_plan() -> Value {
        json!({
            "identifier": "report_on_x",
            "name": "Report on x",
            "description": "report on x",
            "steps": [
                {"id": "E0", "tool_name": "t__search", "input": {"query": "x"}},
            ],
        })
    }

    #[tokio::test]
    async fn describe_tool_gives_a_plan_s_output_and_sees_plans_drafted_in_the_thread() {
        let saved = str::parse::<PlanDoc>(
            "identifier: ranked\nname: Ranked\ndescription: d\nsteps:\n  - id: E0\n    tool_name: t__search\n    input: { query: x }\nfinish:\n  output:\n    results: \"{{E0.results}}\"\n",
        )
        .unwrap();
        let (tools, _rx) = draft_tools(test_pipeline(vec![saved]));
        let described = tools.describe_tool(&json!({"name": "plan__ranked"})).await;
        assert!(!described.is_error, "{}", described.result);
        assert_eq!(
            described.result["outputSchema"]["required"],
            json!(["results"])
        );

        let missing = tools
            .describe_tool(&json!({"name": "plan__report_on_x"}))
            .await;
        assert!(missing.is_error);
        tools
            .draft
            .lock()
            .unwrap()
            .network
            .put_draft(
                crate::workbench::artifact::ArtifactKind::Plan,
                "report_on_x",
                report_yaml(),
                Vec::new(),
            )
            .unwrap();
        let drafted = tools
            .describe_tool(&json!({"name": "plan__report_on_x"}))
            .await;
        assert!(!drafted.is_error, "{}", drafted.result);
        assert!(drafted.result["outputSchema"].is_object());
    }

    #[tokio::test]
    async fn describe_tool_knows_control_steps() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        let outcome = tools.describe_tool(&json!({"name": "route"})).await;
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["name"], json!("route"));
    }

    #[tokio::test]
    async fn describe_tool_knows_native_tools_and_suggests_them_for_a_near_miss() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        let outcome = tools
            .describe_tool(&json!({"name": "builtin__score_candidates"}))
            .await;
        assert!(!outcome.is_error, "{}", outcome.result);
        assert_eq!(outcome.result["name"], "builtin__score_candidates");
        assert!(outcome.result["inputSchema"].is_object());
        let missed = tools
            .describe_tool(&json!({"name": "builtin_score_candidates"}))
            .await;
        assert!(missed.is_error);
        assert!(
            missed
                .result
                .to_string()
                .contains("builtin__score_candidates"),
            "{}",
            missed.result
        );
    }

    #[tokio::test]
    async fn describe_tool_says_when_a_guessed_server_does_not_exist() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        let outcome = tools
            .describe_tool(&json!({"name": "github__list_pull_requests"}))
            .await;
        assert!(outcome.is_error);
        let error = outcome.result["error"].as_str().unwrap();
        assert!(
            error.contains("No MCP server 'github' is configured"),
            "{error}"
        );
        assert!(error.contains("instead of guessing"), "{error}");
    }

    #[tokio::test]
    async fn set_draft_publishes_the_plan_once_as_unsaved() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, mut rx) = draft_tools(pipeline);
        let outcome = call(
            &tools,
            SET_ARTIFACT,
            json!({"kind": "plan", "yaml": report_yaml()}),
        );
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["identifier"], json!("report_on_x"));
        assert_eq!(outcome.result["steps"], json!(1));
        match rx.try_recv().unwrap() {
            WorkbenchEvent::DraftReplaced { doc, dirty } => {
                assert!(dirty);
                assert_eq!(doc.steps[0].id, "E0");
            }
            _ => panic!("expected DraftReplaced"),
        }
        assert!(rx.try_recv().is_err(), "one publish only");
    }

    #[tokio::test]
    async fn set_draft_without_a_plan_publishes_the_last_composed_one_untouched() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        assert!(
            call(&tools, SET_ARTIFACT, json!({"kind": "plan"})).is_error,
            "nothing composed yet"
        );

        struct Composer;
        #[async_trait]
        impl ToolRegistry for Composer {
            async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
                Ok(Vec::new())
            }
            async fn invoke(&self, _name: &str, _input: Value) -> Result<ToolOutcome, ToolError> {
                Ok(ToolOutcome {
                    result: json!({"plan": report_plan(), "valid": true}),
                    is_error: false,
                })
            }
        }
        let capture = ComposedCapture::new(Arc::new(Composer), tools.draft.clone());
        capture
            .invoke(COMPOSE_PLAN_TOOL, json!({"goal": "x"}))
            .await
            .unwrap();

        let outcome = call(&tools, SET_ARTIFACT, json!({"kind": "plan"}));
        assert!(!outcome.is_error, "{:?}", outcome.result);
        assert_eq!(outcome.result["identifier"], json!("report_on_x"));
    }

    #[tokio::test]
    async fn set_draft_refuses_to_discard_unsaved_changes_without_confirmation() {
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        assert!(
            !call(
                &tools,
                SET_ARTIFACT,
                json!({"kind": "plan", "yaml": report_yaml()})
            )
            .is_error
        );

        let refused = call(
            &tools,
            SET_ARTIFACT,
            json!({"kind": "plan", "yaml": report_yaml()}),
        );
        assert!(refused.is_error);
        assert!(
            refused.result["error"]
                .as_str()
                .unwrap()
                .contains("unsaved changes"),
            "{:?}",
            refused.result
        );
        let confirmed = call(
            &tools,
            SET_ARTIFACT,
            json!({"kind": "plan", "yaml": report_yaml(), "overwrite_draft": true}),
        );
        assert!(!confirmed.is_error, "{:?}", confirmed.result);
    }

    const WEATHER_TOOL: &str = "name: weather\ndescription: Current weather for a city\nkind: exec\ncommand: printf\nargs: ['{\"city\":\"%s\"}', '{{input.city}}']\ninput_schema:\n  type: object\n  required: [city]\n  properties:\n    city: {type: string, description: The city}\n";

    fn drafting_tools() -> (
        WorkbenchTools,
        tokio::sync::mpsc::UnboundedReceiver<WorkbenchEvent>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(Vec::new())).clone();
        pipeline.drafted = Arc::new(dive_core::pipeline::Drafted::new(
            Some(dir.path().join("tools")),
            Some(dir.path().join("agents")),
        ));
        let (tools, rx) = draft_tools(Arc::new(pipeline));
        (tools, rx, dir)
    }

    fn shown(rx: &mut tokio::sync::mpsc::UnboundedReceiver<WorkbenchEvent>) -> Option<usize> {
        let mut last = None;
        while let Ok(msg) = rx.try_recv() {
            if let WorkbenchEvent::NetworkChanged(view) = msg {
                last = Some(view.unsaved());
            }
        }
        last
    }

    fn mock_router() -> Arc<dive_llm::ModelRouter> {
        let provider: Arc<dyn dive_llm::ChatProvider> = Arc::new(ScriptedProvider {
            responses: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        });
        Arc::new(dive_llm::ModelRouter::with_providers(
            std::collections::HashMap::from([("mock".to_string(), provider)]),
            dive_config::ModelRoles::from_iter([(
                "default".to_string(),
                dive_config::ModelChoice {
                    kind: None,
                    provider: "mock".into(),
                    model: "m".into(),
                    temperature: None,
                    description: None,
                    fallbacks: Vec::new(),
                    context_window: None,
                },
            )]),
        ))
    }

    async fn answer_yes(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<WorkbenchEvent>,
        starts: &str,
        key: &str,
    ) {
        while let Some(msg) = rx.recv().await {
            if let WorkbenchEvent::GateAsk { reply, input, .. } = msg {
                let prompt = input["prompt"].as_str().unwrap().to_string();
                assert!(prompt.starts_with(starts), "{prompt}");
                reply
                    .send(crate::workbench::runner::UiDecision::Skip {
                        result: json!({ key: true }),
                    })
                    .unwrap();
                break;
            }
        }
    }

    #[tokio::test]
    async fn a_tool_file_that_does_not_parse_or_has_no_name_drafts_nothing() {
        let (tools, _rx, _dir) = drafting_tools();
        for (yaml, says) in [
            (
                "version: 3\nname: weather\ndescription: a: b: c\n",
                "doesn't parse",
            ),
            ("version: 3\ndescription: weather\n", "no top-level `name`"),
        ] {
            let set = tools
                .invoke(
                    crate::workbench::artifact::SET_ARTIFACT,
                    json!({"kind": "tool", "yaml": yaml}),
                )
                .await
                .unwrap();
            assert!(set.is_error, "{}", set.result);
            assert!(set.result["error"].as_str().unwrap().contains(says));
        }
        let listed = tools
            .invoke(
                crate::workbench::artifact::LIST_ARTIFACTS,
                json!({"kind": "tool"}),
            )
            .await
            .unwrap();
        assert!(!listed.result.to_string().contains("unnamed"));
    }

    #[tokio::test]
    async fn a_tool_draft_waits_in_the_network_until_the_network_is_saved() {
        let (tools, mut rx, dir) = drafting_tools();
        let set = tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "tool", "yaml": WEATHER_TOOL}),
            )
            .await
            .unwrap();
        assert!(!set.is_error, "{}", set.result);
        assert_eq!(set.result["problems"], json!([]));
        assert_eq!(shown(&mut rx), Some(1));

        let single = tools
            .invoke(
                crate::workbench::artifact::SAVE_ARTIFACT,
                json!({"kind": "tool"}),
            )
            .await
            .unwrap();
        assert!(single
            .result
            .to_string()
            .contains("workbench__save_network"));

        let untested = tools
            .invoke(crate::workbench::artifact::SAVE_NETWORK, json!({}))
            .await
            .unwrap();
        assert!(untested.is_error);
        assert!(
            untested.result.to_string().contains("test"),
            "{}",
            untested.result
        );
        assert!(!dir.path().join("tools/weather.yaml").exists());

        let saving = tools.invoke(
            crate::workbench::artifact::SAVE_NETWORK,
            json!({"untested": true}),
        );
        let (saved, ()) = tokio::join!(
            saving,
            answer_yes(&mut rx, "Save the network untested: 1 files?", "save")
        );
        let saved = saved.unwrap();
        assert!(!saved.is_error, "{}", saved.result);
        assert!(
            saved.result["saved"]
                .as_str()
                .unwrap()
                .ends_with("(untested)"),
            "{}",
            saved.result
        );
        assert!(dir.path().join("tools/weather.yaml").exists());
        assert!(tools.draft.lock().unwrap().network.is_empty());
        assert_eq!(shown(&mut rx), Some(0));
    }

    #[tokio::test]
    async fn saving_an_opened_agent_replaces_that_file_and_a_new_one_never_replaces_another() {
        use crate::workbench::artifact::{save_network, ArtifactKind};
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(Vec::new())).clone();
        pipeline.drafted = Arc::new(dive_core::pipeline::Drafted::new(
            Some(dir.path().join("tools")),
            Some(dir.path().join("agents")),
        ));
        pipeline.router = mock_router();
        let (tools, _rx) = draft_tools(Arc::new(pipeline));
        let global = dir.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        let opened = global.join("triager.yaml");
        std::fs::write(&opened, "name: triager\n").unwrap();
        let yaml = |name: &str| {
            format!("name: {name}\ndescription: Triages issues\nmodel: default\ntools: []\nsystem_prompt: You triage.\n")
        };
        let (tx, _rx2) = tokio::sync::mpsc::unbounded_channel();

        {
            let network = &mut tools.draft.lock().unwrap().network;
            network.open(
                ArtifactKind::Agent,
                crate::workbench::artifact::Existing {
                    yaml: "name: triager\n".to_string(),
                    editing: Some(opened.clone()),
                    name: "triager".to_string(),
                    origin: crate::workbench::network::Origin::File,
                },
                Vec::new(),
            );
            network
                .put_draft(ArtifactKind::Agent, "triager", yaml("triager"), Vec::new())
                .unwrap();
        }
        save_network(
            &tools.draft,
            &tools.pipeline,
            &tx,
            crate::workbench::artifact::SaveRequest::unattended(),
        )
        .await
        .unwrap();
        assert!(std::fs::read_to_string(&opened)
            .unwrap()
            .contains("You triage."));
        assert!(!dir.path().join("agents/triager.yaml").exists());

        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::write(dir.path().join("agents/other.yaml"), "name: other\n").unwrap();
        tools
            .draft
            .lock()
            .unwrap()
            .network
            .put_draft(ArtifactKind::Agent, "other", yaml("other"), Vec::new())
            .unwrap();
        let refused = save_network(
            &tools.draft,
            &tools.pipeline,
            &tx,
            crate::workbench::artifact::SaveRequest::unattended(),
        )
        .await
        .unwrap_err();
        assert!(refused.contains("already exists"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("agents/other.yaml")).unwrap(),
            "name: other\n",
            "a new draft never replaces another agent's file"
        );
    }

    #[tokio::test]
    async fn a_save_that_stops_partway_says_what_was_written_and_a_retry_replaces_it() {
        use crate::workbench::artifact::{save_network, ArtifactKind};
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(Vec::new())).clone();
        pipeline.drafted = Arc::new(dive_core::pipeline::Drafted::new(
            Some(dir.path().join("tools")),
            Some(dir.path().join("agents")),
        ));
        pipeline.router = mock_router();
        let (tools, _rx) = draft_tools(Arc::new(pipeline));
        let (tx, _rx2) = tokio::sync::mpsc::unbounded_channel();
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::write(dir.path().join("agents/desk.yaml"), "name: desk\n").unwrap();
        {
            let mut state = tools.draft.lock().unwrap();
            state
                .network
                .put_draft(
                    ArtifactKind::Tool,
                    "weather",
                    WEATHER_TOOL.into(),
                    Vec::new(),
                )
                .unwrap();
            state
                .network
                .put_draft(
                    ArtifactKind::Agent,
                    "desk",
                    "name: desk\ndescription: Answers\nmodel: default\ntools: [user__weather]\nsystem_prompt: You answer.\n".into(),
                    Vec::new(),
                )
                .unwrap();
        }
        let stopped = save_network(
            &tools.draft,
            &tools.pipeline,
            &tx,
            crate::workbench::artifact::SaveRequest::unattended(),
        )
        .await
        .unwrap_err();
        assert!(
            stopped.starts_with("saved 1 of 2; agent desk was not saved"),
            "{stopped}"
        );
        assert!(dir.path().join("tools/weather.yaml").exists());
        assert_eq!(
            tools.draft.lock().unwrap().network.nodes.len(),
            2,
            "the drafts stay"
        );

        std::fs::remove_file(dir.path().join("agents/desk.yaml")).unwrap();
        save_network(
            &tools.draft,
            &tools.pipeline,
            &tx,
            crate::workbench::artifact::SaveRequest::unattended(),
        )
        .await
        .unwrap();
        assert!(dir.path().join("agents/desk.yaml").exists());
        assert!(tools.draft.lock().unwrap().network.is_empty());
    }

    #[tokio::test]
    async fn several_drafts_live_side_by_side_and_a_try_names_which() {
        let (tools, _rx, _dir) = drafting_tools();
        for yaml in [
            WEATHER_TOOL.to_string(),
            WEATHER_TOOL.replace("name: weather", "name: forecast"),
        ] {
            let set = tools
                .invoke(
                    crate::workbench::artifact::SET_ARTIFACT,
                    json!({"kind": "tool", "yaml": yaml}),
                )
                .await
                .unwrap();
            assert!(!set.is_error, "{}", set.result);
        }
        assert_eq!(tools.draft.lock().unwrap().network.nodes.len(), 2);
        let ambiguous = tools
            .invoke(
                crate::workbench::artifact::TRY_ARTIFACT,
                json!({"kind": "tool"}),
            )
            .await
            .unwrap();
        assert!(ambiguous.is_error);
        assert!(ambiguous.result.to_string().contains("several tool drafts"));
    }

    #[tokio::test]
    async fn a_sketch_lets_drafts_name_nodes_that_are_not_drafted_yet() {
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(Vec::new())).clone();
        pipeline.drafted = Arc::new(dive_core::pipeline::Drafted::new(
            Some(dir.path().join("tools")),
            Some(dir.path().join("agents")),
        ));
        pipeline.router = mock_router();
        pipeline.catalog = Some(Arc::new(dive_core::pipeline::ToolCatalog::default()));
        let (tools, _rx) = draft_tools(Arc::new(pipeline));
        let sketch = tools
            .invoke(
                crate::workbench::artifact::SET_SKETCH,
                json!({
                    "restate": {"outcome": "Weather in chat", "success": "An answer", "constraint": "Small models", "out_of_scope": []},
                    "nodes": [
                        {"name": "desk", "kind": "agent", "job": "Answers", "model": "default"},
                        {"name": "lookup", "kind": "tool", "job": "Looks it up"},
                        {"name": "report", "kind": "plan", "job": "Reports"}
                    ],
                    "edges": [{"from": "desk", "to": "lookup", "kind": "calls"}]
                }),
            )
            .await
            .unwrap();
        assert!(!sketch.is_error, "{}", sketch.result);
        let agent = tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "agent", "yaml": "name: desk\ndescription: Answers\nmodel: default\ntools: [user__lookup, plan__report]\nsystem_prompt: You answer.\n"}),
            )
            .await
            .unwrap();
        assert!(!agent.is_error, "{}", agent.result);
        assert_eq!(agent.result["problems"], json!([]), "{}", agent.result);

        let stray = tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "tool", "yaml": WEATHER_TOOL}),
            )
            .await
            .unwrap();
        assert!(stray.is_error);
        assert!(
            stray.result.to_string().contains("lookup"),
            "{}",
            stray.result
        );

        let plan = tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "plan", "yaml": "identifier: report\nname: Report\ndescription: Reports\nsteps:\n  - id: E0\n    tool_name: user__lookup\n    input: {}\nfinish: silent\n"}),
            )
            .await
            .unwrap();
        assert!(!plan.is_error, "{}", plan.result);
        assert!(
            tools.draft.lock().unwrap().doc.is_none(),
            "a sketched plan never lands in the plan pane"
        );
        let network = tools.draft.lock().unwrap().network.clone();
        assert_eq!(network.missing(), ["tool lookup"]);

        let untestable = tools
            .invoke(
                crate::workbench::artifact::TEST_NETWORK,
                json!({"entry": "desk", "message": "hi"}),
            )
            .await
            .unwrap();
        assert!(untestable
            .result
            .to_string()
            .contains("not every node is drafted"));
    }

    #[tokio::test]
    async fn every_artifact_tool_routes_on_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(Vec::new())).clone();
        pipeline.drafted = Arc::new(
            dive_core::pipeline::Drafted::new(Some(dir.path().join("tools")), None)
                .with_tool_dirs(vec![dir.path().join("tools")]),
        );
        let (tools, _rx) = draft_tools(Arc::new(pipeline));
        std::fs::create_dir_all(dir.path().join("tools")).unwrap();
        std::fs::write(dir.path().join("tools/weather.yaml"), WEATHER_TOOL).unwrap();
        let invoke = |name: &'static str, input: Value| {
            let tools = &tools;
            async move { tools.invoke(name, input).await.unwrap() }
        };

        let missing = invoke(SET_ARTIFACT, json!({"yaml": WEATHER_TOOL})).await;
        assert!(missing.is_error);
        assert!(missing.result.to_string().contains("kind must be"));

        let shown = invoke(
            SHOW_ARTIFACT,
            json!({"kind": "tool", "name": "user__weather"}),
        )
        .await;
        assert!(!shown.is_error, "{}", shown.result);
        assert_eq!(shown.result["yaml"], json!(WEATHER_TOOL));

        let listed = invoke(LIST_ARTIFACTS, json!({"kind": "tool"})).await;
        assert_eq!(
            listed.result["user_tools"][0]["name"],
            json!("user__weather")
        );
        let agents = invoke(LIST_ARTIFACTS, json!({"kind": "agent"})).await;
        assert!(agents.result["agents"].is_array());

        let plan_discard = invoke(DISCARD_ARTIFACT, json!({"kind": "plan"})).await;
        assert!(plan_discard.is_error);

        let opened = invoke(LOAD_ARTIFACT, json!({"kind": "tool", "name": "weather"})).await;
        assert!(!opened.is_error, "{}", opened.result);
        let wrong = invoke(DISCARD_ARTIFACT, json!({"kind": "agent"})).await;
        assert!(wrong.is_error);
        assert!(wrong.result.to_string().contains("no agent draft"));
        assert!(tools
            .draft
            .lock()
            .unwrap()
            .network
            .node(crate::workbench::ArtifactKind::Tool, "weather")
            .is_some());
        let tried = invoke(TRY_ARTIFACT, json!({"kind": "agent"})).await;
        assert!(tried.result.to_string().contains("workbench__test_network"));
    }

    #[tokio::test]
    async fn an_invalid_draft_shows_its_problems_and_can_be_discarded() {
        let (tools, mut rx, _dir) = drafting_tools();
        let set = tools
            .invoke(
                crate::workbench::artifact::SET_ARTIFACT,
                json!({"kind": "tool", "yaml": "name: weather\ndescription: x\nkind: exec\ncommand: printf\nbogus: 1\n"}),
            )
            .await
            .unwrap();
        assert!(set.result["problems"].to_string().contains("bogus"));
        let discarded = tools
            .invoke(
                crate::workbench::artifact::DISCARD_ARTIFACT,
                json!({"kind": "tool"}),
            )
            .await
            .unwrap();
        assert!(!discarded.is_error);
        assert_eq!(shown(&mut rx), Some(0));
    }

    #[test]
    fn the_architect_cannot_hand_off_while_the_network_has_unsaved_drafts() {
        use crate::workbench::artifact::{handoff_guard, ArtifactKind};
        let (tools, _rx) = draft_tools(test_pipeline(Vec::new()));
        let draft = tools.draft.clone();
        let guard = handoff_guard(draft.clone());
        assert!(guard("network_drafter", "plan_editor").is_none());
        draft
            .lock()
            .unwrap()
            .network
            .put_draft(
                ArtifactKind::Tool,
                "lookup",
                "name: lookup\n".into(),
                Vec::new(),
            )
            .unwrap();
        assert!(guard("network_drafter", "plan_editor").is_some());
        assert!(guard("front_desk", "network_drafter").is_none());
    }

    struct Approves;

    #[async_trait::async_trait]
    impl dive_core::pipeline::Interlocutor for Approves {
        async fn ask(
            &self,
            request: dive_core::pipeline::AskRequest,
        ) -> dive_core::pipeline::AskOutcome {
            let key = request.schema["required"][0]
                .as_str()
                .unwrap_or("run")
                .to_string();
            dive_core::pipeline::AskOutcome::Answered(json!({ key: true }))
        }
    }

    const CRM_TOOL: &str = "version: 3\nname: crm_lookup\ndescription: Looks up a contact\nkind: exec\ncommand: printf\nargs: ['{\"ok\":true}']\nenv:\n  TOKEN:\n    value: \"${DIVE_T_CRM}\"\n    description: \"A CRM token from [settings](https://crm.example/settings).\"\n";

    fn notify_tool(mark: &std::path::Path) -> String {
        format!(
            "version: 3\nname: notify\ndescription: Notifies\nkind: exec\ncommand: sh\nargs: ['-c', 'touch \"$MARK\"']\noutput: text\nenv:\n  MARK: \"{}\"\n  HOOK:\n    value: \"${{DIVE_T_NOTIFY}}\"\n    description: \"The webhook URL from [the hooks page](https://hooks.example).\"\n  LEVEL: \"${{DIVE_T_LEVEL:-info}}\"\n",
            mark.display()
        )
    }

    const REPORT_PLAN: &str = "identifier: report\nname: Report\ndescription: Reports\nsteps:\n  - id: E0\n    tool_name: user__crm_lookup\n    input: {}\n  - id: E1\n    tool_name: user__notify\n    input: {}\nfinish: silent\n";

    fn secret_network(
        secrets: Arc<dyn dive_core::secrets::SecretSource>,
    ) -> (
        WorkbenchTools,
        tokio::sync::mpsc::UnboundedReceiver<WorkbenchEvent>,
        tempfile::TempDir,
    ) {
        use crate::workbench::artifact::ArtifactKind;
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(Vec::new())).clone();
        pipeline.drafted = Arc::new(
            dive_core::pipeline::Drafted::new(Some(dir.path().join("tools")), None)
                .with_secrets(secrets.clone()),
        );
        pipeline.registry = Arc::new(
            dive_core::user_tools::UserToolRegistry::new(
                vec![dive_core::user_tools::parse_tool_source(CRM_TOOL).unwrap()],
                pipeline.router.clone(),
            )
            .with_secrets(secrets.clone()),
        );
        pipeline.interlocutor = Some(Arc::new(Approves));
        let (tools, rx) = draft_tools(Arc::new(pipeline));
        {
            let mut state = tools.draft.lock().unwrap();
            state.network.secrets = Some(secrets);
            state
                .network
                .put_draft(
                    ArtifactKind::Tool,
                    "notify",
                    notify_tool(&dir.path().join("ran")),
                    Vec::new(),
                )
                .unwrap();
            state
                .network
                .put_draft(ArtifactKind::Plan, "report", REPORT_PLAN.into(), Vec::new())
                .unwrap();
        }
        (tools, rx, dir)
    }

    #[tokio::test]
    async fn a_test_with_missing_secrets_runs_nothing_and_says_which_are_needed() {
        let (tools, _rx, dir) = secret_network(Arc::new(dive_core::secrets::MemorySecrets::env()));
        let tested = tools
            .invoke(
                crate::workbench::artifact::TEST_NETWORK,
                json!({"entry": "report", "input": {}}),
            )
            .await
            .unwrap();
        assert!(!tested.is_error, "{}", tested.result);
        assert_eq!(tested.result["status"], "missing_secrets");
        assert_eq!(tested.result["ran"], false);
        let missing: Vec<(String, Vec<String>)> = tested.result["missing_secrets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|secret| {
                (
                    secret["name"].as_str().unwrap().to_string(),
                    serde_json::from_value(secret["tools"].clone()).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            missing,
            [
                (
                    "DIVE_T_CRM".to_string(),
                    vec!["user__crm_lookup".to_string()]
                ),
                (
                    "DIVE_T_NOTIFY".to_string(),
                    vec!["user__notify".to_string()]
                ),
            ],
            "the existing tool's secret is included and a defaulted one is not"
        );
        assert!(tested.result["missing_secrets"][0]["description"]
            .as_str()
            .unwrap()
            .contains("crm.example"));
        assert!(!dir.path().join("ran").exists(), "nothing ran");

        let view = crate::workbench::artifact::view(&tools.draft);
        assert!(view.waiting_on_secrets);
        assert!(view.tests.is_empty());
        assert_eq!(view.test_secrets.len(), 2);
        assert!(view.test_secrets.iter().all(|secret| !secret.set));
        let notify = view
            .nodes
            .iter()
            .find(|node| node.name == "notify")
            .unwrap();
        assert_eq!(notify.secrets.len(), 1);
        assert_eq!(notify.secrets[0].name, "DIVE_T_NOTIFY");
        assert!(!notify.secrets[0].set);
        assert!(notify.secrets[0]
            .description
            .as_deref()
            .unwrap()
            .contains("hooks.example"));
        assert!(tools
            .draft
            .lock()
            .unwrap()
            .network
            .context_section()
            .contains("waiting on secrets"));

        let replayed = crate::workbench::replay::network(
            &[
                dive_core::NewEntry::new(
                    "network_drafter",
                    dive_core::store::EntryBody::Message {
                        message: dive_llm::types::ChatMessage::Assistant {
                            content: None,
                            thinking: Default::default(),
                            tool_calls: vec![dive_llm::types::ToolCall {
                                id: "t1".into(),
                                name: crate::workbench::artifact::TEST_NETWORK.into(),
                                arguments: json!({"entry": "report", "input": {}}),
                            }],
                        },
                    },
                ),
                dive_core::NewEntry::new(
                    "network_drafter",
                    dive_core::store::EntryBody::Message {
                        message: dive_llm::types::ChatMessage::ToolResult {
                            tool_call_id: "t1".into(),
                            content: tested.result.clone(),
                            is_error: false,
                        },
                    },
                ),
            ],
            &[],
            &|_, _| None,
        );
        assert!(replayed.waiting_on_secrets);
        assert_eq!(replayed.test_secrets.len(), 2);
        assert!(replayed.tests.is_empty());
    }

    #[tokio::test]
    async fn secrets_set_in_the_environment_or_the_project_store_let_the_test_run() {
        use dive_core::secrets::{ChainedSource, InProject, MemorySecrets};
        let resolver = InProject::new(
            ChainedSource::new(vec![
                Arc::new(MemorySecrets::env().with("DIVE_T_CRM", "crm-value")),
                Arc::new(MemorySecrets::store().with_project(
                    "DIVE_T_NOTIFY",
                    "p1",
                    "https://hooks.example/1",
                )),
            ]),
            Some("p1".to_string()),
        );
        let (tools, _rx, dir) = secret_network(Arc::new(resolver));
        let tested = tools
            .invoke(
                crate::workbench::artifact::TEST_NETWORK,
                json!({"entry": "report", "input": {}}),
            )
            .await
            .unwrap();
        assert!(tested.result.get("status").is_none(), "{}", tested.result);
        assert!(dir.path().join("ran").exists(), "{}", tested.result);
        let view = crate::workbench::artifact::view(&tools.draft);
        assert!(!view.waiting_on_secrets);
        assert!(view.test_secrets.iter().all(|secret| secret.set));
        assert_eq!(view.tests.len(), 1);
    }

    #[tokio::test]
    async fn trying_a_tool_draft_waits_on_its_secrets_and_a_global_value_counts() {
        use dive_core::secrets::{ChainedSource, InProject, MemorySecrets};
        let (tools, _rx, dir) = secret_network(Arc::new(MemorySecrets::env()));
        let tried = tools
            .invoke(
                crate::workbench::artifact::TRY_ARTIFACT,
                json!({"kind": "tool", "name": "notify", "input": {}}),
            )
            .await
            .unwrap();
        assert_eq!(
            tried.result["status"], "missing_secrets",
            "{}",
            tried.result
        );
        assert_eq!(tried.result["missing_secrets"][0]["name"], "DIVE_T_NOTIFY");
        assert!(!dir.path().join("ran").exists());
        assert!(crate::workbench::artifact::view(&tools.draft).waiting_on_secrets);

        let global: Arc<dyn dive_core::secrets::SecretSource> = Arc::new(InProject::new(
            ChainedSource::new(vec![
                Arc::new(MemorySecrets::env()),
                Arc::new(MemorySecrets::store().with("DIVE_T_NOTIFY", "https://hooks.example/g")),
            ]),
            Some("p1".to_string()),
        ));
        let (tools, _rx, dir) = secret_network(global);
        let tried = tools
            .invoke(
                crate::workbench::artifact::TRY_ARTIFACT,
                json!({"kind": "tool", "name": "notify", "input": {}}),
            )
            .await
            .unwrap();
        assert_eq!(tried.result["ran"], true, "{}", tried.result);
        assert!(dir.path().join("ran").exists());
        let view = crate::workbench::artifact::view(&tools.draft);
        assert!(!view.waiting_on_secrets);
        let notify = view
            .nodes
            .iter()
            .find(|node| node.name == "notify")
            .unwrap();
        assert!(notify.secrets[0].set);
    }

    #[tokio::test]
    async fn an_agent_needs_the_secrets_of_the_tools_it_lists_and_its_subagents_use() {
        use crate::workbench::artifact::ArtifactKind;
        let (tools, _rx, _dir) = secret_network(Arc::new(dive_core::secrets::MemorySecrets::env()));
        {
            let mut state = tools.draft.lock().unwrap();
            for yaml in [
                "name: desk\ndescription: Answers\nmodel: default\ntools: [\"*\"]\nsubagents: [helper]\nsystem_prompt: You answer.\n",
                "name: helper\ndescription: Helps\nmodel: default\ntools: [user__crm_*]\nsystem_prompt: You help.\n",
            ] {
                let name = if yaml.starts_with("name: desk") { "desk" } else { "helper" };
                state
                    .network
                    .put_draft(ArtifactKind::Agent, name, yaml.into(), Vec::new())
                    .unwrap();
            }
        }
        let network = tools.draft.lock().unwrap().network.clone();
        let over = crate::workbench::network::overlay(&tools.pipeline, &network);
        let names = |needed: Vec<crate::workbench::TestSecret>| -> Vec<String> {
            needed.into_iter().map(|secret| secret.name).collect()
        };
        let own = vec!["user__notify".to_string()];
        assert_eq!(
            names(crate::workbench::secrets::needed(&over, "agent__helper", &own).await),
            ["DIVE_T_CRM"]
        );
        assert_eq!(
            names(crate::workbench::secrets::needed(&over, "agent__desk", &own).await),
            ["DIVE_T_NOTIFY", "DIVE_T_CRM"],
            "a bare * covers the network's own tools, and the subagent's tools count"
        );
    }

    fn item_tools(
        packs: Vec<dive_core::packs::PackSource>,
        plans: Vec<PlanDoc>,
    ) -> (
        WorkbenchTools,
        tokio::sync::mpsc::UnboundedReceiver<WorkbenchEvent>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut pipeline = (*test_pipeline(plans)).clone();
        pipeline.drafted = Arc::new(
            dive_core::pipeline::Drafted::new(
                Some(dir.path().join("tools")),
                Some(dir.path().join("agents")),
            )
            .with_tool_dirs(vec![dir.path().join("tools")])
            .with_pack_sources(packs),
        );
        let (tools, rx) = draft_tools(Arc::new(pipeline));
        (tools, rx, dir)
    }

    #[tokio::test]
    async fn a_built_in_tool_opens_as_a_node_and_saves_as_a_project_override() {
        use crate::workbench::artifact::{existing, save_network, ArtifactKind, SaveRequest};
        let (tools, mut rx, dir) = item_tools(Vec::new(), Vec::new());
        let opened = tools
            .invoke(
                crate::workbench::artifact::LOAD_ARTIFACT,
                json!({"kind": "tool", "name": "builtin__reshape"}),
            )
            .await
            .unwrap();
        assert!(!opened.is_error, "{}", opened.result);
        assert_eq!(opened.result["opened"], json!("reshape"));
        let view = tools.draft.lock().unwrap().network.view();
        assert_eq!(view.nodes[0].origin, "builtin");
        assert!(!view.nodes[0].changed);
        assert_eq!(view.unsaved(), 0);
        assert_eq!(shown(&mut rx), Some(0));

        let unchanged = save_network(
            &tools.draft,
            &tools.pipeline,
            &tools.tx,
            SaveRequest::unattended(),
        )
        .await
        .unwrap_err();
        assert!(unchanged.contains("nothing has changed"), "{unchanged}");

        let base = view.nodes[0].base.clone().unwrap();
        let edited = base.replacen("name: reshape", "name: reshape\nread_only: true", 1);
        tools
            .draft
            .lock()
            .unwrap()
            .network
            .put_draft(ArtifactKind::Tool, "reshape", edited.clone(), Vec::new())
            .unwrap();
        assert!(tools.draft.lock().unwrap().network.view().nodes[0].changed);
        let saved = save_network(
            &tools.draft,
            &tools.pipeline,
            &tools.tx,
            SaveRequest::unattended(),
        )
        .await
        .unwrap();
        let path = dir.path().join("tools/builtin/reshape.yaml");
        assert_eq!(saved.files.len(), 1);
        assert_eq!(saved.files[0].destination, "override");
        assert_eq!(saved.files[0].path, path.display().to_string());
        assert!(saved.saved.contains("a project override of the built-in"));
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("read_only: true"));
        assert!(!dir.path().join("tools/reshape.yaml").exists());

        let network = tools.draft.lock().unwrap().network.clone();
        assert!(network.is_edit(), "the item stays open after the save");
        assert!(!network.has_edits());
        let reopened = existing(ArtifactKind::Tool, "builtin__reshape", &tools.pipeline).unwrap();
        assert_eq!(reopened.editing, Some(path));
        assert!(reopened.yaml.contains("read_only: true"));
    }

    #[tokio::test]
    async fn a_new_plan_drafted_while_editing_joins_the_edit_as_a_node() {
        use crate::workbench::artifact::ArtifactKind;
        let (tools, _rx, _dir) = item_tools(Vec::new(), Vec::new());
        let opened = tools
            .invoke(
                crate::workbench::artifact::LOAD_ARTIFACT,
                json!({"kind": "tool", "name": "builtin__reshape"}),
            )
            .await
            .unwrap();
        assert!(!opened.is_error, "{}", opened.result);
        let set = tools
            .invoke(SET_ARTIFACT, json!({"kind": "plan", "yaml": report_yaml()}))
            .await
            .unwrap();
        assert!(!set.is_error, "{}", set.result);
        assert_eq!(set.result["kind"], json!("plan"), "{}", set.result);
        let state = tools.draft.lock().unwrap();
        assert!(state.doc.is_none(), "the plan pane stays empty");
        assert!(state.network.is_edit());
        let node = state
            .network
            .node(ArtifactKind::Plan, "report_on_x")
            .expect("the new plan is a node of the edit");
        assert!(node.base.is_none());
        assert_eq!(state.network.view().unsaved(), 1);
    }

    #[tokio::test]
    async fn a_workbench_agent_opens_for_editing_as_a_built_in() {
        use crate::workbench::artifact::{existing, ArtifactKind};
        let (pipeline, _) = scripted_pipeline(Vec::new());
        let (tools, _rx) = draft_tools(pipeline);
        let opened = existing(ArtifactKind::Agent, "network_editor", &tools.pipeline).unwrap();
        assert_eq!(opened.name, "network_editor");
        assert!(opened.editing.is_none());
        assert!(opened.yaml.contains("name: network_editor"));
    }

    #[tokio::test]
    async fn a_pack_tool_opens_from_its_installed_copy_and_saves_back_to_it() {
        use crate::workbench::artifact::{save_network, ArtifactKind, SaveRequest};
        let packs = tempfile::tempdir().unwrap();
        let pack_dir = packs.path().join("demo");
        std::fs::create_dir_all(pack_dir.join("tools")).unwrap();
        let installed = pack_dir.join("tools/count.yaml");
        std::fs::write(
            &installed,
            "name: count\ndescription: Counts\nkind: exec\ncommand: wc\n",
        )
        .unwrap();
        let source = dive_core::packs::PackSource {
            name: "demo".to_string(),
            tag: Some("v1.0.0".to_string()),
            layer: dive_config::pack::LayerKind::Project,
            dir: pack_dir,
        };
        let (tools, _rx, dir) = item_tools(vec![source], Vec::new());
        let name = crate::workbench::artifact::open_existing(
            &tools.draft,
            &tools.pipeline,
            &tools.tx,
            ArtifactKind::Tool,
            "demo__tool__count",
        )
        .await
        .unwrap();
        assert_eq!(name, "count");
        {
            let network = &mut tools.draft.lock().unwrap().network;
            let node = network.node(ArtifactKind::Tool, "count").unwrap();
            assert_eq!(node.catalog_name(), "demo__tool__count");
            assert_eq!(node.origin.label(), "pack");
            network
                .put_draft(
                    ArtifactKind::Tool,
                    "count",
                    "name: count\ndescription: Counts words\nkind: exec\ncommand: wc\nargs: [-w]\n"
                        .to_string(),
                    Vec::new(),
                )
                .unwrap();
        }
        let saved = save_network(
            &tools.draft,
            &tools.pipeline,
            &tools.tx,
            SaveRequest::unattended(),
        )
        .await
        .unwrap();
        assert_eq!(saved.files[0].destination, "pack");
        assert_eq!(saved.files[0].path, installed.display().to_string());
        assert!(std::fs::read_to_string(&installed)
            .unwrap()
            .contains("Counts words"));
        assert!(!dir.path().join("tools/count.yaml").exists());
    }

    #[tokio::test]
    async fn attaching_another_item_to_an_edit_needs_a_reason_and_the_users_approval() {
        let plan: PlanDoc = dive_core::pipeline::plan_doc(&report_plan()).unwrap();
        let (tools, mut rx, dir) = item_tools(Vec::new(), vec![plan]);
        std::fs::create_dir_all(dir.path().join("tools")).unwrap();
        std::fs::write(dir.path().join("tools/weather.yaml"), WEATHER_TOOL).unwrap();
        let opened = tools
            .invoke(
                crate::workbench::artifact::LOAD_ARTIFACT,
                json!({"kind": "tool", "name": "weather"}),
            )
            .await
            .unwrap();
        assert!(!opened.is_error, "{}", opened.result);

        let unexplained = tools
            .invoke(
                crate::workbench::artifact::LOAD_ARTIFACT,
                json!({"kind": "plan", "name": "report_on_x"}),
            )
            .await
            .unwrap();
        assert!(unexplained.is_error);
        assert!(unexplained.result.to_string().contains("reason"));

        let attaching = tools.invoke(
            crate::workbench::artifact::LOAD_ARTIFACT,
            json!({"kind": "plan", "name": "report_on_x", "reason": "it calls the weather tool"}),
        );
        let (attached, ()) = tokio::join!(
            attaching,
            answer_yes(
                &mut rx,
                "Attach the plan report_on_x to this edit? it calls the weather tool",
                "attach"
            )
        );
        let attached = attached.unwrap();
        assert!(!attached.is_error, "{}", attached.result);
        let network = tools.draft.lock().unwrap().network.clone();
        let plan = network
            .node(
                crate::workbench::artifact::ArtifactKind::Plan,
                "report_on_x",
            )
            .unwrap();
        assert_eq!(plan.origin.label(), "builtin");
        assert!(
            tools.draft.lock().unwrap().doc.is_none(),
            "not the plan pane"
        );
    }
}
