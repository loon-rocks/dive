use super::*;
use crate::packs::InstalledPacks;
use crate::runtime::RuntimeOptions;
use dive_config::pack::LayerKind;
use dive_core::packs::PackSource;
use dive_llm::ModelRouter;
use std::collections::HashMap;
use std::sync::Arc;

const ECHO_TOOL: &str =
    "name: echo\ndescription: Echoes\nkind: exec\ncommand: echo\nargs: ['{}']\n";

const CALLER_PLAN: &str = "identifier: caller\nname: Caller\ndescription: calls echo\nsteps:\n  - id: E1\n    tool_name: user__echo\n    input: {}\n";

const AGENT_STEP_PLAN: &str = "identifier: delegates\nname: Delegates\ndescription: hands echo to an agent\nsteps:\n  - id: E1\n    tool_name: agent\n    input:\n      prompt: go\n      outputSchema: { type: object }\n      tools: [user__echo, plan__caller]\n";

fn agent(name: &str, tools: &[&str], subagents: &[&str]) -> String {
    format!(
        "name: {name}\ndescription: {name}\nmodel: chat\ntools: [{}]\nsubagents: [{}]\nsystem_prompt: hi\n",
        tools.join(", "),
        subagents.join(", ")
    )
}

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in [
            "project/plans",
            "global/plans",
            "project/tools/builtin",
            "project/agents",
            "global/agents",
        ] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        Self { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.path(rel);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn agent_dirs(&self) -> Vec<PathBuf> {
        vec![self.path("project/agents"), self.path("global/agents")]
    }

    fn runtime(&self, packs: InstalledPacks) -> Runtime {
        let mut config = dive_config::Config::default();
        config.tools.paths = vec![self.path("project/tools")];
        config.plans.paths = vec![self.path("project/plans"), self.path("global/plans")];
        let router = ModelRouter::with_providers(HashMap::new(), Default::default());
        let options = RuntimeOptions {
            storage_backend: Some("memory".to_string()),
            capture_content: false,
        };
        Runtime::with_router(config, packs, options, router).unwrap()
    }

    fn removal(&self, kind: ItemKind, name: &str) -> Result<Removal, RemoveError> {
        removal_in(
            &self.runtime(InstalledPacks::default()),
            &self.agent_dirs(),
            kind,
            name,
        )
    }

    fn remove(&self, kind: ItemKind, name: &str) -> Result<Removal, RemoveError> {
        remove_in(
            &self.runtime(InstalledPacks::default()),
            &self.agent_dirs(),
            kind,
            name,
        )
    }
}

fn names(users: &[ItemRef]) -> Vec<String> {
    users
        .iter()
        .map(|user| format!("{} {}", user.kind, user.name))
        .collect()
}

#[test]
fn removing_a_user_tool_deletes_its_file_and_names_every_literal_user() {
    let fixture = Fixture::new();
    let path = fixture.write("project/tools/echo.yaml", ECHO_TOOL);
    fixture.write("project/plans/caller.yaml", CALLER_PLAN);
    fixture.write("project/plans/delegates.yaml", AGENT_STEP_PLAN);
    fixture.write(
        "project/agents/helper.yaml",
        &agent("helper", &["user__echo"], &[]),
    );
    fixture.write(
        "project/agents/everything.yaml",
        &agent("everything", &["\"*\""], &[]),
    );
    let removed = fixture.remove(ItemKind::Tool, "echo").unwrap();
    assert_eq!(removed.name, "user__echo");
    assert_eq!(removed.path, path.display().to_string());
    assert!(!removed.overrides);
    assert_eq!(removed.takes_over, None);
    assert_eq!(
        names(&removed.users),
        ["plan caller", "plan delegates", "agent helper"]
    );
    assert!(!path.exists());
    assert!(removed
        .lines()
        .last()
        .unwrap()
        .ends_with("they fail until updated"));
}

#[test]
fn removing_a_tool_override_brings_the_built_in_back() {
    let fixture = Fixture::new();
    let reshape = dive_core::user_tools::builtin_tool_sources()
        .iter()
        .find(|raw| raw.contains("name: reshape"))
        .unwrap();
    let path = fixture.write("project/tools/builtin/reshape.yaml", reshape);
    let removed = fixture.remove(ItemKind::Tool, "builtin__reshape").unwrap();
    assert_eq!(removed.path, path.display().to_string());
    assert!(removed.overrides);
    assert_eq!(removed.takes_over.as_deref(), Some("built-in"));
    assert!(!path.exists());
}

#[test]
fn a_built_in_without_an_override_and_an_unknown_name_are_refused() {
    let fixture = Fixture::new();
    assert!(matches!(
        fixture.removal(ItemKind::Tool, "builtin__reshape"),
        Err(RemoveError::BuiltIn { .. })
    ));
    assert!(matches!(
        fixture.removal(ItemKind::Plan, "compose_plan"),
        Err(RemoveError::BuiltIn { .. })
    ));
    assert!(matches!(
        fixture.removal(ItemKind::Agent, "chat"),
        Err(RemoveError::BuiltIn { .. })
    ));
    assert!(matches!(
        fixture.removal(ItemKind::Agent, "front_desk"),
        Err(RemoveError::BuiltIn { .. })
    ));
    assert!(matches!(
        fixture.removal(ItemKind::Tool, "nope"),
        Err(RemoveError::NotFound { .. })
    ));
}

