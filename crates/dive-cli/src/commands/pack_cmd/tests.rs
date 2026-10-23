use super::major::{Consent, Majors};
use super::*;
use dive_config::pack::{load_lock, LayerKind, PackLock};
use dive_core::pipeline::{AskOutcome, AskRequest, Interlocutor};
use dive_runtime::packs::{content_hash, InstalledPacks};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

const TOOL: &str = r#"version: 2
name: echo
description: Echoes a name
kind: exec
command: echo
args: ['{"name": "{{input.name}}"}']
input_schema:
  type: object
  required: [name]
  properties:
    name: { type: string }
"#;

const PLAN: &str = r#"version: 2
identifier: whoami
name: Who am I
description: Echoes the installed pack name
steps:
  - id: a
    tool_name: $pack__tool__echo
    input:
      name: "{{pack.name}}"
finish:
  output:
    name: "{{a.name}}"
"#;

const CONFIG: &str =
    "version = 4\n\n# my settings\n[settings]\nmax_agent_iterations = 7   # tuned\n";

fn git(dir: &Path, args: &[&str]) -> String {
    dive_runtime::packs::git::fixture::git(dir, args)
}

fn write(dir: &Path, path: &str, content: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

struct Upstream {
    _dir: TempDir,
    work: PathBuf,
    url: String,
}

impl Upstream {
    fn release(&self, description: &str, tag: &str) {
        write(
            &self.work,
            "pack.toml",
            &format!("version = 1\nname = \"example\"\ndescription = \"{description}\"\n"),
        );
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-q", "-m", description]);
        git(&self.work, &["tag", tag]);
    }
}

fn upstream() -> Upstream {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"]);
    write(&work, "tools/echo.yaml", TOOL);
    write(&work, "plans/whoami.yaml", PLAN);
    write(&work, "README.md", "# example\n");
    let upstream = Upstream {
        url: format!("file://{}", work.display()),
        work,
        _dir: dir,
    };
    upstream.release("one", "v1.0.0");
    write(
        &upstream.work,
        "pack.toml",
        "version = 1\nname = \"example\"\ndescription = \"one point one\"\n",
    );
    git(&upstream.work, &["commit", "-q", "-am", "one point one"]);
    git(&upstream.work, &["tag", "-a", "v1.1.0", "-m", "v1.1.0"]);
    git(&upstream.work, &["branch", "next"]);
    upstream.release("two", "v2.0.0");
    git(&upstream.work, &["tag", "v3.0.0-rc.1"]);
    upstream
}

struct Project {
    _dir: TempDir,
    layer: Layer,
}

fn layer_in(kind: LayerKind, dir: PathBuf, config: &str) -> Layer {
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), config).unwrap();
    Layer {
        kind,
        config_paths: vec![dir.join("config.toml")],
        dir,
        flag: String::new(),
    }
}

fn scratch(config: &str) -> Project {
    let dir = tempfile::tempdir().unwrap();
    let layer = layer_in(LayerKind::Project, dir.path().join(".dive"), config);
    Project { _dir: dir, layer }
}

fn request(source: &str, reference: Option<&str>, as_name: Option<&str>) -> AddRequest {
    AddRequest {
        source: source.to_string(),
        as_name: as_name.map(str::to_string),
        reference: reference.map(str::to_string),
        path: None,
    }
}

fn config(layer: &Layer) -> String {
    std::fs::read_to_string(layer.config_path()).unwrap()
}

fn lock(layer: &Layer) -> PackLock {
    load_lock(&layer.lock_path()).unwrap()
}

fn error(outcome: &Outcome) -> String {
    assert!(outcome.rejected, "{}", outcome.body);
    outcome.body.to_string()
}

fn load(layers: &[&Layer]) -> anyhow::Result<InstalledPacks> {
    let paths: Vec<PathBuf> = layers.iter().map(|layer| layer.config_path()).collect();
    let loaded = dive_config::load_from(&paths, &dive_core::secrets::EnvSource)?;
    InstalledPacks::load(&loaded.pack_layers, &BTreeSet::new())
}

fn staging_left(layer: &Layer) -> Vec<String> {
    std::fs::read_dir(&layer.dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".pack-staging-") || name.ends_with(".tmp"))
        .collect()
}

#[test]
fn add_installs_the_newest_release_as_a_plain_copy_and_follows_its_major() {
    let upstream = upstream();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    let outcome = add(layer, &request(&upstream.url, None, None)).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert_eq!(outcome.body["tag"], "v2.0.0");

    let text = config(layer);
    assert!(text.starts_with(CONFIG), "{text}");
    assert!(
        text.contains(&format!(
            "[packs.example]\ngit = \"{}\"\nref = \"v2\"\n",
            upstream.url
        )),
        "{text}"
    );
    let copy = layer.copy_dir("example");
    assert!(copy.join("plans/whoami.yaml").is_file());
    assert!(!copy.join(".git").exists());
    let locked = &lock(layer).packs["example"];
    assert_eq!(locked.git, upstream.url);
    assert_eq!(locked.tag.as_deref(), Some("v2.0.0"));
    assert_eq!(
        locked.sha,
        git(&upstream.work, &["rev-parse", "v2.0.0^{commit}"])
    );
    assert_eq!(locked.hash, content_hash(&copy).unwrap());
    assert!(staging_left(layer).is_empty());

    let installed = load(&[layer]).unwrap();
    assert!(installed.warnings.is_empty(), "{:?}", installed.warnings);
    assert!(installed
        .plans()
        .any(|plan| plan.identifier == "example__plan__whoami"));
}

#[test]
fn add_follows_a_major_line_a_pinned_tag_or_a_branch() {
    let upstream = upstream();
    let cases = [
        ("v1", Some("v1.1.0"), "v1.1.0"),
        ("v1.0.0", Some("v1.0.0"), "v1.0.0"),
        ("next", None, "v1.1.0"),
    ];
    for (reference, tag, commit) in cases {
        let project = scratch(CONFIG);
        let layer = &project.layer;
        let outcome = add(layer, &request(&upstream.url, Some(reference), None)).unwrap();
        assert!(!outcome.rejected, "{}", outcome.body);
        assert!(config(layer).contains(&format!("ref = \"{reference}\"")));
        let locked = &lock(layer).packs["example"];
        assert_eq!(locked.tag.as_deref(), tag, "{reference}");
        assert_eq!(
            locked.sha,
            git(
                &upstream.work,
                &["rev-parse", &format!("{commit}^{{commit}}")]
            )
        );
        assert_eq!(
            outcome.body["warnings"].as_array().unwrap().is_empty(),
            tag.is_some(),
            "{reference}"
        );
    }

    let project = scratch(CONFIG);
    let refusal = error(&add(&project.layer, &request(&upstream.url, Some("v4"), None)).unwrap());
    assert!(refusal.contains("no v4.*.* release tag"), "{refusal}");
    assert!(!project.layer.dir.join("packs").exists());
}

