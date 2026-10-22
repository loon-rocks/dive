use super::*;
use graph_core::secrets::MemorySecrets;

const GLOBAL: &str = r#"version = 5

[providers.anthropic]
type = "anthropic"
api_key = "${GRAPH_T_ANTHROPIC}"

[providers.local]
type = "openai_compat"
base_url = "http://localhost:11434/v1"

[providers.plain]
type = "openai"
api_key = "sk-written-into-the-file"

[models.default]
provider = "anthropic"
model = "claude-sonnet-5-5"

[models.judge]
provider = "anthropic"
model = "claude-haiku-4-5"

[pricing."claude-haiku-4-5"]
input = 1.0
output = 5.0
"#;

const PROJECT: &str = r#"version = 5

# keep this comment
[providers.systemone]
type = "systemone"
api_key = "${GRAPH_T_SYSTEMONE}"

[models.planner]
provider = "anthropic"
model = "claude-sonnet-5-5"
temperature = 0.0
description = "drafts plans"

[[models.planner.fallbacks]]
provider = "local"
model = "llama3"
context_window = 8192

[models.decider]
provider = "systemone"
model = "jev-1"
"#;

fn layers(global: &str, project: Option<&str>) -> (tempfile::TempDir, Layers) {
    let dir = tempfile::tempdir().unwrap();
    let global_path = dir.path().join("global").join("config.toml");
    std::fs::create_dir_all(global_path.parent().unwrap()).unwrap();
    std::fs::write(&global_path, global).unwrap();
    let file = dir.path().join("site").join(".graph").join("config.toml");
    if let Some(project) = project {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, project).unwrap();
    }
    let layers = Layers {
        load_paths: vec![global_path.clone(), file.clone()],
        global: global_path,
        file,
        global_workspace: false,
    };
    (dir, layers)
}

fn resolver() -> MemorySecrets {
    MemorySecrets::store().with("GRAPH_T_ANTHROPIC", "sk-ant-value")
}

fn text(layers: &Layers) -> String {
    std::fs::read_to_string(&layers.file).unwrap()
}

#[test]
fn the_view_merges_layers_and_marks_where_each_entry_comes_from() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    let view = view_layers(&layers, &resolver());
    assert!(view.problems.is_empty(), "{:?}", view.problems);
    let providers: Vec<(&str, Origin, KeyState)> = view
        .providers
        .iter()
        .map(|p| (p.name.as_str(), p.origin, p.key))
        .collect();
    assert_eq!(
        providers,
        [
            ("anthropic", Origin::Global, KeyState::Set),
            ("local", Origin::Global, KeyState::None),
            ("plain", Origin::Global, KeyState::InFile),
            ("systemone", Origin::Workspace, KeyState::Missing),
        ]
    );
    let roles: Vec<(&str, Origin, Option<ModelKind>)> = view
        .roles
        .iter()
        .map(|r| (r.role.as_str(), r.origin, r.kind))
        .collect();
    assert_eq!(
        roles,
        [
            ("default", Origin::Global, Some(ModelKind::Chat)),
            ("planner", Origin::Workspace, Some(ModelKind::Chat)),
            ("judge", Origin::Global, Some(ModelKind::Chat)),
            ("decider", Origin::Workspace, Some(ModelKind::Decision)),
        ]
    );
    let planner = &view.roles[1];
    assert_eq!(planner.fallbacks[0].model, "llama3");
    assert_eq!(view.providers[0].used_by, ["default", "planner", "judge"]);
    assert_eq!(view.providers[1].used_by, ["planner fallback"]);
    assert!(view.providers[2].key_names.is_empty());
    assert_eq!(view.pricing[0].used_by, ["judge"]);
    assert_eq!(view.unpriced, ["claude-sonnet-5-5", "jev-1", "llama3"]);
    assert_eq!(view.blocked, None);
}

