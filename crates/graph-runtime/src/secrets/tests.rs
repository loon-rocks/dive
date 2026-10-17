use super::*;
use graph_core::secrets::references::ReferenceKind;
use graph_core::secrets::MemorySecrets;

fn write(dir: &Path, path: &str, content: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn secrets(env: MemorySecrets, store: MemorySecrets) -> Secrets {
    Secrets::new(Arc::new(env), Arc::new(store))
}

fn layers(root: &Path) -> Vec<PathBuf> {
    let global = root.join("global");
    let project = root.join("project");
    write(
        &global,
        "config.toml",
        "version = 4\n[providers.anthropic]\ntype = \"anthropic\"\napi_key = \"${GRAPH_T_ANTHROPIC}\"\n",
    );
    write(
        &project,
        "config.toml",
        &format!(
            "version = 4\n[tools]\npaths = [\"{}\"]\n[mcp.linear]\nurl = \"https://mcp.linear.app/mcp\"\nheaders = {{ Authorization = \"Bearer ${{GRAPH_T_LINEAR}}\" }}\n[packs.web]\ngit = \"github.com/acme/graph-packs\"\npath = \"web\"\nref = \"v1\"\n",
            project.join("tools").display()
        ),
    );
    write(
        &project,
        "tools/search.yaml",
        "version: 2\nname: search\ndescription: Searches\nkind: exec\ncommand: echo\nenv:\n  URL: \"${GRAPH_T_URL:-https://x}\"\n  KEY: \"${GRAPH_T_ANTHROPIC}\"\n",
    );
    let copy = project.join("packs/web");
    write(
        &copy,
        "pack.toml",
        "version = 1\nname = \"web\"\ndescription = \"d\"\n[requires]\nenv = [\"GRAPH_T_WEB_TOKEN\"]\n",
    );
    write(
        &copy,
        "tools/fetch.yaml",
        "version: 2\nname: fetch\ndescription: Fetches\nkind: exec\ncommand: echo\nenv:\n  TOKEN: \"${GRAPH_T_WEB_TOKEN}\"\n",
    );
    let hash = crate::packs::content_hash(&copy).unwrap();
    write(
        &project,
        "packs.lock",
        &format!("version = 1\n\n[packs.web]\ngit = \"github.com/acme/graph-packs\"\npath = \"web\"\ntag = \"web/v1\"\nsha = \"a8303d2a8303d2a8303d2a8303d2a8303d2a8303\"\nhash = \"{hash}\"\n"),
    );
    vec![global.join("config.toml"), project.join("config.toml")]
}

#[test]
fn the_listing_covers_config_layers_tools_and_packs_with_sources_and_shadowing() {
    let dir = tempfile::tempdir().unwrap();
    let paths = layers(dir.path());
    let secrets = secrets(
        MemorySecrets::env()
            .with("GRAPH_T_LINEAR", "lin-env-value")
            .with("GRAPH_T_WEB_TOKEN", "web-env-value"),
        MemorySecrets::store()
            .with("GRAPH_T_ANTHROPIC", "sk-stored-value")
            .with("GRAPH_T_WEB_TOKEN", "web-stored-value"),
    );
    let list = listed(collect_layers(&paths, Path::new("."), &secrets), &secrets);
    assert!(list.problems.is_empty(), "{:?}", list.problems);
    let statuses: Vec<(&str, SecretStatus)> = list
        .secrets
        .iter()
        .map(|entry| (entry.name.as_str(), entry.status))
        .collect();
    assert_eq!(
        statuses,
        [
            ("GRAPH_T_ANTHROPIC", SecretStatus::Store),
            ("GRAPH_T_LINEAR", SecretStatus::Env),
            ("GRAPH_T_URL", SecretStatus::Missing),
            ("GRAPH_T_WEB_TOKEN", SecretStatus::EnvShadowsStore),
        ]
    );
    let anthropic = &list.secrets[0].references;
    assert_eq!(
        anthropic
            .iter()
            .map(|reference| (reference.kind, reference.field.as_str()))
            .collect::<Vec<_>>(),
        [
            (ReferenceKind::Config, "providers.anthropic.api_key"),
            (ReferenceKind::Tool, "env.KEY"),
        ]
    );
    let web = &list.secrets[3].references;
    assert!(web
        .iter()
        .any(|r| r.kind == ReferenceKind::PackRequires && r.pack.as_deref() == Some("web")));
    assert!(web
        .iter()
        .any(|r| r.kind == ReferenceKind::PackTool && r.field == "env.TOKEN"));
    assert!(list.secrets[2].references[0].has_default);
    let json = serde_json::to_string(&list).unwrap();
    for value in [
        "lin-env-value",
        "web-env-value",
        "sk-stored-value",
        "web-stored-value",
    ] {
        assert!(!json.contains(value), "{value} leaked into {json}");
    }
    assert!(json.contains("\"env_shadows_store\""));
}

#[test]
fn a_config_that_fails_to_load_still_lists_its_own_references() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "config.toml",
        "version = 4\n[settings]\ndata_dir = \"${GRAPH_T_DATA_DIR}\"\n",
    );
    let secrets = secrets(MemorySecrets::env(), MemorySecrets::store());
    let list = listed(
        collect_layers(&[dir.path().join("config.toml")], Path::new("."), &secrets),
        &secrets,
    );
    assert_eq!(list.secrets[0].name, "GRAPH_T_DATA_DIR");
    assert_eq!(list.secrets[0].status, SecretStatus::Missing);
    assert!(list.problems[0].contains("GRAPH_T_DATA_DIR"));
}

