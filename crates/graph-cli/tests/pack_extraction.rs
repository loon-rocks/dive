mod support;

use support::Scratch;

fn write_global_config(scratch: &Scratch, text: &str) {
    let dir = scratch.path().join(".config/graph");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

#[test]
fn infer_and_reshape_are_served_without_any_pack_config() {
    let scratch = Scratch::new();
    let run = scratch.graph(&["tools", "list", "--json"]);
    run.code_is(0);
    let names: Vec<String> = run.json()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .filter(|name| name.starts_with("builtin__"))
        .collect();
    for name in ["builtin__infer", "builtin__reshape"] {
        assert!(names.contains(&name.to_string()), "{names:?}");
    }
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("builtin__gh_") || name.starts_with("builtin__git_")),
        "{names:?}"
    );
}

#[test]
fn a_version_3_global_pack_list_warns_naming_the_install_command() {
    let scratch = Scratch::new();
    write_global_config(&scratch, "version = 3\n\n[tools]\npacks = [\"github\"]\n");
    let run = scratch.graph(&["tools", "list"]);
    run.code_is(0)
        .stderr_contains("pack 'github' is configured in")
        .stderr_contains("but not installed at")
        .stderr_contains("run `graph pack add github --global`");
}

#[test]
fn config_check_notes_the_converted_pack_list() {
    let scratch = Scratch::new();
    scratch.write_config("version = 3\n\n[tools]\npacks = [\"github\", \"llm\"]\n");
    let run = scratch.graph(&["config", "check", "--json"]);
    run.code_is(0);
    let body = run.json();
    assert_eq!(
        body["warnings"].as_array().unwrap()[..2],
        serde_json::json!([
            "./.graph/config.toml: [tools].packs \"github\" became [packs.github]: its tools are now github__tool__<name> from a pack; install it with `graph pack add github`",
            "./.graph/config.toml: dropped \"llm\" from [tools].packs: its tools are always built in"
        ])
        .as_array()
        .unwrap()[..]
    );
    assert!(
        body["warnings"][2]
            .as_str()
            .unwrap()
            .contains("not installed at ./.graph/packs/github, so its tools, plans and agents are unavailable; run `graph pack add github`"),
        "{body}"
    );
    assert_eq!(body["problems"], serde_json::json!([]), "{body}");
}

#[test]
fn a_version_4_pack_list_is_refused_naming_the_replacement() {
    let scratch = Scratch::new();
    scratch.write_config("version = 4\n\n[tools]\npacks = [\"github\"]\n");
    scratch
        .graph(&["tools", "list"])
        .code_is(1)
        .stderr_contains("[tools].packs was replaced by [packs.<name>] entries; remove it, then install with `graph pack add github`");
}