#[test]
fn saving_a_provider_writes_the_workspace_file_and_keeps_its_comments() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    save_provider_in(
        &layers,
        None,
        &ProviderEdit {
            name: "openai".to_string(),
            kind: ProviderKind::Openai,
            api_key: Some("${OPENAI_API_KEY}".to_string()),
            base_url: None,
            region: None,
            profile: Some(" ".to_string()),
        },
    )
    .unwrap();
    let written = text(&layers);
    assert!(written.contains("# keep this comment"), "{written}");
    assert!(
        written
            .contains("[providers.openai]\ntype = \"openai\"\napi_key = \"${OPENAI_API_KEY}\"\n"),
        "{written}"
    );
    assert!(!written.contains("profile"), "{written}");
    assert!(!std::fs::read_to_string(&layers.global)
        .unwrap()
        .contains("[providers.openai]"));
}

#[test]
fn renaming_a_provider_moves_the_roles_in_the_same_file_with_it() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    save_provider_in(
        &layers,
        Some("systemone"),
        &ProviderEdit {
            name: "jev".to_string(),
            kind: ProviderKind::Systemone,
            api_key: Some("${GRAPH_T_SYSTEMONE}".to_string()),
            base_url: None,
            region: None,
            profile: None,
        },
    )
    .unwrap();
    let written = text(&layers);
    assert!(!written.contains("[providers.systemone]"), "{written}");
    assert!(
        written.contains("[models.decider]\nprovider = \"jev\""),
        "{written}"
    );
}

#[test]
fn a_provider_in_use_cannot_be_removed() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    let error = remove_provider_in(&layers, "systemone")
        .unwrap_err()
        .to_string();
    assert!(error.contains("used by decider"), "{error}");
    remove_role_in(&layers, "decider").unwrap();
    remove_provider_in(&layers, "systemone").unwrap();
    assert!(!text(&layers).contains("systemone"));
}

#[test]
fn saving_a_role_rewrites_its_fallbacks_and_keeps_fields_the_form_does_not_show() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    save_role_in(
        &layers,
        Some("planner"),
        &RoleEdit {
            role: "planner".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-opus-5-5".to_string(),
            temperature: None,
            fallbacks: vec![
                Fallback {
                    provider: "local".to_string(),
                    model: "llama3".to_string(),
                    temperature: None,
                },
                Fallback {
                    provider: "anthropic".to_string(),
                    model: "claude-haiku-4-5".to_string(),
                    temperature: Some(0.5),
                },
            ],
        },
    )
    .unwrap();
    let written = text(&layers);
    assert!(written.contains("model = \"claude-opus-5-5\""), "{written}");
    assert!(
        written.contains("description = \"drafts plans\""),
        "{written}"
    );
    assert!(!written.contains("temperature = 0.0"), "{written}");
    assert!(written.contains("context_window = 8192"), "{written}");
    assert_eq!(
        written.matches("[[models.planner.fallbacks]]").count(),
        2,
        "{written}"
    );
    let view = view_layers(&layers, &resolver());
    let planner = view.roles.iter().find(|r| r.role == "planner").unwrap();
    assert_eq!(planner.fallbacks.len(), 2);
    assert_eq!(planner.fallbacks[1].temperature, Some(0.5));
}

