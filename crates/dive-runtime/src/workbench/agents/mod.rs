use dive_core::pipeline::authoring;
use dive_core::pipeline::doc::PlanDoc;
use dive_core::pipeline::Pipeline;

pub const WORKBENCH_AGENTS: &[&str] = &[
    include_str!("front_desk.yaml"),
    include_str!("network_drafter.yaml"),
    include_str!("plan_drafter.yaml"),
    include_str!("plan_loader.yaml"),
    include_str!("plan_editor.yaml"),
    include_str!("tool_drafter.yaml"),
    include_str!("agent_drafter.yaml"),
    include_str!("network_editor.yaml"),
];

pub const FRONT_DESK: &str = "front_desk";

pub const PLAN_DRAFTER: &str = "plan_drafter";

pub const PLAN_EDITOR: &str = "plan_editor";

pub const NETWORK_EDITOR: &str = "network_editor";

pub fn builtin_sources() -> Vec<&'static str> {
    dive_core::agent::doc::BUILTINS
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
    Edit {
        items: Vec<crate::evidence::Item>,
        evidence: Option<Box<crate::evidence::Evidence>>,
    },
}

pub fn starting_agent(start: &Start, doc: Option<&PlanDoc>) -> &'static str {
    match (start, doc) {
        (Start::FrontDesk, _) | (Start::Resume(_), _) => FRONT_DESK,
        (Start::Plan(_), Some(_)) => PLAN_EDITOR,
        (Start::Plan(_), None) | (Start::Agent(_), _) | (Start::Tool(_), _) => {
            super::artifact::NETWORK_DRAFTER
        }
        (Start::Edit { .. }, _) => NETWORK_EDITOR,
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
            Some(dive_config::ModelKind::Decision) => " (decision model: decide gates only)",
            Some(dive_config::ModelKind::Image) => " (image model: builtin__generate_image only)",
            _ => "",
        };
        let images = match pipeline.router.kind_of_role(name) {
            Some(dive_config::ModelKind::Image) => "",
            _ if !choice.takes_images() => " (takes no images: refs reach it as text)",
            _ => "",
        };
        let description = roles
            .description_of(name)
            .map(|description| format!(": {description}"))
            .unwrap_or_default();
        section.push_str(&format!(
            "- {name} → {}{kind}{images}{description}\n",
            choice.model
        ));
    }
    section.push_str("Every other role takes images: a file ref in what it reads (a tool result, a step's input, an instruction) reaches it as the image.\n");
    section.push_str(&decision_line(pipeline));
    section
}

fn decision_line(pipeline: &Pipeline) -> String {
    let deciders: Vec<String> = pipeline
        .router
        .roles()
        .known_names()
        .into_iter()
        .filter(|name| pipeline.router.kind_of_role(name) == Some(dive_config::ModelKind::Decision))
        .map(|name| format!("`{name}`"))
        .collect();
    if deciders.is_empty() {
        return "Decision model: none configured. Use `infer` on the judge role for classification, yes/no checks, choices and scores; never write a `decide` gate, a builtin__decide step or a decision tool.\n".to_string();
    }
    let default = if deciders.iter().any(|name| name == "`decider`") {
        "decide gates, builtin__decide and decision tools use `decider` by default"
    } else {
        "name it as `model` on decide gates, builtin__decide and decision tools, which default to `decider`"
    };
    format!(
        "Decision model: available as {}; {default}. Use it for every classification, yes/no check, choice and score.\n",
        deciders.join(", ")
    )
}

pub fn design_context(
    agents: std::sync::Arc<dive_core::agent::doc::AgentSet>,
    pipeline: std::sync::Arc<Pipeline>,
) -> dive_core::agent::conversation::ContextHook {
    std::sync::Arc::new(move |agent: &str| match agents.get(agent) {
        Some(doc) if reads_design_rules(&doc) => vec![model_roles(&pipeline)],
        _ => Vec::new(),
    })
}