#[test]
fn show_masks_values_to_their_last_four_characters() {
    let dir = tempfile::tempdir().unwrap();
    let paths = layers(dir.path());
    let secrets = secrets(
        MemorySecrets::env().with("GRAPH_T_WEB_TOKEN", "short"),
        MemorySecrets::store().with("GRAPH_T_WEB_TOKEN", "web-stored-value-wxyz"),
    );
    let shown = detail(
        collect_layers(&paths, Path::new("."), &secrets),
        &secrets,
        "GRAPH_T_WEB_TOKEN",
    );
    assert_eq!(shown.status, SecretStatus::EnvShadowsStore);
    assert_eq!(shown.in_use, Some(Layer::Env));
    assert_eq!(shown.env_value.as_deref(), Some("****"));
    assert_eq!(shown.shared_value.as_deref(), Some("****wxyz"));
    assert_eq!(shown.project_value, None);
    assert_eq!(shown.references.len(), 2);
    assert_eq!(mask("abcdefgh"), "****efgh");
    assert_eq!(mask("abcdefg"), "****");
}

fn project_workspace(id: Option<&str>) -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    if let Some(id) = id {
        write(
            dir.path(),
            ".graph/config.toml",
            &format!("version = 5\n[project]\nid = \"{id}\"\n"),
        );
    }
    let workspace = Workspace::new(dir.path());
    (dir, workspace)
}

fn in_use(secrets: &Secrets, workspace: &Workspace, name: &str) -> Option<Layer> {
    secrets
        .values(name, project_id(workspace).as_deref())
        .in_use()
}

#[test]
fn set_and_remove_go_through_the_store() {
    let (_dir, workspace) = project_workspace(None);
    let secrets = secrets(
        MemorySecrets::env().with("SHADOWED", "env"),
        MemorySecrets::store(),
    );
    let shared = SecretScope::Shared;
    let saved = set(&secrets, &workspace, "NEW_KEY", "value-1234", shared).unwrap();
    assert_eq!(saved.shadowed_by, None);
    assert_eq!(in_use(&secrets, &workspace, "NEW_KEY"), Some(Layer::Shared));
    assert_eq!(
        set(&secrets, &workspace, "SHADOWED", "stored", shared)
            .unwrap()
            .shadowed_by,
        Some(Layer::Env)
    );
    assert!(set(&secrets, &workspace, "EMPTY", "", shared).is_err());
    assert!(set(&secrets, &workspace, "not-a-name", "x", shared).is_err());
    assert!(
        remove(&secrets, &workspace, "NEW_KEY", shared)
            .unwrap()
            .removed
    );
    assert!(
        !remove(&secrets, &workspace, "NEW_KEY", shared)
            .unwrap()
            .removed
    );
    assert_eq!(in_use(&secrets, &workspace, "NEW_KEY"), None);
    assert!(!workspace.root().join(".graph/config.toml").exists());
}

