use super::PackSet;
use crate::tools::{ServerIdentity, ToolRegistry};
use graph_config::pack::{McpRequirement, PackRequires};
use semver::{Version, VersionReq};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const GRAPH_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub graph: String,
    pub os: String,
    pub path: Vec<PathBuf>,
    pub env: BTreeMap<String, String>,
}

impl Machine {
    pub fn current() -> Machine {
        Machine {
            graph: GRAPH_VERSION.to_string(),
            os: std::env::consts::OS.to_string(),
            path: std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default(),
            env: std::env::vars_os()
                .filter_map(|(key, value)| {
                    Some((key.into_string().ok()?, value.into_string().ok()?))
                })
                .collect(),
        }
    }

    pub fn has_binary(&self, name: &str) -> bool {
        if name.contains(std::path::MAIN_SEPARATOR) {
            return is_executable(Path::new(name));
        }
        self.path.iter().any(|dir| is_executable(&dir.join(name)))
    }

    pub fn has_env(&self, name: &str) -> bool {
        self.env.get(name).is_some_and(|value| !value.is_empty())
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evaluation {
    pub refused: Vec<String>,
    pub unmet: Vec<String>,
}

pub fn evaluate(requires: &PackRequires, machine: &Machine) -> Evaluation {
    let mut evaluation = Evaluation::default();
    if let Some(wanted) = &requires.graph {
        if let Some(problem) = graph_problem(wanted, &machine.graph) {
            evaluation.refused.push(problem);
        }
    }
    if !requires.os.is_empty() && !requires.os.contains(&machine.os) {
        evaluation.refused.push(format!(
            "requires.os: needs {}; this machine runs {}",
            requires.os.join(" or "),
            machine.os
        ));
    }
    let missing = absent(&requires.binaries, |binary| machine.has_binary(binary));
    if !missing.is_empty() {
        evaluation.unmet.push(format!(
            "requires.binaries: {} not found on PATH",
            missing.join(", ")
        ));
    }
    let unset = absent(&requires.env, |name| machine.has_env(name));
    if !unset.is_empty() {
        evaluation.unmet.push(format!(
            "requires.env: {} not set (or empty)",
            unset.join(", ")
        ));
    }
    evaluation
}

fn absent(names: &[String], present: impl Fn(&str) -> bool) -> Vec<&str> {
    names
        .iter()
        .map(String::as_str)
        .filter(|name| !present(name))
        .collect()
}

fn meets(wanted: &str, version: &str) -> bool {
    let Ok(requirement) = VersionReq::parse(wanted) else {
        return true;
    };
    Version::parse(version).is_ok_and(|version| requirement.matches(&version))
}

fn graph_problem(wanted: &str, running: &str) -> Option<String> {
    (!meets(wanted, running))
        .then(|| format!("requires.graph: needs graph {wanted}; this is graph {running}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpUse {
    pub site: String,
    pub tool: String,
    pub input: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpNeed {
    pub requirement: McpRequirement,
    pub uses: Vec<McpUse>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackNeeds {
    pub unmet: Vec<String>,
    pub mcp: BTreeMap<String, McpNeed>,
}

impl PackNeeds {
    pub fn is_empty(&self) -> bool {
        self.unmet.is_empty() && self.mcp.is_empty()
    }
}

pub fn mcp_problems(
    prefix: &str,
    need: &McpNeed,
    server: Option<Result<ServerIdentity, String>>,
) -> Vec<String> {
    let field = format!("requires.mcp.{prefix}");
    let identity = match server {
        None => {
            let hint = need
                .requirement
                .url
                .as_ref()
                .map(|url| format!(" (url = \"{url}\")"))
                .unwrap_or_default();
            return vec![format!(
                "{field}: no MCP server is configured under [mcp.{prefix}]{hint}"
            )];
        }
        Some(Err(error)) => {
            return vec![format!("{field}: [mcp.{prefix}] did not connect: {error}")];
        }
        Some(Ok(identity)) => identity,
    };
    let mut problems = Vec::new();
    if let Some(server) = &need.requirement.server {
        if *server != identity.name {
            problems.push(format!(
                "{field}: [mcp.{prefix}] is server '{}', the pack needs '{server}'",
                identity.name
            ));
        }
    }
    if let Some(wanted) = &need.requirement.version {
        if !meets(wanted, &identity.version) {
            problems.push(format!(
                "{field}: [mcp.{prefix}] reports version '{}', the pack needs {wanted}",
                identity.version
            ));
        }
    }
    for found in &need.uses {
        let Some(tool) = identity.tools.iter().find(|tool| tool.name == found.tool) else {
            problems.push(format!(
                "{field}: [mcp.{prefix}] has no tool '{}' ({} names it)",
                found.tool, found.site
            ));
            continue;
        };
        if let Some(input) = &found.input {
            problems.extend(
                crate::pipeline::catalog::input_problems(
                    &found.tool,
                    input.clone(),
                    &tool.input_schema,
                )
                .into_iter()
                .map(|problem| format!("{field}: {}: {problem}", found.site)),
            );
        }
    }
    problems
}

pub type Unmet = BTreeMap<String, Vec<String>>;

pub async fn unmet<'a>(
    packs: &PackSet,
    names: impl IntoIterator<Item = &'a str>,
    registry: &dyn ToolRegistry,
) -> Unmet {
    let mut found = BTreeMap::new();
    for name in names {
        let Some(needs) = packs.needs.get(name) else {
            continue;
        };
        let mut problems = needs.unmet.clone();
        for (prefix, need) in &needs.mcp {
            let server = registry.server_identity(prefix).await;
            problems.extend(mcp_problems(prefix, need, server));
        }
        if !problems.is_empty() {
            found.insert(name.to_string(), problems);
        }
    }
    found
}

pub fn describe_unmet(unmet: &Unmet) -> Vec<String> {
    unmet
        .iter()
        .map(|(pack, problems)| {
            format!(
                "pack '{pack}' has unmet requirements: {}",
                problems.join("; ")
            )
        })
        .collect()
}

pub fn mcp_snippet(prefix: &str, requirement: &McpRequirement) -> Option<String> {
    let url = requirement.url.as_ref()?;
    Some(format!("[mcp.{prefix}]\nurl = \"{url}\""))
}

#[cfg(test)]
mod tests;