#[test]
fn add_without_release_tags_installs_the_default_branch_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"]);
    write(
        &work,
        "pack.toml",
        "version = 1\nname = \"example\"\ndescription = \"d\"\n",
    );
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "first"]);
    let project = scratch(CONFIG);
    let outcome = add(
        &project.layer,
        &request(&work.display().to_string(), None, None),
    )
    .unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert!(config(&project.layer).contains("ref = \"main\""));
    assert!(outcome.body["warnings"][0]
        .as_str()
        .unwrap()
        .contains("no vX.Y.Z release tags"));
    assert_eq!(lock(&project.layer).packs["example"].tag, None);
}

#[test]
fn add_as_installs_under_another_name() {
    let upstream = upstream();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    let outcome = add(layer, &request(&upstream.url, None, Some("files"))).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert!(layer.copy_dir("files").is_dir());
    assert!(config(layer).contains("[packs.files]"));
    let installed = load(&[layer]).unwrap();
    assert!(installed
        .plans()
        .any(|plan| plan.identifier == "files__plan__whoami"));
}

#[test]
fn add_refuses_names_in_use_reserved_or_owned_by_an_mcp_server() {
    let upstream = upstream();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    assert!(
        !add(layer, &request(&upstream.url, None, None))
            .unwrap()
            .rejected
    );
    let before = (config(layer), lock(layer));
    let refusal = error(&add(layer, &request(&upstream.url, Some("v1"), None)).unwrap());
    assert!(refusal.contains("already configured"), "{refusal}");
    assert_eq!((config(layer), lock(layer)), before);

    let refusal = error(&add(layer, &request(&upstream.url, None, Some("builtin"))).unwrap());
    assert!(refusal.contains("reserved"), "{refusal}");

    let served = scratch(&format!("{CONFIG}\n[mcp.example]\ncommand = \"echo\"\n"));
    let refusal = error(&add(&served.layer, &request(&upstream.url, None, None)).unwrap());
    assert!(refusal.contains("[mcp.example]"), "{refusal}");
    assert!(!served.layer.copy_dir("example").exists());
    assert!(!served.layer.lock_path().exists());
    assert!(staging_left(&served.layer).is_empty());
}

#[test]
fn add_installs_a_configured_entry_that_has_no_copy() {
    let upstream = upstream();
    let text = format!(
        "{CONFIG}\n[packs.example]\ngit = \"{}\"\nref = \"v1\"\n",
        upstream.url
    );
    let project = scratch(&text);
    let layer = &project.layer;
    let outcome = add(layer, &request("example", None, None)).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert_eq!(config(layer), text);
    assert_eq!(lock(layer).packs["example"].tag.as_deref(), Some("v1.1.0"));
    load(&[layer]).unwrap();

    let refusal = error(&add(layer, &request("example", None, None)).unwrap());
    assert!(refusal.contains("already installed"), "{refusal}");
}

#[test]
fn a_missing_project_directory_points_at_config_init() {
    let dir = tempfile::tempdir().unwrap();
    let error = project_layer(dir.path().join(".dive"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("dive config init"), "{error}");
}

#[test]
fn a_global_install_is_a_plain_copy_like_a_project_one() {
    let upstream = upstream();
    let dir = tempfile::tempdir().unwrap();
    let layer = layer_in(LayerKind::Global, dir.path().to_path_buf(), CONFIG);
    let outcome = add(&layer, &request(&upstream.url, Some("v1"), None)).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    let copy = layer.copy_dir("example");
    assert!(copy.join("plans/whoami.yaml").is_file());
    assert!(!copy.join(".git").exists());
    assert_eq!(
        lock(&layer).packs["example"].sha,
        git(&upstream.work, &["rev-parse", "v1.1.0^{commit}"])
    );
    assert_eq!(
        lock(&layer).packs["example"].hash,
        content_hash(&copy).unwrap()
    );
}

#[test]
fn list_reports_newer_releases_local_edits_and_shadowing() {
    let upstream = upstream();
    let global_dir = tempfile::tempdir().unwrap();
    let global = layer_in(LayerKind::Global, global_dir.path().to_path_buf(), CONFIG);
    let project = scratch(CONFIG);
    assert!(
        !add(&global, &request(&upstream.url, Some("v1.0.0"), None))
            .unwrap()
            .rejected
    );
    assert!(
        !add(&project.layer, &request(&upstream.url, Some("v1"), None))
            .unwrap()
            .rejected
    );
    upstream.release("one point two", "v1.2.0");
    write(&project.layer.copy_dir("example"), "README.md", "edited\n");

    let outcome = list(&[global.clone(), project.layer.clone()], false).unwrap();
    let rows = outcome.body["packs"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["layer"], "global");
    assert_eq!(rows[0]["tag"], "v1.0.0");
    assert_eq!(rows[0]["shadowedBy"], "project");
    assert_eq!(rows[0]["modified"], false);
    assert_eq!(rows[1]["layer"], "project");
    assert_eq!(rows[1]["tag"], "v1.1.0");
    assert_eq!(rows[1]["newestInRef"], "v1.2.0");
    assert_eq!(rows[1]["newest"], "v2.0.0");
    assert_eq!(rows[1]["modified"], true);
    assert_eq!(rows[1]["shadowed"], false);
    let text = outcome.raw.unwrap();
    assert!(text.contains("shadowed by the project layer"), "{text}");
    assert!(text.contains("local edits"), "{text}");
    assert!(text.contains("newest: v2.0.0"), "{text}");

    let offline = list(std::slice::from_ref(&project.layer), true).unwrap();
    let row = &offline.body["packs"][0];
    assert_eq!(row["modified"], true);
    assert!(row.get("newest").is_none());
    assert!(!offline.raw.unwrap().contains("newest"));
}

#[test]
fn update_moves_within_the_major_and_names_what_it_held_back() {
    let upstream = upstream();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    assert!(
        !add(layer, &request(&upstream.url, Some("v1"), None))
            .unwrap()
            .rejected
    );
    let current = update(layer, &[], false).unwrap();
    assert_eq!(current.body["packs"][0]["status"], "current");

    upstream.release("one point two", "v1.2.0");
    let config_before = config(layer);
    let outcome = update(layer, &["example".to_string()], false).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    let result = &outcome.body["packs"][0];
    assert_eq!(result["status"], "updated");
    assert_eq!(
        (result["from"].as_str(), result["to"].as_str()),
        (Some("v1.1.0"), Some("v1.2.0"))
    );
    assert_eq!(
        result["heldBack"],
        "v2.0.0 available: dive pack update example --major"
    );
    let locked = &lock(layer).packs["example"];
    assert_eq!(locked.tag.as_deref(), Some("v1.2.0"));
    assert_eq!(
        locked.hash,
        content_hash(&layer.copy_dir("example")).unwrap()
    );
    assert_eq!(config(layer), config_before);
    assert!(staging_left(layer).is_empty());
    assert!(
        std::fs::read_to_string(layer.copy_dir("example").join("pack.toml"))
            .unwrap()
            .contains("one point two")
    );
}

#[test]
fn update_skips_a_pinned_tag_and_moves_a_branch_to_its_head() {
    let upstream = upstream();
    let pinned = scratch(CONFIG);
    assert!(
        !add(&pinned.layer, &request(&upstream.url, Some("v1.0.0"), None))
            .unwrap()
            .rejected
    );
    let outcome = update(&pinned.layer, &[], false).unwrap();
    assert_eq!(outcome.body["packs"][0]["status"], "pinned");
    assert_eq!(
        lock(&pinned.layer).packs["example"].tag.as_deref(),
        Some("v1.0.0")
    );

    let branch = scratch(CONFIG);
    assert!(
        !add(&branch.layer, &request(&upstream.url, Some("next"), None))
            .unwrap()
            .rejected
    );
    git(&upstream.work, &["checkout", "-q", "next"]);
    write(&upstream.work, "README.md", "# next\n");
    git(&upstream.work, &["commit", "-q", "-am", "next"]);
    let head = git(&upstream.work, &["rev-parse", "HEAD"]);
    git(&upstream.work, &["checkout", "-q", "main"]);
    let outcome = update(&branch.layer, &[], false).unwrap();
    assert_eq!(
        outcome.body["packs"][0]["status"], "updated",
        "{}",
        outcome.body
    );
    assert_eq!(lock(&branch.layer).packs["example"].sha, head);
    assert_eq!(
        std::fs::read_to_string(branch.layer.copy_dir("example").join("README.md")).unwrap(),
        "# next\n"
    );
}

#[test]
fn update_refuses_local_edits_unless_forced() {
    let upstream = upstream();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    assert!(
        !add(layer, &request(&upstream.url, Some("v1"), None))
            .unwrap()
            .rejected
    );
    let locked = lock(layer);
    let readme = layer.copy_dir("example").join("README.md");
    std::fs::write(&readme, "mine\n").unwrap();

    let refusal = error(&update(layer, &[], false).unwrap());
    assert!(refusal.contains("local edits"), "{refusal}");
    assert!(refusal.contains("--force"), "{refusal}");
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "mine\n");
    assert_eq!(lock(layer), locked);

    let forced = update(layer, &[], true).unwrap();
    assert!(!forced.rejected, "{}", forced.body);
    assert_eq!(forced.body["packs"][0]["status"], "reinstalled");
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "# example\n");
    assert_eq!(lock(layer), locked);
}

