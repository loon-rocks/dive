use anyhow::{bail, Result};
use graph_core::agent::doc::{global_fragments, load_agent_file, AgentDoc, AgentSet};
use graph_core::format::{migrate_file, stamp, Kind, Migrated};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    pub name: String,
    pub description: String,
    pub source: String,
    pub workbench_only: bool,
    pub model: String,
    pub subagents: Vec<String>,
    pub handoffs: Vec<String>,
    pub subagent_io: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentList {
    pub agents: Vec<AgentSummary>,
    pub count: usize,
    pub load_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentDetail {
    pub agent: AgentDoc,
    pub source: String,
    pub yaml: String,
    #[serde(skip)]
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentValidation {
    pub ok: bool,
    pub checked: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub problems: Vec<String>,
}

pub fn subagent_io(doc: &AgentDoc) -> &'static str {
    match (&doc.input_schema, &doc.output_schema) {
        (None, None) => "{prompt} -> {result}",
        (Some(_), None) => "typed input -> {result}",
        (None, Some(_)) => "{prompt} -> typed output",
        (Some(_), Some(_)) => "typed input -> typed output",
    }
}

fn source(doc: &AgentDoc, workbench: bool) -> String {
    if workbench {
        "built-in, workbench only (graph wb)".to_string()
    } else {
        doc.source.describe()
    }
}

fn runnable_and_workbench(
    dirs: &[PathBuf],
    packs: &[AgentDoc],
    workbench_only: &[AgentDoc],
) -> (Vec<(AgentDoc, bool)>, Vec<String>) {
    let (set, errors) = AgentSet::load(graph_core::agent::doc::BUILTINS, dirs);
    let set = set.with_packs(packs.iter().cloned());
    let mut agents: Vec<(AgentDoc, bool)> = set.iter().map(|doc| (doc, false)).collect();
    for doc in workbench_only {
        if !agents.iter().any(|(known, _)| known.name == doc.name) {
            agents.push((doc.clone(), true));
        }
    }
    agents.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name));
    (agents, errors)
}

pub fn list(dirs: &[PathBuf], packs: &[AgentDoc], workbench_only: &[AgentDoc]) -> AgentList {
    let (listed, load_errors) = runnable_and_workbench(dirs, packs, workbench_only);
    let agents: Vec<AgentSummary> = listed
        .iter()
        .map(|(doc, workbench)| AgentSummary {
            name: doc.name.clone(),
            description: doc.description.clone(),
            source: source(doc, *workbench),
            workbench_only: *workbench,
            model: doc.model.clone(),
            subagents: doc.subagents.clone(),
            handoffs: doc.handoffs.clone(),
            subagent_io: subagent_io(doc).to_string(),
        })
        .collect();
    AgentList {
        count: agents.len(),
        agents,
        load_errors,
    }
}

pub fn show(
    dirs: &[PathBuf],
    packs: &[AgentDoc],
    workbench_only: &[AgentDoc],
    name: &str,
) -> Result<AgentDetail> {
    let (listed, _) = runnable_and_workbench(dirs, packs, workbench_only);
    let Some((doc, workbench)) = listed.iter().find(|(doc, _)| doc.name == name).cloned() else {
        let known: Vec<String> = listed.iter().map(|(d, _)| d.name.clone()).collect();
        bail!("unknown agent '{name}' (defined: {})", known.join(", "));
    };
    let mut value = serde_yaml::to_value(&doc)?;
    stamp(Kind::Agent, &mut value);
    Ok(AgentDetail {
        yaml: serde_yaml::to_string(&value)?,
        source: doc.source.describe(),
        label: source(&doc, workbench),
        agent: doc,
    })
}

pub fn validate(
    builtin_sources: &[&str],
    dirs: &[PathBuf],
    packs: &[AgentDoc],
    path: Option<&Path>,
) -> Result<AgentValidation> {
    let (set, mut problems) = AgentSet::load(builtin_sources, dirs);
    let mut set = set.with_packs(packs.iter().cloned());
    let mut checked = "all agents".to_string();
    if let Some(path) = path {
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        checked = path.display().to_string();
        match load_agent_file(path) {
            Ok(doc) => {
                let name = doc.name.clone();
                set = AgentSet::layered(set.iter().collect(), vec![vec![doc]]);
                problems = set.agent_problems(&name, &fragment_names());
            }
            Err(error) => problems = vec![error],
        }
    } else {
        problems.extend(set.validate(&fragment_names()));
    }
    let ok = problems.is_empty();
    Ok(AgentValidation {
        ok,
        error: (!ok).then(|| format!("{checked} has problems")),
        checked,
        problems,
    })
}