#[test]
fn a_change_that_would_not_load_is_refused_and_nothing_is_written() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    let before = text(&layers);
    let unknown = save_role_in(
        &layers,
        None,
        &RoleEdit {
            role: "solver".to_string(),
            provider: "nowhere".to_string(),
            model: "m".to_string(),
            temperature: None,
            fallbacks: Vec::new(),
        },
    )
    .unwrap_err()
    .to_string();
    assert!(unknown.contains("nowhere"), "{unknown}");
    let chat_for_decider = save_role_in(
        &layers,
        Some("decider"),
        &RoleEdit {
            role: "decider".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5".to_string(),
            temperature: None,
            fallbacks: Vec::new(),
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        chat_for_decider.contains("decision-model provider"),
        "{chat_for_decider}"
    );
    assert_eq!(text(&layers), before);
    let leftovers: Vec<_> = std::fs::read_dir(layers.file.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(leftovers, ["config.toml"]);
}

#[test]
fn prices_are_written_under_quoted_model_ids() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    save_price_in(
        &layers,
        None,
        &PriceEdit {
            model: "claude-sonnet-4.5".to_string(),
            input: 3.0,
            output: 15.0,
            cache_write: None,
            cache_read: Some(0.3),
            tiers: None,
        },
    )
    .unwrap();
    let written = text(&layers);
    assert!(
        written.contains(
            "[pricing.\"claude-sonnet-4.5\"]\ninput = 3.0\noutput = 15.0\ncache_read = 0.3\n"
        ),
        "{written}"
    );
    assert!(save_price_in(
        &layers,
        None,
        &PriceEdit {
            model: "m".to_string(),
            input: -1.0,
            output: 1.0,
            cache_write: None,
            cache_read: None,
            tiers: None,
        },
    )
    .is_err());
    remove_price_in(&layers, "claude-sonnet-4.5").unwrap();
    assert!(!text(&layers).contains("pricing"));
}

#[test]
fn the_first_save_creates_the_workspace_file_at_the_current_version() {
    let (_dir, layers) = layers("", None);
    let view = view_layers(&layers, &resolver());
    assert!(view.providers.is_empty() && view.roles.is_empty());
    save_provider_in(
        &layers,
        None,
        &ProviderEdit {
            name: "anthropic".to_string(),
            kind: ProviderKind::Anthropic,
            api_key: Some("${ANTHROPIC_API_KEY}".to_string()),
            base_url: None,
            region: None,
            profile: None,
        },
    )
    .unwrap();
    assert_eq!(
        text(&layers),
        format!("version = {CONFIG_FORMAT}\n\n[providers.anthropic]\ntype = \"anthropic\"\napi_key = \"${{ANTHROPIC_API_KEY}}\"\n")
    );
}

#[test]
fn saving_into_an_older_file_keeps_its_version() {
    let old = "version = 4\n\n# my settings\n[settings]\nhistory_limit = 3\n";
    let (_dir, layers) = layers(GLOBAL, Some(old));
    assert_eq!(view_layers(&layers, &resolver()).blocked, None);
    save_price_in(
        &layers,
        None,
        &PriceEdit {
            model: "m".to_string(),
            input: 1.0,
            output: 2.0,
            cache_write: None,
            cache_read: None,
            tiers: None,
        },
    )
    .unwrap();
    let written = text(&layers);
    assert!(written.starts_with("version = 4\n"), "{written}");
    assert!(written.contains("# my settings"), "{written}");
    assert!(written.contains("[pricing.m]"), "{written}");
}

#[test]
fn a_newer_file_is_shown_but_never_edited() {
    let newer = format!("version = {}\n", CONFIG_FORMAT + 1);
    let (_dir, layers) = layers(GLOBAL, Some(&newer));
    let view = view_layers(&layers, &resolver());
    assert!(view
        .blocked
        .as_deref()
        .unwrap()
        .contains("newer than this app reads"));
    assert!(remove_role_in(&layers, "judge").is_err());
}

#[test]
fn the_global_workspace_edits_the_global_file() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("config.toml");
    std::fs::write(&global, GLOBAL).unwrap();
    let layers = Layers {
        global: global.clone(),
        file: global.clone(),
        global_workspace: true,
        load_paths: vec![global.clone()],
    };
    let view = view_layers(&layers, &resolver());
    assert!(view
        .roles
        .iter()
        .all(|role| role.origin == Origin::Workspace));
    remove_role_in(&layers, "judge").unwrap();
    assert!(!std::fs::read_to_string(&global)
        .unwrap()
        .contains("models.judge"));
}

