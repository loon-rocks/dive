use super::*;
use serde_json::json;
use tempfile::TempDir;

fn layer_in(dir: &Path, config: &str) -> Layer {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("config.toml"), config).unwrap();
    Layer {
        kind: LayerKind::Project,
        config_paths: vec![dir.join("config.toml")],
        dir: dir.to_path_buf(),
        flag: String::new(),
    }
}

fn project(config: &str) -> (TempDir, Layer) {
    let dir = tempfile::tempdir().unwrap();
    let layer = layer_in(&dir.path().join(".dive"), config);
    (dir, layer)
}

fn request(source: &str, path: Option<&str>) -> AddRequest {
    AddRequest {
        source: source.to_string(),
        path: path.map(str::to_string),
        ..AddRequest::default()
    }
}

#[test]
fn the_project_layer_needs_its_directory_and_names_config_init() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path());
    let error = target(&workspace, &LayerChoice::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("dive config init"), "{error}");
    std::fs::create_dir_all(dir.path().join(".dive")).unwrap();
    let layer = target(&workspace, &LayerChoice::default()).unwrap();
    assert_eq!(layer.kind, LayerKind::Project);
    assert_eq!(layer.dir, dir.path().join(".dive"));
    assert_eq!(layer.flag, "");
    assert_eq!(layer.config_paths.len(), 2);
}

#[test]
fn a_dir_choice_targets_that_directory_alone() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path());
    let choice = LayerChoice {
        global: false,
        dir: Some(dir.path().to_path_buf()),
    };
    let layers = listed_layers(&workspace, &choice).unwrap();
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].flag, format!(" --dir {}", dir.path().display()));
    let missing = LayerChoice {
        global: false,
        dir: Some(dir.path().join("absent")),
    };
    assert!(target(&workspace, &missing)
        .unwrap_err()
        .to_string()
        .ends_with("is not a directory"));
}

#[test]
fn an_escaping_path_is_refused_before_any_network() {
    let (_dir, layer) = project("version = 4\n");
    let error = add(&layer, &request("https://example.invalid/r", Some("../x"))).unwrap_err();
    assert!(matches!(error, PackError::InvalidPath(_)), "{error:?}");
    assert!(error.is_refusal());
    assert!(error.to_string().starts_with("path "), "{error}");
    assert!(error.problems().is_empty());
}

#[test]
fn removing_an_absent_pack_is_a_refusal_naming_the_layer() {
    let (_dir, layer) = project("version = 4\n");
    let error = remove(&layer, "ghost").unwrap_err();
    assert!(matches!(error, PackError::NotInstalled(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        format!("pack 'ghost' is not installed in {}", layer.dir.display())
    );
}

#[test]
fn updating_with_nothing_configured_says_so_without_a_layer_key() {
    let (_dir, layer) = project("version = 4\n");
    let updates = update(&layer, &[], false).unwrap();
    assert!(updates.ok);
    assert_eq!(
        updates.note,
        format!("no packs configured in {}", layer.config_path().display())
    );
    assert_eq!(
        serde_json::to_value(&updates).unwrap(),
        json!({"ok": true, "packs": []})
    );
}

#[test]
fn updating_an_unconfigured_name_refuses_that_row() {
    let (_dir, layer) = project("version = 4\n");
    let updates = update(&layer, &["ghost".to_string()], false).unwrap();
    assert!(!updates.ok);
    let value = serde_json::to_value(&updates).unwrap();
    assert_eq!(value["error"], json!("1 of 1 packs not updated"));
    assert_eq!(value["layer"], json!("project"));
    assert_eq!(value["packs"][0]["status"], json!("refused"));
    assert!(value["packs"][0].get("crossed").is_none());
    assert_eq!(value["problems"][0], value["packs"][0]["summary"]);
}

#[test]
fn an_offline_list_reports_configured_but_uninstalled_packs() {
    let (_dir, layer) =
        project("version = 4\n[packs.example]\ngit = \"github.com/owner/repo\"\nref = \"v1\"\n");
    let listed = list(std::slice::from_ref(&layer), true).unwrap();
    assert_eq!(listed.count, 1);
    let row = &listed.packs[0];
    assert!(!row.installed);
    assert_eq!(row.modified, None);
    assert_eq!(listed.text(), "example\tproject\tnot installed\n");
    let value = serde_json::to_value(&listed).unwrap();
    assert_eq!(value["offline"], json!(true));
    assert!(value["packs"][0].get("newest").is_none());
    assert!(value["packs"][0].get("remoteError").is_none());
    assert_eq!(value["packs"][0]["ref"], json!("v1"));
}

#[test]
fn remote_columns_flatten_into_the_row() {
    let mut row = PackRow {
        name: "example".to_string(),
        layer: "project".to_string(),
        dir: ".dive/packs/example".to_string(),
        git: "github.com/owner/repo".to_string(),
        path: String::new(),
        reference: Some("v1".to_string()),
        installed: true,
        tag: Some("v1.2.0".to_string()),
        sha: Some("abcdef0123".to_string()),
        modified: Some(true),
        shadowed: false,
        shadowed_by: None,
        remote: Some(RemoteColumns::Reached {
            tag_moved: None,
            newest: Some("v2.0.0".to_string()),
            newest_in_ref: Some("v1.3.0".to_string()),
        }),
    };
    let value = serde_json::to_value(&row).unwrap();
    assert_eq!(value["newest"], json!("v2.0.0"));
    assert_eq!(value["newestInRef"], json!("v1.3.0"));
    assert_eq!(value["tagMoved"], json!(null));
    assert_eq!(
        row.line(false),
        "example\tproject\tv1.2.0\tref v1: v1.3.0\tnewest: v2.0.0\tlocal edits\n"
    );
    row.remote = Some(RemoteColumns::Unreachable {
        remote_error: "offline".to_string(),
    });
    let value = serde_json::to_value(&row).unwrap();
    assert_eq!(value["remoteError"], json!("offline"));
    assert!(value.get("newest").is_none());
    assert_eq!(
        row.line(false),
        "example\tproject\tv1.2.0\tremote unreachable\tlocal edits\n"
    );
}
