use super::*;
use std::collections::BTreeMap;
use std::path::Path;

const TOOL: &str =
    "version: 2\nname: echo\ndescription: Echoes\nkind: exec\ncommand: echo\nargs: ['{}']\n";

fn write(dir: &Path, path: &str, content: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn entry() -> PackEntry {
    PackEntry {
        git: "github.com/acme/graph-pack-example".to_string(),
        reference: Some("v1".to_string()),
        include: Vec::new(),
        exclude: Vec::new(),
    }
}

fn install(layer_dir: &Path, name: &str, tag: &str, lock: bool) -> String {
    let copy = layer_dir.join("packs").join(name);
    write(
        &copy,
        "pack.toml",
        &format!("version = 1\nname = \"{name}\"\ndescription = \"d\"\n"),
    );
    write(&copy, "tools/echo.yaml", TOOL);
    let hash = content_hash(&copy).unwrap();
    if lock {
        let path = layer_dir.join("packs.lock");
        let mut text = std::fs::read_to_string(&path).unwrap_or_else(|_| "version = 1\n".into());
        text.push_str(&format!(
            "\n[packs.{name}]\ngit = \"github.com/acme/graph-pack-example\"\ntag = \"{tag}\"\nsha = \"abc123\"\nhash = \"{hash}\"\n"
        ));
        std::fs::write(path, text).unwrap();
    }
    hash
}

fn layer(kind: LayerKind, dir: &Path, names: &[&str]) -> PackLayer {
    PackLayer {
        kind,
        dir: dir.to_path_buf(),
        packs: names
            .iter()
            .map(|name| (name.to_string(), entry()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn no_servers() -> BTreeSet<String> {
    BTreeSet::new()
}

#[test]
fn an_installed_pack_loads_with_its_lock_tag() {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path(), "example", "v1.0.0", true);
    let installed = InstalledPacks::load(
        &[layer(LayerKind::Project, dir.path(), &["example"])],
        &no_servers(),
    )
    .unwrap();
    assert!(installed.warnings.is_empty(), "{:?}", installed.warnings);
    assert_eq!(installed.packs.len(), 1);
    assert!(!installed.packs[0].modified());
    assert_eq!(
        installed.packs[0].loaded.source.describe(),
        "pack example v1.0.0 (project)"
    );
    let set = installed.pack_set();
    assert!(set.item("example__tool__echo").is_some());
    assert_eq!(installed.check_json()[0]["modified"], false);
}

#[test]
fn a_configured_pack_with_no_copy_names_the_add_command() {
    let dir = tempfile::tempdir().unwrap();
    let error = InstalledPacks::load(
        &[layer(LayerKind::Global, dir.path(), &["example"])],
        &no_servers(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("graph pack add example --global"), "{error}");
}

#[test]
fn a_copy_with_no_lock_entry_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path(), "example", "v1.0.0", false);
    let error = InstalledPacks::load(
        &[layer(LayerKind::Project, dir.path(), &["example"])],
        &no_servers(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("has no entry in"), "{error}");
    assert!(error.contains("packs.lock"), "{error}");
}

#[test]
fn an_unconfigured_copy_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path(), "example", "v1.0.0", true);
    install(dir.path(), "stray", "v1.0.0", true);
    let error = InstalledPacks::load(
        &[layer(LayerKind::Project, dir.path(), &["example"])],
        &no_servers(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("is not a configured pack"), "{error}");
    assert!(error.contains("[packs.stray]"), "{error}");
}

#[test]
fn an_edited_copy_loads_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path(), "example", "v1.0.0", true);
    write(
        &dir.path().join("packs/example"),
        "tools/echo.yaml",
        &TOOL.replace("Echoes", "Echoes, edited"),
    );
    let installed = InstalledPacks::load(
        &[layer(LayerKind::Project, dir.path(), &["example"])],
        &no_servers(),
    )
    .unwrap();
    assert_eq!(installed.packs.len(), 1);
    assert!(installed.packs[0].modified());
    assert_eq!(installed.warnings.len(), 1);
    assert!(installed.warnings[0].contains("local edits"));
    assert!(installed.check_text().contains("local edits"));
}

#[test]
fn a_project_entry_shadows_the_global_one_whole() {
    let global = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    install(global.path(), "example", "v1.0.0", true);
    install(project.path(), "example", "v2.0.0", true);
    let installed = InstalledPacks::load(
        &[
            layer(LayerKind::Global, global.path(), &["example"]),
            layer(LayerKind::Project, project.path(), &["example"]),
        ],
        &no_servers(),
    )
    .unwrap();
    assert_eq!(installed.packs.len(), 1);
    assert_eq!(installed.packs[0].loaded.source.layer, LayerKind::Project);
    assert_eq!(installed.packs[0].lock.tag.as_deref(), Some("v2.0.0"));
    assert_eq!(installed.shadowed.len(), 1);
    assert_eq!(installed.shadowed[0].layer, LayerKind::Global);
    assert_eq!(installed.shadowed[0].by, LayerKind::Project);
    assert!(installed
        .check_text()
        .contains("shadowed by the project layer"));

    std::fs::remove_dir_all(global.path().join("packs/example")).unwrap();
    assert!(InstalledPacks::load(
        &[
            layer(LayerKind::Global, global.path(), &["example"]),
            layer(LayerKind::Project, project.path(), &["example"]),
        ],
        &no_servers(),
    )
    .is_ok());
}

#[test]
fn reserved_names_and_mcp_collisions_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    install(dir.path(), "linear", "v1.0.0", true);
    let servers = BTreeSet::from(["linear".to_string()]);
    let error = InstalledPacks::load(
        &[layer(LayerKind::Project, dir.path(), &["linear"])],
        &servers,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("[mcp.linear]"), "{error}");

    let dir = tempfile::tempdir().unwrap();
    install(dir.path(), "builtin", "v1.0.0", true);
    let error = InstalledPacks::load(
        &[layer(LayerKind::Project, dir.path(), &["builtin"])],
        &no_servers(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("reserved"), "{error}");
}