#[tokio::test]
async fn a_probe_without_its_key_says_which_name_is_missing() {
    let probe = probe_with(
        &ProviderEdit {
            name: "anthropic".to_string(),
            kind: ProviderKind::Anthropic,
            api_key: Some("${GRAPH_T_UNSET}".to_string()),
            base_url: None,
            region: None,
            profile: None,
        },
        &resolver(),
    )
    .await;
    assert!(!probe.ok);
    assert_eq!(probe.error.as_deref(), Some("GRAPH_T_UNSET isn't set"));
    let bedrock = probe_with(
        &ProviderEdit {
            name: "b".to_string(),
            kind: ProviderKind::Bedrock,
            api_key: None,
            base_url: None,
            region: None,
            profile: None,
        },
        &resolver(),
    )
    .await;
    assert!(!bedrock.listed);
}

#[test]
fn keys_expand_from_references_and_defaults() {
    let resolver = resolver();
    assert_eq!(
        expand_key(Some("${GRAPH_T_ANTHROPIC}"), &resolver)
            .unwrap()
            .as_deref(),
        Some("sk-ant-value")
    );
    assert_eq!(
        expand_key(Some("${GRAPH_T_NONE:-fallback}"), &resolver)
            .unwrap()
            .as_deref(),
        Some("fallback")
    );
    assert_eq!(expand_key(None, &resolver).unwrap(), None);
}

#[test]
fn price_tiers_are_written_kept_on_edit_and_removed_on_request() {
    let (_dir, layers) = layers(GLOBAL, Some(PROJECT));
    let tier = |above: u64, input: f64| graph_config::PriceTier {
        above,
        input: Some(input),
        output: Some(2.5),
        cache_write: None,
        cache_read: None,
    };
    let edit = |tiers: Option<Vec<graph_config::PriceTier>>| PriceEdit {
        model: "claude-haiku-5-5".to_string(),
        input: 0.1,
        output: 0.5,
        cache_write: None,
        cache_read: Some(0.01),
        tiers,
    };
    save_price_in(&layers, None, &edit(Some(vec![tier(100_000, 0.5)]))).unwrap();
    let written = text(&layers);
    assert!(
        written.contains(
            "[[pricing.claude-haiku-5-5.tiers]]\nabove = 100000\ninput = 0.5\noutput = 2.5\n"
        ),
        "{written}"
    );
    let view = view_layers(&layers, &resolver());
    let price = view
        .pricing
        .iter()
        .find(|price| price.model == "claude-haiku-5-5")
        .unwrap();
    assert_eq!(price.tiers, vec![tier(100_000, 0.5)]);

    save_price_in(&layers, Some("claude-haiku-5-5"), &edit(None)).unwrap();
    assert!(text(&layers).contains(".tiers]]"), "{}", text(&layers));

    assert!(save_price_in(
        &layers,
        Some("claude-haiku-5-5"),
        &edit(Some(vec![tier(100_000, 0.5), tier(100_000, 0.6)])),
    )
    .is_err());

    save_price_in(&layers, Some("claude-haiku-5-5"), &edit(Some(Vec::new()))).unwrap();
    assert!(!text(&layers).contains("tiers"), "{}", text(&layers));
}

const CATALOG: &str = r#"{
  "anthropic": {
    "id": "anthropic",
    "models": {
      "claude-sonnet-5-5": {"cost": {"input": 2, "output": 10, "cache_read": 0.1, "cache_write": 2.5}},
      "claude-haiku-4-5": {"cost": {"input": 1, "output": 5, "cache_read": 0.1, "cache_write": 1.25}},
      "claude-haiku-5-5": {"cost": {"input": 0.1, "output": 0.5, "cache_read": 0.01, "cache_write": 0.125,
        "tiers": [{"input": 0.5, "output": 2.5, "cache_read": 0.05, "cache_write": 0.625, "tier": {"type": "context", "size": 100000}}]}}
    }
  },
  "ollama-cloud": {"api": "https://ollama.example.com/v1", "models": {"llama3": {"cost": {"input": 0, "output": 0}}}}
}"#;