#[test]
fn a_new_version_that_does_not_load_leaves_everything_untouched() {
    let upstream = upstream();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    assert!(
        !add(layer, &request(&upstream.url, Some("v1"), None))
            .unwrap()
            .rejected
    );
    git(
        &upstream.work,
        &["checkout", "-q", "-b", "broken", "v1.1.0"],
    );
    write(
        &upstream.work,
        "tools/echo.yaml",
        "version: 2\nname: echo\nbogus: true\n",
    );
    upstream.release("broken", "v1.2.0");
    git(&upstream.work, &["checkout", "-q", "main"]);

    let before = (
        config(layer),
        lock(layer),
        content_hash(&layer.copy_dir("example")).unwrap(),
    );
    let refusal = error(&update(layer, &[], false).unwrap());
    assert!(refusal.contains("v1.2.0 does not load"), "{refusal}");
    assert!(refusal.contains("echo.yaml"), "{refusal}");
    assert_eq!(
        (
            config(layer),
            lock(layer),
            content_hash(&layer.copy_dir("example")).unwrap()
        ),
        before
    );
    assert!(staging_left(layer).is_empty());
}

#[test]
fn remove_deletes_the_pack_and_lists_files_that_still_call_it() {
    let upstream = upstream();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    assert!(
        !add(layer, &request(&upstream.url, None, None))
            .unwrap()
            .rejected
    );
    write(
        &layer.dir,
        "plans/uses.yaml",
        "steps:\n  - tool_name: example__plan__whoami\n",
    );
    write(&layer.dir, "agents/other.yaml", "tools: [user__x]\n");

    let outcome = remove(layer, "example").unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert!(!layer.copy_dir("example").exists());
    assert_eq!(config(layer), CONFIG);
    assert!(lock(layer).packs.is_empty());
    assert!(layer.dir.join("plans/uses.yaml").is_file());
    assert_eq!(
        outcome.body["callers"],
        json!([layer.dir.join("plans/uses.yaml").display().to_string()])
    );
    assert!(staging_left(layer).is_empty());

    let refusal = error(&remove(layer, "example").unwrap());
    assert!(refusal.contains("not installed"), "{refusal}");
}

impl Upstream {
    fn release_requiring(&self, requires: &str, tag: &str) {
        write(
            &self.work,
            "pack.toml",
            &format!(
                "version = 1\nname = \"example\"\ndescription = \"{tag}\"\n\n[requires]\n{requires}\n"
            ),
        );
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-q", "-m", tag]);
        git(&self.work, &["tag", tag]);
    }
}

fn other_os() -> &'static str {
    if std::env::consts::OS == "linux" {
        "macos"
    } else {
        "linux"
    }
}

