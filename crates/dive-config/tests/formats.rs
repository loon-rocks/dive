use dive_config::secrets::EnvSource;
use dive_config::{LoadedConfig, CONFIG_FORMAT, CONFIG_FORMAT_OLDEST};
use std::path::{Path, PathBuf};

fn load_from(paths: &[PathBuf]) -> anyhow::Result<LoadedConfig> {
    dive_config::load_from(paths, &EnvSource)
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture_dir(version: u32) -> PathBuf {
    fixtures_root().join(format!("v{version}"))
}

fn fixtures_in(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn every_version_has_a_fixture_directory() {
    for version in CONFIG_FORMAT_OLDEST..=CONFIG_FORMAT {
        let dir = fixture_dir(version);
        assert!(
            dir.is_dir(),
            "config format {version} has no fixtures under {}",
            dir.display()
        );
        assert!(
            !fixtures_in(&dir).is_empty(),
            "config format {version} has an empty fixture directory"
        );
    }
}

#[test]
fn every_fixture_at_every_format_still_loads() {
    for version in CONFIG_FORMAT_OLDEST..=CONFIG_FORMAT {
        for path in fixtures_in(&fixture_dir(version)) {
            let loaded = load_from(std::slice::from_ref(&path))
                .unwrap_or_else(|e| panic!("{} no longer loads: {e:#}", path.display()));
            let layer = &loaded.layers[0];
            assert_eq!(
                layer.version,
                version,
                "{} sits in the v{version} directory but declares format {}",
                path.display(),
                layer.version
            );
        }
    }
}

#[test]
fn golden_pairs_load_identically_across_formats() {
    for version in (CONFIG_FORMAT_OLDEST..=CONFIG_FORMAT).filter(|v| *v < CONFIG_FORMAT) {
        for older in fixtures_in(&fixture_dir(version)) {
            let newer = fixture_dir(version + 1).join(older.file_name().unwrap());
            if !newer.exists() {
                continue;
            }
            let a =
                toml::to_string(&load_from(std::slice::from_ref(&older)).unwrap().config).unwrap();
            let b =
                toml::to_string(&load_from(std::slice::from_ref(&newer)).unwrap().config).unwrap();
            assert_eq!(
                a,
                b,
                "{} and {} must load to the same config",
                older.display(),
                newer.display()
            );
        }
    }
}

#[test]
fn a_newer_format_is_refused_before_the_schema_is_consulted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "version = {}\n[settings]\nkey_from_the_future = 1\n",
            CONFIG_FORMAT + 1
        ),
    )
    .unwrap();
    let err = format!("{:#}", load_from(std::slice::from_ref(&path)).unwrap_err());
    assert!(err.contains("is config version"), "{err}");
    let window = if CONFIG_FORMAT_OLDEST == CONFIG_FORMAT {
        format!("reads config version {CONFIG_FORMAT}")
    } else {
        format!("reads config versions {CONFIG_FORMAT_OLDEST} to {CONFIG_FORMAT}")
    };
    assert!(err.contains(&window), "{err}");
    assert!(!err.contains("unknown field"), "{err}");
}

#[test]
fn layers_migrate_independently_before_they_merge() {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.toml");
    let project = dir.path().join("project.toml");
    std::fs::write(&global, "[settings]\nhistory_limit = 5\n").unwrap();
    std::fs::write(
        &project,
        format!("version = {CONFIG_FORMAT}\n[settings]\nhistory_limit = 9\n"),
    )
    .unwrap();
    let loaded = load_from(&[global, project]).unwrap();
    assert_eq!(loaded.config.settings.history_limit, 9);
    assert_eq!(loaded.layers.len(), 2);
    assert_eq!(loaded.layers[0].declared, None);
    assert_eq!(loaded.layers[0].version, CONFIG_FORMAT_OLDEST);
    assert_eq!(loaded.layers[1].declared, Some(CONFIG_FORMAT));
    assert!(loaded.layers[0].needs_migration());
    assert!(!loaded.layers[1].needs_migration());
}

#[test]
fn the_version_key_never_reaches_the_typed_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, format!("version = {CONFIG_FORMAT}\n")).unwrap();
    let rendered = toml::to_string(&load_from(&[path]).unwrap().config).unwrap();
    assert!(!rendered.contains("version"), "{rendered}");
}

#[test]
fn infer_reshape_and_the_list_tools_are_always_loaded_by_default() {
    assert_eq!(
        dive_config::ToolPaths::default().always_loaded,
        [
            "builtin__infer",
            "builtin__reshape",
            "builtin__flatten",
            "builtin__dedupe",
            "builtin__sort",
            "builtin__take",
            "builtin__rank",
            "builtin__save_file",
            "builtin__read_file"
        ]
    );
}

