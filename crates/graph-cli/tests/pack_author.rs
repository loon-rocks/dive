mod support;

use serde_json::Value;
use std::path::Path;
use std::process::Command;
use support::{Run, Scratch};

const MANIFEST: &str = "version = 1\nname = \"example\"\ndescription = \"Example\"\n\n[requires]\nos = [\"macos\", \"linux\"]\n";

const ECHO_TOOL: &str = r#"version: 2
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
output_schema:
  type: object
  properties:
    name: { type: string }
"#;

const GREET_PLAN: &str = r#"version: 2
identifier: greet
name: Greet
description: Echoes a name through the pack's tool
input_schema:
  type: object
  required: [name]
  properties:
    name: { type: string }
steps:
  - id: a
    tool_name: $pack__tool__echo
    input:
      name: "{{input.name}}"
finish:
  output:
    name: "{{a.name}}"
"#;

const FILE_INFO_TOOL: &str = r#"version: 2
name: file_info
description: Echoes a file path
kind: exec
command: echo
args: ['{"file": "{{input.file}}"}']
input_schema:
  type: object
  required: [file]
  properties:
    file: { type: string }
"#;

const SAMPLE_PLAN: &str = r#"version: 2
identifier: report
name: Report
description: Calls the v1 tool name
steps:
  - id: a
    tool_name: example__tool__file_stats
    input:
      file: README.md
finish:
  output:
    file: "{{a.file}}"
"#;

const SAMPLE_AGENT: &str = r#"version: 1
name: helper
description: Uses the v1 tool name
model: chat
tools: [example__tool__file_stats]
system_prompt: Use example__tool__file_stats.
"#;

const MIGRATE_PLAN: &str = r#"version: 2
identifier: migrate_v2
name: Migrate to v2
description: Renames file_stats to file_info in the user's files
input_schema:
  type: object
  required: [root]
  properties:
    root: { type: string }
    apply: { type: boolean, default: false }
steps:
  - id: rewrite
    tool_name: $pack__tool__migrate_v2_rewrite
    input:
      root: "{{input.root}}"
      pack: "{{pack.name}}"
finish:
  output:
    applied: "{{input.apply}}"
    files: "{{rewrite.files}}"
"#;

