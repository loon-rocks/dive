use super::event::WorkbenchEvent;
use super::network::{self, Network, NetworkView, TestRun};
use super::runner::interlocutor_for;
use super::tools::SharedDraft;
use graph_core::agent::conversation::{Conversation, HandoffGuard};
use graph_core::pipeline::{AskOutcome, AskRequest, Pipeline, StepPath};
use graph_core::store::EntryBody;
use graph_core::toolbox::AgentToolbox;
use graph_core::{ExcludingRegistry, NullSink, ToolDef, ToolOutcome};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc::UnboundedSender;

pub const SET_ARTIFACT: &str = "workbench__set_artifact";
pub const TRY_ARTIFACT: &str = "workbench__try_artifact";
pub const SAVE_ARTIFACT: &str = "workbench__save_artifact";
pub const DISCARD_ARTIFACT: &str = "workbench__discard_artifact";
pub const LOAD_ARTIFACT: &str = "workbench__load_artifact";
pub const LIST_ARTIFACTS: &str = "workbench__list_artifacts";
pub const SHOW_ARTIFACT: &str = "workbench__show_artifact";
pub const SET_SKETCH: &str = "workbench__set_sketch";
pub const TEST_NETWORK: &str = "workbench__test_network";
pub const FAIL_TEST: &str = "workbench__fail_test";
pub const SAVE_NETWORK: &str = "workbench__save_network";
pub const DISCARD_NETWORK: &str = "workbench__discard_network";
pub const SET_INTERVIEW: &str = "workbench__set_interview";
pub const PROPOSE_DESIGNS: &str = "workbench__propose_designs";

pub const ARTIFACT_TOOLS: [&str; 7] = [
    SET_ARTIFACT,
    TRY_ARTIFACT,
    SAVE_ARTIFACT,
    DISCARD_ARTIFACT,
    LOAD_ARTIFACT,
    LIST_ARTIFACTS,
    SHOW_ARTIFACT,
];

pub const NETWORK_TOOLS: [&str; 7] = [
    SET_SKETCH,
    TEST_NETWORK,
    FAIL_TEST,
    SAVE_NETWORK,
    DISCARD_NETWORK,
    SET_INTERVIEW,
    PROPOSE_DESIGNS,
];

pub const STOP_TOOLS: [&str; 2] = [SET_INTERVIEW, PROPOSE_DESIGNS];

pub fn set_interview(
    draft: &SharedDraft,
    tx: &UnboundedSender<WorkbenchEvent>,
    input: &Value,
) -> ToolOutcome {
    let applied = draft.lock().unwrap().network.set_interview(input);
    match applied {
        Ok(result) => {
            publish(draft, tx);
            outcome(result, false)
        }
        Err(refusal) => error(refusal),
    }
}

pub fn propose_designs(
    draft: &SharedDraft,
    tx: &UnboundedSender<WorkbenchEvent>,
    input: &Value,
) -> ToolOutcome {
    let applied = draft.lock().unwrap().network.propose_designs(input);
    match applied {
        Ok(result) => {
            publish(draft, tx);
            outcome(result, false)
        }
        Err(refusal) => error(refusal),
    }
}

pub const NETWORK_DRAFTER: &str = "network_drafter";
pub const TOOL_DRAFTER: &str = "tool_drafter";
pub const AGENT_DRAFTER: &str = "agent_drafter";

pub type Tester = Arc<OnceLock<Conversation>>;

const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Agent,
    Plan,
    Tool,
}

impl ArtifactKind {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "agent" => Some(Self::Agent),
            "plan" => Some(Self::Plan),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Plan => "plan",
            Self::Tool => "tool",
        }
    }
}

