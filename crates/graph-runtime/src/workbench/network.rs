use super::artifact::ArtifactKind;
use async_trait::async_trait;
use graph_core::agent::doc::{parse_agent_source, AgentDoc, AgentSet};
use graph_core::pipeline::doc::PlanDoc;
use graph_core::pipeline::Pipeline;
use graph_core::user_tools::{UserToolDoc, UserToolRegistry, USER_TOOL_PREFIX};
use graph_core::{CompositeRegistry, ToolDef, ToolError, ToolOutcome, ToolRegistry};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Restate {
    pub outcome: String,
    pub success: String,
    pub constraint: String,
    pub out_of_scope: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    Sketched,
    Drafted,
    Ready,
    Failed,
}

impl NodeStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sketched => "sketched",
            Self::Drafted => "drafted",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub kind: ArtifactKind,
    pub job: String,
    pub model: Option<String>,
    pub principles: Vec<String>,
    pub sketched: bool,
    pub status: NodeStatus,
    pub yaml: Option<String>,
    pub problems: Vec<String>,
    pub last_run: Option<Value>,
    pub editing: Option<PathBuf>,
    pub original: Option<String>,
}

impl Node {
    fn sketched(name: String, kind: ArtifactKind, job: String) -> Self {
        Self {
            name,
            kind,
            job,
            model: None,
            principles: Vec::new(),
            sketched: true,
            status: NodeStatus::Sketched,
            yaml: None,
            problems: Vec::new(),
            last_run: None,
            editing: None,
            original: None,
        }
    }

    pub fn catalog_name(&self) -> String {
        catalog_name(self.kind, &self.name)
    }

    pub fn drafted(&self) -> bool {
        self.yaml.is_some()
    }
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

#[derive(Debug, Clone, PartialEq)]
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
    revision: u64,
    tested: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeView {
    pub name: String,
    pub kind: ArtifactKind,
    pub job: String,
    pub model: Option<String>,
    pub principles: Vec<String>,
    pub sketched: bool,
    pub status: NodeStatus,
    pub yaml: Option<String>,
    pub problems: Vec<String>,
    pub last_run: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NetworkView {
    pub restate: Option<Restate>,
    pub nodes: Vec<NodeView>,
    pub edges: Vec<Edge>,
    pub tests: Vec<TestRun>,
    pub tested: bool,
}

impl NetworkView {
    pub fn is_empty(&self) -> bool {
        self.restate.is_none() && self.nodes.is_empty()
    }

    pub fn unsaved(&self) -> usize {
        self.nodes.iter().filter(|node| node.yaml.is_some()).count()
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
        self.restate.is_none() && self.nodes.is_empty()
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
            success: text(restate, "success"),
            constraint: text(restate, "constraint"),
            out_of_scope: strings(&restate["out_of_scope"]),
        };
        if restate.outcome.is_empty() {
            return Err(
                "restate.outcome is required: the confirmed restate goes with every sketch"
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
            node.job = text(raw, "job");
            node.model = raw["model"]
                .as_str()
                .map(str::to_string)
                .filter(|m| !m.is_empty());
            node.principles = strings(&raw["principles"]);
            node.sketched = true;
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
                node.status = NodeStatus::Drafted;
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

    pub fn open(
        &mut self,
        kind: ArtifactKind,
        name: &str,
        yaml: String,
        editing: Option<PathBuf>,
        original: Option<String>,
        problems: Vec<String>,
    ) {
        if self.node(kind, name).is_none() {
            let mut node = Node::sketched(name.to_string(), kind, String::new());
            node.sketched = false;
            self.nodes.push(node);
        }
        if let Some(node) = self.node_mut(kind, name) {
            node.yaml = Some(yaml);
            node.problems = problems;
            node.last_run = None;
            node.status = NodeStatus::Drafted;
            node.editing = editing;
            node.original = original;
        }
        self.changed();
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
        if node.sketched {
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
                    sketched: node.sketched,
                    status: node.status,
                    yaml: node.yaml.clone(),
                    problems: node.problems.clone(),
                    last_run: node.last_run.clone(),
                })
                .collect(),
            edges: self.edges.clone(),
            tests: self.tests.clone(),
            tested: self.tested(),
        }
    }

    pub fn missing(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| node.sketched && !node.drafted())
            .map(|node| format!("{} {}", node.kind.label(), node.name))
            .collect()
    }

    pub fn with_problems(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| node.drafted() && !node.problems.is_empty())
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
        let mut nodes: Vec<&Node> = self.nodes.iter().filter(|node| node.drafted()).collect();
        nodes.sort_by_key(|node| rank(node.kind));
        nodes
    }

    pub fn clear(&mut self) -> usize {
        let drafted = self.nodes.iter().filter(|node| node.drafted()).count();
        *self = Network {
            revision: self.revision + 1,
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
            section.push_str(&format!(
                "Confirmed restate:\n- Outcome: {}\n- Success: {}\n- Constraint: {}\n- Out of scope: {}\n",
                restate.outcome,
                restate.success,
                restate.constraint,
                if restate.out_of_scope.is_empty() {
                    "(nothing named)".to_string()
                } else {
                    restate.out_of_scope.join("; ")
                }
            ));
        }
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
    graph_core::pipeline::plan_doc(&json!({
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
    graph_core::pipeline::plan_doc(&value).map_err(|error| format!("not a plan document: {error}"))
}

fn parse_tool(yaml: &str) -> Option<UserToolDoc> {
    let doc = graph_core::user_tools::parse_tool_source(yaml).ok()?;
    graph_core::user_tools::validate_tool(&doc).ok()?;
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

pub fn overlay(pipeline: &Pipeline, network: &Network) -> Pipeline {
    let mut tools = Vec::new();
    let mut stubs = Vec::new();
    let mut plans: Vec<PlanDoc> = Vec::new();
    let mut agents: Vec<AgentDoc> = Vec::new();
    for node in &network.nodes {
        match node.kind {
            ArtifactKind::Tool => match node.yaml.as_deref().and_then(parse_tool) {
                Some(doc) => tools.push(doc),
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
                    .and_then(|yaml| parse_plan(yaml).ok())
                    .or_else(|| plan_stub(node))
                {
                    plans.push(doc);
                }
            }
            ArtifactKind::Agent => {
                if let Some(doc) = node
                    .yaml
                    .as_deref()
                    .and_then(|yaml| parse_agent_source(yaml).ok())
                    .or_else(|| agent_stub(node))
                {
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
        .filter_map(|node| {
            node.yaml
                .as_deref()
                .and_then(|yaml| parse_agent_source(yaml).ok())
                .or_else(|| agent_stub(node))
        })
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
}