const WITH_HAIKU: &str = r#"version = 6

[models.fast]
provider = "anthropic"
model = "claude-haiku-5-5"
"#;

fn price_of(layers: &Layers, model: &str) -> PriceEntry {
    view_layers(layers, &resolver())
        .pricing
        .into_iter()
        .find(|price| price.model == model)
        .unwrap()
}

#[test]
fn filling_prices_adds_catalogue_prices_for_unpriced_models_only() {
    let (_dir, layers) = layers(GLOBAL, Some(WITH_HAIKU));
    let catalog = Catalog::parse(CATALOG).unwrap();
    let current = view_layers(&layers, &resolver());
    let fill = fill_prices_in(&layers, &current, &catalog, false).unwrap();
    assert_eq!(fill.added, ["claude-haiku-5-5", "claude-sonnet-5-5"]);
    assert_eq!(fill.unchanged, ["claude-haiku-4-5"]);
    assert!(fill.not_found.is_empty(), "{fill:?}");

    let haiku = price_of(&layers, "claude-haiku-5-5");
    assert_eq!(haiku.source.as_deref(), Some(CATALOG_SOURCE));
    assert_eq!(haiku.tiers.len(), 1);
    assert_eq!(haiku.tiers[0].above, 100_000);
    assert_eq!(haiku.tiers[0].cache_read, Some(0.05));
    assert_eq!(price_of(&layers, "claude-haiku-4-5").source, None);

    let again =
        fill_prices_in(&layers, &view_layers(&layers, &resolver()), &catalog, false).unwrap();
    assert!(again.added.is_empty(), "{again:?}");
}

#[test]
fn refreshing_updates_catalogue_prices_and_keeps_hand_edited_ones() {
    let (_dir, layers) = layers(GLOBAL, Some(WITH_HAIKU));
    let catalog = Catalog::parse(CATALOG).unwrap();
    fill_prices_in(&layers, &view_layers(&layers, &resolver()), &catalog, false).unwrap();
    save_price_in(
        &layers,
        Some("claude-haiku-5-5"),
        &PriceEdit {
            model: "claude-haiku-5-5".to_string(),
            input: 0.2,
            output: 0.5,
            cache_write: None,
            cache_read: None,
            tiers: None,
        },
    )
    .unwrap();
    assert_eq!(price_of(&layers, "claude-haiku-5-5").source, None);

    let newer = Catalog::parse(&CATALOG.replace(
        r#""claude-sonnet-5-5": {"cost": {"input": 2,"#,
        r#""claude-sonnet-5-5": {"cost": {"input": 1.5,"#,
    ))
    .unwrap();
    let fill = fill_prices_in(&layers, &view_layers(&layers, &resolver()), &newer, true).unwrap();
    assert_eq!(fill.updated, ["claude-sonnet-5-5"]);
    assert_eq!(fill.kept, ["claude-haiku-4-5", "claude-haiku-5-5"]);
    assert_eq!(price_of(&layers, "claude-sonnet-5-5").input, 1.5);
    assert_eq!(price_of(&layers, "claude-haiku-5-5").input, 0.2);
}

#[test]
fn compatible_providers_match_the_catalogue_by_host_and_others_stay_unpriced() {
    let catalog = Catalog::parse(CATALOG).unwrap();
    assert!(catalog
        .price(
            ProviderKind::OpenaiCompat,
            Some("https://ollama.example.com/v1"),
            "llama3"
        )
        .is_some());
    assert!(catalog
        .price(
            ProviderKind::OpenaiCompat,
            Some("http://localhost:11434/v1"),
            "llama3"
        )
        .is_none());
    assert!(catalog
        .price(ProviderKind::Systemone, None, "jev-1")
        .is_none());
}
