use graph_core::pipeline::authoring;
use graph_core::pipeline::doc::PlanDoc;
use graph_core::pipeline::Pipeline;

pub const WORKBENCH_AGENTS: &[&str] = &[
    include_str!("front_desk.yaml"),
    include_str!("network_drafter.yaml"),
    include_str!("plan_drafter.yaml"),
    include_str!("plan_loader.yaml"),
    include_str!("plan_editor.yaml"),
    include_str!("tool_drafter.yaml"),
    include_str!("agent_drafter.yaml"),
];

pub const FRONT_DESK: &str = "front_desk";

pub const PLAN_DRAFTER: &str = "plan_drafter";

pub const PLAN_EDITOR: &str = "plan_editor";

pub fn workbench_only() -> Vec<graph_core::agent::doc::AgentDoc> {
    let globals: Vec<String> = graph_core::agent::doc::BUILTINS
        .iter()
        .filter_map(|raw| graph_core::agent::doc::parse_agent_source(raw).ok())
        .map(|doc| doc.name)
        .collect();
    let (set, _) = graph_core::agent::doc::AgentSet::load(WORKBENCH_AGENTS, &[]);
    set.iter()
        .filter(|doc| !globals.contains(&doc.name))
        .collect()
}

pub fn is_workbench_only(name: &str) -> bool {
    workbench_only().iter().any(|doc| doc.name == name)
}

pub fn builtin_sources() -> Vec<&'static str> {
    graph_core::agent::doc::BUILTINS
        .iter()
        .chain(WORKBENCH_AGENTS)
        .copied()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    FrontDesk,
    Resume(String),
    Plan(Option<String>),
    Agent(Option<String>),
    Tool(Option<String>),
}

pub fn starting_agent(start: &Start, doc: Option<&PlanDoc>) -> &'static str {
    match (start, doc) {
        (Start::FrontDesk, _) | (Start::Resume(_), _) => FRONT_DESK,
        (Start::Plan(_), Some(_)) => PLAN_EDITOR,
        (Start::Plan(_), None) | (Start::Agent(_), _) | (Start::Tool(_), _) => {
            super::artifact::NETWORK_DRAFTER
        }
    }
}

pub fn model_roles(pipeline: &Pipeline) -> String {
    let roles = pipeline.router.roles();
    let mut section = String::from(
        "## Model roles\nA node's model role, and the model of every agent, prompt tool and infer step, is one of these configured names; no other name works:\n",
    );
    for name in roles.known_names() {
        let Some(choice) = roles.resolve(name) else {
            continue;
        };
        let kind = match pipeline.router.kind_of_role(name) {
            Some(graph_config::ModelKind::Decision) => " (decision model: decide gates only)",
            _ => "",
        };
        let description = choice
            .description
            .as_deref()
            .map(|description| format!(": {description}"))
            .unwrap_or_default();
        section.push_str(&format!("- {name} → {}{kind}{description}\n", choice.model));
    }
    section
}

pub(crate) fn current_draft(pipeline: &Pipeline, doc: &Option<PlanDoc>) -> String {
    let mut section = String::from("## Current draft\n");
    match doc {
        Some(doc) => {
            section.push_str(&format!(
                "The plan pane currently shows '{}', current as of this turn:\n",
                doc.identifier
            ));
            match authoring::to_yaml(doc) {
                Ok(yaml) => section.push_str(&yaml),
                Err(_) => section.push_str("(unserializable draft)\n"),
            }
            let problems = super::tools::plan_problems(pipeline, doc);
            if problems.is_empty() {
                section.push_str("It validates with no problems.\n");
            } else {
                section.push_str(&format!(
                    "Its validation problems:\n- {}\n",
                    problems.join("\n- ")
                ));
            }
        }
        None => section.push_str("(none yet — the pane is empty)"),
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment::{context_hook, AttachmentHost};
    use crate::workbench::attachment::{NetworkAttachment, NetworkSetup};
    use graph_core::agent::doc::{global_fragments, AgentSet};
    use std::sync::Arc;

    #[test]
    fn the_workbench_agents_load_and_validate_together() {
        let (set, errors) = AgentSet::load(&builtin_sources(), &[]);
        assert!(errors.is_empty(), "{errors:?}");
        let fragments: Vec<&str> = global_fragments().into_keys().collect();
        let problems = set.validate(&fragments);
        assert!(problems.is_empty(), "{problems:?}");
        for name in [
            "front_desk",
            "network_drafter",
            "plan_drafter",
            "plan_loader",
            "plan_editor",
            "tool_drafter",
            "agent_drafter",
        ] {
            assert!(set.get(name).is_some(), "{name}");
        }
    }

    #[test]
    fn each_way_into_the_workbench_starts_at_its_agent() {
        let doc: PlanDoc = str::parse::<graph_core::pipeline::doc::PlanDoc>(
            "identifier: demo\nname: Demo\ndescription: d\nsteps: []\n",
        )
        .unwrap();
        assert_eq!(starting_agent(&Start::FrontDesk, None), "front_desk");
        assert_eq!(starting_agent(&Start::Plan(None), None), "network_drafter");
        assert_eq!(
            starting_agent(&Start::Plan(Some("demo".into())), Some(&doc)),
            "plan_editor"
        );
        assert_eq!(starting_agent(&Start::Agent(None), None), "network_drafter");
        assert_eq!(
            starting_agent(&Start::Tool(Some("x".into())), None),
            "network_drafter"
        );
    }

    #[test]
    fn the_current_draft_section_carries_the_yaml() {
        let doc: PlanDoc = str::parse::<graph_core::pipeline::doc::PlanDoc>(
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
        .unwrap();
        let mut pipeline = (*super::super::tools::tests::test_pipeline(Vec::new())).clone();
        pipeline.catalog = Some(Arc::new(graph_core::pipeline::ToolCatalog::default()));
        let section = current_draft(&pipeline, &Some(doc));
        assert!(section.starts_with("## Current draft"));
        assert!(section.contains("identifier: demo"));
        assert!(section.contains("Its validation problems:\n- "));
        assert!(section.contains("t__search"));
        assert!(current_draft(&pipeline, &None).contains("none yet"));
    }

    #[test]
    fn each_agent_gets_the_sections_of_the_attachment_kinds_it_reads() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let root = tempfile::tempdir().unwrap();
        let host: Arc<dyn AttachmentHost> = Arc::new(
            NetworkAttachment::new(NetworkSetup {
                pipeline: super::super::tools::tests::test_pipeline(Vec::new()),
                tx,
                plans_dir: None,
                root: root.path().to_path_buf(),
                doc: None,
            })
            .unwrap(),
        );
        let (set, _) = AgentSet::load(&builtin_sources(), &[]);
        let hook = context_hook(Arc::new(set), vec![host]);
        for builder in [
            "network_drafter",
            "plan_drafter",
            "tool_drafter",
            "agent_drafter",
        ] {
            let sections = hook(builder);
            assert_eq!(sections.len(), 2, "{builder}");
            assert!(sections[0].starts_with("## Model roles"), "{builder}");
            assert!(sections[1].starts_with("## The network draft"), "{builder}");
        }
        assert_eq!(hook("plan_editor").len(), 1);
        assert!(hook("plan_editor")[0].starts_with("## Current draft"));
        assert!(hook("plan_loader").is_empty());
        assert!(hook("front_desk").is_empty());
        assert!(hook("chat").is_empty());
    }
}