#[test]
fn add_refuses_a_release_needing_a_newer_graph_and_installs_nothing() {
    let upstream = upstream();
    upstream.release_requiring("graph = \">=99\"", "v4.0.0");
    let project = scratch(CONFIG);
    let layer = &project.layer;
    let refusal = error(&add(layer, &request(&upstream.url, None, None)).unwrap());
    assert!(
        refusal.contains(&format!(
            "requires.dive: needs Dive >=99; this is Dive {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{refusal}"
    );
    assert!(!layer.copy_dir("example").exists());
    assert_eq!(config(layer), CONFIG);
    assert!(lock(layer).packs.is_empty());
    assert!(staging_left(layer).is_empty());
}

#[test]
fn add_installs_a_release_whose_manifest_names_no_os() {
    for requires in [None, Some("os = []")] {
        let upstream = upstream();
        if let Some(requires) = requires {
            upstream.release_requiring(requires, "v4.0.0");
        }
        let project = scratch(CONFIG);
        let layer = &project.layer;
        let outcome = add(layer, &request(&upstream.url, None, None)).unwrap();
        assert!(!outcome.rejected, "{}", outcome.body);
        assert!(layer.copy_dir("example").is_dir());
        assert!(lock(layer).packs.contains_key("example"));
    }
}

#[test]
fn update_refuses_a_release_for_another_os_and_keeps_the_installed_one() {
    let upstream = upstream();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    assert!(
        !add(layer, &request(&upstream.url, Some("v1"), None))
            .unwrap()
            .rejected
    );
    upstream.release_requiring(&format!("os = [\"{}\"]", other_os()), "v1.2.0");
    let refusal = error(&update(layer, &[], false).unwrap());
    assert!(
        refusal.contains(&format!(
            "requires.os: needs {}; this machine runs {}",
            other_os(),
            std::env::consts::OS
        )),
        "{refusal}"
    );
    assert_eq!(lock(layer).packs["example"].tag.as_deref(), Some("v1.1.0"));
}

const MCP_CONFIG: &str = "version = 4\n\n[mcp.everything]\ncommand = \"true\"\n";

const UNMET: &str = r#"binaries = ["dive-pack-no-such-binary"]
env = ["DIVE_PACK_TEST_NEVER_SET"]

[requires.mcp.everything]
server = "mcp-servers/everything"

[requires.mcp.linear]
url = "https://mcp.linear.app/mcp"
"#;

#[test]
fn add_reports_missing_binaries_env_and_unconfigured_mcp_servers_with_a_snippet() {
    let upstream = upstream();
    upstream.release_requiring(UNMET, "v4.0.0");
    let project = scratch(MCP_CONFIG);
    let layer = &project.layer;
    let outcome = add(layer, &request(&upstream.url, None, None)).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert_eq!(
        outcome.body["unmet"],
        json!([
            "requires.binaries: dive-pack-no-such-binary not found on PATH",
            "requires.env: DIVE_PACK_TEST_NEVER_SET not set (or empty)",
            "requires.mcp.linear: no MCP server is configured under [mcp.linear] (url = \"https://mcp.linear.app/mcp\")",
        ])
    );
    assert_eq!(
        outcome.body["mcpSnippets"],
        json!(["[mcp.linear]\nurl = \"https://mcp.linear.app/mcp\""])
    );
    let note = outcome.body["note"].as_str().unwrap();
    assert!(
        note.contains(
            "example: unmet requires.binaries: dive-pack-no-such-binary not found on PATH"
        ),
        "{note}"
    );
    assert!(
        note.contains("  [mcp.linear]\n    url = \"https://mcp.linear.app/mcp\""),
        "{note}"
    );
    assert!(!config(layer).contains("[mcp.linear]"));
    assert!(layer.copy_dir("example").is_dir());
}

#[test]
fn update_reports_what_the_new_release_needs() {
    let upstream = upstream();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    assert!(
        !add(layer, &request(&upstream.url, Some("v1"), None))
            .unwrap()
            .rejected
    );
    upstream.release_requiring("binaries = [\"dive-pack-no-such-binary\"]", "v1.2.0");
    let outcome = update(layer, &[], false).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert_eq!(
        outcome.body["packs"][0]["unmet"],
        json!(["requires.binaries: dive-pack-no-such-binary not found on PATH"])
    );
    assert_eq!(lock(layer).packs["example"].tag.as_deref(), Some("v1.2.0"));
}

const RENAME_TOOL: &str = r#"version: 2
name: migrate_vMAJOR_rewrite
description: Renames the FROM tool to TO in the user's plans
kind: exec
command: sh
args:
  - "-c"
  - |-
    cd "$1"
    files=""
    for f in plans/*.yaml; do
      [ -f "$f" ] || continue
      grep -q "$2__tool__FROM" "$f" || continue
      files="$files $f"
      if [ "$3" = true ]; then
        sed -i.bak "s/$2__tool__FROM/$2__tool__TO/g" "$f"
        rm -f "$f.bak"
      fi
    done
    FAIL
    printf '{"files": ['
    sep=""
    for f in $files; do
      printf '%s{"path": "%s", "changed": true, "applied": %s, "diff": "-%s__tool__FROM +%s__tool__TO"}' "$sep" "$f" "$3" "$2" "$2"
      sep=","
    done
    printf ']}'
  - rewrite
  - "{{input.root}}"
  - "{{input.pack}}"
  - "{{input.apply}}"
output: json
input_schema:
  type: object
  required: [root, pack, apply]
  properties:
    root: { type: string }
    pack: { type: string }
    apply: { type: boolean }
"#;

const MIGRATION_PLAN: &str = r#"version: 2
identifier: migrate_vMAJOR
name: Migrate to vMAJOR
description: Renames FROM to TO in the user's plans
input_schema:
  type: object
  required: [root]
  properties:
    root: { type: string }
    apply: { type: boolean, default: false }
steps:
  - id: rewrite
    tool_name: $pack__tool__migrate_vMAJOR_rewrite
    input:
      root: "{{input.root}}"
      pack: "{{pack.name}}"
      apply: "{{input.apply}}"
finish:
  output:
    applied: "{{input.apply}}"
    files: "{{rewrite.files}}"
"#;

const USER_PLAN: &str = r#"version: 2
identifier: report
name: Report
description: Calls the pack's v1 tool
steps:
  - id: a
    tool_name: example__tool__file_stats
    input:
      name: README.md
finish:
  output:
    name: "{{a.name}}"
"#;

#[derive(Clone, Copy, PartialEq)]
enum Third {
    Migration,
    Failing,
    Guide,
    Bare,
}

fn named_tool(name: &str) -> String {
    TOOL.replace("name: echo", &format!("name: {name}"))
}

fn migration(work: &Path, major: u64, from: &str, to: &str, fail: bool) {
    let fill = |text: &str| {
        text.replace("MAJOR", &major.to_string())
            .replace("FROM", from)
            .replace("TO", to)
            .replace(
                "FAIL",
                if fail {
                    "[ \"$3\" != true ] || { echo 'disk full' >&2; exit 1; }"
                } else {
                    ""
                },
            )
    };
    write(
        work,
        &format!("plans/migrations/migrate_v{major}.yaml"),
        &fill(MIGRATION_PLAN),
    );
    write(
        work,
        &format!("tools/migrations/migrate_v{major}_rewrite.yaml"),
        &fill(RENAME_TOOL),
    );
}

fn majors_upstream(third: Third) -> Upstream {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"]);
    write(&work, "tools/file_stats.yaml", &named_tool("file_stats"));
    let upstream = Upstream {
        url: format!("file://{}", work.display()),
        work,
        _dir: dir,
    };
    upstream.release("one", "v1.0.0");
    upstream.release("one point one", "v1.1.0");
    let work = &upstream.work;
    std::fs::remove_file(work.join("tools/file_stats.yaml")).unwrap();
    write(work, "tools/file_info.yaml", &named_tool("file_info"));
    migration(work, 2, "file_stats", "file_info", false);
    upstream.release("two", "v2.0.0");
    std::fs::remove_file(work.join("tools/file_info.yaml")).unwrap();
    write(work, "tools/file_meta.yaml", &named_tool("file_meta"));
    match third {
        Third::Migration => migration(work, 3, "file_info", "file_meta", false),
        Third::Failing => migration(work, 3, "file_info", "file_meta", true),
        Third::Guide => write(
            work,
            "MIGRATING.md",
            "# Migrating\n\n## v1 to v2\n\nRun the migration.\n\n## v2 to v3\n\n- `tool__file_info` is renamed `tool__file_meta`.\n\n## v3 to v4\n\nLater.\n",
        ),
        Third::Bare => write(work, "MIGRATING.md", "# Migrating\n\n## v1 to v2\n\nRun it.\n"),
    }
    upstream.release("three", "v3.0.0");
    upstream
}

struct Asker {
    answers: std::sync::Mutex<Vec<bool>>,
    asked: std::sync::Mutex<Vec<(String, String)>>,
    watched: PathBuf,
}

#[async_trait::async_trait]
impl Interlocutor for Asker {
    async fn ask(&self, request: AskRequest) -> AskOutcome {
        let seen = std::fs::read_to_string(&self.watched).unwrap_or_default();
        self.asked.lock().unwrap().push((request.prompt, seen));
        let mut answers = self.answers.lock().unwrap();
        if answers.is_empty() {
            return AskOutcome::Unavailable("no more answers".to_string());
        }
        AskOutcome::Answered(json!({"proceed": answers.remove(0)}))
    }
}

struct Installed {
    project: Project,
    _upstream: Upstream,
}

impl Installed {
    fn layer(&self) -> &Layer {
        &self.project.layer
    }

    fn user_plan(&self) -> String {
        std::fs::read_to_string(self.layer().dir.join("plans/report.yaml")).unwrap()
    }

    fn asker(&self, answers: &[bool]) -> Arc<Asker> {
        Arc::new(Asker {
            answers: std::sync::Mutex::new(answers.to_vec()),
            asked: std::sync::Mutex::new(Vec::new()),
            watched: self.layer().dir.join("plans/report.yaml"),
        })
    }

    async fn cross(&self, consent: Consent, names: &[&str]) -> Outcome {
        let mut config = dive_config::Config::default();
        config.storage.backend = dive_config::StorageBackend::Memory;
        let majors = Majors {
            consent: &consent,
            config: &config,
            events: Arc::new(dive_core::NullSink),
        };
        let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        major::update(self.layer(), &names, false, &majors)
            .await
            .unwrap()
    }
}

fn installed_at(third: Third, reference: &str) -> Installed {
    let upstream = majors_upstream(third);
    let project = scratch(&format!(
        "{CONFIG}\n[packs.example]\ngit = \"{}\"\nref = \"{reference}\"   # the line\n",
        upstream.url
    ));
    let layer = &project.layer;
    let added = add(layer, &request("example", None, None)).unwrap();
    assert!(!added.rejected, "{}", added.body);
    write(&layer.dir, "plans/report.yaml", USER_PLAN);
    Installed {
        project,
        _upstream: upstream,
    }
}

fn yes() -> Consent {
    Consent {
        yes: true,
        asker: None,
    }
}

fn asking(asker: &Arc<Asker>) -> Consent {
    Consent {
        yes: false,
        asker: Some(asker.clone() as Arc<dyn Interlocutor>),
    }
}

fn assert_at(installed: &Installed, tag: &str, reference: &str) {
    let layer = installed.layer();
    let locked = &lock(layer).packs["example"];
    assert_eq!(locked.tag.as_deref(), Some(tag));
    assert_eq!(
        locked.hash,
        content_hash(&layer.copy_dir("example")).unwrap()
    );
    assert!(
        config(layer).contains(&format!("ref = \"{reference}\"   # the line")),
        "{}",
        config(layer)
    );
    assert!(staging_left(layer).is_empty(), "{:?}", staging_left(layer));
}

#[tokio::test(flavor = "multi_thread")]
async fn major_with_yes_crosses_each_major_in_turn_running_its_migration() {
    let installed = installed_at(Third::Migration, "v1");
    let outcome = installed.cross(yes(), &[]).await;
    assert!(!outcome.rejected, "{}", outcome.body);
    let row = &outcome.body["packs"][0];
    assert_eq!(row["status"], "updated");
    assert_eq!(
        (row["from"].as_str(), row["to"].as_str()),
        (Some("v1.1.0"), Some("v3.0.0"))
    );
    let crossed = row["crossed"].as_array().unwrap();
    assert_eq!(crossed[0]["migration"], "migrate_v2");
    assert_eq!(crossed[1]["migration"], "migrate_v3");
    assert_eq!(crossed[0]["files"][0]["path"], "plans/report.yaml");
    assert_eq!(crossed[1]["files"][0]["applied"], true);
    assert!(
        installed.user_plan().contains("example__tool__file_meta"),
        "{}",
        installed.user_plan()
    );
    assert!(!installed.user_plan().contains("file_info"));
    assert_at(&installed, "v3.0.0", "v3");
    assert!(installed
        .layer()
        .copy_dir("example")
        .join("tools/file_meta.yaml")
        .is_file());
    let note = outcome.body["note"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        note.contains(&format!(
            "v2.0.0 -> v3.0.0: migrate_v3 rewrote 1 file(s): {}",
            installed.layer().dir.join("plans/report.yaml").display()
        )),
        "{note}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_migration_is_shown_as_a_dry_run_before_it_is_applied() {
    let installed = installed_at(Third::Migration, "v1");
    let asker = installed.asker(&[true, true]);
    let outcome = installed.cross(asking(&asker), &["example"]).await;
    assert!(!outcome.rejected, "{}", outcome.body);
    let asked = asker.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2);
    assert!(
        asked[0]
            .0
            .contains("example v1.1.0 -> v2.0.0: migrate_v2 proposes"),
        "{}",
        asked[0].0
    );
    assert!(
        asked[0]
            .0
            .contains("-example__tool__file_stats +example__tool__file_info"),
        "{}",
        asked[0].0
    );
    assert!(asked[0].1.contains("example__tool__file_stats"));
    assert!(
        asked[1]
            .0
            .contains("-example__tool__file_info +example__tool__file_meta"),
        "{}",
        asked[1].0
    );
    assert!(asked[1].1.contains("example__tool__file_info"));
    assert_at(&installed, "v3.0.0", "v3");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_major_without_a_migration_shows_its_migrating_section_and_asks() {
    let installed = installed_at(Third::Guide, "v1");
    let asker = installed.asker(&[true, true]);
    let outcome = installed.cross(asking(&asker), &[]).await;
    assert!(!outcome.rejected, "{}", outcome.body);
    let asked = asker.asked.lock().unwrap().clone();
    assert!(
        asked[1]
            .0
            .contains("has no plans/migrations/migrate_v3.yaml"),
        "{}",
        asked[1].0
    );
    assert!(
        asked[1].0.contains(
            "## v2 to v3:\n\n- `tool__file_info` is renamed `tool__file_meta`.\n\nInstall"
        ),
        "{}",
        asked[1].0
    );
    assert!(!asked[1].0.contains("Later."));
    let row = &outcome.body["packs"][0];
    assert_eq!(row["crossed"][1]["migration"], Value::Null);
    assert_eq!(
        row["crossed"][1]["guide"],
        "- `tool__file_info` is renamed `tool__file_meta`."
    );
    assert!(installed.user_plan().contains("example__tool__file_info"));
    assert_at(&installed, "v3.0.0", "v3");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_major_with_no_migration_and_no_section_says_so() {
    let installed = installed_at(Third::Bare, "v1");
    let asker = installed.asker(&[true, true]);
    let outcome = installed.cross(asking(&asker), &[]).await;
    assert!(!outcome.rejected, "{}", outcome.body);
    let asked = asker.asked.lock().unwrap().clone();
    assert!(
        asked[1]
            .0
            .contains("its MIGRATING.md has no ## v2 to v3 section"),
        "{}",
        asked[1].0
    );
    let note = outcome.body["note"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        note.contains("MIGRATING.md has no section for this major"),
        "{note}"
    );
    assert_at(&installed, "v3.0.0", "v3");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_terminal_or_yes_nothing_changes() {
    let installed = installed_at(Third::Migration, "v1");
    let config_before = config(installed.layer());
    let lock_before = std::fs::read_to_string(installed.layer().lock_path()).unwrap();
    let outcome = installed
        .cross(
            Consent {
                yes: false,
                asker: None,
            },
            &[],
        )
        .await;
    assert!(error(&outcome).contains("there is no terminal to ask on; nothing was changed"));
    assert_eq!(config(installed.layer()), config_before);
    assert_eq!(
        std::fs::read_to_string(installed.layer().lock_path()).unwrap(),
        lock_before
    );
    assert_eq!(installed.user_plan(), USER_PLAN);
    assert!(installed
        .layer()
        .copy_dir("example")
        .join("tools/file_stats.yaml")
        .is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_migration_leaves_the_pack_at_the_last_completed_major() {
    let installed = installed_at(Third::Failing, "v1");
    let outcome = installed.cross(yes(), &[]).await;
    let body = error(&outcome);
    let row = &outcome.body["packs"][0];
    assert_eq!(row["status"], "refused");
    assert_eq!(row["stoppedAt"], "v2.0.0");
    assert_eq!(row["left"], json!(["v3.0.0"]));
    assert!(body.contains("migrate_v3 failed"), "{body}");
    assert!(
        body.contains("continue with `dive pack update example --major`"),
        "{body}"
    );
    assert!(
        installed.user_plan().contains("example__tool__file_info"),
        "{}",
        installed.user_plan()
    );
    assert!(installed
        .layer()
        .copy_dir("example")
        .join("tools/file_info.yaml")
        .is_file());
    assert_at(&installed, "v2.0.0", "v2");
}

#[tokio::test(flavor = "multi_thread")]
async fn declining_stops_cleanly_at_the_last_completed_major() {
    let installed = installed_at(Third::Migration, "v1");
    let asker = installed.asker(&[true, false]);
    let outcome = installed.cross(asking(&asker), &[]).await;
    assert!(!outcome.rejected, "{}", outcome.body);
    let row = &outcome.body["packs"][0];
    assert_eq!(row["status"], "declined");
    assert_eq!(row["stoppedAt"], "v2.0.0");
    assert!(
        row["summary"].as_str().unwrap().contains("v3.0.0 declined"),
        "{row}"
    );
    assert!(installed.user_plan().contains("example__tool__file_info"));
    assert_at(&installed, "v2.0.0", "v2");
}

#[tokio::test(flavor = "multi_thread")]
async fn major_skips_a_pinned_tag_and_names_a_branch_as_having_no_majors() {
    let pinned = installed_at(Third::Migration, "v1.0.0");
    let outcome = pinned.cross(yes(), &[]).await;
    assert_eq!(outcome.body["packs"][0]["status"], "pinned");
    assert_eq!(pinned.user_plan(), USER_PLAN);

    let branch = installed_at(Third::Migration, "main");
    let outcome = branch.cross(yes(), &[]).await;
    let row = &outcome.body["packs"][0];
    assert_eq!(row["status"], "branch");
    assert!(
        row["summary"]
            .as_str()
            .unwrap()
            .contains("tracks branch 'main', which has no majors to cross"),
        "{row}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn major_on_the_newest_major_updates_within_it() {
    let installed = installed_at(Third::Migration, "v3");
    let outcome = installed.cross(yes(), &[]).await;
    assert_eq!(outcome.body["packs"][0]["status"], "current");
}

struct PacksRepo {
    _dir: TempDir,
    work: PathBuf,
    url: String,
}

impl PacksRepo {
    fn describe(&self, pack: &str, description: &str) {
        write(
            &self.work,
            &format!("{pack}/pack.toml"),
            &format!("version = 1\nname = \"{pack}\"\ndescription = \"{description}\"\n"),
        );
    }

    fn commit(&self, message: &str) {
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "-q", "-m", message]);
    }
}

fn packs_repo() -> PacksRepo {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"]);
    let repo = PacksRepo {
        url: format!("file://{}", work.display()),
        work,
        _dir: dir,
    };
    for pack in ["a", "b"] {
        write(&repo.work, &format!("{pack}/tools/echo.yaml"), TOOL);
        write(&repo.work, &format!("{pack}/plans/whoami.yaml"), PLAN);
        repo.describe(pack, "one");
    }
    write(&repo.work, "README.md", "# packs\n");
    repo.commit("one");
    git(&repo.work, &["tag", "a/v1.0.0"]);
    git(&repo.work, &["tag", "b/v1.0.0"]);
    repo.describe("a", "one point one");
    repo.commit("a one point one");
    git(&repo.work, &["tag", "-a", "a/v1.1.0", "-m", "a/v1.1.0"]);
    repo.describe("b", "two");
    repo.commit("b two");
    git(&repo.work, &["tag", "b/v2.0.0"]);
    git(&repo.work, &["tag", "v9.0.0"]);
    repo
}

fn request_at(source: &str, path: &str, reference: Option<&str>) -> AddRequest {
    AddRequest {
        path: Some(path.to_string()),
        ..request(source, reference, None)
    }
}

fn description(layer: &Layer, name: &str) -> String {
    std::fs::read_to_string(layer.copy_dir(name).join("pack.toml")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn packs_in_one_repository_each_follow_their_own_tags() {
    let repo = packs_repo();
    let project = scratch(&format!(
        "{CONFIG}\n[packs.b]\ngit = \"{}\"\npath = \"b\"\nref = \"v1\"\n",
        repo.url
    ));
    let layer = &project.layer;

    let added = add(layer, &request_at(&repo.url, "a", None)).unwrap();
    assert!(!added.rejected, "{}", added.body);
    assert_eq!(added.body["tag"], "a/v1.1.0");
    assert_eq!(added.body["ref"], "v1");
    let note = added.body["note"].as_str().unwrap();
    assert!(
        note.starts_with(&format!("installed a v1.1.0 from a/ in {}", repo.url)),
        "{note}"
    );
    assert!(
        config(layer).contains(&format!(
            "[packs.a]\ngit = \"{}\"\npath = \"a\"\nref = \"v1\"\n",
            repo.url
        )),
        "{}",
        config(layer)
    );
    assert!(description(layer, "a").contains("one point one"));
    assert!(!layer.copy_dir("a").join("a").exists());
    assert!(!layer.copy_dir("a").join("README.md").exists());

    let added = add(layer, &request("b", None, None)).unwrap();
    assert!(!added.rejected, "{}", added.body);
    assert_eq!(added.body["tag"], "b/v1.0.0");
    let locked = lock(layer);
    assert_eq!(
        (
            locked.packs["a"].path.as_str(),
            locked.packs["a"].tag.as_deref()
        ),
        ("a", Some("a/v1.1.0"))
    );
    assert_eq!(
        (
            locked.packs["b"].path.as_str(),
            locked.packs["b"].tag.as_deref()
        ),
        ("b", Some("b/v1.0.0"))
    );
    let installed = load(&[layer]).unwrap();
    let described: Vec<String> = installed
        .packs
        .iter()
        .map(|pack| pack.loaded.source.describe())
        .collect();
    assert_eq!(
        described,
        ["pack a v1.1.0 (project)", "pack b v1.0.0 (project)"]
    );

    let listed = list(std::slice::from_ref(layer), false).unwrap();
    let rows = listed.body["packs"].as_array().unwrap();
    assert_eq!(
        (&rows[0]["tag"], &rows[0]["newestInRef"], &rows[0]["newest"]),
        (&json!("v1.1.0"), &json!("v1.1.0"), &json!("v1.1.0"))
    );
    assert_eq!(
        (&rows[1]["tag"], &rows[1]["newestInRef"], &rows[1]["newest"]),
        (&json!("v1.0.0"), &json!("v1.0.0"), &json!("v2.0.0"))
    );

    let outcome = update(layer, &[], false).unwrap();
    let rows = outcome.body["packs"].as_array().unwrap();
    assert_eq!(rows[0]["status"], "current");
    assert_eq!(rows[1]["status"], "current");
    assert_eq!(
        rows[1]["heldBack"],
        "v2.0.0 available: dive pack update b --major"
    );

    repo.describe("a", "one point two");
    repo.commit("a one point two");
    git(&repo.work, &["tag", "a/v1.2.0"]);
    let outcome = update(layer, &[], false).unwrap();
    let rows = outcome.body["packs"].as_array().unwrap();
    assert_eq!(rows[0]["status"], "updated", "{}", outcome.body);
    assert_eq!(
        (rows[0]["from"].as_str(), rows[0]["to"].as_str()),
        (Some("v1.1.0"), Some("v1.2.0"))
    );
    assert_eq!(rows[1]["status"], "current");
    assert_eq!(lock(layer).packs["a"].tag.as_deref(), Some("a/v1.2.0"));
    assert!(description(layer, "a").contains("one point two"));

    let mut settings = dive_config::Config::default();
    settings.storage.backend = dive_config::StorageBackend::Memory;
    let consent = yes();
    let majors = Majors {
        consent: &consent,
        config: &settings,
        events: Arc::new(dive_core::NullSink),
    };
    let outcome = major::update(layer, &[], false, &majors).await.unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    let rows = outcome.body["packs"].as_array().unwrap();
    assert_eq!(rows[0]["status"], "current", "{}", outcome.body);
    assert_eq!(rows[1]["status"], "updated", "{}", outcome.body);
    assert_eq!(
        (rows[1]["from"].as_str(), rows[1]["to"].as_str()),
        (Some("v1.0.0"), Some("v2.0.0"))
    );
    assert_eq!(lock(layer).packs["b"].tag.as_deref(), Some("b/v2.0.0"));
    assert!(description(layer, "b").contains("two"));
    assert!(config(layer).contains("path = \"b\"\nref = \"v2\"\n"));
    assert!(staging_left(layer).is_empty());
}

#[test]
fn a_path_without_a_pack_or_outside_the_repository_is_refused() {
    let repo = packs_repo();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    let missing = add(layer, &request_at(&repo.url, "c", Some("main"))).unwrap();
    let text = error(&missing);
    assert!(text.contains("at main has no directory c"), "{text}");
    let bare = add(layer, &request_at(&repo.url, "", Some("main"))).unwrap();
    let text = error(&bare);
    assert!(text.contains("has no pack.toml at its root"), "{text}");
    for path in ["../a", "/a", "a/", "a//b", "./a"] {
        let outcome = add(layer, &request_at(&repo.url, path, None)).unwrap();
        assert!(error(&outcome).contains("path"), "{path}");
    }
    let releases = add(layer, &request_at(&repo.url, "a", Some("v2"))).unwrap();
    let text = error(&releases);
    assert!(
        text.contains("a/ in file://")
            && text.contains("has no v2.*.* release tag (release tags: v1.0.0, v1.1.0)"),
        "{text}"
    );
    assert!(!layer.copy_dir("a").exists());
    assert!(staging_left(layer).is_empty());
}

#[test]
fn a_branch_tracking_pack_moves_only_when_its_own_directory_changes() {
    let repo = packs_repo();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    let added = add(layer, &request_at(&repo.url, "a", Some("main"))).unwrap();
    assert!(!added.rejected, "{}", added.body);
    let sha = lock(layer).packs["a"].sha.clone();

    repo.describe("b", "elsewhere");
    repo.commit("b only");
    let outcome = update(layer, &[], false).unwrap();
    assert_eq!(
        outcome.body["packs"][0]["status"], "current",
        "{}",
        outcome.body
    );
    assert_eq!(lock(layer).packs["a"].sha, sha);
    assert!(staging_left(layer).is_empty());

    repo.describe("a", "moved");
    repo.commit("a moved");
    let outcome = update(layer, &[], false).unwrap();
    assert_eq!(
        outcome.body["packs"][0]["status"], "updated",
        "{}",
        outcome.body
    );
    assert_eq!(
        lock(layer).packs["a"].sha,
        git(&repo.work, &["rev-parse", "HEAD"])
    );
    assert!(description(layer, "a").contains("moved"));
}

#[test]
fn a_pack_at_the_repository_root_ignores_tags_under_directories() {
    let upstream = upstream();
    git(&upstream.work, &["tag", "sub/v7.0.0"]);
    let project = scratch(CONFIG);
    let added = add(&project.layer, &request(&upstream.url, None, None)).unwrap();
    assert!(!added.rejected, "{}", added.body);
    assert_eq!(added.body["tag"], "v2.0.0");
    assert_eq!(added.body["path"], "");
    assert!(!config(&project.layer).contains("path ="));
    assert_eq!(lock(&project.layer).packs["example"].path, "");
}

fn moved_b(repo: &PacksRepo) -> String {
    repo.describe("b", "tampered");
    repo.commit("b tampered");
    git(&repo.work, &["tag", "-f", "b/v1.0.0"]);
    git(&repo.work, &["rev-parse", "HEAD"])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_release_tag_that_moved_since_install_is_refused() {
    let repo = packs_repo();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    for path in ["a", "b"] {
        let added = add(layer, &request_at(&repo.url, path, Some("v1"))).unwrap();
        assert!(!added.rejected, "{}", added.body);
    }
    let installed = lock(layer).packs["b"].sha.clone();
    git(&repo.work, &["tag", "-f", "a/v1.0.0", "HEAD"]);
    let moved = moved_b(&repo);
    assert_ne!(moved, installed);
    let config_before = config(layer);
    let lock_before = lock(layer);
    let description_before = description(layer, "b");
    let unchanged = || {
        assert_eq!(config(layer), config_before);
        assert_eq!(lock(layer), lock_before);
        assert_eq!(description(layer, "b"), description_before);
        assert!(staging_left(layer).is_empty());
    };
    let expected = format!(
        "the tag b/v1.0.0 in {} now points at {moved} but was installed from {installed}; a published release must never move, so Dive won't install it; contact the pack's maintainers",
        repo.url
    );

    let outcome = update(layer, &[], false).unwrap();
    let text = error(&outcome);
    let rows = outcome.body["packs"].as_array().unwrap();
    assert_eq!(rows[0]["status"], "current", "{text}");
    assert_eq!(rows[1]["status"], "refused", "{text}");
    assert_eq!(rows[1]["summary"], format!("b: {expected}"));
    unchanged();

    let mut settings = dive_config::Config::default();
    settings.storage.backend = dive_config::StorageBackend::Memory;
    let consent = yes();
    let majors = Majors {
        consent: &consent,
        config: &settings,
        events: Arc::new(dive_core::NullSink),
    };
    let crossed = major::update(layer, &["b".to_string()], false, &majors)
        .await
        .unwrap();
    assert!(error(&crossed).contains(&expected));
    unchanged();

    let listed = list(std::slice::from_ref(layer), false).unwrap();
    assert!(!listed.rejected, "{}", listed.body);
    let rows = listed.body["packs"].as_array().unwrap();
    assert_eq!(rows[0]["tagMoved"], Value::Null);
    assert_eq!(rows[1]["tagMoved"], expected);
    assert!(listed
        .raw
        .as_deref()
        .unwrap()
        .contains("release tag moved since install"));

    let again = add(layer, &request_at(&repo.url, "b", Some("v1"))).unwrap();
    assert!(error(&again).contains(&expected));
    let renamed = add(
        layer,
        &AddRequest {
            as_name: Some("other".to_string()),
            ..request_at(&repo.url, "b", Some("v1"))
        },
    )
    .unwrap();
    assert!(error(&renamed).contains(&expected));
    unchanged();
    std::fs::remove_dir_all(layer.copy_dir("b")).unwrap();
    let reinstall = add(layer, &request("b", None, None)).unwrap();
    assert!(error(&reinstall).contains(&expected));
    assert!(!layer.copy_dir("b").exists());
    assert!(!layer.copy_dir("other").exists());
    assert_eq!(config(layer), config_before);
    assert_eq!(lock(layer), lock_before);
}

#[test]
fn a_moved_tag_the_lock_never_recorded_is_not_refused() {
    let repo = packs_repo();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    let added = add(layer, &request_at(&repo.url, "a", Some("v1"))).unwrap();
    assert!(!added.rejected, "{}", added.body);
    repo.describe("a", "one point two");
    repo.commit("a one point two");
    git(&repo.work, &["tag", "a/v1.2.0"]);
    repo.describe("a", "one point two again");
    repo.commit("a one point two again");
    git(&repo.work, &["tag", "-f", "a/v1.2.0"]);
    git(&repo.work, &["tag", "-f", "a/v1.0.0"]);
    let outcome = update(layer, &[], false).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert_eq!(outcome.body["packs"][0]["status"], "updated");
    assert_eq!(lock(layer).packs["a"].tag.as_deref(), Some("a/v1.2.0"));
    assert!(description(layer, "a").contains("one point two again"));
}

#[test]
fn a_branch_tracking_pack_whose_branch_is_rewritten_still_updates() {
    let repo = packs_repo();
    let project = scratch(CONFIG);
    let layer = &project.layer;
    repo.describe("a", "ahead");
    repo.commit("a ahead");
    let added = add(layer, &request_at(&repo.url, "a", Some("main"))).unwrap();
    assert!(!added.rejected, "{}", added.body);
    git(&repo.work, &["reset", "-q", "--hard", "HEAD~1"]);
    repo.describe("a", "rewritten");
    repo.commit("a rewritten");

    let listed = list(std::slice::from_ref(layer), false).unwrap();
    assert_eq!(listed.body["packs"][0]["tagMoved"], Value::Null);
    let outcome = update(layer, &[], false).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert_eq!(outcome.body["packs"][0]["status"], "updated");
    assert_eq!(
        lock(layer).packs["a"].sha,
        git(&repo.work, &["rev-parse", "HEAD"])
    );
    assert!(description(layer, "a").contains("rewritten"));
}
