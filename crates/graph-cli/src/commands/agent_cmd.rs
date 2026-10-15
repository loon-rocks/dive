use crate::cli::AgentCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::Result;
use graph_core::agent::doc::{agent_dirs, AgentDoc};
use graph_runtime::ops;
use std::path::{Path, PathBuf};

pub fn run(command: AgentCommand) -> Result<()> {
    let dirs = agent_dirs();
    if let AgentCommand::Migrate { path, json } = command {
        return report(migrate(&path)?, json);
    }
    let installed = graph_runtime::runtime::load_packs(&crate::host::load_config()?)?;
    let packs: Vec<AgentDoc> = installed.agents().cloned().collect();
    match command {
        AgentCommand::List { json } => report(list(&dirs, &installed.public_agents()), json),
        AgentCommand::Show { name, json } => report(show(&dirs, &packs, &name)?, json),
        AgentCommand::Validate { path, json } => {
            report(validate(&dirs, &packs, path.as_deref())?, json)
        }
        AgentCommand::Migrate { .. } => unreachable!("handled before packs load"),
    }
}

pub(crate) fn list(dirs: &[PathBuf], packs: &[AgentDoc]) -> Outcome {
    let listed = ops::agent::list(dirs, packs, &crate::workbench::agents::workbench_only());
    let text: String = listed
        .agents
        .iter()
        .map(|agent| format!("{}\t{}\t{}\n", agent.name, agent.source, agent.description))
        .collect();
    let errors = listed.load_errors.len();
    let outcome = Outcome::raw(text, serde_json::to_value(&listed).unwrap_or_default());
    if errors == 0 {
        outcome
    } else {
        outcome.with_note(format!(
            "{errors} agent file(s) failed to load — run `graph agent validate`"
        ))
    }
}

pub(crate) fn show(dirs: &[PathBuf], packs: &[AgentDoc], name: &str) -> Result<Outcome> {
    let detail = ops::agent::show(
        dirs,
        packs,
        &crate::workbench::agents::workbench_only(),
        name,
    )?;
    let body = serde_json::to_value(&detail)?;
    Ok(Outcome::raw(detail.yaml, body).with_note(format!("source: {}", detail.label)))
}

pub(crate) fn validate(
    dirs: &[PathBuf],
    packs: &[AgentDoc],
    path: Option<&Path>,
) -> Result<Outcome> {
    let verdict = ops::agent::validate(
        &crate::workbench::agents::builtin_sources(),
        dirs,
        packs,
        path,
    )?;
    let body = serde_json::to_value(&verdict)?;
    Ok(if verdict.ok {
        Outcome::ok(body)
    } else {
        Outcome::rejected(body)
    })
}

fn migrate(path: &Path) -> Result<Outcome> {
    Ok(crate::commands::plan_cmd::migrated_outcome(
        ops::agent::migrate(path)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::agent::doc::{AgentSet, AgentSource};

    fn write_agent(dir: &Path, file: &str, body: &str) -> PathBuf {
        let path = dir.join(file);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn list_includes_the_builtin_chat_agent_and_project_files() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "search_bot.yaml",
            "name: search_bot\ndescription: searches\nmodel: chat\ntools: []\ninput_schema: {type: object, required: [query], properties: {query: {type: string, description: what to find}}}\noutput_schema: {type: object, properties: {hits: {type: array}}}\nsystem_prompt: hi\n",
        );
        let outcome = list(&[dir.path().to_path_buf()], &[]);
        let names: Vec<&str> = outcome.body["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "agent_drafter",
                "chat",
                "front_desk",
                "plan_drafter",
                "plan_editor",
                "plan_loader",
                "search_bot",
                "tool_drafter"
            ]
        );
        assert_eq!(outcome.body["agents"][0]["source"], "built-in");
        let by_name = |name: &str| {
            outcome.body["agents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|agent| agent["name"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(by_name("plan_drafter")["workbenchOnly"], true);
        assert_eq!(by_name("tool_drafter")["workbenchOnly"], false);
        assert_eq!(by_name("chat")["workbenchOnly"], false);
        assert_eq!(
            outcome.body["agents"][6]["subagentIo"],
            "typed input -> typed output"
        );
        assert_eq!(
            outcome.body["agents"][7]["subagentIo"],
            "{prompt} -> {result}"
        );
    }

    #[test]
    fn show_prints_stamped_yaml_from_the_overriding_file() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "chat.yaml",
            "name: chat\ndescription: mine\nmodel: chat\ntools: []\nsystem_prompt: hi\n",
        );
        let outcome = show(&[dir.path().to_path_buf()], &[], "chat").unwrap();
        let yaml = outcome.raw.unwrap();
        assert!(yaml.starts_with("version: 1\n"), "{yaml}");
        assert!(yaml.contains("description: mine"));
        assert!(outcome.body["source"]
            .as_str()
            .unwrap()
            .ends_with("chat.yaml"));
        assert!(show(&[], &[], "nope").is_err());
    }

    #[test]
    fn validate_reports_one_files_problems_against_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_agent(
            dir.path(),
            "bad.yaml",
            "name: bad\ndescription: d\nmodel: chat\ntools: [map]\nsubagents: [ghost]\nsystem_prompt: hi\n",
        );
        let outcome = validate(&[], &[], Some(&path)).unwrap();
        assert!(outcome.rejected);
        let problems = outcome.body["problems"].to_string();
        assert!(problems.contains("'map' is a plan step"), "{problems}");
        assert!(problems.contains("subagent 'ghost'"), "{problems}");
        let clean = validate(&[], &[], None).unwrap();
        assert!(!clean.rejected, "{}", clean.body);
    }

    #[test]
    fn migrate_stamps_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_agent(
            dir.path(),
            "a.yaml",
            "name: a\ndescription: d\nmodel: chat\ntools: []\nsystem_prompt: hi\n",
        );
        let outcome = migrate(&path).unwrap();
        assert_eq!(outcome.body["to"], 1);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("version: 1\n"));
    }

    #[test]
    fn builtin_sources_are_reported_as_built_in() {
        let (set, _) = AgentSet::load(&crate::workbench::agents::builtin_sources(), &[]);
        assert_eq!(set.get("chat").unwrap().source, AgentSource::Builtin);
    }
}