pub fn reads_design_rules(doc: &dive_core::agent::doc::AgentDoc) -> bool {
    doc.system_prompt.contains("{{rules.design}}")
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
    use dive_core::agent::doc::{global_fragments, AgentSet};
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
            "network_editor",
        ] {
            assert!(set.get(name).is_some(), "{name}");
        }
    }

    #[test]
    fn each_way_into_the_workbench_starts_at_its_agent() {
        let doc: PlanDoc = str::parse::<dive_core::pipeline::doc::PlanDoc>(
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
        assert_eq!(
            starting_agent(
                &Start::Edit {
                    items: Vec::new(),
                    evidence: None
                },
                None
            ),
            "network_editor"
        );
    }

    #[test]
    fn the_current_draft_section_carries_the_yaml() {
        let doc: PlanDoc = str::parse::<dive_core::pipeline::doc::PlanDoc>(
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
        pipeline.catalog = Some(Arc::new(dive_core::pipeline::ToolCatalog::default()));
        let section = current_draft(&pipeline, &Some(doc));
        assert!(section.starts_with("## Current draft"));
        assert!(section.contains("identifier: demo"));
        assert!(section.contains("Its validation problems:\n- "));
        assert!(section.contains("t__search"));
        assert!(current_draft(&pipeline, &None).contains("none yet"));
    }

    struct NoDecider;

    #[async_trait::async_trait]
    impl dive_llm::DecisionProvider for NoDecider {
        async fn decide(
            &self,
            _req: dive_llm::decision::DecisionRequest,
        ) -> Result<dive_llm::decision::DecisionResponse, dive_llm::LlmError> {
            Err(dive_llm::LlmError::Unsupported("not in tests".to_string()))
        }
    }

    fn choice(provider: &str) -> dive_config::ModelChoice {
        dive_config::ModelChoice {
            kind: None,
            provider: provider.to_string(),
            model: format!("{provider}-model"),
            temperature: None,
            description: None,
            fallbacks: Vec::new(),
            context_window: None,
            images: None,
        }
    }

    fn pipeline_with_roles(roles: &[(&str, &str)]) -> Pipeline {
        let router = dive_llm::ModelRouter::with_providers(
            std::collections::HashMap::new(),
            roles
                .iter()
                .map(|(role, provider)| {
                    let mut choice = choice(provider);
                    if *provider == "ts" {
                        choice.kind = Some(dive_config::ModelKind::Decision);
                    }
                    if *provider == "text" {
                        choice.images = Some(false);
                    }
                    (role.to_string(), choice)
                })
                .collect(),
        )
        .with_deciders(std::collections::HashMap::from([(
            "ts".to_string(),
            Arc::new(NoDecider) as Arc<dyn dive_llm::DecisionProvider>,
        )]));
        let mut pipeline = (*super::super::tools::tests::test_pipeline(Vec::new())).clone();
        pipeline.router = Arc::new(router);
        pipeline
    }

    #[test]
    fn the_model_roles_section_marks_the_roles_that_take_no_images() {
        let section = model_roles(&pipeline_with_roles(&[("default", "p"), ("judge", "text")]));
        assert!(
            section.contains("- judge → text-model (takes no images: refs reach it as text)"),
            "{section}"
        );
        assert!(
            section.contains("- default → p-model: General"),
            "{section}"
        );
        assert!(
            section
                .trim_end()
                .lines()
                .last()
                .unwrap()
                .starts_with("Decision model:"),
            "{section}"
        );
    }

    #[test]
    fn the_model_roles_section_says_whether_a_decision_model_is_available() {
        let with = model_roles(&pipeline_with_roles(&[("default", "p"), ("decider", "ts")]));
        assert!(
            with.contains("Decision model: available as `decider`; decide gates, builtin__decide and decision tools use `decider` by default."),
            "{with}"
        );
        let custom = model_roles(&pipeline_with_roles(&[("default", "p"), ("triage", "ts")]));
        assert!(
            custom.contains("Decision model: available as `triage`; name it as `model`"),
            "{custom}"
        );
        let without = model_roles(&pipeline_with_roles(&[("default", "p")]));
        assert!(
            without.contains("Decision model: none configured. Use `infer` on the judge role"),
            "{without}"
        );
        assert!(!without.contains("available as"), "{without}");
    }

    #[test]
    fn the_design_principles_reach_every_agent_that_designs() {
        let (set, _) = AgentSet::load(&builtin_sources(), &[]);
        let workbench = AgentSet::load(WORKBENCH_AGENTS, &[]).0;
        let globals = AgentSet::load(dive_core::agent::doc::BUILTINS, &[]).0;
        let session = std::collections::BTreeMap::from([
            ("date", "today".to_string()),
            ("user", String::new()),
        ]);
        let fragments = global_fragments();
        let design = dive_core::prompts::DESIGN_RULES;
        assert!(design.contains("Decision models first"));
        for (agents, name) in [
            (&workbench, "network_drafter"),
            (&workbench, "network_editor"),
            (&workbench, "tool_drafter"),
            (&workbench, "plan_drafter"),
            (&workbench, "agent_drafter"),
            (&globals, "tool_drafter"),
            (&globals, "agent_drafter"),
        ] {
            let doc = agents.get(name).unwrap();
            let rendered =
                dive_core::agent::doc::render_system_prompt(&doc, &fragments, &session).unwrap();
            assert!(rendered.contains(design), "{name}");
            assert!(reads_design_rules(&doc), "{name}");
        }
        assert!(!reads_design_rules(&set.get("chat").unwrap()));
        assert!(!reads_design_rules(&set.get("front_desk").unwrap()));
    }

    #[test]
    fn agents_outside_the_workbench_that_read_the_design_rules_get_the_model_roles() {
        let globals = Arc::new(AgentSet::load(dive_core::agent::doc::BUILTINS, &[]).0);
        let hook = design_context(
            globals,
            Arc::new(pipeline_with_roles(&[("default", "p"), ("decider", "ts")])),
        );
        for name in ["tool_drafter", "agent_drafter"] {
            let sections = hook(name);
            assert_eq!(sections.len(), 1, "{name}");
            assert!(sections[0].starts_with("## Model roles"), "{name}");
            assert!(sections[0].contains("Decision model: available"), "{name}");
        }
        assert!(hook("chat").is_empty());
        assert!(hook("nobody").is_empty());
    }

    #[test]
    fn every_agent_sees_a_loaded_draft_and_nothing_once_it_is_unloaded() {
        let build = || {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let root = tempfile::tempdir().unwrap();
            NetworkAttachment::new(NetworkSetup {
                pipeline: super::super::tools::tests::test_pipeline(Vec::new()),
                tx,
                plans_dir: None,
                root: root.path().to_path_buf(),
                doc: None,
            })
            .unwrap()
        };
        let loaded: Arc<dyn AttachmentHost> = Arc::new(build());
        let hook = context_hook(vec![loaded]);
        for agent in [
            "chat",
            "front_desk",
            "network_drafter",
            "plan_editor",
            "anyone",
        ] {
            let sections = hook(agent);
            assert_eq!(sections.len(), 2, "{agent}");
            assert!(sections[0].starts_with("## Model roles"), "{agent}");
            assert!(sections[1].starts_with("## The network draft"), "{agent}");
        }
        let unloaded: Arc<dyn AttachmentHost> = Arc::new(build().unloaded());
        assert!(context_hook(vec![unloaded])("network_drafter").is_empty());
    }
}
