use crate::runtime::Runtime;
use dive_core::agent::doc::{builtin_agents, load_agent_dir, AgentDoc, AgentSet, AgentSource};
use dive_core::pipeline::catalog::{agent_tool_patterns, step_tools};
use dive_core::pipeline::doc::{builtin_plan_docs, load_plan_docs, DocError, PlanDoc};
use dive_core::user_tools::{BUILTIN_TOOL_PREFIX, USER_TOOL_PREFIX};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Plan,
    Tool,
    Agent,
}

impl std::fmt::Display for ItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ItemKind::Plan => "plan",
            ItemKind::Tool => "tool",
            ItemKind::Agent => "agent",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRef {
    pub kind: ItemKind,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Removal {
    pub kind: ItemKind,
    pub name: String,
    pub path: String,
    pub overrides: bool,
    pub takes_over: Option<String>,
    pub users: Vec<ItemRef>,
}

impl Removal {
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!("removed {}", self.path)];
        match self.takes_over.as_deref() {
            Some("built-in") => {
                lines.push(format!("the built-in {} {} is back", self.kind, self.name))
            }
            Some(source) => lines.push(format!("{source} takes over")),
            None => {}
        }
        if !self.users.is_empty() {
            let users: Vec<String> = self
                .users
                .iter()
                .map(|user| format!("{} {}", user.kind, user.name))
                .collect();
            let consequence = match self.takes_over {
                Some(_) => "they use the copy that took over",
                None => "they fail until updated",
            };
            lines.push(format!(
                "still used by {} — {consequence}",
                users.join(", ")
            ));
        }
        lines
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error("no {kind} named '{name}'")]
    NotFound { kind: ItemKind, name: String },
    #[error("{kind} '{name}' is built in and has no override to delete")]
    BuiltIn { kind: ItemKind, name: String },
    #[error("{kind} '{name}' belongs to the pack '{pack}' — remove the whole pack with `dive pack remove {pack}`")]
    Pack {
        kind: ItemKind,
        name: String,
        pack: String,
    },
    #[error("{0}")]
    Failed(String),
}

pub fn removal(runtime: &Runtime, kind: ItemKind, name: &str) -> Result<Removal, RemoveError> {
    removal_in(runtime, &dive_core::agent::doc::agent_dirs(), kind, name)
}

pub fn remove(runtime: &Runtime, kind: ItemKind, name: &str) -> Result<Removal, RemoveError> {
    remove_in(runtime, &dive_core::agent::doc::agent_dirs(), kind, name)
}

fn remove_in(
    runtime: &Runtime,
    agent_dirs: &[PathBuf],
    kind: ItemKind,
    name: &str,
) -> Result<Removal, RemoveError> {
    let removal = removal_in(runtime, agent_dirs, kind, name)?;
    let path = Path::new(&removal.path);
    std::fs::remove_file(path).map_err(|error| {
        RemoveError::Failed(format!("couldn't delete {}: {error}", removal.path))
    })?;
    match kind {
        ItemKind::Tool => {
            if let Some(bare) = removal.name.strip_prefix(USER_TOOL_PREFIX) {
                runtime.drafted().forget_tool(bare);
            }
        }
        ItemKind::Agent => runtime.drafted().forget_agent(&removal.name),
        ItemKind::Plan => {}
    }
    Ok(removal)
}

fn removal_in(
    runtime: &Runtime,
    agent_dirs: &[PathBuf],
    kind: ItemKind,
    name: &str,
) -> Result<Removal, RemoveError> {
    let found = match kind {
        ItemKind::Plan => plan_file(runtime, name)?,
        ItemKind::Tool => tool_file(runtime, name)?,
        ItemKind::Agent => agent_file(runtime, agent_dirs, name)?,
    };
    Ok(Removal {
        users: users(runtime, agent_dirs, kind, &found.name),
        kind,
        name: found.name,
        path: found.path.display().to_string(),
        overrides: found.overrides,
        takes_over: found.takes_over,
    })
}

struct Found {
    name: String,
    path: PathBuf,
    overrides: bool,
    takes_over: Option<String>,
}

fn plan_dirs(runtime: &Runtime) -> Vec<PathBuf> {
    runtime
        .config
        .plans
        .paths
        .iter()
        .map(|p| dive_config::expand_tilde(p))
        .collect()
}

fn plan_file(runtime: &Runtime, name: &str) -> Result<Found, RemoveError> {
    let builtin = builtin_plan_docs()
        .into_iter()
        .any(|doc| doc.identifier == name);
    let pack = runtime
        .packs
        .plans()
        .find(|doc| doc.identifier == name)
        .cloned();
    let Some(path) = runtime.find_plan_file(name) else {
        return Err(match (builtin, pack) {
            (true, _) => RemoveError::BuiltIn {
                kind: ItemKind::Plan,
                name: name.to_string(),
            },
            (false, Some(doc)) => RemoveError::Pack {
                kind: ItemKind::Plan,
                name: name.to_string(),
                pack: doc.pack.map(|source| source.name).unwrap_or_default(),
            },
            (false, None) => RemoveError::NotFound {
                kind: ItemKind::Plan,
                name: name.to_string(),
            },
        });
    };
    let loaded = load_plan_docs(&plan_dirs(runtime));
    let shown = path.display().to_string();
    let next_file = loaded
        .docs
        .iter()
        .filter(|doc| doc.identifier == name)
        .filter_map(|doc| doc.path.as_ref().map(|p| p.display().to_string()))
        .chain(loaded.skipped.iter().filter_map(|error| match error {
            DocError::Duplicate { path, identifier } if identifier == name => Some(path.clone()),
            _ => None,
        }))
        .find(|other| *other != shown);
    let takes_over = next_file
        .or_else(|| builtin.then(|| "built-in".to_string()))
        .or_else(|| pack.map(|doc| doc.source_label()));
    Ok(Found {
        name: name.to_string(),
        path,
        overrides: builtin,
        takes_over,
    })
}