fn rewrite_tool(from: &str) -> String {
    format!(
        r#"version: 2
name: migrate_v2_rewrite
description: Rewrites the v1 tool name in place
kind: exec
command: sh
args:
  - "-c"
  - |-
    cd "$1"
    for f in plans/*.yaml agents/*.yaml; do
      [ -f "$f" ] || continue
      sed -i.bak "s/$2__tool__{from}/$2__tool__file_info/g" "$f"
      rm -f "$f.bak"
    done
    printf '{{"files": 1}}'
  - rewrite
  - "{{{{input.root}}}}"
  - "{{{{input.pack}}}}"
input_schema:
  type: object
  required: [root, pack]
  properties:
    root: {{ type: string }}
    pack: {{ type: string }}
"#
    )
}

fn write(dir: &Path, path: &str, content: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn git_env() -> [(&'static str, &'static str); 6] {
    [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@example.com"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@example.com"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ]
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .envs(git_env())
        .args(["-c", "init.defaultBranch=main"])
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

fn commit(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

fn graph(scratch: &Scratch, args: &[&str]) -> Run {
    scratch.graph_env(args, &git_env())
}

fn problems(run: &Run) -> Vec<String> {
    run.json()["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem.as_str().unwrap().to_string())
        .collect()
}

#[track_caller]
fn has(problems: &[String], needle: &str) {
    assert!(
        problems.iter().any(|problem| problem.contains(needle)),
        "no problem mentions {needle:?}: {problems:#?}"
    );
}

#[test]
fn a_new_scaffold_passes_check_and_test_and_new_refuses_a_non_empty_directory() {
    let scratch = Scratch::new();
    graph(&scratch, &["pack", "new", "demo", "repo"]).code_is(0);
    for file in [
        "pack.toml",
        "tools/word_count.yaml",
        "plans/count_words.yaml",
        "agents/word_counter.yaml",
        "README.md",
        "CHANGELOG.md",
        "MIGRATING.md",
    ] {
        assert!(
            scratch.exists(&format!("repo/demo/{file}")),
            "missing {file}"
        );
    }
    assert!(!scratch.exists("repo/demo/.github"));
    assert!(!scratch.exists("repo/demo/.gitignore"));
    let manifest = scratch.read("repo/demo/pack.toml");
    assert!(manifest.contains("name = \"demo\""), "{manifest}");
    assert!(manifest.contains("graph = \">="), "{manifest}");
    let workflow = scratch.read("repo/.github/workflows/packs.yaml");
    assert!(workflow.contains("run: graph pack test\n"), "{workflow}");
    assert!(
        workflow.contains(&format!(
            "ghcr.io/tylerdavis/graph:v{}",
            env!("CARGO_PKG_VERSION")
        )),
        "{workflow}"
    );
    assert!(scratch.read("repo/.gitignore").contains(".graph/"));
    let readme = scratch.read("repo/demo/README.md");
    assert!(readme.contains("--path demo"), "{readme}");
    assert!(readme.contains("patch> demo`"), "{readme}");
    assert!(readme.contains("`demo/vX.Y.Z`"), "{readme}");

    std::fs::write(
        scratch.path().join("repo/.github/workflows/packs.yaml"),
        "mine\n",
    )
    .unwrap();
    graph(&scratch, &["pack", "new", "other", "repo"]).code_is(0);
    assert!(scratch.exists("repo/other/pack.toml"));
    assert_eq!(scratch.read("repo/.github/workflows/packs.yaml"), "mine\n");

    let run = graph(&scratch, &["pack", "check", "repo", "--json"]);
    run.code_is(0);
    let packs = run.json()["packs"].as_array().unwrap().clone();
    let names: Vec<&str> = packs
        .iter()
        .map(|pack| pack["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["demo", "other"]);
    graph(&scratch, &["pack", "test", "repo"]).code_is(0);

    write(
        &scratch.path().join("repo"),
        "other/plans/broken.yaml",
        "version: 2\n",
    );
    let run = graph(&scratch, &["pack", "test", "repo", "--json"]);
    run.code_is(1);
    assert!(
        run.json()["error"]
            .as_str()
            .unwrap()
            .starts_with("1 of 2 packs in repo fail"),
        "{}",
        run.json()
    );
    has(&problems(&run), "other: ");
    std::fs::remove_file(scratch.path().join("repo/other/plans/broken.yaml")).unwrap();

    graph(&scratch, &["pack", "check", "repo/.github"])
        .code_is(1)
        .stderr_contains("none of its subdirectories has one");

    graph(&scratch, &["pack", "check", "repo/demo"]).code_is(0);
    graph(&scratch, &["pack", "test", "repo/demo"]).code_is(0);
    let run = graph(
        &scratch,
        &[
            "pack",
            "run",
            "plan__count_words",
            "--input",
            "file=repo/demo/README.md",
            "--path",
            "repo/demo",
        ],
    );
    run.code_is(0);
    let output: Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(output["words"].as_i64().unwrap() > 10, "{output}");

    graph(&scratch, &["pack", "new", "other", "repo"])
        .code_is(1)
        .stderr_contains("is not empty");
    graph(&scratch, &["pack", "new", "Bad__Name", "fresh"])
        .code_is(1)
        .stderr_contains("pack name");
}

#[test]
fn check_reports_every_problem_class_at_once() {
    let scratch = Scratch::new();
    let pack = scratch.path().join("pack");
    write(
        &pack,
        "pack.toml",
        &format!("{MANIFEST}binaries = [\"graph-pack-no-such-binary\"]\n"),
    );
    write(&pack, "tools/echo.yaml", ECHO_TOOL);
    let long = "a".repeat(50);
    write(
        &pack,
        &format!("tools/{long}.yaml"),
        &ECHO_TOOL.replace("name: echo", &format!("name: {long}")),
    );
    write(
        &pack,
        "plans/literal.yaml",
        &GREET_PLAN
            .replace("identifier: greet", "identifier: literal")
            .replace("$pack__tool__echo", "example__tool__echo"),
    );
    write(
        &pack,
        "plans/unresolved.yaml",
        &GREET_PLAN
            .replace("identifier: greet", "identifier: unresolved")
            .replace("$pack__tool__echo", "$pack__tool__missing"),
    );
    write(
        &pack,
        "plans/mismatch.yaml",
        &GREET_PLAN
            .replace("identifier: greet", "identifier: mismatch")
            .replace("\"{{input.name}}\"", "3"),
    );
    write(
        &pack,
        "plans/migrations/upgrade.yaml",
        &MIGRATE_PLAN
            .replace("identifier: migrate_v2", "identifier: upgrade")
            .replace("    apply: { type: boolean, default: false }\n", ""),
    );
    write(
        &pack,
        "tools/migrations/migrate_v2_rewrite.yaml",
        &rewrite_tool("file_stats"),
    );

    let run = graph(&scratch, &["pack", "check", "pack", "--json"]);
    run.code_is(1);
    let found = problems(&run);
    has(
        &found,
        "names its own pack literally as 'example__tool__echo'; write '$pack__tool__echo'",
    );
    has(&found, "unknown tool 'example__tool__missing'");
    has(&found, "longer than 64 characters");
    has(&found, &format!("example__tool__{long} (65)"));
    has(
        &found,
        "migration plan 'upgrade' must be named migrate_v<N>",
    );
    has(
        &found,
        "migration plan 'upgrade' must take exactly the inputs {root, apply}, got {root}",
    );
    has(&found, "graph-pack-no-such-binary not found on PATH");
    has(&found, "plans/mismatch.yaml");
    has(
        &found,
        "input for example__tool__echo at /name: 3 is not of type",
    );
}

#[test]
fn check_refuses_unmet_graph_and_os_requirements_instead_of_stopping() {
    let scratch = Scratch::new();
    let pack = scratch.path().join("pack");
    write(&pack, "pack.toml", &format!("{MANIFEST}graph = \">=99\"\n"));
    write(&pack, "tools/echo.yaml", ECHO_TOOL);
    write(
        &pack,
        "plans/unresolved.yaml",
        &GREET_PLAN.replace("$pack__tool__echo", "$pack__tool__missing"),
    );
    let run = graph(&scratch, &["pack", "check", "pack", "--json"]);
    run.code_is(1);
    let found = problems(&run);
    has(&found, "requires.graph: needs graph >=99");
    has(&found, "unknown tool 'example__tool__missing'");
}

#[test]
fn check_requires_the_manifest_to_name_the_systems_the_pack_runs_on() {
    let scratch = Scratch::new();
    let pack = scratch.path().join("pack");
    write(&pack, "tools/echo.yaml", ECHO_TOOL);
    write(&pack, "plans/greet.yaml", GREET_PLAN);
    let bare = MANIFEST.replace("os = [\"macos\", \"linux\"]\n", "");
    let missing = "pack.toml: requires.os is not set; list the systems this pack runs on, such as os = [\"macos\", \"linux\"]";

    for manifest in [bare.clone(), format!("{bare}os = []\n")] {
        write(&pack, "pack.toml", &manifest);
        let run = graph(&scratch, &["pack", "check", "pack", "--json"]);
        run.code_is(1);
        assert_eq!(problems(&run), vec![missing.to_string()]);
    }

    write(&pack, "pack.toml", &format!("{bare}os = [\"plan9\"]\n"));
    let run = graph(&scratch, &["pack", "check", "pack", "--json"]);
    run.code_is(1);
    has(
        &problems(&run),
        "requires.os: 'plan9' is not a supported OS; use macos or linux",
    );

    write(&pack, "pack.toml", MANIFEST);
    graph(&scratch, &["pack", "check", "pack", "--json"]).code_is(0);
    graph(&scratch, &["pack", "test", "pack"]).code_is(0);

    write(&pack, "pack.toml", &bare);
    graph(&scratch, &["pack", "test", "pack"]).code_is(1);
}

#[test]
fn check_leaves_calls_to_an_unconfigured_required_mcp_server_unchecked_and_checks_a_configured_one()
{
    let scratch = Scratch::new();
    let pack = scratch.path().join("pack");
    write(
        &pack,
        "pack.toml",
        &format!("{MANIFEST}\n[requires.mcp.everything]\nserver = \"mcp-servers/everything\"\n"),
    );
    write(
        &pack,
        "plans/ping.yaml",
        &GREET_PLAN
            .replace("identifier: greet", "identifier: ping")
            .replace("$pack__tool__echo", "everything__echo")
            .replace("name: \"{{input.name}}\"", "message: \"{{input.name}}\""),
    );
    let run = graph(&scratch, &["pack", "check", "pack", "--json"]);
    run.code_is(0);
    let notes = run.json()["notes"].to_string();
    assert!(
        notes.contains(
            "no [mcp.everything] is configured here, so calls to everything__* were not checked"
        ),
        "{notes}"
    );

    scratch.write_config("[mcp.everything]\ncommand = \"graph-pack-no-such-server\"\n");
    let run = graph(&scratch, &["pack", "check", "pack", "--json"]);
    run.code_is(1);
    has(
        &problems(&run),
        "requires.mcp.everything: [mcp.everything] did not connect",
    );
}

fn migrating_pack(dir: &Path, rewrite_from: &str) {
    write(dir, "pack.toml", MANIFEST);
    write(dir, "tools/file_info.yaml", FILE_INFO_TOOL);
    write(dir, "plans/migrations/migrate_v2.yaml", MIGRATE_PLAN);
    write(
        dir,
        "tools/migrations/migrate_v2_rewrite.yaml",
        &rewrite_tool(rewrite_from),
    );
    write(dir, "tests/migrations/v2/plans/report.yaml", SAMPLE_PLAN);
    write(dir, "tests/migrations/v2/agents/helper.yaml", SAMPLE_AGENT);
}

#[test]
fn run_runs_a_private_migration_tool_and_a_migration_plan_under_the_manifest_name() {
    let scratch = Scratch::new();
    let pack = scratch.path().join("pack");
    migrating_pack(&pack, "file_stats");
    let root = scratch.path().join("users");
    write(&root, "plans/report.yaml", SAMPLE_PLAN);
    let root_text = root.display().to_string();

    let run = graph(
        &scratch,
        &[
            "pack",
            "run",
            "tool__migrate_v2_rewrite",
            "--input",
            &format!("root={root_text}"),
            "--input",
            "pack=example",
            "--path",
            "pack",
        ],
    );
    run.code_is(0);
    assert!(scratch
        .read("users/plans/report.yaml")
        .contains("example__tool__file_info"));

    write(&root, "plans/report.yaml", SAMPLE_PLAN);
    let run = graph(
        &scratch,
        &[
            "pack",
            "run",
            "plan__migrate_v2",
            &format!(r#"{{"root": "{root_text}", "apply": true}}"#),
            "--path",
            "pack",
            "--json",
        ],
    );
    run.code_is(0);
    let body = run.json();
    assert_eq!(body["item"], "example__plan__migrate_v2");
    assert_eq!(body["result"]["applied"], true);
    assert!(scratch
        .read("users/plans/report.yaml")
        .contains("example__tool__file_info"));

    graph(&scratch, &["pack", "run", "tool__nope", "--path", "pack"])
        .code_is(1)
        .stderr_contains("pack 'example' has no item 'tool__nope'");
}

#[test]
fn test_runs_each_migration_over_its_samples_and_catches_one_that_leaves_a_v1_name() {
    let scratch = Scratch::new();
    let good = scratch.path().join("good");
    migrating_pack(&good, "file_stats");
    let run = graph(&scratch, &["pack", "test", "good", "--json"]);
    run.code_is(0);
    assert_eq!(run.json()["cases"][0]["ran"][0], "migrate_v2");
    assert!(scratch
        .read("good/tests/migrations/v2/plans/report.yaml")
        .contains("file_stats"));

    let bad = scratch.path().join("bad");
    migrating_pack(&bad, "no_such_tool");
    let run = graph(&scratch, &["pack", "test", "bad", "--json"]);
    run.code_is(1);
    let found = problems(&run);
    has(
        &found,
        "tests/migrations/v2: plans/report.yaml: step a: unknown tool 'example__tool__file_stats'",
    );
    has(&found, "tests/migrations/v2: agents/helper.yaml: tools:");

    let orphan = scratch.path().join("orphan");
    migrating_pack(&orphan, "file_stats");
    write(
        &orphan,
        "tests/migrations/v3/plans/report.yaml",
        SAMPLE_PLAN,
    );
    let found = problems(&graph(&scratch, &["pack", "test", "orphan", "--json"]));
    has(
        &found,
        "tests/migrations/v3: there is no plans/migrations/migrate_v3.yaml",
    );
}

fn released_pack(scratch: &Scratch) -> std::path::PathBuf {
    let pack = scratch.path().join("pack");
    write(&pack, "pack.toml", MANIFEST);
    write(&pack, "tools/echo.yaml", ECHO_TOOL);
    write(&pack, "plans/greet.yaml", GREET_PLAN);
    git(&pack, &["init", "-q"]);
    commit(&pack, "first");
    pack
}

#[test]
fn release_gates_the_bump_on_the_interface_and_tags_without_pushing() {
    let scratch = Scratch::new();
    let pack = released_pack(&scratch);

    write(&pack, "notes.txt", "uncommitted");
    graph(&scratch, &["pack", "release", "major", "pack"])
        .code_is(1)
        .stderr_contains("uncommitted changes");
    std::fs::remove_file(pack.join("notes.txt")).unwrap();

    let run = graph(&scratch, &["pack", "release", "major", "pack", "--json"]);
    run.code_is(0);
    let body = run.json();
    assert_eq!(body["tag"], "v1.0.0");
    assert_eq!(body["pushed"], false);
    assert_eq!(git(&pack, &["cat-file", "-t", "v1.0.0"]), "tag");
    assert_eq!(git(&pack, &["remote"]), "");

    std::fs::remove_file(pack.join("plans/greet.yaml")).unwrap();
    commit(&pack, "drop greet");
    let run = graph(&scratch, &["pack", "release", "minor", "pack", "--json"]);
    run.code_is(1);
    has(&problems(&run), "plan__greet removed or renamed (major)");

    graph(&scratch, &["pack", "release", "major", "pack"])
        .code_is(1)
        .stderr_contains("needs plans/migrations/migrate_v2.yaml or a `## v1 to v2` section");

    write(
        &pack,
        "MIGRATING.md",
        "# Migrating\n\n## v1 to v2\n\n- `plan__greet` is gone.\n",
    );
    commit(&pack, "migrating");
    let run = graph(&scratch, &["pack", "release", "major", "pack", "--json"]);
    run.code_is(0);
    assert_eq!(run.json()["tag"], "v2.0.0");

    write(&pack, "plans/greet.yaml", GREET_PLAN);
    commit(&pack, "greet again");
    let run = graph(&scratch, &["pack", "release", "patch", "pack", "--json"]);
    run.code_is(1);
    has(&problems(&run), "plan__greet added (minor)");
    let run = graph(&scratch, &["pack", "release", "minor", "pack", "--json"]);
    run.code_is(0);
    assert_eq!(run.json()["tag"], "v2.1.0");
    assert_eq!(git(&pack, &["cat-file", "-t", "v2.1.0"]), "tag");

    let tags = git(&pack, &["tag", "--list"]);
    assert_eq!(tags, "v1.0.0\nv2.0.0\nv2.1.0");
}

#[test]
fn release_in_a_packs_repository_works_within_the_packs_own_directory_and_tags() {
    let scratch = Scratch::new();
    let repo = scratch.path().join("repo");
    for dir in ["a", "b"] {
        write(
            &repo,
            &format!("{dir}/pack.toml"),
            &MANIFEST.replace("example", dir),
        );
        write(&repo, &format!("{dir}/tools/echo.yaml"), ECHO_TOOL);
        write(&repo, &format!("{dir}/plans/greet.yaml"), GREET_PLAN);
    }
    git(&repo, &["init", "-q"]);
    commit(&repo, "first");
    git(&repo, &["tag", "a/v1.0.0"]);
    git(&repo, &["tag", "-a", "a/v1.1.0", "-m", "a v1.1.0"]);
    git(&repo, &["tag", "b/v2.0.0"]);
    git(&repo, &["tag", "v9.0.0"]);

    graph(&scratch, &["pack", "release", "patch", "repo"])
        .code_is(1)
        .stderr_contains("has no pack.toml");

    write(&repo, "b/notes.txt", "uncommitted");
    let run = graph(&scratch, &["pack", "release", "patch", "repo/a", "--json"]);
    run.code_is(0);
    assert_eq!(run.json()["tag"], "a/v1.1.1");
    assert_eq!(run.json()["previous"], "a/v1.1.0");
    assert_eq!(run.json()["version"], "v1.1.1");

    write(
        &repo,
        "a/plans/hello.yaml",
        &GREET_PLAN.replace("identifier: greet", "identifier: hello"),
    );
    git(&repo, &["add", "a"]);
    git(&repo, &["commit", "-q", "-m", "hello"]);
    let run = graph(&scratch, &["pack", "release", "patch", "repo/a", "--json"]);
    run.code_is(1);
    has(&problems(&run), "plan__hello added (minor)");
    let run = graph(&scratch, &["pack", "release", "minor", "repo/a", "--json"]);
    run.code_is(0);
    assert_eq!(run.json()["tag"], "a/v1.2.0");
    assert_eq!(run.json()["previous"], "a/v1.1.1");
    assert_eq!(git(&repo, &["cat-file", "-t", "a/v1.2.0"]), "tag");
    assert!(
        run.json()["note"]
            .as_str()
            .unwrap()
            .contains("git push origin a/v1.2.0"),
        "{}",
        run.json()
    );

    write(&repo, "a/notes.txt", "uncommitted");
    graph(&scratch, &["pack", "release", "patch", "repo/a"])
        .code_is(1)
        .stderr_contains("uncommitted changes");
    std::fs::remove_file(repo.join("a/notes.txt")).unwrap();

    let run = graph(&scratch, &["pack", "release", "major", "repo/b", "--json"]);
    run.code_is(1);
    assert!(
        run.json()["error"]
            .as_str()
            .unwrap()
            .contains("uncommitted changes"),
        "{}",
        run.json()
    );
}

#[test]
fn a_first_minor_or_patch_release_starts_below_one() {
    let scratch = Scratch::new();
    released_pack(&scratch);
    let run = graph(&scratch, &["pack", "release", "patch", "pack", "--json"]);
    run.code_is(0);
    assert_eq!(run.json()["tag"], "v0.0.1");
    let run = graph(&scratch, &["pack", "release", "minor", "pack", "--json"]);
    run.code_is(0);
    assert_eq!(run.json()["tag"], "v0.1.0");
}
