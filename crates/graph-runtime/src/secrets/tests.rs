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
    assert_eq!(shown.env_value.as_deref(), Some("****"));
    assert_eq!(shown.store_value.as_deref(), Some("****wxyz"));
    assert_eq!(shown.references.len(), 2);
    assert_eq!(mask("abcdefgh"), "****efgh");
    assert_eq!(mask("abcdefg"), "****");
}

#[test]
fn set_and_remove_go_through_the_store() {
    let secrets = secrets(
        MemorySecrets::env().with("SHADOWED", "env"),
        MemorySecrets::store(),
    );
    let saved = set(&secrets, "NEW_KEY", "value-1234").unwrap();
    assert!(!saved.shadowed_by_env);
    assert_eq!(secrets.status("NEW_KEY"), SecretStatus::Store);
    assert!(set(&secrets, "SHADOWED", "stored").unwrap().shadowed_by_env);
    assert!(set(&secrets, "EMPTY", "").is_err());
    assert!(set(&secrets, "not-a-name", "x").is_err());
    assert!(remove(&secrets, "NEW_KEY").unwrap().removed);
    assert!(!remove(&secrets, "NEW_KEY").unwrap().removed);
    assert_eq!(secrets.status("NEW_KEY"), SecretStatus::Missing);
}

#[test]
fn an_unavailable_store_is_reported_and_every_name_falls_back_to_the_environment() {
    let secrets = secrets(
        MemorySecrets::env().with("SET", "value"),
        MemorySecrets::unavailable(),
    );
    assert_eq!(secrets.status("SET"), SecretStatus::Env);
    assert_eq!(secrets.status("UNSET"), SecretStatus::Missing);
    assert!(!secrets.store_state().available);
    assert!(set(&secrets, "SET", "x").is_err());
}