#[test]
fn a_pack_tool_is_refused_naming_the_pack() {
    let fixture = Fixture::new();
    let pack_dir = fixture.path("pack");
    std::fs::create_dir_all(pack_dir.join("tools")).unwrap();
    std::fs::write(
        pack_dir.join("pack.toml"),
        "version = 1\nname = \"example\"\ndescription = \"files\"\n",
    )
    .unwrap();
    std::fs::write(pack_dir.join("README.md"), "# example\n").unwrap();
    std::fs::write(pack_dir.join("tools/echo.yaml"), ECHO_TOOL).unwrap();
    let loaded = dive_core::packs::load_pack(
        PackSource {
            name: "example".to_string(),
            tag: None,
            layer: LayerKind::Project,
            dir: pack_dir,
        },
        &[],
        &[],
    )
    .unwrap();
    let runtime = fixture.runtime(InstalledPacks::of(loaded));
    let refused = removal_in(
        &runtime,
        &fixture.agent_dirs(),
        ItemKind::Tool,
        "example__tool__echo",
    )
    .unwrap_err();
    assert!(matches!(&refused, RemoveError::Pack { pack, .. } if pack == "example"));
    assert!(refused.to_string().contains("dive pack remove example"));
}

#[test]
fn removing_a_project_plan_lets_the_shadowed_global_copy_take_over() {
    let fixture = Fixture::new();
    let project = fixture.write("project/plans/caller.yaml", CALLER_PLAN);
    let global = fixture.write("global/plans/caller.yaml", CALLER_PLAN);
    fixture.write("project/plans/delegates.yaml", AGENT_STEP_PLAN);
    let removed = fixture.remove(ItemKind::Plan, "caller").unwrap();
    assert_eq!(removed.path, project.display().to_string());
    assert_eq!(removed.takes_over, Some(global.display().to_string()));
    assert!(!removed.overrides);
    assert_eq!(names(&removed.users), ["plan delegates"]);
    assert!(!project.exists());
    assert!(global.exists());
}

#[test]
fn a_broken_plan_can_still_be_removed() {
    let fixture = Fixture::new();
    let path = fixture.write("project/plans/broken.yaml", "identifier: broken\n");
    let removed = fixture.remove(ItemKind::Plan, "broken").unwrap();
    assert_eq!(removed.path, path.display().to_string());
    assert!(!path.exists());
}

#[test]
fn removing_a_built_in_plan_override_reverts_it() {
    let fixture = Fixture::new();
    let compose = dive_core::pipeline::doc::BUILTIN_PLANS
        .iter()
        .find(|raw| raw.contains("identifier: compose_plan"))
        .unwrap();
    fixture.write("project/plans/compose_plan.yaml", compose);
    let removal = fixture.removal(ItemKind::Plan, "compose_plan").unwrap();
    assert!(removal.overrides);
    assert_eq!(removal.takes_over.as_deref(), Some("built-in"));
}

#[test]
fn removing_a_project_agent_names_callers_and_the_global_copy() {
    let fixture = Fixture::new();
    let project = fixture.write("project/agents/helper.yaml", &agent("helper", &[], &[]));
    let global = fixture.write("global/agents/helper.yaml", &agent("helper", &[], &[]));
    fixture.write("project/agents/boss.yaml", &agent("boss", &[], &["helper"]));
    fixture.write(
        "project/plans/runs.yaml",
        "identifier: runs\nname: Runs\ndescription: runs helper\nsteps:\n  - id: E1\n    tool_name: agent__helper\n    input: {}\n",
    );
    let removed = fixture.remove(ItemKind::Agent, "helper").unwrap();
    assert_eq!(removed.path, project.display().to_string());
    assert_eq!(removed.takes_over, Some(global.display().to_string()));
    assert_eq!(names(&removed.users), ["plan runs", "agent boss"]);
    assert!(!project.exists());
}

#[test]
fn removing_a_built_in_agent_override_reverts_it() {
    let fixture = Fixture::new();
    let chat = dive_core::agent::doc::BUILTINS[0];
    fixture.write("global/agents/chat.yaml", chat);
    let removal = fixture.removal(ItemKind::Agent, "chat").unwrap();
    assert!(removal.overrides);
    assert_eq!(removal.takes_over.as_deref(), Some("built-in"));
    assert_eq!(
        removal.lines(),
        [
            format!(
                "removed {}",
                fixture.path("global/agents/chat.yaml").display()
            ),
            "the built-in agent chat is back".to_string(),
        ]
    );
}

#[tokio::test]
async fn a_tool_drafted_this_session_leaves_the_catalog_when_removed() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime(InstalledPacks::default());
    let pipeline = runtime
        .pipeline(&runtime.store().unwrap(), Arc::new(dive_core::NullSink))
        .await
        .unwrap();
    let saved = pipeline
        .save_tool_file_at(ECHO_TOOL, true, false, false, None)
        .await;
    assert!(saved["path"].is_string(), "{saved}");
    assert!(runtime
        .drafted()
        .tools()
        .iter()
        .any(|doc| doc.name == "echo"));
    remove_in(&runtime, &fixture.agent_dirs(), ItemKind::Tool, "echo").unwrap();
    assert!(runtime.drafted().tools().is_empty());
}
