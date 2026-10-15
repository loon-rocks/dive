use super::*;
use crate::tools::{ToolDef, ToolError, ToolOutcome};
use serde_json::json;

fn machine() -> Machine {
    let bin = tempfile::tempdir().unwrap().keep();
    let jq = bin.join("jq");
    std::fs::write(&jq, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&jq, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(bin.join("readme"), "not a program").unwrap();
    Machine {
        graph: "0.17.2".to_string(),
        os: "macos".to_string(),
        path: vec![bin],
        env: BTreeMap::from([
            ("GH_TOKEN".to_string(), "secret".to_string()),
            ("EMPTY".to_string(), String::new()),
        ]),
        stored: BTreeSet::new(),
    }
}

fn requires(value: Value) -> PackRequires {
    serde_json::from_value(value).unwrap()
}

#[test]
fn a_machine_meeting_every_requirement_has_nothing_to_report() {
    let evaluation = evaluate(
        &requires(json!({
            "graph": ">=0.17",
            "os": ["macos", "linux"],
            "binaries": ["jq"],
            "env": ["GH_TOKEN"]
        })),
        &machine(),
    );
    assert_eq!(evaluation, Evaluation::default());
}

#[test]
fn a_newer_graph_or_another_os_refuses_naming_what_this_machine_has() {
    let evaluation = evaluate(
        &requires(json!({"graph": ">=99", "os": ["linux"]})),
        &machine(),
    );
    assert_eq!(
        evaluation.refused,
        [
            "requires.graph: needs graph >=99; this is graph 0.17.2",
            "requires.os: needs linux; this machine runs macos",
        ]
    );
    assert!(evaluation.unmet.is_empty());
}

#[test]
fn missing_binaries_and_unset_or_empty_env_are_unmet_not_refused() {
    let evaluation = evaluate(
        &requires(json!({
            "binaries": ["jq", "readme", "graph-pack-no-such-binary"],
            "env": ["GH_TOKEN", "EMPTY", "NEVER_SET"]
        })),
        &machine(),
    );
    assert!(evaluation.refused.is_empty());
    assert_eq!(
        evaluation.unmet,
        [
            "requires.binaries: readme, graph-pack-no-such-binary not found on PATH",
            "requires.env: EMPTY, NEVER_SET not set (or empty)",
        ]
    );
}

#[test]
fn a_required_env_name_in_the_credential_store_counts_as_met() {
    let store = crate::secrets::MemorySecrets::store()
        .with("STORED_TOKEN", "from-store")
        .with("EMPTY", "also-stored")
        .with("BLANK_IN_STORE", "");
    let names: Vec<String> = ["STORED_TOKEN", "EMPTY", "BLANK_IN_STORE", "NEVER_SET"]
        .iter()
        .map(|name| name.to_string())
        .collect();
    let stocked = machine().with_stored(&names, &store);
    let evaluation = evaluate(&requires(json!({"env": names})), &stocked);
    assert_eq!(
        evaluation.unmet,
        ["requires.env: BLANK_IN_STORE, NEVER_SET not set (or empty)"]
    );
    let none = machine().with_stored(&names, &crate::secrets::MemorySecrets::unavailable());
    assert!(none.stored.is_empty());
}

fn def(name: &str, schema: Value) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: String::new(),
        input_schema: schema,
        output_schema: None,
        output_example: None,
        read_only: None,
    }
}

fn identity() -> ServerIdentity {
    ServerIdentity {
        name: "mcp-servers/everything".to_string(),
        version: "2.0.0".to_string(),
        tools: vec![def(
            "everything__echo",
            json!({
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"]
            }),
        )],
    }
}

fn need(server: &str, version: &str, uses: Vec<McpUse>) -> McpNeed {
    McpNeed {
        requirement: McpRequirement {
            server: Some(server.to_string()),
            version: Some(version.to_string()),
            url: Some("https://example.com/mcp".to_string()),
        },
        uses,
    }
}

fn echo(input: Value) -> McpUse {
    McpUse {
        site: "plan 'demo__plan__say' step E1".to_string(),
        tool: "everything__echo".to_string(),
        input: input.as_object().cloned(),
    }
}

#[test]
fn a_matching_server_meets_the_requirement() {
    let need = need(
        "mcp-servers/everything",
        ">=2.0",
        vec![echo(json!({"message": "{{input.text}}"}))],
    );
    assert!(mcp_problems("everything", &need, Some(Ok(identity()))).is_empty());
}