#[test]
fn always_loaded_tools_parse_as_names_and_globs() {
    let path = fixture_dir(CONFIG_FORMAT).join("always_loaded.toml");
    let config = load_from(std::slice::from_ref(&path)).unwrap().config;
    assert_eq!(
        config.tools.always_loaded,
        ["builtin__infer", "builtin__reshape", "linear__get_*"]
    );
}

#[test]
fn pack_entries_read_every_field() {
    let path = fixture_dir(CONFIG_FORMAT).join("packs.toml");
    let loaded = load_from(std::slice::from_ref(&path)).unwrap();
    let example = &loaded.config.packs["example"];
    assert_eq!(example.git, "github.com/acme/graph-packs");
    assert_eq!(example.path, "example");
    assert_eq!(loaded.config.packs["bare"].path, "");
    assert_eq!(example.reference.as_deref(), Some("v2"));
    assert_eq!(example.include, ["plan__*"]);
    assert_eq!(example.exclude, ["tool__file_info"]);
    assert_eq!(loaded.config.packs["bare"].reference, None);
    assert_eq!(
        loaded.pack_layers[0].packs.keys().collect::<Vec<_>>(),
        ["bare", "example", "pinned"]
    );
}

#[test]
fn file_storage_and_mcp_file_modes_read_every_field() {
    let path = fixture_dir(CONFIG_FORMAT).join("files.toml");
    let loaded = load_from(&[path]).unwrap();
    let files = loaded.config.storage.files.expect("storage.files");
    assert_eq!(files.backend, dive_config::FilesBackend::S3);
    assert_eq!(files.endpoint.as_deref(), Some("https://s3.example.test"));
    assert_eq!(files.bucket.as_deref(), Some("graph"));
    assert_eq!(files.region.as_deref(), Some("garage"));
    assert_eq!(files.prefix.as_deref(), Some("team"));
    assert_eq!(files.access_key_id.as_deref(), Some("key"));
    assert_eq!(files.secret_access_key.as_deref(), Some("secret"));
    assert!(files.missing_env.is_empty());
    assert_eq!(
        loaded.config.mcp["browser"].files,
        dive_config::FilesMode::Url
    );
}

#[test]
fn mcp_servers_pass_file_references_as_paths_unless_told_otherwise() {
    let path = fixture_dir(CONFIG_FORMAT).join("mcp_description.toml");
    let loaded = load_from(&[path]).unwrap();
    assert!(loaded
        .config
        .mcp
        .values()
        .all(|server| server.files == dive_config::FilesMode::Path));
    assert!(loaded.config.storage.files.is_none());
}

#[test]
fn an_unset_secret_in_file_storage_is_recorded_instead_of_failing_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "version = {CONFIG_FORMAT}\n[storage.files]\nbackend = \"s3\"\nbucket = \"b\"\nsecret_access_key = \"${{DIVE_TEST_NEVER_SET_FILES_SECRET}}\"\n"
        ),
    )
    .unwrap();
    let loaded = load_from(&[path]).unwrap();
    let files = loaded.config.storage.files.unwrap();
    assert_eq!(files.missing_env.len(), 1);
    assert_eq!(files.missing_env[0].field, "secret_access_key");
    assert_eq!(files.missing_env[0].var, "DIVE_TEST_NEVER_SET_FILES_SECRET");
}

#[test]
fn image_roles_take_their_type_and_price_image_tokens() {
    let path = fixture_dir(CONFIG_FORMAT).join("images.toml");
    let config = load_from(&[path]).unwrap().config;
    assert_eq!(
        config.providers["gemini"].kind,
        dive_config::ProviderKind::Gemini
    );
    assert_eq!(
        config.models.kind_of("image"),
        Some(dive_config::ModelKind::Image)
    );
    assert_eq!(
        config.models.kind_of("hero_art"),
        Some(dive_config::ModelKind::Image)
    );
    assert_eq!(
        config.pricing["gemini-nano-banana-2.1"].output_image,
        Some(30.0)
    );
}

#[test]
fn roles_and_fallbacks_take_images_unless_they_say_not() {
    let path = fixture_dir(CONFIG_FORMAT).join("image_input.toml");
    let config = load_from(std::slice::from_ref(&path)).unwrap().config;
    let default = config.models.get("default").unwrap();
    assert_eq!(default.images, None);
    assert!(default.takes_images());
    assert!(!default.fallbacks[0].takes_images());
    assert!(!config.models.get("judge").unwrap().takes_images());
}
