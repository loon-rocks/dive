mod support;

use support::{Scratch, ECHO_PLAN};

const PACK_PLAN: &str = r#"version: 2
identifier: shape
name: Shape
description: Reshapes a word
steps:
  - id: E1
    tool_name: builtin__reshape
    input:
      shape:
        said: from the pack
finish:
  output:
    said: "{{E1.said}}"
"#;

const WRAP_PLAN: &str = r#"
identifier: wrap
name: Wrap
description: Calls the pack plan after a first step
steps:
  - id: E0
    tool_name: builtin__reshape
    input:
      shape:
        first: true
  - id: E1
    tool_name: example__plan__shape
    input: {}
output:
  said: "{{E1.said}}"
"#;

fn with_pack(requires: &str, extra_config: &str) -> Scratch {
    let scratch = Scratch::new();
    scratch.write_config(&format!(
        "version = 4\n\n[packs.example]\ngit = \"https://example.com/dive-pack-example.git\"\nref = \"v1\"\n{extra_config}"
    ));
    let pack = scratch.path().join(".dive/packs/example");
    std::fs::create_dir_all(pack.join("plans")).unwrap();
    std::fs::write(
        pack.join("pack.toml"),
        format!("version = 1\nname = \"example\"\ndescription = \"Example\"\n\n[requires]\n{requires}\n"),
    )
    .unwrap();
    std::fs::write(pack.join("plans/shape.yaml"), PACK_PLAN).unwrap();
    std::fs::write(
        scratch.path().join(".dive/packs.lock"),
        format!(
            "version = 1\n\n[packs.example]\ngit = \"https://example.com/dive-pack-example.git\"\ntag = \"v1.0.0\"\nsha = \"{}\"\nhash = \"sha256:{}\"\n",
            "0".repeat(40),
            "0".repeat(64)
        ),
    )
    .unwrap();
    scratch.write_plan("wrap", WRAP_PLAN);
    scratch.write_plan("echo_ok", ECHO_PLAN);
    scratch
}

const MISSING: &str =
    "binaries = [\"dive-pack-no-such-binary\"]\nenv = [\"DIVE_PACK_TEST_NEVER_SET\"]";

#[test]
fn config_check_reports_unmet_binaries_and_env_as_warnings() {
    let scratch = with_pack(MISSING, "");
    let run = scratch.dive(&["config", "check", "--json"]);
    run.code_is(0);
    let body = run.json();
    assert_eq!(
        body["packs"][0]["unmet"],
        serde_json::json!([
            "requires.binaries: dive-pack-no-such-binary not found on PATH",
            "requires.env: DIVE_PACK_TEST_NEVER_SET not set (or empty)"
        ])
    );
    let warnings = body["warnings"].to_string();
    assert!(
        warnings.contains("pack 'example' has unmet requirements"),
        "{warnings}"
    );

    scratch
        .dive(&["config", "check"])
        .code_is(0)
        .stdout_contains("  unmet requires.binaries: dive-pack-no-such-binary not found on PATH");
}

#[test]
fn a_run_reaching_the_pack_fails_before_its_first_step_and_one_that_cannot_runs() {
    let scratch = with_pack(MISSING, "");
    let run = scratch.dive_env(&["plan", "run", "wrap"], &[("DIVE_EVENTS", "jsonl")]);
    run.code_is(1)
        .stderr_contains("pack 'example' has unmet requirements")
        .stderr_contains("dive-pack-no-such-binary");
    assert!(
        !run.stderr.contains("\"step_started\""),
        "no step may start: {}",
        run.stderr
    );

    scratch
        .dive_env(
            &["plan", "run", "wrap"],
            &[("DIVE_PACK_TEST_NEVER_SET", "set")],
        )
        .code_is(1)
        .stderr_contains("dive-pack-no-such-binary");

    let plain = scratch.dive(&["plan", "run", "echo_ok", r#"{"word":"hi"}"#]);
    plain.code_is(0);
    assert_eq!(plain.json(), serde_json::json!({"said": "hi"}));
}

#[test]
fn a_pack_needing_a_newer_graph_refuses_to_load_by_name() {
    let scratch = with_pack("graph = \">=99\"", "");
    scratch
        .dive(&["plan", "run", "echo_ok", r#"{"word":"hi"}"#])
        .code_is(1)
        .stderr_contains("pack 'example': requires.dive: needs Dive >=99");
    scratch
        .dive(&["config", "check"])
        .code_is(1)
        .stderr_contains("requires.dive: needs Dive >=99");
}

#[test]
fn an_mcp_requirement_without_a_server_or_with_one_that_cannot_connect_is_unmet() {
    let requires = "[requires.mcp.everything]\nserver = \"mcp-servers/everything\"\nurl = \"https://example.com/mcp\"";
    let scratch = with_pack(requires, "");
    let body = scratch.dive(&["config", "check", "--json"]).json();
    assert_eq!(
        body["packs"][0]["unmet"],
        serde_json::json!([
            "requires.mcp.everything: no MCP server is configured under [mcp.everything] (url = \"https://example.com/mcp\")"
        ])
    );
    scratch
        .dive(&["plan", "run", "wrap"])
        .code_is(1)
        .stderr_contains("no MCP server is configured under [mcp.everything]");

    let scratch = with_pack(
        requires,
        "\n[mcp.everything]\ncommand = \"dive-pack-no-such-server\"\n",
    );
    let body = scratch.dive(&["config", "check", "--json"]).json();
    let unmet = body["packs"][0]["unmet"].to_string();
    assert!(
        unmet.contains("requires.mcp.everything: [mcp.everything] did not connect"),
        "{unmet}"
    );
}