fn fragment_names() -> Vec<&'static str> {
    global_fragments().into_keys().collect()
}

pub fn migrate(path: &Path) -> Result<Migrated> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let check = |value: &serde_yaml::Value| -> Result<(), String> {
        let yaml = serde_yaml::to_string(value).map_err(|e| e.to_string())?;
        graph_core::agent::doc::parse_agent_source(&yaml).map(|_| ())
    };
    migrate_file(Kind::Agent, path, &check).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_agent(dir: &Path, file: &str, body: &str) -> PathBuf {
        let path = dir.join(file);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn workbench_agent() -> AgentDoc {
        graph_core::agent::doc::parse_agent_source(
            "name: wb_helper\ndescription: helps\nmodel: chat\ntools: []\nsystem_prompt: hi\n",
        )
        .unwrap()
    }

    #[test]
    fn list_merges_project_files_and_workbench_agents_with_the_cli_shape() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "search_bot.yaml",
            "name: search_bot\ndescription: searches\nmodel: chat\ntools: []\ninput_schema: {type: object}\nsystem_prompt: hi\n",
        );
        let listed = list(&[dir.path().to_path_buf()], &[], &[workbench_agent()]);
        assert_eq!(listed.count, listed.agents.len());
        let bot = listed
            .agents
            .iter()
            .find(|agent| agent.name == "search_bot")
            .unwrap();
        assert_eq!(bot.subagent_io, "typed input -> {result}");
        let helper = listed
            .agents
            .iter()
            .find(|agent| agent.name == "wb_helper")
            .unwrap();
        assert!(helper.workbench_only);
        assert_eq!(helper.source, "built-in, workbench only (graph wb)");
        let value = serde_json::to_value(&listed).unwrap();
        assert_eq!(value["loadErrors"], json!([]));
        assert_eq!(
            value["agents"][0]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            [
                "description",
                "handoffs",
                "model",
                "name",
                "source",
                "subagentIo",
                "subagents",
                "workbenchOnly"
            ]
        );
    }

    #[test]
    fn show_stamps_the_yaml_and_names_unknown_agents() {
        let detail = show(&[], &[], &[workbench_agent()], "wb_helper").unwrap();
        assert!(
            detail
                .yaml
                .starts_with(&format!("version: {}\n", graph_core::format::AGENT_FORMAT)),
            "{}",
            detail.yaml
        );
        assert_eq!(detail.label, "built-in, workbench only (graph wb)");
        let value = serde_json::to_value(&detail).unwrap();
        assert_eq!(value["agent"]["name"], "wb_helper");
        let error = show(&[], &[], &[], "nope").unwrap_err().to_string();
        assert!(
            error.starts_with("unknown agent 'nope' (defined: "),
            "{error}"
        );
    }

    #[test]
    fn validate_reports_a_bad_file_with_an_error_and_passes_the_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_agent(
            dir.path(),
            "bad.yaml",
            "name: bad\ndescription: d\nmodel: chat\ntools: [map]\nsubagents: [ghost]\nsystem_prompt: hi\n",
        );
        let verdict = validate(graph_core::agent::doc::BUILTINS, &[], &[], Some(&path)).unwrap();
        assert!(!verdict.ok);
        assert_eq!(
            verdict.error,
            Some(format!("{} has problems", path.display()))
        );
        let clean = validate(graph_core::agent::doc::BUILTINS, &[], &[], None).unwrap();
        assert!(clean.ok, "{clean:?}");
        assert!(serde_json::to_value(&clean).unwrap().get("error").is_none());
    }

    #[test]
    fn migrate_stamps_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_agent(
            dir.path(),
            "a.yaml",
            "name: a\ndescription: d\nmodel: chat\ntools: []\nsystem_prompt: hi\n",
        );
        assert_eq!(migrate(&path).unwrap().to, graph_core::format::AGENT_FORMAT);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with(&format!("version: {}\n", graph_core::format::AGENT_FORMAT)));
    }
}