#[test]
fn a_project_value_wins_over_the_shared_one_and_the_environment_wins_over_both() {
    let (_dir, workspace) = project_workspace(Some("p1"));
    let secrets = secrets(
        MemorySecrets::env(),
        MemorySecrets::store()
            .with("SLACK_BOT_TOKEN", "xoxb-shared-value")
            .with_project("SLACK_BOT_TOKEN", "p1", "xoxb-project-value")
            .with_project("SLACK_BOT_TOKEN", "p2", "xoxb-other-project"),
    );
    assert_eq!(
        reveal(&secrets, &workspace, "SLACK_BOT_TOKEN").as_deref(),
        Some("xoxb-project-value")
    );
    let shown = show(&workspace, &secrets, "SLACK_BOT_TOKEN").unwrap();
    assert_eq!(shown.in_use, Some(Layer::Project));
    assert_eq!(shown.status, SecretStatus::Store);
    assert_eq!(shown.project.as_deref(), Some("p1"));
    assert_eq!(shown.project_value.as_deref(), Some("****alue"));
    assert_eq!(shown.shared_value.as_deref(), Some("****alue"));
    let resolver =
        graph_config::secrets::InProject::new(secrets.resolver(), project_id(&workspace));
    assert_eq!(
        resolver.resolve("SLACK_BOT_TOKEN").unwrap().value,
        "xoxb-project-value"
    );

    let env = secrets_with_env(&secrets, "SLACK_BOT_TOKEN", "xoxb-env-value");
    assert_eq!(
        in_use(&env, &workspace, "SLACK_BOT_TOKEN"),
        Some(Layer::Env)
    );
    assert_eq!(
        reveal(&env, &workspace, "SLACK_BOT_TOKEN").as_deref(),
        Some("xoxb-env-value")
    );

    let (_other, elsewhere) = project_workspace(Some("p3"));
    assert_eq!(
        reveal(&secrets, &elsewhere, "SLACK_BOT_TOKEN").as_deref(),
        Some("xoxb-shared-value")
    );
}

fn secrets_with_env(base: &Secrets, name: &str, value: &str) -> Secrets {
    Secrets::new(
        Arc::new(MemorySecrets::env().with(name, value)),
        base.store.clone(),
    )
}

#[test]
fn the_first_project_set_writes_a_project_id_and_later_sets_reuse_it() {
    let (dir, workspace) = project_workspace(None);
    write(
        dir.path(),
        ".graph/config.toml",
        "version = 5\n\n# my models\n[models.default]\nprovider = \"anthropic\"\nmodel = \"claude\"\n",
    );
    let secrets = secrets(MemorySecrets::env(), MemorySecrets::store());
    let project = SecretScope::Project;
    let saved = set(&secrets, &workspace, "TOKEN", "project-value", project).unwrap();
    let id = saved.project.clone().unwrap();
    assert!(graph_core::secrets::is_project_id(&id), "{id}");
    let text = std::fs::read_to_string(dir.path().join(".graph/config.toml")).unwrap();
    assert!(text.contains("# my models"), "{text}");
    assert!(
        text.contains(&format!("[project]\nid = \"{id}\"")),
        "{text}"
    );
    assert!(!text.contains("project-value"), "{text}");
    assert_eq!(project_id(&workspace).as_deref(), Some(id.as_str()));

    let again = set(&secrets, &workspace, "OTHER", "value-2", project).unwrap();
    assert_eq!(again.project.as_deref(), Some(id.as_str()));
    assert_eq!(
        secrets.store.get_in("TOKEN", Some(&id)).unwrap().as_deref(),
        Some("project-value")
    );
    assert_eq!(secrets.store.get("TOKEN").unwrap(), None);

    let shared = set(
        &secrets,
        &workspace,
        "TOKEN",
        "shared-value",
        SecretScope::Shared,
    )
    .unwrap();
    assert_eq!(shared.shadowed_by, Some(Layer::Project));
    assert!(
        remove(&secrets, &workspace, "TOKEN", project)
            .unwrap()
            .removed
    );
    assert_eq!(
        reveal(&secrets, &workspace, "TOKEN").as_deref(),
        Some("shared-value")
    );
}

