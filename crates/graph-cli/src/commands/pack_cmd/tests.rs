use super::*;
use crate::packs::InstalledPacks;
use graph_config::pack::{load_lock, PackLock};
use std::collections::BTreeSet;
use std::process::Command;
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
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
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
    let layer = layer_in(LayerKind::Project, dir.path().join(".graph"), config);
    Project { _dir: dir, layer }
}

fn request(source: &str, reference: Option<&str>, as_name: Option<&str>) -> AddRequest {
    AddRequest {
        source: source.to_string(),
        as_name: as_name.map(str::to_string),
        reference: reference.map(str::to_string),
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
    let loaded = graph_config::load_from(&paths)?;
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
    let error = project_layer(dir.path().join(".graph"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("graph config init"), "{error}");
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
        "v2.0.0 available: graph pack update example --major"
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
            "requires.graph: needs graph >=99; this is graph {}",
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

const UNMET: &str = r#"binaries = ["graph-pack-no-such-binary"]
env = ["GRAPH_PACK_TEST_NEVER_SET"]

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
            "requires.binaries: graph-pack-no-such-binary not found on PATH",
            "requires.env: GRAPH_PACK_TEST_NEVER_SET not set (or empty)",
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
            "example: unmet requires.binaries: graph-pack-no-such-binary not found on PATH"
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
    upstream.release_requiring("binaries = [\"graph-pack-no-such-binary\"]", "v1.2.0");
    let outcome = update(layer, &[], false).unwrap();
    assert!(!outcome.rejected, "{}", outcome.body);
    assert_eq!(
        outcome.body["packs"][0]["unmet"],
        json!(["requires.binaries: graph-pack-no-such-binary not found on PATH"])
    );
    assert_eq!(lock(layer).packs["example"].tag.as_deref(), Some("v1.2.0"));
}