#[test]
fn no_configured_server_names_the_prefix_and_the_url() {
    let need = need("x", ">=1", Vec::new());
    assert_eq!(
        mcp_problems("everything", &need, None),
        ["requires.mcp.everything: no MCP server is configured under [mcp.everything] (url = \"https://example.com/mcp\")"]
    );
}

#[test]
fn a_server_name_or_version_mismatch_is_named() {
    let need = need("linear-mcp-server", ">=3", Vec::new());
    assert_eq!(
        mcp_problems("everything", &need, Some(Ok(identity()))),
        [
            "requires.mcp.everything: [mcp.everything] is server 'mcp-servers/everything', the pack needs 'linear-mcp-server'",
            "requires.mcp.everything: [mcp.everything] reports version '2.0.0', the pack needs >=3",
        ]
    );
}

#[test]
fn a_missing_tool_or_mismatched_input_is_named_with_the_site() {
    let missing = McpUse {
        site: "agent 'demo__agent__helper' tools".to_string(),
        tool: "everything__nope".to_string(),
        input: None,
    };
    let need = need(
        "mcp-servers/everything",
        ">=2",
        vec![echo(json!({"message": 5})), missing],
    );
    let problems = mcp_problems("everything", &need, Some(Ok(identity())));
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems[0].starts_with(
            "requires.mcp.everything: plan 'demo__plan__say' step E1: input for everything__echo at /message:"
        ),
        "{problems:?}"
    );
    assert_eq!(
        problems[1],
        "requires.mcp.everything: [mcp.everything] has no tool 'everything__nope' (agent 'demo__agent__helper' tools names it)"
    );
}

#[test]
fn a_connection_failure_is_unmet() {
    let need = need("x", ">=1", Vec::new());
    let problems = mcp_problems("everything", &need, Some(Err("spawn failed".to_string())));
    assert_eq!(
        problems,
        ["requires.mcp.everything: [mcp.everything] did not connect: spawn failed"]
    );
}

struct Servers;

#[async_trait::async_trait]
impl ToolRegistry for Servers {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(Vec::new())
    }

    async fn invoke(&self, name: &str, _input: Value) -> Result<ToolOutcome, ToolError> {
        Err(ToolError::Unknown(name.to_string()))
    }

    async fn server_identity(&self, server: &str) -> Option<Result<ServerIdentity, String>> {
        (server == "everything").then(|| Ok(identity()))
    }
}

#[tokio::test]
async fn unmet_collects_static_and_mcp_problems_for_the_named_packs_only() {
    let mut packs = PackSet::default();
    packs.needs.insert(
        "tools".to_string(),
        PackNeeds {
            unmet: vec!["requires.binaries: gh not found on PATH".to_string()],
            mcp: BTreeMap::from([(
                "everything".to_string(),
                need("mcp-servers/everything", ">=2", vec![echo(json!({}))]),
            )]),
        },
    );
    packs.needs.insert(
        "fine".to_string(),
        PackNeeds {
            unmet: Vec::new(),
            mcp: BTreeMap::from([(
                "everything".to_string(),
                need("mcp-servers/everything", ">=2", Vec::new()),
            )]),
        },
    );
    packs.needs.insert(
        "skipped".to_string(),
        PackNeeds {
            unmet: vec!["requires.env: X not set (or empty)".to_string()],
            mcp: BTreeMap::new(),
        },
    );
    let found = unmet(&packs, ["tools", "fine"], &Servers).await;
    assert_eq!(found.keys().collect::<Vec<_>>(), ["tools"]);
    assert_eq!(found["tools"].len(), 2, "{found:?}");
    assert!(
        found["tools"][1].contains("\"message\" is a required property"),
        "{found:?}"
    );
    assert_eq!(
        describe_unmet(&found)[0],
        format!(
            "pack 'tools' has unmet requirements: {}",
            found["tools"].join("; ")
        )
    );
}

#[test]
fn a_snippet_is_built_from_the_url() {
    let requirement = McpRequirement {
        server: None,
        version: None,
        url: Some("https://mcp.linear.app/mcp".to_string()),
    };
    assert_eq!(
        mcp_snippet("linear", &requirement).unwrap(),
        "[mcp.linear]\nurl = \"https://mcp.linear.app/mcp\""
    );
    assert_eq!(mcp_snippet("linear", &McpRequirement::default()), None);
}