#[test]
fn a_new_project_config_is_created_holding_only_the_id() {
    let (dir, workspace) = project_workspace(None);
    let secrets = secrets(MemorySecrets::env(), MemorySecrets::store());
    let saved = set(&secrets, &workspace, "TOKEN", "v", SecretScope::Project).unwrap();
    let text = std::fs::read_to_string(dir.path().join(".graph/config.toml")).unwrap();
    assert_eq!(
        text,
        format!(
            "version = {}\n\n[project]\nid = \"{}\"\n",
            graph_config::CONFIG_FORMAT,
            saved.project.unwrap()
        )
    );
    graph_config::load_from(
        &workspace.config_paths()[1..],
        &graph_core::secrets::EnvSource,
    )
    .unwrap();
}

#[test]
fn removing_a_project_value_without_a_project_id_writes_nothing() {
    let (dir, workspace) = project_workspace(None);
    let secrets = secrets(
        MemorySecrets::env(),
        MemorySecrets::store().with("TOKEN", "x"),
    );
    assert!(
        !remove(&secrets, &workspace, "TOKEN", SecretScope::Project)
            .unwrap()
            .removed
    );
    assert!(!dir.path().join(".graph").exists());
    assert_eq!(secrets.store.get("TOKEN").unwrap().as_deref(), Some("x"));
}

#[test]
fn config_references_resolve_from_the_project_entry_first() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "config.toml",
        "version = 5\n[project]\nid = \"p1\"\n[settings]\ndata_dir = \"${GRAPH_T_DIR}\"\n",
    );
    let secrets = secrets(
        MemorySecrets::env(),
        MemorySecrets::store()
            .with("GRAPH_T_DIR", "/shared")
            .with_project("GRAPH_T_DIR", "p1", "/project"),
    );
    let loaded =
        graph_config::load_from(&[dir.path().join("config.toml")], &secrets.resolver()).unwrap();
    assert_eq!(loaded.config.settings.data_dir, PathBuf::from("/project"));
    assert_eq!(loaded.config.project.id.as_deref(), Some("p1"));
}

#[test]
fn an_unavailable_store_is_reported_and_every_name_falls_back_to_the_environment() {
    let (_dir, workspace) = project_workspace(None);
    let secrets = secrets(
        MemorySecrets::env().with("SET", "value"),
        MemorySecrets::unavailable(),
    );
    assert_eq!(in_use(&secrets, &workspace, "SET"), Some(Layer::Env));
    assert_eq!(in_use(&secrets, &workspace, "UNSET"), None);
    assert!(!secrets.store_state().available);
    assert!(set(&secrets, &workspace, "SET", "x", SecretScope::Shared).is_err());
    assert!(set(&secrets, &workspace, "SET", "x", SecretScope::Project).is_err());
    assert!(!workspace.root().join(".graph").exists());
}

