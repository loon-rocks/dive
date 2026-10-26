use super::artifact::{ArtifactKind, Existing};
use super::secrets::{SecretState, TestSecret};
use async_trait::async_trait;
use dive_core::agent::doc::{parse_agent_source, AgentDoc, AgentSet};
use dive_core::packs::{ItemKind, PackSource};
use dive_core::pipeline::doc::PlanDoc;
use dive_core::pipeline::Pipeline;
use dive_core::secrets::SecretSource;
use dive_core::user_tools::{UserToolDoc, UserToolRegistry, BUILTIN_TOOL_PREFIX, USER_TOOL_PREFIX};
use dive_core::{CompositeRegistry, ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct Restate {
    pub outcome: String,
    pub problem: String,
    pub change: String,
    pub success: String,
    pub constraint: String,
    pub out_of_scope: Vec<String>,
}

impl Restate {
    pub fn is_edit(&self) -> bool {
        !self.problem.is_empty() || !self.change.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditRole {
    Edit,
    New,
    AsIs,
}

impl EditRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::New => "new",
            Self::AsIs => "as is",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Handoff,
    Subagent,
    Calls,
}

impl EdgeKind {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "handoff" => Some(Self::Handoff),
            "subagent" => Some(Self::Subagent),
            "calls" => Some(Self::Calls),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Handoff => "handoff",
            Self::Subagent => "subagent",
            Self::Calls => "calls",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Sketched,
    Proposed,
    Changed,
    Drafting,
    Question,
    Drafted,
    Ready,
    Failed,
}

impl NodeStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sketched => "sketched",
            Self::Proposed => "proposed",
            Self::Changed => "changed",
            Self::Drafting => "drafting",
            Self::Question => "question",
            Self::Drafted => "drafted",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FactState {
    Known,
    Guess,
    Asking,
    Missing,
}

impl FactState {
    fn parse(text: &str) -> Self {
        match text {
            "known" => Self::Known,
            "guess" => Self::Guess,
            "asking" => Self::Asking,
            _ => Self::Missing,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Known => "known",
            Self::Guess => "guess",
            Self::Asking => "asking",
            Self::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Fact {
    pub label: String,
    pub text: String,
    pub state: FactState,
    pub previous: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct Interview {
    pub reply: String,
    pub confidence: u8,
    pub missing: String,
    pub question: Option<String>,
    pub guess: Option<String>,
    pub facts: Vec<Fact>,
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct Design {
    pub title: String,
    pub model: Option<String>,
    pub nodes: Vec<(ArtifactKind, String)>,
    pub lines: Vec<String>,
    pub principles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct Designs {
    pub yours: Design,
    pub suggested: Design,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Origin {
    #[default]
    New,
    File,
    Builtin,
    Pack(PackSource),
}

impl Origin {
    pub fn label(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::File => "file",
            Self::Builtin => "builtin",
            Self::Pack(_) => "pack",
        }
    }

    pub fn destination(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::File => "file",
            Self::Builtin => "override",
            Self::Pack(_) => "pack",
        }
    }

    pub fn pack(&self) -> Option<&PackSource> {
        match self {
            Self::Pack(source) => Some(source),
            _ => None,
        }
    }
}

fn item_kind(kind: ArtifactKind) -> ItemKind {
    match kind {
        ArtifactKind::Tool => ItemKind::Tool,
        ArtifactKind::Plan => ItemKind::Plan,
        ArtifactKind::Agent => ItemKind::Agent,
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub kind: ArtifactKind,
    pub job: String,
    pub model: Option<String>,
    pub principles: Vec<String>,
    pub takes: String,
    pub returns: String,
    pub change: String,
    pub sketched: bool,
    pub status: NodeStatus,
    pub yaml: Option<String>,
    pub problems: Vec<String>,
    pub last_run: Option<Value>,
    pub editing: Option<PathBuf>,
    pub original: Option<String>,
    pub builder: Option<String>,
    pub note: Option<String>,
    pub origin: Origin,
    pub base: Option<String>,
}

impl Node {
    fn sketched(name: String, kind: ArtifactKind, job: String) -> Self {
        Self {
            name,
            kind,
            job,
            model: None,
            principles: Vec::new(),
            takes: String::new(),
            returns: String::new(),
            change: String::new(),
            sketched: true,
            status: NodeStatus::Sketched,
            yaml: None,
            problems: Vec::new(),
            last_run: None,
            editing: None,
            original: None,
            builder: None,
            note: None,
            origin: Origin::New,
            base: None,
        }
    }

    pub fn runtime_name(&self) -> String {
        match &self.origin {
            Origin::Pack(source) => {
                dive_core::packs::catalog_name(&source.name, item_kind(self.kind), &self.name)
            }
            _ => self.name.clone(),
        }
    }

    pub fn catalog_name(&self) -> String {
        match (&self.origin, self.kind) {
            (Origin::Pack(_), _) => self.runtime_name(),
            (Origin::Builtin, ArtifactKind::Tool) => format!("{BUILTIN_TOOL_PREFIX}{}", self.name),
            _ => catalog_name(self.kind, &self.name),
        }
    }

    pub fn names(&self, name: &str) -> bool {
        self.name == name || self.catalog_name() == name || self.runtime_name() == name
    }

    pub fn drafted(&self) -> bool {
        self.yaml.is_some()
    }

    pub fn role(&self) -> EditRole {
        match (&self.base, self.change.is_empty()) {
            (None, _) => EditRole::New,
            (Some(_), false) => EditRole::Edit,
            (Some(_), true) => EditRole::AsIs,
        }
    }

    pub fn edit_pending(&self) -> bool {
        self.sketched && self.role() == EditRole::Edit && !self.changed()
    }

    pub fn changed(&self) -> bool {
        match (&self.yaml, &self.base) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(yaml), Some(base)) => yaml != base && !same_document(yaml, base),
        }
    }
}

fn same_document(a: &str, b: &str) -> bool {
    let parse = |text: &str| serde_yaml::from_str::<serde_yaml::Value>(text).ok();
    matches!((parse(a), parse(b)), (Some(a), Some(b)) if a == b)
}

pub fn catalog_name(kind: ArtifactKind, name: &str) -> String {
    match kind {
        ArtifactKind::Tool => format!("{USER_TOOL_PREFIX}{name}"),
        ArtifactKind::Plan => format!("plan__{name}"),
        ArtifactKind::Agent => format!("agent__{name}"),
    }
}

pub fn bare_name(kind: ArtifactKind, name: &str) -> String {
    let prefix = match kind {
        ArtifactKind::Tool => USER_TOOL_PREFIX,
        ArtifactKind::Plan => "plan__",
        ArtifactKind::Agent => "agent__",
    };
    name.strip_prefix(prefix).unwrap_or(name).to_string()
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TestRun {
    pub entry: String,
    pub message: String,
    pub passed: bool,
    pub ran: Vec<String>,
    pub answer: String,
}

#[derive(Debug, Clone, Default)]
pub struct Network {
    pub restate: Option<Restate>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub tests: Vec<TestRun>,
    pub interview: Option<Interview>,
    pub designs: Option<Designs>,
    pub testing: Option<String>,
    pub catalog: Vec<(String, String)>,
    pub secrets: Option<Arc<dyn SecretSource>>,
    pub test_secrets: Vec<TestSecret>,
    pub waiting_on_secrets: bool,
    revision: u64,
    tested: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NodeSecret {
    pub name: String,
    pub description: Option<String>,
    pub set: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct NodeView {
    pub name: String,
    pub kind: ArtifactKind,
    pub job: String,
    pub model: Option<String>,
    pub principles: Vec<String>,
    pub takes: String,
    pub returns: String,
    pub change: String,
    pub role: EditRole,
    pub sketched: bool,
    pub status: NodeStatus,
    pub yaml: Option<String>,
    pub problems: Vec<String>,
    pub last_run: Option<Value>,
    pub builder: Option<String>,
    pub note: Option<String>,
    pub secrets: Vec<NodeSecret>,
    pub origin: &'static str,
    pub pack: Option<String>,
    pub path: Option<String>,
    pub base: Option<String>,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct NetworkView {
    pub restate: Option<Restate>,
    pub nodes: Vec<NodeView>,
    pub edges: Vec<Edge>,
    pub tests: Vec<TestRun>,
    pub tested: bool,
    pub interview: Option<Interview>,
    pub designs: Option<Designs>,
    pub testing: Option<String>,
    pub test_secrets: Vec<SecretState>,
    pub waiting_on_secrets: bool,
}

impl NetworkView {
    pub fn is_empty(&self) -> bool {
        self.restate.is_none()
            && self.nodes.is_empty()
            && self.interview.is_none()
            && self.designs.is_none()
    }

    pub fn unsaved(&self) -> usize {
        self.nodes.iter().filter(|node| node.changed).count()
    }

    pub fn is_edit(&self) -> bool {
        self.nodes.iter().any(|node| node.base.is_some())
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().trim().to_string()
}

impl Network {
    pub fn is_empty(&self) -> bool {
        self.restate.is_none()
            && self.nodes.is_empty()
            && self.interview.is_none()
            && self.designs.is_none()
    }

    pub fn set_interview(&mut self, input: &Value) -> Result<Value, String> {
        let confidence = input["confidence"]
            .as_u64()
            .ok_or("confidence is required: a whole number from 0 to 100")?
            .min(100) as u8;
        let facts = input["facts"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|fact| Fact {
                label: text(fact, "label"),
                text: text(fact, "text"),
                state: FactState::parse(fact["state"].as_str().unwrap_or_default()),
                previous: fact["previous"]
                    .as_str()
                    .map(str::to_string)
                    .filter(|previous| !previous.is_empty()),
            })
            .filter(|fact| !fact.label.is_empty())
            .collect();
        let optional = |key: &str| {
            input[key]
                .as_str()
                .map(str::to_string)
                .filter(|value| !value.trim().is_empty())
        };
        self.interview = Some(Interview {
            reply: text(input, "reply"),
            confidence,
            missing: text(input, "missing"),
            question: optional("question"),
            guess: optional("guess"),
            facts,
            confirmed: input["confirmed"].as_bool().unwrap_or(false),
        });
        self.changed();
        Ok(json!({"confidence": confidence, "shown": true}))
    }

    pub fn propose_designs(&mut self, input: &Value) -> Result<Value, String> {
        let design = |value: &Value| -> Result<Design, String> {
            let title = text(value, "title");
            if title.is_empty() {
                return Err("each design needs a title".to_string());
            }
            Ok(Design {
                title,
                model: value["model"].as_str().map(str::to_string),
                nodes: value["nodes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|node| {
                        let kind = ArtifactKind::parse(node["kind"].as_str()?)?;
                        Some((kind, bare_name(kind, node["name"].as_str()?)))
                    })
                    .collect(),
                lines: strings(&value["lines"]),
                principles: strings(&value["principles"]),
            })
        };
        self.designs = Some(Designs {
            yours: design(&input["yours"])?,
            suggested: design(&input["suggested"])?,
        });
        self.changed();
        Ok(json!({"shown": true}))
    }

    pub fn builder_started(&mut self, node: &str, builder: &str) {
        self.designs = None;
        if let Some(found) = self.nodes.iter_mut().find(|found| found.name == node) {
            found.status = NodeStatus::Drafting;
            found.builder = Some(builder.to_string());
            found.note = Some(format!("{builder} is drafting it"));
        }
    }

    pub fn builder_finished(&mut self, node: &str, builder: &str, output: &Value, failed: bool) {
        let Some(found) = self.nodes.iter_mut().find(|found| found.name == node) else {
            return;
        };
        let summary = output["summary"].as_str().unwrap_or_default().to_string();
        let (status, note) = match (failed, output["status"].as_str()) {
            (true, _) | (_, Some("failed")) => (
                NodeStatus::Failed,
                if summary.is_empty() {
                    format!("{builder} couldn't build it")
                } else {
                    format!("{builder} couldn't build it: {summary}")
                },
            ),
            (_, Some("question")) => (
                NodeStatus::Question,
                format!("{builder} is waiting on your answer"),
            ),
            _ if found.yaml.is_some() => (
                if found.problems.is_empty() {
                    NodeStatus::Drafted
                } else {
                    NodeStatus::Failed
                },
                if summary.is_empty() {
                    builder.to_string()
                } else {
                    format!("{builder} · {summary}")
                },
            ),
            _ => (
                NodeStatus::Sketched,
                format!("{builder} returned without a draft"),
            ),
        };
        found.status = status;
        found.builder = Some(builder.to_string());
        found.note = Some(note);
    }

    pub fn start_test(&mut self, entry: &str) {
        self.testing = Some(entry.to_string());
        self.waiting_on_secrets = false;
    }

    pub fn checked_secrets(&mut self, needed: Vec<TestSecret>) {
        self.test_secrets = needed;
        self.waiting_on_secrets = false;
    }

    pub fn wait_on_secrets(&mut self, needed: Vec<TestSecret>) {
        self.test_secrets = needed;
        self.waiting_on_secrets = true;
        self.testing = None;
    }

    fn node_secrets(&self, node: &Node) -> Vec<NodeSecret> {
        if node.kind != ArtifactKind::Tool {
            return Vec::new();
        }
        let Some(doc) = node
            .yaml
            .as_deref()
            .and_then(|yaml| dive_core::user_tools::parse_tool_source(yaml).ok())
        else {
            return Vec::new();
        };
        dive_core::user_tools::needed_secrets(&doc)
            .into_iter()
            .map(|needed| NodeSecret {
                set: self
                    .secrets
                    .as_deref()
                    .is_some_and(|source| dive_core::secrets::is_set(source, &needed.name)),
                name: needed.name,
                description: needed.description,
            })
            .collect()
    }

    pub fn node(&self, kind: ArtifactKind, name: &str) -> Option<&Node> {
        self.nodes
            .iter()
            .find(|node| node.kind == kind && node.name == name)
    }

    fn node_mut(&mut self, kind: ArtifactKind, name: &str) -> Option<&mut Node> {
        self.nodes
            .iter_mut()
            .find(|node| node.kind == kind && node.name == name)
    }

    pub fn tested(&self) -> bool {
        self.tested == Some(self.revision) && !self.nodes.is_empty()
    }

    fn changed(&mut self) {
        self.revision += 1;
    }

    pub fn set_sketch(&mut self, input: &Value) -> Result<Value, String> {
        let restate = &input["restate"];
        let restate = Restate {
            outcome: text(restate, "outcome"),
            problem: text(restate, "problem"),
            change: text(restate, "change"),
            success: text(restate, "success"),
            constraint: text(restate, "constraint"),
            out_of_scope: strings(&restate["out_of_scope"]),
        };
        if restate.is_edit() {
            if restate.problem.is_empty() || restate.change.is_empty() || restate.success.is_empty()
            {
                return Err(
                    "an edit's restate needs problem, change and success: what went wrong or what's wanted, the change, and how the test shows it worked"
                        .to_string(),
                );
            }
        } else if restate.outcome.is_empty() {
            return Err(
                "restate.outcome is required: the confirmed restate goes with every sketch (an edit passes problem, change and success instead)"
                    .to_string(),
            );
        }
        let mut sketched = Vec::new();
        for raw in input["nodes"].as_array().cloned().unwrap_or_default() {
            let kind = raw["kind"]
                .as_str()
                .and_then(ArtifactKind::parse)
                .ok_or_else(|| {
                    format!("node {raw}: kind must be \"agent\", \"plan\" or \"tool\"")
                })?;
            let name = bare_name(kind, &text(&raw, "name"));
            if name.is_empty() {
                return Err(format!("node {raw}: name is required"));
            }
            if sketched.iter().any(|(_, other): &(ArtifactKind, Value)| {
                bare_name(kind, &text(other, "name")) == name
            }) {
                return Err(format!(
                    "two nodes are named '{name}'; node names are unique"
                ));
            }
            sketched.push((kind, raw));
        }
        if sketched.is_empty() {
            return Err("a sketch needs at least one node".to_string());
        }
        let mut edges = Vec::new();
        for raw in input["edges"].as_array().cloned().unwrap_or_default() {
            let kind = raw["kind"]
                .as_str()
                .and_then(EdgeKind::parse)
                .ok_or_else(|| {
                    format!("edge {raw}: kind must be \"handoff\", \"subagent\" or \"calls\"")
                })?;
            let from = text(&raw, "from");
            let to = text(&raw, "to");
            let known = |name: &str| {
                sketched
                    .iter()
                    .any(|(kind, node)| bare_name(*kind, &text(node, "name")) == name)
            };
            if !known(&from) {
                return Err(format!(
                    "edge from '{from}': no node by that name in the sketch"
                ));
            }
            if to.is_empty() {
                return Err(format!("edge from '{from}': `to` is required"));
            }
            edges.push(Edge { from, to, kind });
        }
        let mut nodes = Vec::new();
        for (kind, raw) in &sketched {
            let name = bare_name(*kind, &text(raw, "name"));
            let mut node = self
                .nodes
                .iter()
                .find(|node| node.kind == *kind && node.name == name)
                .cloned()
                .unwrap_or_else(|| Node::sketched(name.clone(), *kind, String::new()));
            let job = text(raw, "job");
            let model = raw["model"]
                .as_str()
                .map(str::to_string)
                .filter(|m| !m.is_empty());
            let principles = strings(&raw["principles"]);
            let existed = self.restate.is_some()
                && self
                    .nodes
                    .iter()
                    .any(|old| old.kind == *kind && old.name == name);
            let revised = existed
                && (node.job != job || node.model != model || node.principles != principles);
            node.job = job;
            node.model = model;
            node.principles = principles;
            node.takes = text(raw, "takes");
            node.returns = text(raw, "returns");
            node.change = text(raw, "change");
            node.sketched = true;
            if raw["proposed"].as_bool().unwrap_or(false) && !node.drafted() {
                node.status = NodeStatus::Proposed;
            } else if revised && !node.drafted() {
                node.status = NodeStatus::Changed;
            } else if !node.drafted()
                && matches!(node.status, NodeStatus::Proposed | NodeStatus::Changed)
            {
                node.status = NodeStatus::Sketched;
            }
            nodes.push(node);
        }
        let mut unsketched = Vec::new();
        for node in &self.nodes {
            let kept = nodes
                .iter()
                .any(|kept: &Node| kept.kind == node.kind && kept.name == node.name);
            if !kept && node.drafted() {
                let mut node = node.clone();
                node.sketched = false;
                unsketched.push(node.name.clone());
                nodes.push(node);
            }
        }
        self.restate = Some(restate);
        self.nodes = nodes;
        self.edges = edges;
        self.designs = None;
        self.changed();
        let mut summary = json!({
            "nodes": self.nodes.iter().filter(|node| node.sketched).count(),
            "edges": self.edges.len(),
            "drafted": self.nodes.iter().filter(|node| node.sketched && node.drafted()).map(|node| node.name.clone()).collect::<Vec<_>>(),
        });
        if !unsketched.is_empty() {
            summary["no_longer_sketched"] = json!(unsketched);
            summary["note"] = json!("these drafts are no longer in the sketch; discard them with workbench__discard_artifact, or put them back in the sketch");
        }
        Ok(summary)
    }

    pub fn put_draft(
        &mut self,
        kind: ArtifactKind,
        name: &str,
        yaml: String,
        problems: Vec<String>,
    ) -> Result<(), String> {
        let sketch = self.restate.is_some();
        match self.node_mut(kind, name) {
            Some(node) => {
                node.yaml = Some(yaml);
                node.problems = problems;
                node.last_run = None;
                if node.status != NodeStatus::Drafting {
                    node.status = NodeStatus::Drafted;
                }
            }
            None if sketch => {
                let names: Vec<String> = self
                    .nodes
                    .iter()
                    .filter(|node| node.kind == kind && node.sketched)
                    .map(|node| node.name.clone())
                    .collect();
                return Err(format!(
                    "the sketch has no {} named '{name}'{}; draft only nodes from the sketch, under their sketched names",
                    kind.label(),
                    if names.is_empty() {
                        format!(" (it has no {} nodes)", kind.label())
                    } else {
                        format!(" (its {} nodes: {})", kind.label(), names.join(", "))
                    }
                ));
            }
            None => {
                let mut node = Node::sketched(name.to_string(), kind, String::new());
                node.sketched = false;
                node.yaml = Some(yaml);
                node.problems = problems;
                node.status = NodeStatus::Drafted;
                self.nodes.push(node);
            }
        }
        self.changed();
        Ok(())
    }

    pub fn open(&mut self, kind: ArtifactKind, found: Existing, problems: Vec<String>) {
        let name = found.name.clone();
        if self.node(kind, &name).is_none() {
            let mut node = Node::sketched(name.clone(), kind, String::new());
            node.sketched = false;
            self.nodes.push(node);
        }
        if let Some(node) = self.node_mut(kind, &name) {
            node.yaml = Some(found.yaml.clone());
            node.base = Some(found.yaml);
            node.problems = problems;
            node.last_run = None;
            node.status = NodeStatus::Drafted;
            node.editing = found.editing;
            node.original = Some(name);
            node.origin = found.origin;
        }
        self.changed();
    }

    pub fn is_edit(&self) -> bool {
        self.nodes.iter().any(|node| node.base.is_some())
    }

    fn close_sketch(&mut self) {
        self.restate = None;
        self.edges.clear();
        self.interview = None;
        self.designs = None;
        for node in &mut self.nodes {
            node.sketched = false;
            node.change.clear();
        }
    }

    pub fn saved(&mut self, written: &[(ArtifactKind, String, PathBuf)]) {
        if !self.is_edit() {
            self.clear();
            return;
        }
        self.close_sketch();
        for (kind, name, path) in written {
            if let Some(node) = self.node_mut(*kind, name) {
                node.base = node.yaml.clone();
                node.editing = Some(path.clone());
                if node.origin == Origin::New {
                    node.origin = Origin::File;
                }
                node.status = NodeStatus::Drafted;
            }
        }
        self.tests.clear();
        self.untest();
        self.changed();
    }

    pub fn discard_all(&mut self) -> usize {
        if !self.is_edit() {
            return self.clear();
        }
        let changed = self.nodes.iter().filter(|node| node.changed()).count();
        self.close_sketch();
        self.nodes.retain(|node| node.base.is_some());
        for node in &mut self.nodes {
            node.yaml = node.base.clone();
            node.problems.clear();
            node.last_run = None;
            node.status = NodeStatus::Drafted;
            node.builder = None;
            node.note = None;
        }
        self.tests.clear();
        self.testing = None;
        self.untest();
        self.changed();
        changed
    }

    pub fn has_edits(&self) -> bool {
        self.nodes.iter().any(Node::changed)
    }

    pub fn untest(&mut self) {
        self.tested = None;
    }

    pub fn mark_saved(&mut self, kind: ArtifactKind, name: &str, path: PathBuf) {
        if let Some(node) = self.node_mut(kind, name) {
            node.editing = Some(path);
        }
    }

    pub fn record_run(&mut self, kind: ArtifactKind, name: &str, yaml: &str, run: Value) {
        if let Some(node) = self.node_mut(kind, name) {
            if node.yaml.as_deref() == Some(yaml) {
                node.last_run = Some(run);
            }
        }
    }

    pub fn discard(&mut self, kind: ArtifactKind, name: &str) -> Option<String> {
        let index = self
            .nodes
            .iter()
            .position(|node| node.kind == kind && node.name == name && node.drafted())?;
        let node = &mut self.nodes[index];
        if node.sketched && node.base.is_some() {
            node.yaml = node.base.clone();
            node.problems.clear();
            node.last_run = None;
            node.status = NodeStatus::Drafted;
        } else if node.sketched {
            node.yaml = None;
            node.problems.clear();
            node.last_run = None;
            node.status = NodeStatus::Sketched;
        } else {
            self.nodes.remove(index);
        }
        self.changed();
        Some(format!("discarded the {} draft '{name}'", kind.label()))
    }

    pub fn record_test(&mut self, run: TestRun) {
        self.testing = None;
        if run.passed {
            self.tested = Some(self.revision);
            for node in &mut self.nodes {
                if node.drafted() && node.problems.is_empty() {
                    node.status =
                        if run.ran.contains(&node.catalog_name()) || run.ran.contains(&node.name) {
                            NodeStatus::Ready
                        } else {
                            node.status
                        };
                }
            }
        }
        self.tests.push(run);
    }

    pub fn fail_last_test(&mut self, reason: &str, nodes: &[String]) -> bool {
        let Some(last) = self.tests.last_mut() else {
            return false;
        };
        last.passed = false;
        if !reason.is_empty() {
            last.answer = format!("{} (failed: {reason})", last.answer);
        }
        self.untest();
        self.mark_failed(nodes);
        true
    }

    pub fn mark_failed(&mut self, names: &[String]) {
        for node in &mut self.nodes {
            if names.contains(&node.name) || names.contains(&node.catalog_name()) {
                node.status = NodeStatus::Failed;
            }
        }
    }

    pub fn view(&self) -> NetworkView {
        NetworkView {
            restate: self.restate.clone(),
            nodes: self
                .nodes
                .iter()
                .map(|node| NodeView {
                    name: node.name.clone(),
                    kind: node.kind,
                    job: node.job.clone(),
                    model: node.model.clone(),
                    principles: node.principles.clone(),
                    takes: node.takes.clone(),
                    returns: node.returns.clone(),
                    change: node.change.clone(),
                    role: node.role(),
                    sketched: node.sketched,
                    status: node.status,
                    yaml: node.yaml.clone(),
                    problems: node.problems.clone(),
                    last_run: node.last_run.clone(),
                    builder: node.builder.clone(),
                    note: node.note.clone(),
                    secrets: self.node_secrets(node),
                    origin: node.origin.label(),
                    pack: node.origin.pack().map(|source| source.name.clone()),
                    path: node.editing.as_ref().map(|path| path.display().to_string()),
                    base: node.base.clone(),
                    changed: node.changed(),
                })
                .collect(),
            edges: self.edges.clone(),
            tests: self.tests.clone(),
            tested: self.tested(),
            interview: self.interview.clone(),
            designs: self.designs.clone(),
            testing: self.testing.clone(),
            test_secrets: super::secrets::with_state(&self.test_secrets, self.secrets.as_deref()),
            waiting_on_secrets: self.waiting_on_secrets,
        }
    }

    pub fn missing(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| node.sketched && (!node.drafted() || node.edit_pending()))
            .map(|node| {
                if node.edit_pending() {
                    format!(
                        "{} {} (its edit isn't made yet)",
                        node.kind.label(),
                        node.name
                    )
                } else {
                    format!("{} {}", node.kind.label(), node.name)
                }
            })
            .collect()
    }

    pub fn with_problems(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| node.changed() && !node.problems.is_empty())
            .map(|node| {
                format!(
                    "{} {}: {}",
                    node.kind.label(),
                    node.name,
                    node.problems.join("; ")
                )
            })
            .collect()
    }

    pub fn in_save_order(&self) -> Vec<&Node> {
        let rank = |kind: ArtifactKind| match kind {
            ArtifactKind::Tool => 0,
            ArtifactKind::Plan => 1,
            ArtifactKind::Agent => 2,
        };
        let mut nodes: Vec<&Node> = self.nodes.iter().filter(|node| node.changed()).collect();
        nodes.sort_by_key(|node| rank(node.kind));
        nodes
    }

    pub fn clear(&mut self) -> usize {
        let drafted = self.nodes.iter().filter(|node| node.drafted()).count();
        *self = Network {
            revision: self.revision + 1,
            secrets: self.secrets.take(),
            ..Network::default()
        };
        drafted
    }

    pub fn context_section(&self) -> String {
        let mut section = String::from("## The network draft\n");
        if self.is_empty() {
            section.push_str("(empty: nothing is sketched or drafted yet)\n");
            return section;
        }
        if let Some(restate) = &self.restate {
            let out_of_scope = if restate.out_of_scope.is_empty() {
                "(nothing named)".to_string()
            } else {
                restate.out_of_scope.join("; ")
            };
            if restate.is_edit() {
                section.push_str(&format!(
                    "Confirmed restate of the edit:\n- Problem: {}\n- Change: {}\n- Success: {}\n- Out of scope: {}\n",
                    restate.problem, restate.change, restate.success, out_of_scope
                ));
            } else {
                section.push_str(&format!(
                    "Confirmed restate:\n- Outcome: {}\n- Success: {}\n- Constraint: {}\n- Out of scope: {}\n",
                    restate.outcome, restate.success, restate.constraint, out_of_scope
                ));
            }
        }
        let edit = self.is_edit() && self.restate.is_some();
        section.push_str("Nodes:\n");
        for node in &self.nodes {
            section.push_str(&format!(
                "- {} {} (catalog name {}): {}{}{} [{}{}]\n",
                node.kind.label(),
                node.name,
                node.catalog_name(),
                if node.job.is_empty() {
                    "no job written"
                } else {
                    &node.job
                },
                node.model
                    .as_deref()
                    .map(|model| format!("; model role {model}"))
                    .unwrap_or_default(),
                if node.principles.is_empty() {
                    String::new()
                } else {
                    format!("; principles: {}", node.principles.join(", "))
                },
                node.status.label(),
                if node.sketched {
                    ""
                } else {
                    ", not in the sketch"
                },
            ));
            if !node.takes.is_empty() || !node.returns.is_empty() {
                section.push_str(&format!(
                    "  takes: {} · returns: {}\n",
                    if node.takes.is_empty() {
                        "nothing"
                    } else {
                        &node.takes
                    },
                    if node.returns.is_empty() {
                        "nothing"
                    } else {
                        &node.returns
                    }
                ));
            }
            if edit && node.sketched {
                section.push_str(&match node.role() {
                    EditRole::Edit if node.edit_pending() => {
                        format!("  edit, not made yet: {}\n", node.change)
                    }
                    EditRole::Edit => format!("  edit, made: {}\n", node.change),
                    EditRole::New => "  new in this edit\n".to_string(),
                    EditRole::AsIs => "  as is: context only, nothing changes in it\n".to_string(),
                });
            }
            if let Some(note) = &node.note {
                section.push_str(&format!("  {note}\n"));
            }
        }
        if !self.edges.is_empty() {
            section.push_str("Edges:\n");
            for edge in &self.edges {
                section.push_str(&format!(
                    "- {} {} {}\n",
                    edge.from,
                    edge.kind.label(),
                    edge.to
                ));
            }
        }
        for node in self.nodes.iter().filter(|node| node.drafted()) {
            section.push_str(&format!(
                "\n### {} {} draft\n{}\n",
                node.kind.label(),
                node.name,
                node.yaml.as_deref().unwrap_or_default().trim_end()
            ));
            if node.problems.is_empty() {
                section.push_str("It has no problems.\n");
            } else {
                section.push_str(&format!(
                    "Its problems:\n- {}\n",
                    node.problems.join("\n- ")
                ));
            }
            if let Some(run) = &node.last_run {
                section.push_str(&format!("Its last test run: {run}\n"));
            }
        }
        if !self.catalog.is_empty() {
            section.push_str("\nCatalog tools the sketch uses (call them exactly as named):\n");
            for (name, shape) in &self.catalog {
                section.push_str(&format!("- {name}: {shape}\n"));
            }
        }
        if let Some(test) = self.tests.last() {
            section.push_str(&format!(
                "\nLast network test ({}): \"{}\" sent to {}; ran {}. Answer: {}\n",
                if test.passed { "passed" } else { "failed" },
                test.message,
                test.entry,
                test.ran.join(", "),
                test.answer
            ));
            if !self.tested() {
                section.push_str(
                    "The network changed after that test; test it again before saving.\n",
                );
            }
        }
        if self.waiting_on_secrets {
            let missing: Vec<String> =
                super::secrets::with_state(&self.test_secrets, self.secrets.as_deref())
                    .into_iter()
                    .filter(|secret| !secret.set)
                    .map(|secret| format!("{} ({})", secret.name, secret.tools.join(", ")))
                    .collect();
            section.push_str(&format!(
                "\nTesting is waiting on secrets the user has to set: {}. Run the test again only when the user says they're set.\n",
                if missing.is_empty() {
                    "all of them are set now".to_string()
                } else {
                    missing.join(", ")
                }
            ));
        }
        section
    }
}

fn agent_stub(node: &Node) -> Option<AgentDoc> {
    let yaml = serde_yaml::to_string(&json!({
        "name": node.name,
        "description": if node.job.is_empty() { "Not drafted yet." } else { node.job.as_str() },
        "model": node.model.clone().unwrap_or_else(|| "chat".to_string()),
        "system_prompt": "Not drafted yet.",
    }))
    .ok()?;
    parse_agent_source(&yaml).ok()
}

fn plan_stub(node: &Node) -> Option<PlanDoc> {
    dive_core::pipeline::plan_doc(&json!({
        "identifier": node.name,
        "name": node.name,
        "description": if node.job.is_empty() { "Not drafted yet." } else { node.job.as_str() },
        "steps": [],
    }))
    .ok()
}

pub fn parse_plan(yaml: &str) -> Result<PlanDoc, String> {
    let value: Value = serde_yaml::from_str(yaml)
        .map_err(|error| format!("the plan isn't valid YAML: {error}"))?;
    dive_core::pipeline::plan_doc(&value).map_err(|error| format!("not a plan document: {error}"))
}

fn parse_tool(yaml: &str) -> Option<UserToolDoc> {
    let doc = dive_core::user_tools::parse_tool_source(yaml).ok()?;
    dive_core::user_tools::validate_tool(&doc).ok()?;
    Some(doc)
}

struct Stubs {
    defs: Vec<ToolDef>,
}

#[async_trait]
impl ToolRegistry for Stubs {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(self.defs.clone())
    }

    async fn invoke(&self, name: &str, _input: Value) -> Result<ToolOutcome, ToolError> {
        if self.defs.iter().any(|def| def.name == name) {
            return Ok(ToolOutcome {
                result: json!({"error": format!("{name} is in the sketch but not drafted yet")}),
                is_error: true,
            });
        }
        Err(ToolError::Unknown(name.to_string()))
    }
}

pub fn resolved_plan(origin: &Origin, yaml: &str) -> Result<PlanDoc, String> {
    let mut doc = parse_plan(yaml)?;
    if let Origin::Pack(source) = origin {
        let problems = dive_core::packs::resolve_plan_steps(&mut doc.steps, &source.name);
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        doc.identifier =
            dive_core::packs::catalog_name(&source.name, ItemKind::Plan, &doc.identifier);
        doc.pack = Some(source.clone());
    }
    Ok(doc)
}

pub fn resolved_agent(origin: &Origin, yaml: &str) -> Result<AgentDoc, String> {
    let mut doc = parse_agent_source(yaml)?;
    if let Origin::Pack(source) = origin {
        let problems = dive_core::packs::resolve_agent(&mut doc, &source.name);
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        doc.name = dive_core::packs::catalog_name(&source.name, ItemKind::Agent, &doc.name);
        doc.source = dive_core::agent::doc::AgentSource::Pack {
            source: source.clone(),
            path: source.dir.join("agents"),
        };
    }
    Ok(doc)
}

fn node_agent(node: &Node) -> Option<AgentDoc> {
    node.yaml
        .as_deref()
        .and_then(|yaml| resolved_agent(&node.origin, yaml).ok())
        .or_else(|| agent_stub(node))
}

pub fn overlay(pipeline: &Pipeline, network: &Network) -> Pipeline {
    let mut tools = Vec::new();
    let mut builtins = Vec::new();
    let mut packed: Vec<(PackSource, Vec<UserToolDoc>)> = Vec::new();
    let mut stubs = Vec::new();
    let mut plans: Vec<PlanDoc> = Vec::new();
    let mut agents: Vec<AgentDoc> = Vec::new();
    for node in &network.nodes {
        match node.kind {
            ArtifactKind::Tool => match node.yaml.as_deref().and_then(parse_tool) {
                Some(doc) => match &node.origin {
                    Origin::Builtin => builtins.push(doc),
                    Origin::Pack(source) => {
                        match packed.iter_mut().find(|(known, _)| known == source) {
                            Some((_, docs)) => docs.push(doc),
                            None => packed.push((source.clone(), vec![doc])),
                        }
                    }
                    Origin::New | Origin::File => tools.push(doc),
                },
                None => stubs.push(ToolDef {
                    name: node.catalog_name(),
                    description: node.job.clone(),
                    input_schema: json!({"type": "object"}),
                    output_schema: None,
                    output_example: None,
                    read_only: None,
                }),
            },
            ArtifactKind::Plan => {
                if let Some(doc) = node
                    .yaml
                    .as_deref()
                    .and_then(|yaml| resolved_plan(&node.origin, yaml).ok())
                    .or_else(|| plan_stub(node))
                {
                    plans.push(doc);
                }
            }
            ArtifactKind::Agent => {
                if let Some(doc) = node_agent(node) {
                    agents.push(doc);
                }
            }
        }
    }
    let mut over = pipeline.clone();
    let mut layers: Vec<Arc<dyn ToolRegistry>> = Vec::new();
    if !tools.is_empty() {
        layers.push(Arc::new(
            UserToolRegistry::new(tools.clone(), pipeline.router.clone())
                .with_secrets(pipeline.drafted.secrets.clone()),
        ));
    }
    if !builtins.is_empty() {
        layers.push(Arc::new(
            UserToolRegistry::builtins(builtins, pipeline.router.clone())
                .with_secrets(pipeline.drafted.secrets.clone()),
        ));
    }
    for (source, docs) in packed {
        layers.push(Arc::new(
            UserToolRegistry::pack(&source.name, docs, pipeline.router.clone())
                .with_pack_dir(&source.dir)
                .with_secrets(pipeline.drafted.secrets.clone()),
        ));
    }
    if !stubs.is_empty() {
        layers.push(Arc::new(Stubs {
            defs: stubs.clone(),
        }));
    }
    layers.push(pipeline.registry.clone());
    over.registry = Arc::new(CompositeRegistry::new(layers));
    let mut all_plans: Vec<PlanDoc> = pipeline
        .plans
        .iter()
        .filter(|doc| !plans.iter().any(|plan| plan.identifier == doc.identifier))
        .cloned()
        .collect();
    all_plans.extend(plans.iter().cloned());
    over.plans = Arc::new(all_plans);
    over.agents = Arc::new(AgentSet::layered(
        pipeline.agents.iter().collect(),
        vec![agents.clone()],
    ));
    if let Some(catalog) = pipeline.live_catalog() {
        let mut catalog = catalog;
        for doc in &tools {
            catalog
                .user_tools
                .insert(format!("{USER_TOOL_PREFIX}{}", doc.name));
        }
        for def in &stubs {
            catalog.user_tools.insert(def.name.clone());
        }
        for doc in &plans {
            catalog.plans.insert(doc.identifier.clone());
        }
        for doc in &agents {
            catalog.agents.insert(doc.name.clone());
        }
        over.catalog = Some(Arc::new(catalog));
    }
    over
}

pub fn network_agents(network: &Network) -> Vec<AgentDoc> {
    network
        .nodes
        .iter()
        .filter(|node| node.kind == ArtifactKind::Agent)
        .filter_map(node_agent)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sketch() -> Value {
        json!({
            "restate": {
                "outcome": "Ask in chat what changed since the last release",
                "success": "One line per PR",
                "constraint": "Small models",
                "out_of_scope": ["Posting to Slack"]
            },
            "nodes": [
                {"name": "release_desk", "kind": "agent", "job": "Answers release questions", "model": "chat", "principles": ["Plans before agents"]},
                {"name": "plan__release_digest", "kind": "plan", "job": "Briefing"},
                {"name": "changelog_section", "kind": "tool", "job": "Reads a CHANGELOG section"}
            ],
            "edges": [
                {"from": "release_desk", "to": "release_digest", "kind": "calls"}
            ]
        })
    }

    #[test]
    fn a_sketch_lists_its_nodes_by_bare_name_and_keeps_drafts_across_revisions() {
        let mut network = Network::default();
        network.set_sketch(&sketch()).unwrap();
        assert_eq!(network.nodes.len(), 3);
        assert_eq!(network.nodes[1].name, "release_digest");
        assert_eq!(network.nodes[1].catalog_name(), "plan__release_digest");
        assert_eq!(network.missing().len(), 3);

        network
            .put_draft(
                ArtifactKind::Tool,
                "changelog_section",
                "name: changelog_section".into(),
                Vec::new(),
            )
            .unwrap();
        network.set_sketch(&sketch()).unwrap();
        assert!(network
            .node(ArtifactKind::Tool, "changelog_section")
            .unwrap()
            .drafted());

        let mut smaller = sketch();
        smaller["nodes"].as_array_mut().unwrap().pop();
        let summary = network.set_sketch(&smaller).unwrap();
        assert_eq!(summary["no_longer_sketched"], json!(["changelog_section"]));
        assert!(
            !network
                .node(ArtifactKind::Tool, "changelog_section")
                .unwrap()
                .sketched
        );
    }

    #[test]
    fn a_draft_outside_the_sketch_is_refused_once_there_is_a_sketch() {
        let mut network = Network::default();
        network
            .put_draft(
                ArtifactKind::Tool,
                "loose",
                "name: loose".into(),
                Vec::new(),
            )
            .unwrap();
        assert_eq!(network.nodes.len(), 1);
        network.set_sketch(&sketch()).unwrap();
        let refused = network
            .put_draft(
                ArtifactKind::Agent,
                "other",
                "name: other".into(),
                Vec::new(),
            )
            .unwrap_err();
        assert!(refused.contains("release_desk"), "{refused}");
    }

    #[test]
    fn a_sketch_needs_a_restate_and_edges_that_start_at_a_node() {
        let mut network = Network::default();
        let mut bad = sketch();
        bad["restate"]["outcome"] = json!("");
        assert!(network.set_sketch(&bad).unwrap_err().contains("restate"));
        let mut bad = sketch();
        bad["edges"][0]["from"] = json!("nobody");
        assert!(network.set_sketch(&bad).unwrap_err().contains("nobody"));
    }

    #[test]
    fn a_passing_test_counts_until_the_network_changes() {
        let mut network = Network::default();
        network.set_sketch(&sketch()).unwrap();
        for (kind, name) in [
            (ArtifactKind::Agent, "release_desk"),
            (ArtifactKind::Plan, "release_digest"),
            (ArtifactKind::Tool, "changelog_section"),
        ] {
            network
                .put_draft(kind, name, format!("name: {name}"), Vec::new())
                .unwrap();
        }
        network.record_test(TestRun {
            entry: "release_desk".into(),
            message: "what changed?".into(),
            passed: true,
            ran: vec!["release_desk".into(), "plan__release_digest".into()],
            answer: "ok".into(),
        });
        assert!(network.tested());
        assert_eq!(
            network
                .node(ArtifactKind::Agent, "release_desk")
                .unwrap()
                .status,
            NodeStatus::Ready
        );
        assert_eq!(
            network
                .node(ArtifactKind::Tool, "changelog_section")
                .unwrap()
                .status,
            NodeStatus::Drafted
        );
        network
            .put_draft(
                ArtifactKind::Tool,
                "changelog_section",
                "name: changelog_section\n".into(),
                Vec::new(),
            )
            .unwrap();
        assert!(!network.tested());
        let order: Vec<ArtifactKind> = network
            .in_save_order()
            .iter()
            .map(|node| node.kind)
            .collect();
        assert_eq!(
            order,
            [ArtifactKind::Tool, ArtifactKind::Plan, ArtifactKind::Agent]
        );
    }

    #[test]
    fn the_context_section_carries_the_restate_nodes_and_drafts() {
        let mut network = Network::default();
        assert!(network.context_section().contains("empty"));
        network.set_sketch(&sketch()).unwrap();
        network
            .put_draft(
                ArtifactKind::Tool,
                "changelog_section",
                "name: changelog_section".into(),
                vec!["bad".into()],
            )
            .unwrap();
        let section = network.context_section();
        assert!(section.contains("Out of scope: Posting to Slack"));
        assert!(section.contains("plan__release_digest"));
        assert!(section.contains("### tool changelog_section draft"));
        assert!(section.contains("- bad"));
    }

    #[test]
    fn a_saved_file_with_its_keys_reordered_is_not_a_change() {
        let mut node = Node::sketched("rank".into(), ArtifactKind::Plan, String::new());
        node.base =
            Some("identifier: rank\nsteps:\n- id: a\n  input:\n    by: url\n    items: x\n".into());
        node.yaml =
            Some("identifier: rank\nsteps:\n- id: a\n  input:\n    items: x\n    by: url\n".into());
        assert!(!node.changed());
        node.yaml =
            Some("identifier: rank\nsteps:\n- id: a\n  input:\n    items: y\n    by: url\n".into());
        assert!(node.changed());
        node.base = None;
        assert!(node.changed());
    }

    #[test]
    fn builders_report_progress_on_their_node() {
        let mut network = Network::default();
        network.set_sketch(&sketch()).unwrap();
        network.builder_started("changelog_section", "tool_drafter");
        assert_eq!(
            network
                .node(ArtifactKind::Tool, "changelog_section")
                .unwrap()
                .status,
            NodeStatus::Drafting
        );
        network
            .put_draft(
                ArtifactKind::Tool,
                "changelog_section",
                "name: changelog_section".into(),
                Vec::new(),
            )
            .unwrap();
        network.builder_finished(
            "changelog_section",
            "tool_drafter",
            &json!({"status": "drafted", "summary": "reads a section"}),
            false,
        );
        let node = network
            .node(ArtifactKind::Tool, "changelog_section")
            .unwrap();
        assert_eq!(node.status, NodeStatus::Drafted);
        assert_eq!(node.note.as_deref(), Some("tool_drafter · reads a section"));
        network.builder_finished(
            "release_desk",
            "agent_drafter",
            &json!({"status": "question"}),
            false,
        );
        assert_eq!(
            network
                .node(ArtifactKind::Agent, "release_desk")
                .unwrap()
                .status,
            NodeStatus::Question
        );
    }

    #[test]
    fn a_revised_sketch_marks_changed_and_proposed_nodes() {
        let mut network = Network::default();
        network.set_sketch(&sketch()).unwrap();
        let mut revised = sketch();
        revised["nodes"][1]["job"] = json!("Briefing with a risk section");
        revised["nodes"].as_array_mut().unwrap().push(
            json!({"name": "post_message", "kind": "tool", "job": "Posts", "proposed": true}),
        );
        network.set_sketch(&revised).unwrap();
        assert_eq!(
            network
                .node(ArtifactKind::Plan, "release_digest")
                .unwrap()
                .status,
            NodeStatus::Changed
        );
        assert_eq!(
            network
                .node(ArtifactKind::Tool, "post_message")
                .unwrap()
                .status,
            NodeStatus::Proposed
        );
    }

    #[test]
    fn a_builder_starting_settles_the_design_question() {
        let mut network = Network::default();
        network
            .propose_designs(&json!({
                "yours": {"title": "Patch", "nodes": [], "lines": []},
                "suggested": {"title": "Rework", "nodes": [], "lines": []}
            }))
            .unwrap();
        assert!(network.designs.is_some());
        network.builder_started("word_count", "tool_drafter");
        assert!(network.designs.is_none());
    }

    #[test]
    fn the_interview_and_designs_reach_the_view() {
        let mut network = Network::default();
        network
            .set_interview(&json!({
                "reply": "r",
                "confidence": 55,
                "missing": "what counts as risky",
                "question": "What makes a change risky?",
                "guess": "Config changes",
                "facts": [{"label": "Outcome", "text": "A briefing", "state": "known"}, {"label": "Risky", "text": "", "state": "asking", "previous": "anything"}]
            }))
            .unwrap();
        network
            .propose_designs(&json!({
                "yours": {"title": "One big agent", "nodes": [{"kind": "agent", "name": "release_agent"}], "lines": ["1 large turn"]},
                "suggested": {"title": "Digest plan", "nodes": [{"kind": "plan", "name": "plan__release_digest"}], "lines": ["38 small calls"], "principles": ["Map/reduce"]}
            }))
            .unwrap();
        let view = network.view();
        let interview = view.interview.clone().unwrap();
        assert_eq!(interview.confidence, 55);
        assert_eq!(interview.facts[1].state, FactState::Asking);
        assert_eq!(interview.facts[1].previous.as_deref(), Some("anything"));
        let designs = view.designs.clone().unwrap();
        assert_eq!(
            designs.suggested.nodes,
            [(ArtifactKind::Plan, "release_digest".to_string())]
        );
        assert!(!view.is_empty() && view.restate.is_none());
        network.set_sketch(&sketch()).unwrap();
        assert!(
            network.designs.is_none(),
            "a sketch settles the design question"
        );
    }

    const AGENT: &str = "name: research_agent\nmodel: chat\ntools: [plan__ranked_search]\n";
    const RANKED: &str = "identifier: ranked_search\nname: Ranked search\nsteps: []\n";

    fn opened(name: &str, yaml: &str) -> Existing {
        Existing {
            yaml: yaml.to_string(),
            editing: Some(PathBuf::from(format!("/work/{name}.yaml"))),
            name: name.to_string(),
            origin: Origin::File,
        }
    }

    fn edit() -> Network {
        let mut network = Network::default();
        network.open(
            ArtifactKind::Agent,
            opened("research_agent", AGENT),
            Vec::new(),
        );
        network.open(
            ArtifactKind::Plan,
            opened("ranked_search", RANKED),
            Vec::new(),
        );
        network
    }

    fn edit_sketch() -> Value {
        json!({
            "restate": {
                "problem": "research_agent spends a turn each round picking pages",
                "change": "A plan searches, ranks and reads in one call",
                "success": "The F1 question answers with every claim linked to a page it read",
                "out_of_scope": ["The date tool"]
            },
            "nodes": [
                {"name": "research_agent", "kind": "agent", "job": "Researches", "change": "Calls research_round instead of picking pages"},
                {"name": "research_round", "kind": "plan", "job": "ranked_search → map crawl4ai_read", "takes": "question, queries", "returns": "pages, unread"},
                {"name": "ranked_search", "kind": "plan", "job": "Ranked search"}
            ],
            "edges": [
                {"from": "research_agent", "to": "research_round", "kind": "calls"},
                {"from": "research_round", "to": "ranked_search", "kind": "calls"}
            ]
        })
    }

    fn role(network: &Network, kind: ArtifactKind, name: &str) -> EditRole {
        network.node(kind, name).unwrap().role()
    }

    #[test]
    fn an_edit_keeps_being_an_edit_once_it_is_sketched() {
        let mut network = edit();
        assert!(network.is_edit());
        network.set_sketch(&edit_sketch()).unwrap();
        assert!(network.is_edit());
        assert!(network.view().is_edit());
        assert!(network.restate.as_ref().unwrap().is_edit());
        assert_eq!(
            role(&network, ArtifactKind::Agent, "research_agent"),
            EditRole::Edit
        );
        assert_eq!(
            role(&network, ArtifactKind::Plan, "research_round"),
            EditRole::New
        );
        assert_eq!(
            role(&network, ArtifactKind::Plan, "ranked_search"),
            EditRole::AsIs
        );
        assert_eq!(
            network.missing(),
            [
                "agent research_agent (its edit isn't made yet)",
                "plan research_round"
            ]
        );

        network
            .put_draft(
                ArtifactKind::Plan,
                "research_round",
                "identifier: research_round\nname: Research round\nsteps: []\n".into(),
                Vec::new(),
            )
            .unwrap();
        network
            .put_draft(
                ArtifactKind::Agent,
                "research_agent",
                format!("{AGENT}description: edited\n"),
                Vec::new(),
            )
            .unwrap();
        assert!(network.missing().is_empty());
        assert_eq!(network.view().unsaved(), 2);
    }

    #[test]
    fn an_edit_restate_needs_its_problem_change_and_success() {
        let mut network = edit();
        let mut bad = edit_sketch();
        bad["restate"]["success"] = json!("");
        let refused = network.set_sketch(&bad).unwrap_err();
        assert!(refused.contains("problem, change and success"), "{refused}");
        let mut neither = edit_sketch();
        neither["restate"] = json!({"success": "s", "out_of_scope": []});
        let refused = network.set_sketch(&neither).unwrap_err();
        assert!(refused.contains("restate.outcome"), "{refused}");
    }

    #[test]
    fn the_context_section_marks_what_each_node_of_an_edit_does() {
        let mut network = edit();
        network.set_sketch(&edit_sketch()).unwrap();
        let section = network.context_section();
        assert!(
            section.contains("Confirmed restate of the edit:"),
            "{section}"
        );
        assert!(section.contains("- Change: A plan searches, ranks and reads in one call"));
        assert!(
            section.contains("edit, not made yet: Calls research_round instead of picking pages")
        );
        assert!(section.contains("new in this edit"));
        assert!(section.contains("as is: context only"));
        assert!(!section.contains("Outcome:"));
    }

    #[test]
    fn saving_an_edit_closes_its_sketch_and_keeps_every_item_open() {
        let mut network = edit();
        network.set_sketch(&edit_sketch()).unwrap();
        network
            .put_draft(
                ArtifactKind::Plan,
                "research_round",
                "identifier: research_round\nname: Research round\nsteps: []\n".into(),
                Vec::new(),
            )
            .unwrap();
        network.saved(&[(
            ArtifactKind::Plan,
            "research_round".to_string(),
            PathBuf::from("/work/plans/research_round.yaml"),
        )]);
        assert!(network.restate.is_none());
        assert!(network.edges.is_empty());
        assert_eq!(network.nodes.len(), 3, "the new plan stays open");
        assert!(network
            .nodes
            .iter()
            .all(|node| !node.sketched && node.change.is_empty()));
        let round = network.node(ArtifactKind::Plan, "research_round").unwrap();
        assert!(round.base.is_some() && !round.changed());
        assert!(network.is_edit());
    }

    #[test]
    fn discarding_an_edit_drops_its_new_items_and_its_sketch() {
        let mut network = edit();
        network.set_sketch(&edit_sketch()).unwrap();
        network
            .put_draft(
                ArtifactKind::Agent,
                "research_agent",
                format!("{AGENT}description: edited\n"),
                Vec::new(),
            )
            .unwrap();
        assert_eq!(network.discard_all(), 1);
        assert!(network.restate.is_none());
        assert_eq!(network.nodes.len(), 2);
        assert!(!network.has_edits());
    }

    #[test]
    fn discarding_one_sketched_edit_goes_back_to_its_saved_file() {
        let mut network = edit();
        network.set_sketch(&edit_sketch()).unwrap();
        network
            .put_draft(
                ArtifactKind::Agent,
                "research_agent",
                format!("{AGENT}description: edited\n"),
                Vec::new(),
            )
            .unwrap();
        network
            .discard(ArtifactKind::Agent, "research_agent")
            .unwrap();
        let agent = network.node(ArtifactKind::Agent, "research_agent").unwrap();
        assert_eq!(agent.yaml.as_deref(), Some(AGENT));
        assert!(agent.edit_pending());
    }
}