fn yaml_name(yaml: &str) -> String {
    serde_yaml::from_str::<serde_yaml::Value>(yaml)
        .ok()
        .and_then(|doc| {
            doc.get("name")
                .and_then(|name| name.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| "unnamed".to_string())
}

pub fn view(draft: &SharedDraft) -> NetworkView {
    draft.lock().unwrap().network.view()
}

pub fn publish(draft: &SharedDraft, tx: &UnboundedSender<WorkbenchEvent>) {
    let _ = tx.send(WorkbenchEvent::NetworkChanged(Box::new(view(draft))));
}

fn outcome(result: Value, is_error: bool) -> ToolOutcome {
    ToolOutcome { result, is_error }
}

fn error(message: impl Into<String>) -> ToolOutcome {
    outcome(json!({"error": message.into()}), true)
}

pub async fn check(
    pipeline: &Pipeline,
    network: &Network,
    kind: ArtifactKind,
    yaml: &str,
) -> Vec<String> {
    let over = network::overlay(pipeline, network);
    match kind {
        ArtifactKind::Tool => over.check_tool_file(yaml),
        ArtifactKind::Agent => over.check_agent_file(yaml).await.err().unwrap_or_default(),
        ArtifactKind::Plan => match network::parse_plan(yaml) {
            Ok(doc) => super::tools::plan_problems(&over, &doc),
            Err(problem) => vec![problem],
        },
    }
}

pub async fn put(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<WorkbenchEvent>,
    kind: ArtifactKind,
    name: &str,
    yaml: &str,
) -> ToolOutcome {
    let network = draft.lock().unwrap().network.clone();
    let problems = check(pipeline, &network, kind, yaml).await;
    let placed =
        draft
            .lock()
            .unwrap()
            .network
            .put_draft(kind, name, yaml.to_string(), problems.clone());
    if let Err(refusal) = placed {
        return error(refusal);
    }
    publish(draft, tx);
    outcome(
        json!({"name": name, "kind": kind.label(), "problems": problems}),
        false,
    )
}

pub async fn set_artifact(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<WorkbenchEvent>,
    kind: ArtifactKind,
    input: &Value,
) -> ToolOutcome {
    let Some(yaml) = input["yaml"]
        .as_str()
        .filter(|yaml| !yaml.trim().is_empty())
    else {
        return error("yaml must be the whole file");
    };
    let name = yaml_name(yaml);
    put(draft, pipeline, tx, kind, &name, yaml).await
}

fn pick(
    network: &Network,
    kind: ArtifactKind,
    name: Option<&str>,
) -> Result<(String, String), String> {
    let drafted: Vec<&network::Node> = network
        .nodes
        .iter()
        .filter(|node| node.kind == kind && node.drafted())
        .collect();
    let node = match name {
        Some(name) => {
            let name = network::bare_name(kind, name);
            drafted.into_iter().find(|node| node.name == name)
        }
        None if drafted.len() == 1 => drafted.into_iter().next(),
        None if drafted.is_empty() => None,
        None => {
            return Err(format!(
                "several {} drafts are open ({}); pass `name`",
                kind.label(),
                drafted
                    .iter()
                    .map(|node| node.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    };
    let node = node.ok_or_else(|| {
        format!(
            "there is no {} draft{} to use",
            kind.label(),
            name.map(|name| format!(" named '{name}'"))
                .unwrap_or_default()
        )
    })?;
    Ok((node.name.clone(), node.yaml.clone().unwrap_or_default()))
}

pub async fn try_artifact(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<WorkbenchEvent>,
    input: &Value,
) -> ToolOutcome {
    let picked = pick(
        &draft.lock().unwrap().network,
        ArtifactKind::Tool,
        input["name"].as_str(),
    );
    let (name, yaml) = match picked {
        Ok(found) => found,
        Err(refusal) => return error(refusal),
    };
    let asking = pipeline
        .clone()
        .with_interlocutor(interlocutor_for(pipeline, tx));
    let sample = match &input["input"] {
        Value::Object(_) => input["input"].clone(),
        _ => json!({}),
    };
    let run = asking.try_tool_file(&yaml, sample).await;
    draft
        .lock()
        .unwrap()
        .network
        .record_run(ArtifactKind::Tool, &name, &yaml, run.clone());
    publish(draft, tx);
    outcome(for_the_model(run), false)
}

const SAMPLE_CHARS: usize = 1500;

fn for_the_model(run: Value) -> Value {
    if run["ran"] != json!(true) {
        return run;
    }
    let full = serde_json::to_string_pretty(&run["result"]).unwrap_or_default();
    let total = full.chars().count();
    let mut trimmed = json!({
        "ran": true,
        "is_error": run["is_error"],
        "shape": run["shape"],
    });
    if total <= SAMPLE_CHARS {
        trimmed["result"] = run["result"].clone();
    } else {
        trimmed["sample"] = json!(full.chars().take(SAMPLE_CHARS).collect::<String>());
        trimmed["note"] = json!(format!(
            "the result is {total} characters; this is the first {SAMPLE_CHARS}. Its shape above covers the whole result, and the user sees all of it in the pane."
        ));
    }
    trimmed
}

pub struct Existing {
    pub yaml: String,
    pub editing: Option<PathBuf>,
    pub name: String,
}

pub fn existing(kind: ArtifactKind, name: &str, tool_dirs: &[PathBuf]) -> Result<Existing, String> {
    match kind {
        ArtifactKind::Agent => {
            let (set, _) = graph_core::agent::doc::AgentSet::load(
                graph_core::agent::doc::BUILTINS,
                &graph_core::agent::doc::agent_dirs(),
            );
            let doc = set
                .get(name)
                .ok_or_else(|| format!("there's no agent named '{name}'"))?;
            let (yaml, editing) = match &doc.source {
                graph_core::agent::doc::AgentSource::File(path) => (
                    std::fs::read_to_string(path)
                        .map_err(|e| format!("can't read {}: {e}", path.display()))?,
                    Some(path.clone()),
                ),
                graph_core::agent::doc::AgentSource::Builtin
                | graph_core::agent::doc::AgentSource::Pack { .. } => {
                    let mut value = serde_yaml::to_value(&doc).map_err(|e| e.to_string())?;
                    graph_core::format::stamp(graph_core::format::Kind::Agent, &mut value);
                    (
                        serde_yaml::to_string(&value).map_err(|e| e.to_string())?,
                        None,
                    )
                }
            };
            Ok(Existing {
                yaml,
                editing,
                name: name.to_string(),
            })
        }
        ArtifactKind::Tool => {
            let bare = network::bare_name(kind, name);
            let docs = graph_core::user_tools::load_user_tools(tool_dirs)?;
            let doc = docs.into_iter().find(|doc| doc.name == bare).ok_or_else(|| {
                format!("there's no user tool named '{bare}'; built-in pack tools can't be edited here")
            })?;
            let path = doc
                .path
                .ok_or_else(|| format!("'{bare}' has no file to edit"))?;
            Ok(Existing {
                yaml: std::fs::read_to_string(&path)
                    .map_err(|e| format!("can't read {}: {e}", path.display()))?,
                editing: Some(path),
                name: bare,
            })
        }
        ArtifactKind::Plan => Err("plans open in the plan pane".to_string()),
    }
}

pub async fn open_existing(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<WorkbenchEvent>,
    kind: ArtifactKind,
    name: &str,
) -> Result<String, String> {
    let found = existing(kind, name, &pipeline.drafted.tool_dirs)?;
    if draft
        .lock()
        .unwrap()
        .network
        .node(kind, &found.name)
        .is_some_and(|node| node.drafted())
    {
        return Err(format!(
            "the {} '{}' is already open as a draft",
            kind.label(),
            found.name
        ));
    }
    let network = draft.lock().unwrap().network.clone();
    let problems = check(pipeline, &network, kind, &found.yaml).await;
    draft.lock().unwrap().network.open(
        kind,
        &found.name,
        found.yaml,
        found.editing,
        Some(found.name.clone()),
        problems,
    );
    publish(draft, tx);
    Ok(found.name)
}

pub fn list_agents(pipeline: &Pipeline) -> ToolOutcome {
    let agents: Vec<Value> = pipeline
        .agents
        .iter()
        .map(|doc| {
            json!({
                "name": doc.name,
                "description": doc.description,
                "source": doc.source.describe(),
                "typed": doc.input_schema.is_some() || doc.output_schema.is_some(),
            })
        })
        .collect();
    outcome(json!({"count": agents.len(), "agents": agents}), false)
}

pub async fn list_tools(pipeline: &Pipeline) -> ToolOutcome {
    let user: Vec<Value> = graph_core::user_tools::load_user_tools(&pipeline.drafted.tool_dirs)
        .unwrap_or_default()
        .into_iter()
        .map(|doc| {
            json!({
                "name": format!("{}{}", graph_core::user_tools::USER_TOOL_PREFIX, doc.name),
                "description": doc.description,
                "path": doc.path.map(|path| path.display().to_string()),
            })
        })
        .collect();
    let servers: Vec<Value> = pipeline
        .registry
        .servers()
        .await
        .into_iter()
        .map(|server| json!({"name": server.name, "description": server.description}))
        .collect();
    outcome(json!({"user_tools": user, "mcp_servers": servers}), false)
}

pub fn discard_artifact(
    draft: &SharedDraft,
    tx: &UnboundedSender<WorkbenchEvent>,
    kind: ArtifactKind,
    name: Option<&str>,
) -> Result<String, String> {
    let picked = pick(&draft.lock().unwrap().network, kind, name);
    let (name, _) = picked?;
    let discarded = draft
        .lock()
        .unwrap()
        .network
        .discard(kind, &name)
        .ok_or_else(|| "there is no draft to discard".to_string())?;
    publish(draft, tx);
    Ok(discarded)
}

pub fn discard_network(
    draft: &SharedDraft,
    tx: &UnboundedSender<WorkbenchEvent>,
) -> Option<String> {
    let (empty, drafted) = {
        let mut state = draft.lock().unwrap();
        let empty = state.network.is_empty();
        (empty, state.network.clear())
    };
    if empty {
        return None;
    }
    publish(draft, tx);
    Some(format!("discarded the network and its {drafted} drafts"))
}

pub fn set_sketch(
    draft: &SharedDraft,
    tx: &UnboundedSender<WorkbenchEvent>,
    input: &Value,
) -> ToolOutcome {
    let applied = draft.lock().unwrap().network.set_sketch(input);
    match applied {
        Ok(summary) => {
            publish(draft, tx);
            outcome(summary, false)
        }
        Err(refusal) => error(refusal),
    }
}

pub fn fail_test(
    draft: &SharedDraft,
    tx: &UnboundedSender<WorkbenchEvent>,
    input: &Value,
) -> ToolOutcome {
    let nodes: Vec<String> = input["nodes"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let reason = input["reason"].as_str().unwrap_or_default().to_string();
    if !draft
        .lock()
        .unwrap()
        .network
        .fail_last_test(&reason, &nodes)
    {
        return error("there is no test run to fail; run workbench__test_network first");
    }
    publish(draft, tx);
    outcome(json!({"failed": nodes, "reason": reason}), false)
}

async fn ask_yes(
    pipeline: &Pipeline,
    tx: &UnboundedSender<WorkbenchEvent>,
    what: &str,
    prompt: String,
    key: &str,
) -> Result<(), String> {
    let interlocutor = interlocutor_for(pipeline, tx);
    let answer = interlocutor
        .ask(AskRequest {
            path: StepPath::top(what),
            call_stack: Vec::new(),
            prompt,
            schema: json!({
                "type": "object",
                "required": [key],
                "properties": {key: {"type": "boolean", "description": "Go ahead?"}}
            }),
            default: Some(json!({key: true})),
        })
        .await;
    match answer {
        AskOutcome::Answered(answer) if answer[key] == json!(true) => Ok(()),
        AskOutcome::Unavailable(reason) => Err(format!("the user couldn't be asked: {reason}")),
        _ => Err("the user said no".to_string()),
    }
}

fn ran_in(entries: &[graph_core::NewEntry], tools_used: &[String]) -> Vec<String> {
    let mut ran = BTreeSet::new();
    for entry in entries {
        match &entry.body {
            EntryBody::Message { .. } if entry.author != "user" => {
                ran.insert(entry.author.clone());
            }
            EntryBody::Handoff { to, .. } => {
                ran.insert(to.clone());
            }
            EntryBody::SubagentRun { agent, .. } => {
                ran.insert(agent.clone());
            }
            _ => {}
        }
    }
    ran.extend(tools_used.iter().cloned());
    ran.into_iter().collect()
}

pub async fn test_network(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tester: &Tester,
    tx: &UnboundedSender<WorkbenchEvent>,
    input: &Value,
) -> ToolOutcome {
    let network = draft.lock().unwrap().network.clone();
    let missing = network.missing();
    if !missing.is_empty() {
        return error(format!(
            "not every node is drafted yet: {}",
            missing.join(", ")
        ));
    }
    let broken = network.with_problems();
    if !broken.is_empty() {
        return error(format!(
            "fix these drafts before testing: {}",
            broken.join(" | ")
        ));
    }
    let entry = input["entry"].as_str().unwrap_or_default().to_string();
    let Some(node) = network
        .nodes
        .iter()
        .find(|node| node.name == entry || node.catalog_name() == entry)
        .cloned()
    else {
        return error(format!(
            "`entry` must name an agent or plan in the network, not '{entry}'"
        ));
    };
    let message = input["message"].as_str().unwrap_or_default().to_string();
    let plan_input = match &input["input"] {
        Value::Object(_) => input["input"].clone(),
        _ => json!({}),
    };
    let what = match node.kind {
        ArtifactKind::Agent => format!("send \"{message}\" to {}", node.name),
        _ => format!("run {} with {plan_input}", node.catalog_name()),
    };
    if node.kind == ArtifactKind::Agent && message.trim().is_empty() {
        return error("`message` is required to test an agent");
    }
    if let Err(refusal) = ask_yes(
        pipeline,
        tx,
        "test_network",
        format!("Test the network: {what}? Tools in it run for real, on each agent's own model, and nothing is saved."),
        "run",
    )
    .await
    {
        return error(format!("not tested: {refusal}"));
    }
    draft.lock().unwrap().network.start_test(&node.name);
    publish(draft, tx);
    let asking = pipeline
        .clone()
        .with_interlocutor(interlocutor_for(pipeline, tx));
    let over = Arc::new(network::overlay(&asking, &network));
    let run = match node.kind {
        ArtifactKind::Tool => {
            let called = over
                .registry
                .invoke(&node.catalog_name(), plan_input.clone())
                .await;
            let (passed, answer) = match called {
                Ok(outcome) => (
                    !outcome.is_error,
                    serde_json::to_string(&outcome.result).unwrap_or_default(),
                ),
                Err(failure) => (false, failure.to_string()),
            };
            TestRun {
                entry: node.catalog_name(),
                message: plan_input.to_string(),
                passed,
                ran: vec![node.catalog_name()],
                answer,
            }
        }
        ArtifactKind::Agent => {
            let Some(base) = tester.get() else {
                return error("network tests need the workbench conversation");
            };
            if message.trim().is_empty() {
                return error("`message` is required to test an agent");
            }
            let mut conversation = base.clone();
            conversation.agents = Arc::new(graph_core::agent::doc::AgentSet::layered(
                base.agents.iter().collect(),
                vec![network::network_agents(&network)],
            ));
            let toolbox =
                AgentToolbox::new(over.registry.clone(), over.clone(), over.plans.to_vec());
            conversation.catalog = Arc::new(ExcludingRegistry::new(
                Arc::new(toolbox),
                vec!["plan_and_execute".to_string()],
            ));
            conversation.pipeline = over.clone();
            conversation.context = None;
            conversation.handoff_guard = None;
            conversation.prompt_overrides.clear();
            conversation.events = Arc::new(NullSink);
            let turn =
                tokio::time::timeout(TEST_LIMIT, conversation.run_turn(&[], &node.name, &message))
                    .await
                    .unwrap_or_else(|_| {
                        Err(graph_core::agent::conversation::ConversationError::Setup {
                            agent: node.name.clone(),
                            message: format!("the test ran longer than {}s", TEST_LIMIT.as_secs()),
                        })
                    });
            match turn {
                Ok(turn) => TestRun {
                    entry: node.name.clone(),
                    message: message.clone(),
                    passed: true,
                    ran: ran_in(&turn.entries, &turn.tools_used),
                    answer: turn.text,
                },
                Err(failure) => TestRun {
                    entry: node.name.clone(),
                    message: message.clone(),
                    passed: false,
                    ran: Vec::new(),
                    answer: failure.to_string(),
                },
            }
        }
        _ => {
            let call = match tokio::time::timeout(
                TEST_LIMIT,
                over.call_plan(&node.name, plan_input.clone()),
            )
            .await
            {
                Ok(call) => call,
                Err(_) => graph_core::pipeline::PlanCall {
                    result: json!({"error": format!("the test ran longer than {}s; test with a smaller input", TEST_LIMIT.as_secs())}),
                    is_error: true,
                    aborted: false,
                },
            };
            TestRun {
                entry: node.catalog_name(),
                message: plan_input.to_string(),
                passed: !call.is_error,
                ran: vec![node.catalog_name()],
                answer: serde_json::to_string(&call.result).unwrap_or_default(),
            }
        }
    };
    let report = json!({
        "passed": run.passed,
        "ran": run.ran,
        "answer": run.answer,
        "note": "passed means it ran without an error; judge the answer against the restate, and call workbench__fail_test when it falls short",
    });
    draft.lock().unwrap().network.record_test(run);
    publish(draft, tx);
    outcome(report, false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Configured,
    Project,
    Everywhere,
}

impl Scope {
    pub fn parse(text: Option<&str>) -> Self {
        match text {
            Some("project") => Self::Project,
            Some("everywhere") => Self::Everywhere,
            _ => Self::Configured,
        }
    }
}

pub struct SaveRequest<'a> {
    pub plans_dir: Option<&'a Path>,
    pub root: Option<&'a Path>,
    pub scope: Scope,
    pub skip: Vec<String>,
    pub untested: bool,
    pub confirm: bool,
}

impl SaveRequest<'_> {
    pub fn unattended() -> Self {
        Self {
            plans_dir: None,
            root: None,
            scope: Scope::Configured,
            skip: Vec::new(),
            untested: true,
            confirm: false,
        }
    }
}

fn scoped_dir(scope: Scope, root: Option<&Path>) -> Option<PathBuf> {
    match scope {
        Scope::Configured => None,
        Scope::Project => root.map(|root| crate::runtime::Workspace::new(root).project_dir()),
        Scope::Everywhere => graph_config::global_config_path()
            .parent()
            .map(Path::to_path_buf),
    }
}

fn kind_dir(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Tool => "tools",
        ArtifactKind::Plan => "plans",
        ArtifactKind::Agent => "agents",
    }
}

fn target(
    node: &network::Node,
    pipeline: &Pipeline,
    plans_dir: Option<&Path>,
    scoped: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(path) = &node.editing {
        return Ok(path.clone());
    }
    if let Some(dir) = scoped {
        return Ok(dir
            .join(kind_dir(node.kind))
            .join(format!("{}.yaml", node.name)));
    }
    match node.kind {
        ArtifactKind::Tool => pipeline
            .drafted
            .tools_dir
            .clone()
            .map(|dir| dir.join(format!("{}.yaml", node.name)))
            .ok_or_else(|| "no tools directory is configured ([tools].paths)".to_string()),
        ArtifactKind::Agent => pipeline
            .drafted
            .agents_dir
            .clone()
            .map(|dir| dir.join(format!("{}.yaml", node.name)))
            .ok_or_else(|| "no agents directory is available".to_string()),
        ArtifactKind::Plan => {
            let doc = network::parse_plan(node.yaml.as_deref().unwrap_or_default())?;
            graph_core::pipeline::authoring::target_path(&doc, plans_dir).map_err(|e| e.to_string())
        }
    }
}

pub async fn save_network(
    draft: &SharedDraft,
    pipeline: &Pipeline,
    tx: &UnboundedSender<WorkbenchEvent>,
    request: SaveRequest<'_>,
) -> Result<String, String> {
    let SaveRequest {
        plans_dir,
        root,
        scope,
        skip,
        untested,
        confirm,
    } = request;
    let scoped = scoped_dir(scope, root);
    let network = draft.lock().unwrap().network.clone();
    if network.in_save_order().is_empty() {
        return Err("there is nothing drafted to save".to_string());
    }
    let missing = network.missing();
    if !missing.is_empty() {
        return Err(format!(
            "not every node is drafted yet: {}",
            missing.join(", ")
        ));
    }
    let broken = network.with_problems();
    if !broken.is_empty() {
        return Err(format!(
            "these drafts have problems: {}",
            broken.join(" | ")
        ));
    }
    if !untested && !network.tested() {
        return Err("the network hasn't passed a test since it last changed: test it with workbench__test_network first".to_string());
    }
    let mut plan = Vec::new();
    for node in network.in_save_order() {
        if skip.contains(&node.name) {
            continue;
        }
        plan.push((
            node.clone(),
            target(node, pipeline, plans_dir, scoped.as_deref())?,
        ));
    }
    if plan.is_empty() {
        return Err("every drafted file was left out of the save".to_string());
    }
    if confirm {
        let listing: Vec<String> = plan
            .iter()
            .map(|(node, path)| {
                format!("- {} {} → {}", node.kind.label(), node.name, path.display())
            })
            .collect();
        ask_yes(
            pipeline,
            tx,
            "save_network",
            format!(
                "Save the network: {} files?\n{}",
                plan.len(),
                listing.join("\n")
            ),
            "save",
        )
        .await?;
    }
    let over = network::overlay(pipeline, &network);
    let mut saved = Vec::new();
    for (node, path) in &plan {
        let yaml = node.yaml.clone().unwrap_or_default();
        let replacing = node.editing.is_some();
        let at = node
            .editing
            .clone()
            .or_else(|| scoped.as_ref().map(|_| path.clone()));
        let result = match node.kind {
            ArtifactKind::Tool => {
                let saved = pipeline
                    .save_tool_file_at(&yaml, true, replacing, false, at.as_deref())
                    .await;
                saved["path"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| refusal(&saved))
            }
            ArtifactKind::Agent => {
                let saved = over
                    .save_agent_file_at(&yaml, replacing, false, at.as_deref())
                    .await;
                saved["path"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| refusal(&saved))
            }
            ArtifactKind::Plan => network::parse_plan(&yaml).and_then(|doc| {
                graph_core::pipeline::authoring::write_doc(&doc, path, replacing)
                    .map(|()| path.display().to_string())
                    .map_err(|e| e.to_string())
            }),
        };
        match result {
            Ok(written) => saved.push((node.kind, node.name.clone(), written)),
            Err(problem) => {
                {
                    let mut state = draft.lock().unwrap();
                    for (kind, name, written) in &saved {
                        state
                            .network
                            .mark_saved(*kind, name, PathBuf::from(written));
                    }
                }
                publish(draft, tx);
                return Err(format!(
                    "saved {} of {}; {} {} was not saved: {problem}",
                    saved.len(),
                    plan.len(),
                    node.kind.label(),
                    node.name
                ));
            }
        }
    }
    draft.lock().unwrap().network.clear();
    publish(draft, tx);
    Ok(format!(
        "saved {}",
        saved
            .iter()
            .map(|(_, _, path)| path.clone())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn refusal(saved: &Value) -> String {
    let mut message = saved["error"].as_str().unwrap_or("not saved").to_string();
    if let Some(problems) = saved["problems"]
        .as_array()
        .filter(|problems| !problems.is_empty())
    {
        let listed: Vec<&str> = problems.iter().filter_map(Value::as_str).collect();
        message.push_str(&format!(": {}", listed.join("; ")));
    }
    message
}

pub fn handoff_guard(draft: SharedDraft) -> HandoffGuard {
    Arc::new(move |from: &str, _to: &str| {
        let state = draft.lock().unwrap();
        let unsaved = state.network.view().unsaved();
        (from == NETWORK_DRAFTER && unsaved > 0).then(|| {
            format!("the network has {unsaved} unsaved drafts: save it with workbench__save_network or discard it with workbench__discard_network before handing off")
        })
    })
}

pub fn context_section(draft: &SharedDraft) -> String {
    draft.lock().unwrap().network.context_section()
}

fn kind_property(description: &str) -> Value {
    json!({"type": "string", "enum": ["plan", "agent", "tool"], "description": description})
}

pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: SET_ARTIFACT.to_string(),
            description: "Put a whole agent, tool or plan file in the network draft as the draft \
                          of the sketch node it names, replacing that node's previous draft. It \
                          is checked right away, with nodes of the sketch that aren't drafted yet \
                          counting as existing, and the problems come back; nothing is written to \
                          disk. For a plan, omit `yaml` to use exactly what plan__compose_plan \
                          last returned. With no network in progress, a plan goes to the plan \
                          pane instead; replacing a plan there with unsaved changes fails unless \
                          overwrite_draft is true, which needs the user's confirmation."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": kind_property("What the file is"),
                    "yaml": {"type": "string", "description": "The whole file. Required for an agent or tool; omit it for a plan to use what plan__compose_plan returned"},
                    "overwrite_draft": {"type": "boolean", "description": "Plan pane only: discard unsaved changes to the current plan draft, only after the user confirms"}
                }
            }),
            output_schema: None,
            output_example: Some(
                json!({"name": "changelog_section", "kind": "tool", "problems": []}),
            ),
            read_only: Some(true),
        },
        ToolDef {
            name: TRY_ARTIFACT.to_string(),
            description: "Run a draft. A tool draft runs once on a sample input after the user \
                          approves it, with the result shown in the pane; pass `name` when \
                          several tool drafts are open. The plan in the plan pane runs only when \
                          the user asks: its steps stream to the pane; gated=true makes it a \
                          debug run that pauses for the USER's decision (step / continue / skip \
                          / abort) and breaks on any failing call, and `breakpoints` (top-level \
                          step ids, implies gated) run freely to those steps. Agents and network \
                          plans are tried with workbench__test_network."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": kind_property("What the draft is"),
                    "name": {"type": "string", "description": "Tools: which tool draft, when there are several"},
                    "input": {"type": "object", "description": "A tool's sample input matching its input_schema, or a plan's input validated against its input_schema"},
                    "gated": {"type": "boolean", "description": "Plans only: debug run, pausing for the user's decisions and breaking on errors. Default false."},
                    "breakpoints": {"type": "array", "items": {"type": "string"}, "description": "Plans only: top-level step ids to pause at. Implies gated; the run proceeds until a breakpoint (or a failing call) and pauses for the USER."}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(false),
        },
        ToolDef {
            name: SAVE_ARTIFACT.to_string(),
            description: "Save the plan in the plan pane, only when the user asks to save it. It \
                          goes back to the file it was loaded from, or into the plans directory \
                          when it's new. Agents, tools and network plans are saved together \
                          with workbench__save_network."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": {"type": "string", "enum": ["plan"], "description": "Always plan"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(false),
        },
        ToolDef {
            name: LIST_ARTIFACTS.to_string(),
            description: "List what exists of one kind. Plans: identifier, name, description \
                          and step count. Agents: name, description, where each comes from, and \
                          whether it takes typed input. Tools: the user tools (name, \
                          description, file) and the connected MCP servers."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": kind_property("What to list")
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: SHOW_ARTIFACT.to_string(),
            description: "Read an existing plan, agent or user tool as YAML without touching \
                          any draft, to inspect it or use it as a reference."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind", "name"],
                "properties": {
                    "kind": kind_property("What to read"),
                    "name": {"type": "string", "description": "A plan identifier or plan file path, an agent's name, or a tool's name (with or without user__)"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: LOAD_ARTIFACT.to_string(),
            description: "Open an existing plan, agent or user tool for editing. A plan opens in \
                          the plan pane (it fails when the plan draft has unsaved changes \
                          unless overwrite_draft is true); an agent or tool opens as a node of \
                          the network draft, and saving the network replaces its file (an \
                          edited built-in agent is saved as a project override)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind", "name"],
                "properties": {
                    "kind": kind_property("What to open"),
                    "name": {"type": "string", "description": "A plan identifier or plan file path, an agent's name, or a tool's name (with or without user__)"},
                    "overwrite_draft": {"type": "boolean", "description": "Plans only: load over a plan draft with unsaved changes, only after the user confirms discarding them"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: DISCARD_ARTIFACT.to_string(),
            description: "Throw away one agent, tool or plan draft in the network, only when the \
                          user asks or the sketch dropped it. A sketched node goes back to not \
                          drafted. The plan pane's plan can't be discarded."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": kind_property("What the draft is"),
                    "name": {"type": "string", "description": "Which draft, when there are several of this kind"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: SET_SKETCH.to_string(),
            description: "Put the design sketch of the network in the draft pane, replacing the \
                          previous sketch: the confirmed restate, every node (agent, plan or \
                          tool) with its job, model role and the design principles it applies, \
                          and the edges between nodes. Nodes keep their drafts across sketches; \
                          a drafted node left out of the new sketch is reported so it can be \
                          discarded. Nothing is drafted or written."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["restate", "nodes"],
                "properties": {
                    "restate": {
                        "type": "object",
                        "required": ["outcome", "success", "constraint", "out_of_scope"],
                        "properties": {
                            "outcome": {"type": "string"},
                            "success": {"type": "string"},
                            "constraint": {"type": "string"},
                            "out_of_scope": {"type": "array", "items": {"type": "string"}}
                        }
                    },
                    "nodes": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["name", "kind", "job"],
                            "properties": {
                                "name": {"type": "string", "description": "The file's name: a tool name without user__, a plan identifier, or an agent name"},
                                "kind": {"type": "string", "enum": ["agent", "plan", "tool"]},
                                "job": {"type": "string", "description": "What this node does, in one sentence"},
                                "model": {"type": "string", "description": "The model role it runs on (agents, prompt tools, and plan steps that call a model); omit when it calls no model"},
                                "principles": {"type": "array", "items": {"type": "string"}, "description": "The design principles this node applies"},
                                "takes": {"type": "string", "description": "Its input, field by field, as the nodes that call it will pass it"},
                                "returns": {"type": "string", "description": "Its output, field by field, as the nodes that read it expect it"},
                                "proposed": {"type": "boolean", "description": "True for a node a pending question would add"}
                            }
                        }
                    },
                    "edges": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["from", "to", "kind"],
                            "properties": {
                                "from": {"type": "string", "description": "A node name"},
                                "to": {"type": "string", "description": "A node name, or an existing tool, plan or agent"},
                                "kind": {"type": "string", "enum": ["handoff", "subagent", "calls"]}
                            }
                        }
                    }
                }
            }),
            output_schema: None,
            output_example: Some(json!({"nodes": 4, "edges": 3, "drafted": []})),
            read_only: Some(true),
        },
        ToolDef {
            name: TEST_NETWORK.to_string(),
            description: "Test the drafted, unsaved network once, after the user approves: one \
                          conversation turn starting at an agent node with `message`, or one run \
                          of a plan node with `input`. Every agent runs on its own model and \
                          tools run for real; nothing is saved. Returns which nodes ran and the \
                          answer. Every node has to be drafted without problems first."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["entry"],
                "properties": {
                    "entry": {"type": "string", "description": "The agent or plan node the test starts at"},
                    "message": {"type": "string", "description": "Agents: the user message to send"},
                    "input": {"type": "object", "description": "Plans: the plan's input"}
                }
            }),
            output_schema: None,
            output_example: Some(
                json!({"passed": true, "ran": ["release_desk", "plan__release_digest"], "answer": "…"}),
            ),
            read_only: Some(false),
        },
        ToolDef {
            name: FAIL_TEST.to_string(),
            description: "Mark the last network test as failed when its answer falls short of \
                          the restate, naming the nodes at fault and why. The pane shows it, and \
                          the network has to pass a new test before it can be saved."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["reason"],
                "properties": {
                    "reason": {"type": "string"},
                    "nodes": {"type": "array", "items": {"type": "string"}}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: SAVE_NETWORK.to_string(),
            description: "Save every drafted node of the network, after the user confirms the \
                          list of files: tools first, then plans, then agents. Every sketched \
                          node has to be drafted without problems, and the network has to have \
                          passed a test since it last changed unless `untested` is true because \
                          the user said not to test it."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "untested": {"type": "boolean", "description": "Save without a passing test, only when the user said not to test it"},
                    "scope": {"type": "string", "enum": ["project", "everywhere"], "description": "This project's .graph, or the global config for every project; omit for the configured directories"},
                    "skip": {"type": "array", "items": {"type": "string"}, "description": "Node names the user chose not to save"}
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(false),
        },
        ToolDef {
            name: SET_INTERVIEW.to_string(),
            description: "Ask the user your next interview question and end your turn. `reply` \
                          is your whole reply as the user reads it; the pane shows the interview: \
                          your confidence, what is still missing, the question with your guess, \
                          and what you know so far as facts (known, guess, asking, missing). Use \
                          it for every interview reply, including the restate you ask the user \
                          to confirm (confirmed: false until they say yes)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["reply", "confidence", "facts"],
                "properties": {
                    "reply": {"type": "string", "description": "Your whole reply to the user, as they read it in the chat: your read of the request, the question and your guess, or the restate"},
                    "confidence": {"type": "integer", "minimum": 0, "maximum": 100},
                    "missing": {"type": "string", "description": "What you still don't know, in a few words; empty when nothing is"},
                    "question": {"type": "string", "description": "The one question you're asking now, when you're asking one"},
                    "guess": {"type": "string", "description": "Your guess at its answer, with the reasoning"},
                    "facts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["label", "text", "state"],
                            "properties": {
                                "label": {"type": "string", "description": "Outcome, Who, Where, Success, Constraint, Out of scope, or a topic of this request"},
                                "text": {"type": "string"},
                                "state": {"type": "string", "enum": ["known", "guess", "asking", "missing"]},
                                "previous": {"type": "string", "description": "When feedback reopens a confirmed fact: what it said before"}
                            }
                        }
                    },
                    "confirmed": {"type": "boolean", "description": "True once the user said yes to the restate"}
                }
            }),
            output_schema: None,
            output_example: Some(json!({"confidence": 55, "shown": true})),
            read_only: Some(true),
        },
        ToolDef {
            name: PROPOSE_DESIGNS.to_string(),
            description: "Push back on a wasteful design: show the user's design and your better \
                          one side by side in the pane, and end your turn. Put the pushback in \
                          your response text and ask which they want."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["reply", "yours", "suggested"],
                "properties": {
                    "reply": {"type": "string", "description": "Your pushback as the user reads it, ending with the question which design they want"},
                    "yours": {"$ref": "#/$defs/design"},
                    "suggested": {"$ref": "#/$defs/design"}
                },
                "$defs": {
                    "design": {
                        "type": "object",
                        "required": ["title", "nodes", "lines"],
                        "properties": {
                            "title": {"type": "string"},
                            "model": {"type": "string", "description": "The model it mostly runs on, when one dominates"},
                            "nodes": {"type": "array", "items": {"type": "object", "required": ["kind", "name"], "properties": {"kind": {"type": "string", "enum": ["agent", "plan", "tool"]}, "name": {"type": "string"}}}},
                            "lines": {"type": "array", "items": {"type": "string"}, "description": "Short facts: what it is, the calls per request, how it's tested"},
                            "principles": {"type": "array", "items": {"type": "string"}}
                        }
                    }
                }
            }),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
        ToolDef {
            name: DISCARD_NETWORK.to_string(),
            description: "Throw away the whole network draft (sketch and every draft), only when \
                          the user asks."
                .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            output_schema: None,
            output_example: None,
            read_only: Some(true),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_test_result_reaches_the_model_as_its_shape_and_a_sample() {
        let long = "x".repeat(5000);
        let trimmed = for_the_model(json!({
            "ran": true,
            "is_error": false,
            "result": {"text": long},
            "shape": {"type": "object", "properties": {"text": {"type": "string"}}}
        }));
        assert!(trimmed.get("result").is_none());
        assert_eq!(trimmed["shape"]["properties"]["text"]["type"], "string");
        assert_eq!(
            trimmed["sample"].as_str().unwrap().chars().count(),
            SAMPLE_CHARS
        );
        assert!(trimmed["note"].as_str().unwrap().contains("characters"));

        let short = for_the_model(
            json!({"ran": true, "is_error": false, "result": {"ok": 1}, "shape": {}}),
        );
        assert_eq!(short["result"], json!({"ok": 1}));
    }

    #[test]
    fn the_test_run_names_every_agent_and_tool_that_took_part() {
        let entries = vec![
            graph_core::NewEntry::new(
                "triage",
                EntryBody::Handoff {
                    from: "triage".into(),
                    to: "release_desk".into(),
                    message: String::new(),
                    via: None,
                },
            ),
            graph_core::NewEntry::new(
                "release_desk",
                EntryBody::SubagentRun {
                    agent: "risk_check".into(),
                    caller: "release_desk".into(),
                    input: json!({}),
                    messages: Vec::new(),
                    output: json!({}),
                    final_: true,
                },
            ),
        ];
        assert_eq!(
            ran_in(&entries, &["plan__release_digest".to_string()]),
            ["plan__release_digest", "release_desk", "risk_check"]
        );
    }
}