#[test]
fn stored_names_are_listed_even_when_nothing_references_them() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "config.toml",
        "version = 5\n[project]\nid = \"p1\"\n",
    );
    let secrets = secrets(
        MemorySecrets::env(),
        MemorySecrets::store()
            .with("SHARED_ONLY", "shared-value")
            .with_project("MINE", "p1", "project-value")
            .with_project("THEIRS", "p2", "other-value"),
    );
    let list = listed(
        collect_layers(&[dir.path().join("config.toml")], Path::new("."), &secrets),
        &secrets,
    );
    let names: Vec<(&str, Option<Layer>, usize)> = list
        .secrets
        .iter()
        .map(|entry| (entry.name.as_str(), entry.in_use, entry.references.len()))
        .collect();
    assert_eq!(
        names,
        [
            ("MINE", Some(Layer::Project), 0),
            ("SHARED_ONLY", Some(Layer::Shared), 0),
        ]
    );
}

#[derive(Debug)]
struct Locked;

impl graph_core::secrets::SecretSource for Locked {
    fn resolve(&self, _name: &str) -> Option<graph_core::secrets::Resolved> {
        None
    }
}

impl SecretStore for Locked {
    fn status(&self) -> Result<(), graph_core::secrets::StoreError> {
        Ok(())
    }

    fn get_in(
        &self,
        _name: &str,
        _project: Option<&str>,
    ) -> Result<Option<String>, graph_core::secrets::StoreError> {
        Err(graph_core::secrets::StoreError::Failed(
            "User interaction is not allowed.".to_string(),
        ))
    }

    fn set_in(
        &self,
        _name: &str,
        _project: Option<&str>,
        _value: &str,
    ) -> Result<(), graph_core::secrets::StoreError> {
        Ok(())
    }

    fn remove_in(
        &self,
        _name: &str,
        _project: Option<&str>,
    ) -> Result<bool, graph_core::secrets::StoreError> {
        Ok(false)
    }
}

#[test]
fn an_entry_the_store_refuses_to_read_says_so_rather_than_missing() {
    let (_dir, workspace) = project_workspace(None);
    let secrets = Secrets::new(Arc::new(MemorySecrets::env()), Arc::new(Locked));
    let shown = show(&workspace, &secrets, "SYSTEMONE_API_KEY").unwrap();
    assert_eq!(shown.status, SecretStatus::Missing);
    assert!(shown
        .read_error
        .unwrap()
        .contains("User interaction is not allowed"));
}

#[test]
fn a_tool_env_description_reaches_the_listing_and_the_detail() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    write(
        &project,
        "config.toml",
        &format!(
            "version = 4\n[tools]\npaths = [\"{}\"]\n",
            project.join("tools").display()
        ),
    );
    write(
        &project,
        "tools/crm.yaml",
        "version: 3\nname: crm\ndescription: Contacts\nkind: exec\ncommand: echo\nenv:\n  TOKEN:\n    value: \"${GRAPH_T_CRM_TOKEN}\"\n    description: \"A CRM token. Create one under [Settings](https://crm.example/settings).\"\n",
    );
    let paths = vec![project.join("config.toml")];
    let secrets = secrets(MemorySecrets::env(), MemorySecrets::store());
    let list = listed(collect_layers(&paths, Path::new("."), &secrets), &secrets);
    assert!(list.problems.is_empty(), "{:?}", list.problems);
    let entry = list
        .secrets
        .iter()
        .find(|entry| entry.name == "GRAPH_T_CRM_TOKEN")
        .unwrap();
    assert_eq!(
        entry.description.as_deref(),
        Some("A CRM token. Create one under [Settings](https://crm.example/settings).")
    );
    assert_eq!(
        entry.references[0].description, entry.description,
        "the reference carries it too"
    );
    let detail = detail(
        collect_layers(&paths, Path::new("."), &secrets),
        &secrets,
        "GRAPH_T_CRM_TOKEN",
    );
    assert_eq!(detail.description, entry.description);
    let body = serde_json::to_value(&list).unwrap();
    assert!(body["secrets"][0]["description"]
        .as_str()
        .unwrap()
        .contains("crm.example"));
}