fn tool_file(runtime: &Runtime, name: &str) -> Result<Found, RemoveError> {
    let docs = runtime
        .tool_docs()
        .map_err(|error| RemoveError::Failed(format!("{error:#}")))?;
    let not_found = || RemoveError::NotFound {
        kind: ItemKind::Tool,
        name: name.to_string(),
    };
    if let Some(bare) = name.strip_prefix(BUILTIN_TOOL_PREFIX) {
        let doc = docs
            .builtins
            .iter()
            .find(|doc| doc.name == bare)
            .ok_or_else(not_found)?;
        let path = doc.path.clone().ok_or_else(|| RemoveError::BuiltIn {
            kind: ItemKind::Tool,
            name: name.to_string(),
        })?;
        return Ok(Found {
            name: name.to_string(),
            path,
            overrides: true,
            takes_over: Some("built-in".to_string()),
        });
    }
    for (source, tools) in &docs.packs {
        let prefix =
            dive_core::packs::catalog_name(&source.name, dive_core::packs::ItemKind::Tool, "");
        if name
            .strip_prefix(&prefix)
            .is_some_and(|bare| tools.iter().any(|doc| doc.name == bare))
        {
            return Err(RemoveError::Pack {
                kind: ItemKind::Tool,
                name: name.to_string(),
                pack: source.name.clone(),
            });
        }
    }
    let bare = name.strip_prefix(USER_TOOL_PREFIX).unwrap_or(name);
    let path = docs
        .user
        .iter()
        .find(|doc| doc.name == bare)
        .and_then(|doc| doc.path.clone())
        .ok_or_else(not_found)?;
    Ok(Found {
        name: format!("{USER_TOOL_PREFIX}{bare}"),
        path,
        overrides: false,
        takes_over: None,
    })
}

fn builtin_agent_names() -> Vec<String> {
    builtin_agents(&crate::workbench::agents::builtin_sources())
        .into_iter()
        .map(|doc| doc.name)
        .collect()
}

fn agent_file(runtime: &Runtime, dirs: &[PathBuf], name: &str) -> Result<Found, RemoveError> {
    if let Some(doc) = runtime.packs.agents().find(|doc| doc.name == name) {
        return Err(RemoveError::Pack {
            kind: ItemKind::Agent,
            name: name.to_string(),
            pack: doc
                .source
                .pack()
                .map(|source| source.name.clone())
                .unwrap_or_default(),
        });
    }
    let builtin = builtin_agent_names().iter().any(|known| known == name);
    let mut files = dirs.iter().filter_map(|dir| {
        load_agent_dir(dir)
            .docs
            .into_iter()
            .find(|doc| doc.name == name)
            .and_then(|doc| match doc.source {
                AgentSource::File(path) => Some(path),
                _ => None,
            })
    });
    let Some(path) = files.next() else {
        return Err(match builtin {
            true => RemoveError::BuiltIn {
                kind: ItemKind::Agent,
                name: name.to_string(),
            },
            false => RemoveError::NotFound {
                kind: ItemKind::Agent,
                name: name.to_string(),
            },
        });
    };
    let takes_over = files
        .next()
        .map(|next| next.display().to_string())
        .or_else(|| builtin.then(|| "built-in".to_string()));
    Ok(Found {
        name: name.to_string(),
        path,
        overrides: builtin,
        takes_over,
    })
}

fn users(runtime: &Runtime, agent_dirs: &[PathBuf], kind: ItemKind, name: &str) -> Vec<ItemRef> {
    let reference = match kind {
        ItemKind::Plan => format!("plan__{name}"),
        ItemKind::Tool => name.to_string(),
        ItemKind::Agent => format!("agent__{name}"),
    };
    let mut plans: Vec<PlanDoc> = load_plan_docs(&plan_dirs(runtime)).docs;
    plans.extend(runtime.packs.plans().cloned());
    let mut found: Vec<ItemRef> = plans
        .iter()
        .filter(|doc| !(kind == ItemKind::Plan && doc.identifier == name))
        .filter(|doc| {
            step_tools(&doc.steps)
                .into_iter()
                .map(|(_, tool)| tool)
                .chain(
                    agent_tool_patterns(&doc.steps)
                        .into_iter()
                        .map(|(_, tool)| tool),
                )
                .any(|tool| tool == reference)
        })
        .map(|doc| ItemRef {
            kind: ItemKind::Plan,
            name: doc.identifier.clone(),
        })
        .collect();
    let (agents, _) = AgentSet::load(&[], agent_dirs);
    let agents: Vec<AgentDoc> = agents
        .with_packs(runtime.packs.agents().cloned())
        .iter()
        .collect();
    found.extend(
        agents
            .iter()
            .filter(|doc| !(kind == ItemKind::Agent && doc.name == name))
            .filter(|doc| {
                doc.tools.iter().any(|entry| entry.name == reference)
                    || (kind == ItemKind::Agent
                        && (doc.subagents.iter().any(|agent| agent == name)
                            || doc.handoffs.iter().any(|agent| agent == name)))
            })
            .map(|doc| ItemRef {
                kind: ItemKind::Agent,
                name: doc.name.clone(),
            }),
    );
    found
}

#[cfg(test)]
mod tests;
