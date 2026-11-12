use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

use crate::agent::doc::AgentSet;
use crate::packs::split_catalog_name;
use crate::pipeline::catalog::{agent_tool_patterns, glob_matches, step_calls, ToolCatalog};
use crate::pipeline::doc::PlanDoc;
use crate::pipeline::plan::WORKBENCH_TOOL_PREFIX;
use crate::pipeline::{
    is_native_tool, AGENT_TOOL, ASK_TOOL, EXIT_TOOL, FILTER_TOOL, MAP_TOOL, REDUCE_TOOL, ROUTE_TOOL,
};

const PLAN_AND_EXECUTE: &str = "plan_and_execute";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Item {
    Plan(String),
    Agent(String),
    Tool(String),
    Server(String),
}

impl Item {
    pub fn kind(&self) -> &'static str {
        match self {
            Item::Plan(_) => "plan",
            Item::Agent(_) => "agent",
            Item::Tool(_) => "tool",
            Item::Server(_) => "MCP server",
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Item::Plan(name) | Item::Agent(name) | Item::Tool(name) | Item::Server(name) => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "via", content = "step", rename_all = "snake_case")]
pub enum Via {
    Step(String),
    Tool,
    Subagent,
    Handoff,
    RequiresServers,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hop {
    pub item: Item,
    #[serde(flatten)]
    pub via: Via,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    NotInCatalog,
    ServerNotConfigured,
    PatternMatchesNothing,
}

impl Reason {
    pub fn describe(self) -> &'static str {
        match self {
            Reason::NotInCatalog => "not in this project's catalog",
            Reason::ServerNotConfigured => "not configured under [mcp.*]",
            Reason::PatternMatchesNothing => "matches nothing in this project's catalog",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Missing {
    pub item: Item,
    pub reason: Reason,
    pub path: Vec<Hop>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind} '{name}' can't run here: {}", render(.missing))]
pub struct Unrunnable {
    pub kind: &'static str,
    pub name: String,
    pub missing: Vec<Missing>,
}

fn render(missing: &[Missing]) -> String {
    missing
        .iter()
        .map(|entry| {
            let path = entry
                .path
                .iter()
                .map(|hop| format!("{} ({})", hop.item.name(), hop.via.describe()))
                .collect::<Vec<_>>()
                .join(" → ");
            format!("{path} is {}", entry.reason.describe())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

impl Via {
    pub fn describe(&self) -> String {
        match self {
            Via::Step(step) => format!("step {step}"),
            Via::Tool => "tool".to_string(),
            Via::Subagent => "subagent".to_string(),
            Via::Handoff => "handoff".to_string(),
            Via::RequiresServers => "requires_servers".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Requirements {
    missing: BTreeMap<Item, Vec<Missing>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Caller {
    Plan,
    Agent,
}

#[derive(Debug, Clone)]
enum Target {
    Node(Item),
    Leaf(Item, Reason),
}

#[derive(Debug, Clone)]
struct Edge {
    via: Via,
    target: Target,
}

struct Graph<'a> {
    plans: &'a [PlanDoc],
    agent_names: BTreeSet<String>,
    catalog: &'a ToolCatalog,
}

impl Requirements {
    pub fn compute(plans: &[PlanDoc], agents: &AgentSet, catalog: &ToolCatalog) -> Self {
        let graph = Graph {
            plans,
            agent_names: agents.iter().map(|doc| doc.name).collect(),
            catalog,
        };
        let mut edges: BTreeMap<Item, Vec<Edge>> = BTreeMap::new();
        for doc in plans {
            edges
                .entry(Item::Plan(doc.identifier.clone()))
                .or_insert_with(|| graph.plan_edges(doc));
        }
        for doc in agents.iter() {
            let edges_of = graph.agent_edges(&doc);
            edges.insert(Item::Agent(doc.name), edges_of);
        }
        let missing = edges
            .keys()
            .map(|root| (root.clone(), walk(root, &edges)))
            .filter(|(_, found)| !found.is_empty())
            .collect();
        Self { missing }
    }

    pub fn missing(&self, item: &Item) -> &[Missing] {
        self.missing.get(item).map_or(&[], Vec::as_slice)
    }

    pub fn runnable(&self, item: &Item) -> bool {
        self.missing(item).is_empty()
    }

    pub fn plan_runnable(&self, identifier: &str) -> bool {
        self.runnable(&Item::Plan(identifier.to_string()))
    }

    pub fn agent_runnable(&self, name: &str) -> bool {
        self.runnable(&Item::Agent(name.to_string()))
    }

    pub fn refusal(&self, item: &Item) -> Option<Unrunnable> {
        let missing = self.missing(item);
        (!missing.is_empty()).then(|| Unrunnable {
            kind: item.kind(),
            name: item.name().to_string(),
            missing: missing.to_vec(),
        })
    }
}

fn walk(root: &Item, edges: &BTreeMap<Item, Vec<Edge>>) -> Vec<Missing> {
    let mut found: Vec<Missing> = Vec::new();
    let mut visited: BTreeSet<&Item> = BTreeSet::from([root]);
    let mut queue: VecDeque<(&Item, Vec<Hop>)> = VecDeque::from([(root, Vec::new())]);
    while let Some((node, path)) = queue.pop_front() {
        for edge in edges.get(node).map_or(&[][..], Vec::as_slice) {
            let (item, reason) = match &edge.target {
                Target::Leaf(item, reason) => (item, Some(*reason)),
                Target::Node(item) if edges.contains_key(item) => (item, None),
                Target::Node(item) => (item, Some(Reason::NotInCatalog)),
            };
            let mut next = path.clone();
            next.push(Hop {
                item: item.clone(),
                via: edge.via.clone(),
            });
            match reason {
                Some(reason) => {
                    if !found.iter().any(|missing| &missing.item == item) {
                        found.push(Missing {
                            item: item.clone(),
                            reason,
                            path: next,
                        });
                    }
                }
                None => {
                    if visited.insert(item) {
                        queue.push_back((item, next));
                    }
                }
            }
        }
    }
    found
}

impl Graph<'_> {
    fn plan_edges(&self, doc: &PlanDoc) -> Vec<Edge> {
        let mut edges = Vec::new();
        for (step, tool, _) in step_calls(&doc.steps) {
            if is_control(&tool) {
                continue;
            }
            if let Some(target) = self.reference(&tool, Caller::Plan) {
                edges.push(Edge {
                    via: Via::Step(step),
                    target,
                });
            }
        }
        for (step, pattern) in agent_tool_patterns(&doc.steps) {
            if let Some(target) = self.reference(&pattern, Caller::Plan) {
                edges.push(Edge {
                    via: Via::Step(step),
                    target,
                });
            }
        }
        for server in &doc.requires_servers {
            if !self.catalog.mcp_servers.contains(server) {
                edges.push(Edge {
                    via: Via::RequiresServers,
                    target: Target::Leaf(Item::Server(server.clone()), Reason::ServerNotConfigured),
                });
            }
        }
        edges
    }

    fn agent_edges(&self, doc: &crate::agent::doc::AgentDoc) -> Vec<Edge> {
        let tools = doc.tools.iter().filter_map(|entry| {
            self.reference(&entry.name, Caller::Agent)
                .map(|target| Edge {
                    via: Via::Tool,
                    target,
                })
        });
        let subagents = doc.subagents.iter().map(|name| Edge {
            via: Via::Subagent,
            target: Target::Node(Item::Agent(name.clone())),
        });
        let handoffs = doc.handoffs.iter().map(|name| Edge {
            via: Via::Handoff,
            target: Target::Node(Item::Agent(name.clone())),
        });
        tools.chain(subagents).chain(handoffs).collect()
    }

    fn reference(&self, name: &str, caller: Caller) -> Option<Target> {
        if name.contains('*') {
            return self
                .pattern_reason(name, caller)
                .map(|(item, reason)| Target::Leaf(item, reason));
        }
        let packs = &self.catalog.packs;
        if let Some(target) = packs.plan_target(name) {
            return Some(Target::Node(Item::Plan(target.to_string())));
        }
        if let Some(target) = packs.agent_target(name) {
            return Some(Target::Node(Item::Agent(target.to_string())));
        }
        self.tool_reason(name, caller)
            .map(|(item, reason)| Target::Leaf(item, reason))
    }

    fn tool_reason(&self, name: &str, caller: Caller) -> Option<(Item, Reason)> {
        if is_native_tool(name) || name.starts_with(WORKBENCH_TOOL_PREFIX) {
            return None;
        }
        let catalog = self.catalog;
        if let Some((_, kind, _)) = catalog.packs.routed(name) {
            let absent = kind == crate::packs::ItemKind::Tool && catalog.packs.item(name).is_none();
            return absent.then(|| (Item::Tool(name.to_string()), Reason::NotInCatalog));
        }
        if split_catalog_name(name).is_some() {
            return (caller == Caller::Plan)
                .then(|| (Item::Tool(name.to_string()), Reason::NotInCatalog));
        }
        let (namespace, _) = name.split_once("__")?;
        let known = match namespace {
            "builtin" => catalog.builtin_tools.contains(name),
            "user" => catalog.user_tools.contains(name),
            server => {
                return (!catalog.mcp_servers.contains(server)).then(|| {
                    (
                        Item::Server(server.to_string()),
                        Reason::ServerNotConfigured,
                    )
                })
            }
        };
        (!known).then(|| (Item::Tool(name.to_string()), Reason::NotInCatalog))
    }

    fn pattern_reason(&self, pattern: &str, caller: Caller) -> Option<(Item, Reason)> {
        let (namespace, _) = pattern.split_once("__")?;
        if namespace.contains('*') || pattern.starts_with(WORKBENCH_TOOL_PREFIX) {
            return None;
        }
        let catalog = self.catalog;
        let nothing = || {
            (
                Item::Tool(pattern.to_string()),
                Reason::PatternMatchesNothing,
            )
        };
        if catalog.packs.is_installed(namespace) {
            return catalog
                .packs
                .matching(pattern)
                .next()
                .is_none()
                .then(nothing);
        }
        if split_catalog_name(pattern).is_some() && caller == Caller::Agent {
            return None;
        }
        let matched = match namespace {
            "builtin" => catalog
                .builtin_tools
                .iter()
                .any(|name| glob_matches(pattern, name)),
            "user" => catalog
                .user_tools
                .iter()
                .any(|name| glob_matches(pattern, name)),
            "plan" => self
                .plans
                .iter()
                .any(|doc| glob_matches(pattern, &doc.tool_name())),
            "agent" => self
                .agent_names
                .iter()
                .any(|name| glob_matches(pattern, &format!("agent__{name}"))),
            server => {
                return (!catalog.mcp_servers.contains(server)).then(|| {
                    (
                        Item::Server(server.to_string()),
                        Reason::ServerNotConfigured,
                    )
                })
            }
        };
        (!matched).then(nothing)
    }
}

fn is_control(tool: &str) -> bool {
    [
        AGENT_TOOL,
        ASK_TOOL,
        EXIT_TOOL,
        ROUTE_TOOL,
        FILTER_TOOL,
        MAP_TOOL,
        REDUCE_TOOL,
        PLAN_AND_EXECUTE,
    ]
    .contains(&tool)
}

#[cfg(test)]
#[path = "requirements_tests.rs"]
mod tests;
