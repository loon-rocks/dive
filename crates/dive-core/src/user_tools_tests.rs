//! Tests for user-defined tools.

use crate::user_tools::*;
use crate::{ToolError, ToolRegistry};
use async_trait::async_trait;
use dive_config::{ModelChoice, ModelRoles};
use dive_llm::types::{ChatRequest, ChatResponse, EventStream, StopReason, Usage};
use dive_llm::{ChatProvider, LlmError, ModelRouter};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn router() -> Arc<ModelRouter> {
    struct Canned;
    #[async_trait]
    impl ChatProvider for Canned {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            let structured = req
                .response_schema
                .as_ref()
                .map(|_| json!({"category": "bug"}));
            let user = match &req.messages[0] {
                dive_llm::types::ChatMessage::User { content, .. } => content.clone(),
                _ => String::new(),
            };
            let content = if req.system.is_empty() {
                format!("echo: {user}")
            } else {
                format!("echo [{}]: {user}", req.system)
            };
            Ok(ChatResponse {
                thinking: Vec::new(),
                content: Some(content),
                tool_calls: vec![],
                structured,
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
            unimplemented!()
        }
    }
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("mock".into(), Arc::new(Canned));
    Arc::new(ModelRouter::with_providers(
        providers,
        ModelRoles::from([(
            "default",
            ModelChoice {
                kind: None,
                provider: "mock".into(),
                model: "m".into(),
                temperature: None,
                description: None,
                fallbacks: Vec::new(),
                context_window: None,
                images: None,
            },
        )]),
    ))
}

fn doc(yaml: &str) -> UserToolDoc {
    let doc: UserToolDoc = serde_yaml::from_str(yaml).unwrap();
    validate_tool(&doc).unwrap();
    doc
}

fn registry(docs: Vec<UserToolDoc>) -> UserToolRegistry {
    UserToolRegistry::new(docs, router())
}

#[tokio::test]
async fn exec_tool_renders_args_and_parses_json() {
    let tool = doc(r#"
name: emit
description: emit json
kind: exec
command: echo
args: ['{"got": "{{input.word}}"}']
input_schema:
  type: object
  required: [word]
  properties:
    word: { type: string }
"#);
    let registry = registry(vec![tool]);
    let outcome = registry
        .invoke("user__emit", json!({"word": "hello"}))
        .await
        .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(outcome.result, json!({"got": "hello"}));
}

#[tokio::test]
async fn exec_tool_text_mode_and_failure() {
    let text_tool = doc(r#"
name: say
description: say text
kind: exec
command: echo
args: ["plain output"]
output: text
"#);
    let failing = doc(r#"
name: fail
description: exits nonzero
kind: exec
command: sh
args: ["-c", "echo oops >&2; exit 3"]
"#);
    let registry = registry(vec![text_tool, failing]);

    let outcome = registry.invoke("user__say", json!({})).await.unwrap();
    assert_eq!(outcome.result, json!({"text": "plain output"}));

    let outcome = registry.invoke("user__fail", json!({})).await.unwrap();
    assert!(outcome.is_error);
    assert_eq!(outcome.result["stderr"], "oops");
}

fn env_then_store(
    env: crate::secrets::MemorySecrets,
    store: crate::secrets::MemorySecrets,
) -> crate::secrets::ChainedSource {
    crate::secrets::ChainedSource::new(vec![Arc::new(env), Arc::new(store)])
}

#[tokio::test]
async fn exec_tool_env_reads_a_stored_secret_when_the_environment_misses() {
    let tool = doc(r#"
name: show_token
description: prints its token
kind: exec
command: sh
args: ["-c", "printf %s \"$TOKEN\""]
output: text
env:
  TOKEN: "${DIVE_TEST_STORED_TOOL_TOKEN}"
"#);
    let secrets = env_then_store(
        crate::secrets::MemorySecrets::env(),
        crate::secrets::MemorySecrets::store().with("DIVE_TEST_STORED_TOOL_TOKEN", "tok-1234"),
    );
    let registry = registry(vec![tool]).with_secrets(Arc::new(secrets));
    let outcome = registry
        .invoke("user__show_token", json!({}))
        .await
        .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(outcome.result, json!({"text": "tok-1234"}));
}

#[test]
fn an_env_default_applies_only_when_both_sources_miss() {
    let secrets = env_then_store(
        crate::secrets::MemorySecrets::env().with("IN_ENV", "env-value"),
        crate::secrets::MemorySecrets::store()
            .with("IN_ENV", "store-value")
            .with("IN_STORE", "store-value"),
    );
    let expand = |text: &str| crate::user_tools::expand_env(text, &secrets).unwrap();
    assert_eq!(expand("${IN_ENV:-fallback}"), "env-value");
    assert_eq!(expand("${IN_STORE:-fallback}"), "store-value");
    assert_eq!(expand("${IN_NEITHER:-fallback}"), "fallback");
    assert!(crate::user_tools::expand_env("${IN_NEITHER}", &secrets)
        .unwrap_err()
        .contains("IN_NEITHER"));
}

#[test]
fn a_stored_secret_is_never_reported_unset() {
    let doc = crate::user_tools::parse_tool_source(
        "name: search\ndescription: x\nkind: exec\ncommand: echo\nenv:\n  A: \"${DIVE_TEST_ONLY_STORED}\"\n  B: \"${DIVE_TEST_IN_NO_SOURCE}\"\n",
    )
    .unwrap();
    let secrets = env_then_store(
        crate::secrets::MemorySecrets::env(),
        crate::secrets::MemorySecrets::store().with("DIVE_TEST_ONLY_STORED", "s"),
    );
    assert_eq!(
        crate::user_tools::unset_env_vars(&doc, &secrets),
        ["DIVE_TEST_IN_NO_SOURCE"]
    );
}

#[tokio::test]
async fn exec_tool_missing_input_is_a_schema_error() {
    let tool = doc(r#"
name: emit
description: emit json
kind: exec
command: echo
args: ['{{input.word}}']
input_schema:
  type: object
  required: [word]
  properties:
    word: { type: string }
"#);
    let registry = registry(vec![tool]);
    let outcome = registry.invoke("user__emit", json!({})).await.unwrap();
    assert!(outcome.is_error);
    assert!(outcome.result["problems"][0]
        .as_str()
        .unwrap()
        .contains("word"));
}

#[tokio::test]
async fn prompt_tool_returns_structured_output_when_schema_given() {
    let tool = doc(r#"
name: classify
description: classify text
kind: prompt
prompt: "Classify: {{input.text}}"
output_schema:
  type: object
  properties:
    category: { type: string }
"#);
    let registry = registry(vec![tool]);
    let outcome = registry
        .invoke("user__classify", json!({"text": "login is broken"}))
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"category": "bug"}));
}

#[test]
fn validation_rejects_non_input_roots_and_bad_names() {
    let bad_root: UserToolDoc = serde_yaml::from_str(
        r#"
name: bad
description: x
kind: exec
command: echo
args: ["{{E0.values}}"]
"#,
    )
    .unwrap();
    assert!(validate_tool(&bad_root).unwrap_err().contains("E0"));

    let bad_name: UserToolDoc = serde_yaml::from_str(
        r#"
name: "no spaces"
description: x
kind: exec
command: echo
"#,
    )
    .unwrap();
    assert!(validate_tool(&bad_name).is_err());
}

fn fixture_pack_tools(pack: &str) -> Result<Vec<UserToolDoc>, String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/packs")
        .join(pack)
        .join("tools");
    load_user_tools(&[dir])
}

#[test]
fn github_pack_loads_and_validates() {
    let docs = fixture_pack_tools("github").unwrap();
    let mut names: Vec<&str> = docs.iter().map(|d| d.name.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "gh_pr_comment",
            "gh_pr_inline_comments",
            "gh_pr_meta",
            "gh_pr_review_threads",
            "gh_pr_thread_sync",
            "gh_pr_ticket",
            "gh_release",
            "git_changed_files",
            "git_diff",
            "git_file",
            "git_grep",
            "git_log"
        ]
    );
    // The comment tools post; everything else is read-only.
    for doc in &docs {
        let read_only = doc.read_only.unwrap_or(false);
        let posts = matches!(
            doc.name.as_str(),
            "gh_pr_comment" | "gh_pr_inline_comments" | "gh_pr_thread_sync"
        );
        assert_eq!(read_only, !posts, "{}", doc.name);
    }
}

fn gh_pr_ticket_with(dir: &std::path::Path, pr_json: &str) -> UserToolDoc {
    use std::os::unix::fs::PermissionsExt;
    let shim = dir.join("gh");
    std::fs::write(&shim, "#!/bin/sh\nprintf '%s' \"$FAKE_PR\"\n").unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut doc = fixture_pack_tools("github")
        .unwrap()
        .into_iter()
        .find(|d| d.name == "gh_pr_ticket")
        .expect("github pack ships gh_pr_ticket");
    let ToolKind::Exec { env, .. } = &mut doc.kind else {
        panic!("gh_pr_ticket must be an exec tool")
    };
    env.insert(
        "PATH".to_string(),
        format!(
            "{}:{}",
            dir.display(),
            std::env::var("PATH").unwrap_or_default()
        )
        .into(),
    );
    env.insert("FAKE_PR".to_string(), pr_json.to_string().into());
    doc
}

async fn ticket_for(pr_json: Value, pattern: Option<&str>) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let doc = gh_pr_ticket_with(dir.path(), &pr_json.to_string());
    let registry = UserToolRegistry::pack("github", vec![doc], router());
    let mut input = json!({"pr": 1});
    if let Some(pattern) = pattern {
        input["pattern"] = json!(pattern);
    }
    let outcome = registry
        .invoke("github__tool__gh_pr_ticket", input)
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    outcome.result
}

#[tokio::test]
async fn reshape_returns_a_bare_value_for_a_single_template_shape() {
    let docs = load_builtin_tools().unwrap();
    let registry = UserToolRegistry::builtins(docs, router());
    let outcome = registry
        .invoke("builtin__reshape", json!({"shape": "abc123"}))
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(outcome.result, json!("abc123"));
}

#[tokio::test]
async fn gh_pr_ticket_ignores_prose_and_standard_names_by_default() {
    let prose = json!({
        "headRefName": "agents/drafting",
        "title": "feat: run every 2 hours, up to 3 times",
        "body": "Encodes UTF-8 and checks SHA-256 per ISO-8601 dates. Every 2 runs."
    });
    assert_eq!(
        ticket_for(prose, None).await,
        json!({"found": false, "ticket": null, "source": null})
    );
}

#[tokio::test]
async fn gh_pr_ticket_reads_lowercase_branches_and_uppercase_titles() {
    let branch = json!({"headRefName": "ln-9418/hub-renderer", "title": "feat: hubs", "body": ""});
    assert_eq!(
        ticket_for(branch, None).await,
        json!({"found": true, "ticket": "LN-9418", "source": "branch"})
    );
    let title = json!({"headRefName": "portfolio", "title": "feat: render hubs (LN-9418)", "body": "every 2"});
    assert_eq!(
        ticket_for(title, None).await,
        json!({"found": true, "ticket": "LN-9418", "source": "title"})
    );
    let explicit = json!({"headRefName": "x", "title": "y", "body": "fixes proj-12"});
    assert_eq!(
        ticket_for(explicit, Some("[a-z]+-[0-9]+")).await,
        json!({"found": true, "ticket": "PROJ-12", "source": "body"})
    );
}

#[tokio::test]
async fn git_log_defaults_render_and_an_empty_range_is_not_an_error() {
    // Runs `git log HEAD..HEAD` in this checkout: an empty range must come
    // back as a gateable {count: 0}, not a failure. Also covers the boolean
    // input `include_merges` reaching argv as "false" from its schema
    // default — a default that failed to render would make the script's
    // `[ "$include_merges" = "true" ]` comparison operate on a template.
    let docs = fixture_pack_tools("github").unwrap();
    let registry = UserToolRegistry::pack("github", docs, router());

    let outcome = registry
        .invoke(
            "github__tool__git_log",
            json!({"base": "HEAD", "head": "HEAD"}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({"commits": [], "count": 0, "total": 0, "truncated": false})
    );
}

#[test]
fn git_log_keeps_commits_whose_subject_has_no_pr_number() {
    // Regression: jq's `capture` emits *nothing* (rather than erroring) on
    // no match, so building `pr` straight from it dropped the entire commit
    // object — a log of PR-less subjects came back `{commits: [], count: N}`.
    // The `// null` keeps the field present and the commit in the list.
    let docs = fixture_pack_tools("github").unwrap();
    let log = docs.iter().find(|d| d.name == "git_log").unwrap();
    let ToolKind::Exec { args, .. } = &log.kind else {
        panic!("git_log is an exec tool");
    };
    let script = args.join("\n");
    assert!(
        script.contains("| .n | tonumber) // null)"),
        "pr must fall back to null when capture matches nothing: {script}"
    );
}

#[test]
fn gh_pr_review_threads_requires_the_payload_keys_thread_sync_embeds() {
    // The round-trip contract: gh_pr_thread_sync embeds a finding payload,
    // gh_pr_review_threads only returns threads whose payload carries the
    // required keys. If the required set drifts from what callers embed,
    // findings silently vanish from the read side (skipped, not errored) —
    // pin the set so a change to either side is a conscious one.
    let docs = fixture_pack_tools("github").unwrap();
    let threads = docs
        .iter()
        .find(|d| d.name == "gh_pr_review_threads")
        .unwrap();
    let ToolKind::Exec { args, .. } = &threads.kind else {
        panic!("gh_pr_review_threads is an exec tool");
    };
    let script = args.join("\n");
    assert!(
        script.contains(r#"["label","severity","invariant","file","line","#),
        "required payload keys changed — update embedders to match: {script}"
    );
}

#[tokio::test]
async fn gh_pr_thread_sync_with_nothing_to_do_reports_empty_lifecycle() {
    // Empty creates + threads and no sweep_marker must not touch gh at all:
    // every loop renders zero iterations, and the aggregation still emits
    // the full lifecycle shape E8-style templates depend on.
    let docs = fixture_pack_tools("github").unwrap();
    let registry = UserToolRegistry::pack("github", docs, router());

    let outcome = registry
        .invoke(
            "github__tool__gh_pr_thread_sync",
            json!({
                "pr": 1,
                "head_sha": "deadbeef",
                "marker": "test:finding",
                "creates": [],
                "threads": []
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({
            "creates": [],
            "threads": [],
            "created_count": 0,
            "resolved_now_count": 0,
            "declined_now_count": 0,
            "still_open_count": 0,
            "transitions_count": 0,
            "has_creates": false,
            "has_transitions": false
        })
    );
}

#[test]
fn infer_decide_and_reshape_are_the_built_in_tools() {
    let names: Vec<String> = load_builtin_tools()
        .unwrap()
        .into_iter()
        .map(|doc| doc.name)
        .collect();
    assert_eq!(names, ["infer", "decide", "reshape", "generate_image"]);
}

#[tokio::test]
async fn pack_tools_serve_under_their_pack_namespace() {
    let docs = fixture_pack_tools("github").unwrap();
    let registry = UserToolRegistry::pack("github", docs, router());

    let names: Vec<String> = registry
        .tools()
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(
        names.iter().all(|n| n.starts_with("github__tool__")),
        "{names:?}"
    );
    assert!(names.contains(&"github__tool__gh_pr_meta".to_string()));

    let err = registry
        .invoke("builtin__gh_pr_meta", json!({"pr": 1}))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Unknown(_)));
}

#[test]
fn a_builtin_override_dir_replaces_infer_decide_and_reshape() {
    let dir = tempfile::tempdir().unwrap();
    let overrides = dir.path().join(BUILTIN_OVERRIDE_DIR);
    std::fs::create_dir(&overrides).unwrap();
    for name in ["infer", "decide", "reshape"] {
        std::fs::write(
            overrides.join(format!("{name}.yaml")),
            format!("name: {name}\ndescription: my {name}\nkind: exec\ncommand: echo\n"),
        )
        .unwrap();
    }
    let loaded = load_user_tools(&builtin_override_dirs(&[dir.path().to_path_buf()])).unwrap();
    let merged = apply_tool_overrides(load_builtin_tools().unwrap(), loaded).unwrap();
    let descriptions: Vec<(&str, &str)> = merged
        .iter()
        .map(|doc| (doc.name.as_str(), doc.description.as_str()))
        .collect();
    assert_eq!(
        descriptions,
        [
            ("infer", "my infer"),
            ("decide", "my decide"),
            ("reshape", "my reshape"),
            ("generate_image", descriptions[3].1),
        ]
    );
}

#[tokio::test]
async fn slack_pack_loads_and_validates() {
    let docs = fixture_pack_tools("slack").unwrap();
    let names: Vec<&str> = docs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["slack_post_message"]);
    // Posting is a side effect; the catalog must never advertise it read-only.
    assert_eq!(docs[0].read_only, Some(false));

    let defs = UserToolRegistry::pack("slack", docs, router())
        .tools()
        .await
        .unwrap();
    assert_eq!(defs[0].name, "slack__tool__slack_post_message");
    assert_eq!(defs[0].read_only, Some(false));
    // channel and ts are the threading contract — a planner only learns it
    // from the declared output schema.
    let out = defs[0].output_schema.as_ref().unwrap();
    for field in ["channel", "ts", "permalink"] {
        assert!(out["properties"][field].is_object(), "{field}: {out}");
    }
}

#[tokio::test]
async fn builtin_infer_returns_text_or_caller_structured_output() {
    let docs = load_builtin_tools().unwrap();
    assert_eq!(
        docs.iter().map(|doc| doc.name.as_str()).collect::<Vec<_>>(),
        ["infer", "decide", "reshape", "generate_image"]
    );
    let registry = UserToolRegistry::builtins(docs, router());

    // No output_schema in the call: plain text.
    let outcome = registry
        .invoke("builtin__infer", json!({"instruction": "Summarize X"}))
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    let text = outcome.result["text"].as_str().unwrap();
    assert!(
        text.starts_with("echo [You are one step of an automated plan.")
            && text.ends_with("]: Summarize X"),
        "infer runs as a non-interactive plan step by default: {text}"
    );

    // With an output_schema: structured, validated JSON.
    let outcome = registry
        .invoke(
            "builtin__infer",
            json!({
                "instruction": "Classify X",
                "output_schema": {
                    "type": "object",
                    "properties": {"category": {"type": "string"}},
                    "required": ["category"],
                },
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(outcome.result, json!({"category": "bug"}));
}

#[tokio::test]
async fn llm_pack_infer_sends_a_caller_system_prompt_as_the_system_prompt() {
    let docs = load_builtin_tools().unwrap();
    let registry = UserToolRegistry::builtins(docs, router());
    let outcome = registry
        .invoke(
            "builtin__infer",
            json!({"instruction": "Summarize X", "system": "You summarize."}),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.result,
        json!({"text": "echo [You summarize.]: Summarize X"})
    );
}

// ── Reshape ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn builtin_reshape_returns_the_callers_shape_verbatim() {
    let docs = load_builtin_tools().unwrap();
    let registry = UserToolRegistry::builtins(docs, router());

    // A caller-supplied shape is rendered by the *pipeline*, against the
    // surrounding step's scope, before the tool is dispatched. The tool
    // itself never renders it: text that survived that pass — LLM prose
    // quoting Helm/Actions/mustache tags, most of all — must not be
    // re-parsed as Dive templates. See
    // `caller_shape_survives_model_authored_template_syntax` for the
    // plan-level counterpart.
    let outcome = registry
        .invoke(
            "builtin__reshape",
            json!({
                "shape": {
                    "base_sha": "abc",
                    "pr": 12,
                    "tags": ["bug"],
                    "helm": "image: {{ index .Values.image.tag }}",
                    "actions": "token: ${{ github.token }}",
                    "mustache": "{{#items}}x{{/items}}",
                },
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({
            "base_sha": "abc",
            "pr": 12,
            "tags": ["bug"],
            "helm": "image: {{ index .Values.image.tag }}",
            "actions": "token: ${{ github.token }}",
            "mustache": "{{#items}}x{{/items}}",
        }),
        "byte-identical pass-through, no second render"
    );
}

#[tokio::test]
async fn reshape_is_read_only() {
    let docs = load_builtin_tools().unwrap();
    let registry = UserToolRegistry::builtins(docs, router());

    // Pure transform: advertised read-only.
    let read_only = registry
        .tools()
        .await
        .unwrap()
        .iter()
        .find(|d| d.name == "builtin__reshape")
        .unwrap()
        .read_only;
    assert_eq!(read_only, Some(true));
}

#[test]
fn reshape_validation_needs_a_shape_and_rejects_non_input_roots() {
    // A fixed-shape reshape tool with no shape and no caller_shape is invalid.
    let no_shape: UserToolDoc = serde_yaml::from_str(
        r#"
name: bad
description: x
kind: reshape
"#,
    )
    .unwrap();
    assert!(validate_tool(&no_shape)
        .unwrap_err()
        .contains("caller_shape"));

    // Fixed-shape leaves are validated at load time: only {{input.*}}.
    let bad_root: UserToolDoc = serde_yaml::from_str(
        r#"
name: bad
description: x
kind: reshape
shape:
  x: "{{E0.value}}"
"#,
    )
    .unwrap();
    assert!(validate_tool(&bad_root).unwrap_err().contains("E0"));

    // A fixed shape that only references input passes.
    let ok = doc(r#"
name: fixed
description: fixed projection
kind: reshape
shape:
  sha: "{{input.baseRefOid}}"
"#);
    let registry = registry(vec![ok]);
    let _ = &registry;
}

#[tokio::test]
async fn reshape_fixed_shape_renders_against_input() {
    let tool = doc(r#"
name: fixed
description: fixed projection
kind: reshape
shape:
  sha: "{{input.baseRefOid}}"
  n: "{{input.number}}"
"#);
    let registry = registry(vec![tool]);
    let outcome = registry
        .invoke("user__fixed", json!({"baseRefOid": "abc", "number": 7}))
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(outcome.result, json!({"sha": "abc", "n": 7}));
}

#[tokio::test]
async fn reshape_fixed_shape_bad_path_is_a_tool_error() {
    // A fixed shape is the one shape the tool still renders, so a path the
    // input lacks is a tool error, not a panic — the same contract as an
    // exec that exits non-zero.
    let bad = doc(r#"
name: missing
description: fixed projection over a field that isn't there
kind: reshape
shape:
  x: "{{input.nope}}"
"#);
    let registry = registry(vec![bad]);
    let outcome = registry
        .invoke("user__missing", json!({"id": 1}))
        .await
        .unwrap();
    assert!(outcome.is_error, "{:?}", outcome.result);
    assert!(outcome.result["error"]
        .as_str()
        .unwrap()
        .contains("reshape failed"));
}

#[tokio::test]
async fn caller_output_schema_requires_opt_in() {
    // A plain prompt tool ignores output_schema in the input — the schema
    // surface is the doc's unless the tool sets caller_output_schema.
    let tool = doc(r#"
name: summarize
description: summarize text
kind: prompt
prompt: "Summarize: {{input.text}}"
"#);
    let registry = registry(vec![tool]);
    let outcome = registry
        .invoke(
            "user__summarize",
            json!({"text": "hi", "output_schema": {"type": "object"}}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(outcome.result, json!({"text": "echo: Summarize: hi"}));
}

#[tokio::test]
async fn caller_schema_without_type_gets_object_defaulted() {
    // A caller who authored `output_schema` as just `{properties, required}`
    // (no top-level `type`) must still get valid structured output — the
    // provider is handed a schema with `"type": "object"` filled in, not the
    // invalid one that would 400. Capture what reaches the provider.
    use std::sync::Mutex;
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Option<Value>>>);
    #[async_trait]
    impl ChatProvider for Capture {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            *self.0.lock().unwrap() = req.response_schema.as_ref().map(|s| s.schema.clone());
            Ok(ChatResponse {
                thinking: Vec::new(),
                content: None,
                tool_calls: vec![],
                structured: Some(json!({"pattern": "x", "reason": "y"})),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
            unimplemented!()
        }
    }
    let seen = Arc::new(Mutex::new(None));
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("mock".into(), Arc::new(Capture(seen.clone())));
    let router = Arc::new(ModelRouter::with_providers(
        providers,
        ModelRoles::from([(
            "default",
            ModelChoice {
                kind: None,
                provider: "mock".into(),
                model: "m".into(),
                temperature: None,
                description: None,
                fallbacks: Vec::new(),
                context_window: None,
                images: None,
            },
        )]),
    ));
    let tool = doc(r#"
name: infer
description: generic inference
kind: prompt
prompt: "{{input.instruction}}"
caller_output_schema: true
input_schema:
  type: object
  required: [instruction]
  properties:
    instruction: { type: string }
    output_schema: { type: object }
"#);
    let registry = UserToolRegistry::new(vec![tool], router);
    let outcome = registry
        .invoke(
            "user__infer",
            json!({
                "instruction": "classify",
                // No top-level `type` — exactly the shape that 400'd in the wild.
                "output_schema": {
                    "properties": {"pattern": {"type": "string"}, "reason": {"type": "string"}},
                    "required": ["pattern", "reason"],
                },
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "result: {:?}", outcome.result);
    let forwarded = seen.lock().unwrap().clone().expect("schema forwarded");
    assert_eq!(
        forwarded["type"], "object",
        "top-level type must be defaulted before reaching the provider"
    );
}

// ── Git-backed pack tools (real git, scratch repo, no network) ───────────

/// One-commit git repo: greeting.txt at the root, src/lib.rs with two fns.
fn scratch_git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "test"]);
    std::fs::write(
        dir.path().join("greeting.txt"),
        "hello graph\nsecond line\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "fn alpha() {}\nfn beta() {}\n",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["-c", "commit.gpgsign=false", "commit", "-qm", "init"]);
    dir
}

/// The named github pack tool, pinned to run inside `cwd`.
fn github_tool_in(name: &str, cwd: &std::path::Path) -> UserToolDoc {
    let mut doc = fixture_pack_tools("github")
        .unwrap()
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no pack tool '{name}'"));
    if let ToolKind::Exec { cwd: dir, .. } = &mut doc.kind {
        *dir = Some(cwd.to_path_buf());
    }
    doc
}

#[tokio::test]
async fn git_file_reads_content_at_ref_and_marks_truncation() {
    let repo = scratch_git_repo();
    let registry = UserToolRegistry::pack(
        "github",
        vec![github_tool_in("git_file", repo.path())],
        router(),
    );

    // max_bytes defaulted: full content.
    let outcome = registry
        .invoke(
            "github__tool__git_file",
            json!({"ref": "HEAD", "path": "greeting.txt"}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(outcome.result, json!({"text": "hello graph\nsecond line"}));

    // Over budget: capped, with the truncation marker line.
    let outcome = registry
        .invoke(
            "github__tool__git_file",
            json!({"ref": "HEAD", "path": "greeting.txt", "max_bytes": 5}),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.result,
        json!({"text": "hello\n\n[file truncated: showing 5 of 24 bytes]"})
    );

    // A missing path fails loudly, not as empty content.
    let outcome = registry
        .invoke(
            "github__tool__git_file",
            json!({"ref": "HEAD", "path": "missing.txt"}),
        )
        .await
        .unwrap();
    assert!(outcome.is_error);
}

#[tokio::test]
async fn git_grep_returns_structured_matches_with_defaults() {
    let repo = scratch_git_repo();
    let registry = UserToolRegistry::pack(
        "github",
        vec![github_tool_in("git_grep", repo.path())],
        router(),
    );

    // ref/paths/max_matches all defaulted (HEAD, everything, 200).
    let outcome = registry
        .invoke(
            "github__tool__git_grep",
            json!({"pattern": "fn (alpha|beta)"}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({
            "matches": [
                {"path": "src/lib.rs", "line": 1, "text": "fn alpha() {}"},
                {"path": "src/lib.rs", "line": 2, "text": "fn beta() {}"},
            ],
            "count": 2,
            "truncated": false,
        })
    );

    // Pathspec scoping + cap: one match returned, the cut flagged.
    let outcome = registry
        .invoke(
            "github__tool__git_grep",
            json!({"pattern": "fn", "paths": "src/", "max_matches": 1}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.result["count"], 1);
    assert_eq!(outcome.result["truncated"], true);

    // No matches is a result, not an error.
    let outcome = registry
        .invoke(
            "github__tool__git_grep",
            json!({"pattern": "nowhere_to_be_found"}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({"matches": [], "count": 0, "truncated": false})
    );

    // A bad ref fails loudly instead of reading as "no matches".
    let outcome = registry
        .invoke(
            "github__tool__git_grep",
            json!({"pattern": "fn", "ref": "no-such-ref"}),
        )
        .await
        .unwrap();
    assert!(outcome.is_error);
}

// ── Prompt-tool output_schema enforcement ────────────────────────────────

/// Router whose chat role returns `chat_value` and repair role `repair_value`.
fn schema_router(chat_value: Value, repair_value: Value) -> Arc<ModelRouter> {
    struct Fixed(Value);
    #[async_trait]
    impl ChatProvider for Fixed {
        async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                thinking: Vec::new(),
                content: None,
                tool_calls: vec![],
                structured: Some(self.0.clone()),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
            unimplemented!()
        }
    }
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("chat".into(), Arc::new(Fixed(chat_value)));
    providers.insert("fixer".into(), Arc::new(Fixed(repair_value)));
    let choice = |provider: &str| ModelChoice {
        kind: None,
        provider: provider.into(),
        model: "m".into(),
        temperature: None,
        description: None,
        fallbacks: Vec::new(),
        context_window: None,
        images: None,
    };
    Arc::new(ModelRouter::with_providers(
        providers,
        ModelRoles::from([("default", choice("chat")), ("repair", choice("fixer"))]),
    ))
}

fn strict_prompt_tool() -> UserToolDoc {
    doc(r#"
name: strict
description: prompt tool with a required output field
kind: prompt
prompt: "judge {{input.x}}"
output_schema:
  type: object
  required: [category, severity]
  properties:
    category: { type: string }
    severity: { type: string }
"#)
}

#[tokio::test]
async fn prompt_output_missing_field_is_repaired() {
    let router = schema_router(
        json!({"category": "bug"}),                    // invalid: no severity
        json!({"category": "bug", "severity": "low"}), // repair fixes it
    );
    let registry = UserToolRegistry::new(vec![strict_prompt_tool()], router);
    let outcome = registry
        .invoke("user__strict", json!({"x": 1}))
        .await
        .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(outcome.result["severity"], "low");
}

#[tokio::test]
async fn prompt_output_unrepairable_is_an_error() {
    let router = schema_router(
        json!({"category": "bug"}), // invalid
        json!({"category": "bug"}), // repair returns the same invalid doc
    );
    let registry = UserToolRegistry::new(vec![strict_prompt_tool()], router);
    let err = registry
        .invoke("user__strict", json!({"x": 1}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("after repair"), "{err}");
}

#[tokio::test]
async fn a_caller_can_take_an_unrepairable_output_back() {
    let router = schema_router(json!({"category": "bug"}), json!({"category": "bug"}));
    let tool = doc(r#"
name: infer
description: generic inference
kind: prompt
prompt: "{{input.instruction}}"
caller_output_schema: true
input_schema:
  type: object
  required: [instruction]
  properties:
    instruction: { type: string }
    output_schema: { type: object }
    on_mismatch: { type: string }
"#);
    let registry = UserToolRegistry::new(vec![tool], router);
    let schema = json!({
        "type": "object",
        "required": ["category", "severity"],
        "properties": {"category": {"type": "string"}, "severity": {"type": "string"}},
    });
    let outcome = registry
        .invoke(
            "user__infer",
            json!({"instruction": "classify", "output_schema": schema, "on_mismatch": "return"}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(outcome.result, json!({"category": "bug"}));
    let err = registry
        .invoke(
            "user__infer",
            json!({"instruction": "classify", "output_schema": schema}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("after repair"), "{err}");
}

#[tokio::test]
async fn prompt_output_valid_passes_untouched() {
    let router = schema_router(
        json!({"category": "bug", "severity": "high"}), // already valid
        json!({"category": "WRONG", "severity": "WRONG"}), // repair must not run
    );
    let registry = UserToolRegistry::new(vec![strict_prompt_tool()], router);
    let outcome = registry
        .invoke("user__strict", json!({"x": 1}))
        .await
        .unwrap();
    assert_eq!(outcome.result["severity"], "high");
}

// ── Custom roles ─────────────────────────────────────────────────────────

/// Router that echoes the resolved model id back in the response text,
/// with two custom roles alongside the default — one described (and so
/// advertised), one not.
fn named_model_router() -> Arc<ModelRouter> {
    struct Echo;
    #[async_trait]
    impl ChatProvider for Echo {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                thinking: Vec::new(),
                content: Some(format!("model={}", req.model)),
                tool_calls: vec![],
                structured: None,
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
            unimplemented!()
        }
    }
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("mock".into(), Arc::new(Echo));
    let choice = |model: &str, description: Option<&str>| ModelChoice {
        kind: None,
        provider: "mock".into(),
        model: model.into(),
        temperature: None,
        description: description.map(str::to_string),
        fallbacks: Vec::new(),
        context_window: None,
        images: None,
    };
    Arc::new(ModelRouter::with_providers(
        providers,
        ModelRoles::from([
            ("default", choice("default-model", None)),
            ("nano", choice("nano-model", Some("fast and cheap"))),
            ("deep", choice("deep-model", None)),
        ]),
    ))
}

#[tokio::test]
async fn prompt_tool_model_pin_routes_to_named_model() {
    let tool = doc(r#"
name: pinned
description: runs on a named model
kind: prompt
prompt: "go"
model: nano
"#);
    let registry = UserToolRegistry::new(vec![tool], named_model_router());
    let outcome = registry.invoke("user__pinned", json!({})).await.unwrap();
    assert_eq!(outcome.result, json!({"text": "model=nano-model"}));
}

#[tokio::test]
async fn caller_model_overrides_the_doc_pin() {
    let tool = doc(r#"
name: pick
description: caller picks the model
kind: prompt
prompt: "go"
model: deep
caller_model: true
"#);
    let registry = UserToolRegistry::new(vec![tool], named_model_router());
    let outcome = registry
        .invoke("user__pick", json!({"model": "nano"}))
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"text": "model=nano-model"}));
    let outcome = registry.invoke("user__pick", json!({})).await.unwrap();
    assert_eq!(outcome.result, json!({"text": "model=deep-model"}));
}

#[tokio::test]
async fn unknown_model_name_errors_listing_configured_names() {
    let tool = doc(r#"
name: broken
description: pins a model that is not configured
kind: prompt
prompt: "go"
model: bogus
"#);
    let registry = UserToolRegistry::new(vec![tool], named_model_router());
    let err = registry
        .invoke("user__broken", json!({}))
        .await
        .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("no model named 'bogus'"), "{message}");
    assert!(message.contains("nano"), "{message}");
}

#[tokio::test]
async fn role_names_still_resolve_with_default_fallback() {
    let tool = doc(r#"
name: solver_tool
description: runs under the solver role
kind: prompt
prompt: "go"
model: solver
"#);
    let registry = UserToolRegistry::new(vec![tool], named_model_router());
    // No solver entry configured: falls back to default.
    let outcome = registry
        .invoke("user__solver_tool", json!({}))
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"text": "model=default-model"}));
}

#[tokio::test]
async fn caller_model_tools_advertise_named_models_in_the_catalog() {
    let docs = load_builtin_tools().unwrap();

    let registry = UserToolRegistry::builtins(docs.clone(), named_model_router());
    let defs = registry.tools().await.unwrap();
    let infer = defs.iter().find(|d| d.name == "builtin__infer").unwrap();
    let model = &infer.input_schema["properties"]["model"];
    assert_eq!(
        model["enum"],
        json!(["chat", "default", "judge", "nano", "solver"]),
        "standard chat roles that fall back to default are offered too"
    );
    let description = model["description"].as_str().unwrap();
    assert!(
        description.contains("nano — fast and cheap"),
        "{description}"
    );
    assert!(
        description.contains("default — General-purpose model"),
        "{description}"
    );
    assert!(!description.contains("deep"), "{description}");

    let registry = UserToolRegistry::builtins(docs, router());
    let defs = registry.tools().await.unwrap();
    let infer = defs.iter().find(|d| d.name == "builtin__infer").unwrap();
    assert!(infer.input_schema["properties"].get("model").is_none());
}

#[tokio::test]
async fn llm_pack_infer_routes_by_model_input() {
    let docs = load_builtin_tools().unwrap();
    let registry = UserToolRegistry::builtins(docs, named_model_router());
    let outcome = registry
        .invoke(
            "builtin__infer",
            json!({"instruction": "go", "model": "nano"}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.result, json!({"text": "model=nano-model"}));
}

// ── Slack pack (real bash and jq, fake curl, no network) ─────────────────

/// A directory holding a fake `curl` that appends its argv *and* its stdin
/// config to `$CURL_LOG`, then answers with the canned bodies from the
/// environment. Only curl is faked — bash and jq are the real ones, so the
/// tests exercise the pack's actual payload building and error handling.
fn curl_shim() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let shim = dir.path().join("curl");
    std::fs::write(
        &shim,
        r#"#!/bin/sh
{ printf 'ARGS'; for a in "$@"; do printf ' %s' "$a"; done; printf '\n'; } >> "$CURL_LOG"
printf 'CONFIG ' >> "$CURL_LOG"; cat >> "$CURL_LOG"
case " $* " in
  *chat.postMessage*)  printf '%s' "$FAKE_POST" ;;
  *chat.getPermalink*) printf '%s' "$FAKE_PERMALINK" ;;
  *) echo "unexpected curl call: $*" >&2; exit 9 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// The slack pack tool with `curl` resolving to the shim in `dir` and the
/// canned responses supplied through the doc's `env` map. `run_exec` overlays
/// that map onto the inherited environment, so the test never mutates its own
/// process — and a real SLACK_BOT_TOKEN exported on the dev machine can't
/// leak into the assertions. Rust resolves `bash` from the parent PATH; bash
/// then resolves `curl` from the child env, where the shim comes first.
fn slack_tool_with(dir: &std::path::Path, post: &str, permalink: &str) -> UserToolDoc {
    let mut doc = fixture_pack_tools("slack")
        .unwrap()
        .into_iter()
        .find(|d| d.name == "slack_post_message")
        .expect("slack pack ships slack_post_message");
    let ToolKind::Exec { env, .. } = &mut doc.kind else {
        panic!("slack_post_message must be an exec tool")
    };
    env.insert(
        "PATH".to_string(),
        format!(
            "{}:{}",
            dir.display(),
            std::env::var("PATH").unwrap_or_default()
        )
        .into(),
    );
    env.insert("SLACK_BOT_TOKEN".to_string(), "xoxb-test-token".into());
    env.insert(
        "CURL_LOG".to_string(),
        dir.join("curl.log").display().to_string().into(),
    );
    env.insert("FAKE_POST".to_string(), post.to_string().into());
    env.insert("FAKE_PERMALINK".to_string(), permalink.to_string().into());
    doc
}

fn slack_registry(doc: UserToolDoc) -> UserToolRegistry {
    UserToolRegistry::pack("slack", vec![doc], router())
}

#[tokio::test]
async fn slack_post_message_posts_and_keeps_the_token_out_of_argv() {
    let shim = curl_shim();
    let registry = slack_registry(slack_tool_with(
        shim.path(),
        r#"{"ok":true,"channel":"C0EN9","ts":"1717171717.000100"}"#,
        r#"{"ok":true,"permalink":"https://acme.slack.com/archives/C0EN9/p1717171717000100"}"#,
    ));

    let outcome = registry
        .invoke(
            "slack__tool__slack_post_message",
            // Quotes and braces in the text would break a concatenated
            // payload; jq escapes them.
            json!({"channel": "#eng", "text": r#"deploy *green* — "done", {ok}"#}),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({
            "channel": "C0EN9",
            "ts": "1717171717.000100",
            "permalink": "https://acme.slack.com/archives/C0EN9/p1717171717000100",
        })
    );

    let log = std::fs::read_to_string(shim.path().join("curl.log")).unwrap();
    // thread_ts defaults to "" and is dropped from the payload entirely —
    // Slack rejects an empty thread_ts.
    assert!(
        log.contains(r##"{"channel":"#eng","text":"deploy *green* — \"done\", {ok}"}"##),
        "{log}"
    );
    // The token travels in the curl config on stdin...
    assert!(
        log.contains(r#"CONFIG header = "Authorization: Bearer xoxb-test-token""#),
        "{log}"
    );
    // ...and never in argv, which the process table exposes.
    for line in log.lines().filter(|l| l.starts_with("ARGS")) {
        assert!(!line.contains("xoxb-test-token"), "token in argv: {line}");
    }
    // The permalink lookup used the resolved id, not the "#eng" name.
    assert!(log.contains("channel=C0EN9"), "{log}");
}

#[tokio::test]
async fn slack_post_message_threads_a_reply_under_a_parent_ts() {
    let shim = curl_shim();
    let registry = slack_registry(slack_tool_with(
        shim.path(),
        r#"{"ok":true,"channel":"C0EN9","ts":"1717171800.000200"}"#,
        r#"{"ok":true,"permalink":"https://acme.slack.com/archives/C0EN9/p1717171800000200"}"#,
    ));

    let outcome = registry
        .invoke(
            "slack__tool__slack_post_message",
            json!({
                "channel": "C0EN9",
                "text": "details",
                "thread_ts": "1717171717.000100",
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{:?}", outcome.result);
    // The reply's own ts comes back — not the parent's, so a chain of
    // replies can keep threading off the original.
    assert_eq!(outcome.result["ts"], "1717171800.000200");

    let log = std::fs::read_to_string(shim.path().join("curl.log")).unwrap();
    assert!(log.contains(r#""thread_ts":"1717171717.000100""#), "{log}");
}

#[tokio::test]
async fn slack_post_message_treats_ok_false_as_a_tool_error() {
    let shim = curl_shim();
    let registry = slack_registry(slack_tool_with(
        shim.path(),
        r#"{"ok":false,"error":"channel_not_found"}"#,
        r#"{"ok":true,"permalink":"never-reached"}"#,
    ));

    let outcome = registry
        .invoke(
            "slack__tool__slack_post_message",
            json!({"channel": "#nope", "text": "hi"}),
        )
        .await
        .unwrap();
    // Slack answers HTTP 200 for a post it refused. The step must still
    // fail: a plan cannot be allowed to report a message it never
    // delivered.
    assert!(outcome.is_error, "{:?}", outcome.result);
    assert!(
        outcome.result["stderr"]
            .as_str()
            .unwrap()
            .contains("channel_not_found"),
        "{:?}",
        outcome.result
    );
    // And no permalink lookup for a message that doesn't exist.
    let log = std::fs::read_to_string(shim.path().join("curl.log")).unwrap();
    assert!(!log.contains("chat.getPermalink"), "{log}");
}

#[tokio::test]
async fn slack_post_message_reports_a_non_json_body() {
    let shim = curl_shim();
    // A proxy or rate-limit page instead of the API's JSON: there is no
    // `.error` to quote, so the body itself has to reach the operator.
    let registry = slack_registry(slack_tool_with(
        shim.path(),
        "<html>too many requests</html>",
        "{}",
    ));

    let outcome = registry
        .invoke(
            "slack__tool__slack_post_message",
            json!({"channel": "C1", "text": "hi"}),
        )
        .await
        .unwrap();
    assert!(outcome.is_error, "{:?}", outcome.result);
    assert!(
        outcome.result["stderr"]
            .as_str()
            .unwrap()
            .contains("too many requests"),
        "{:?}",
        outcome.result
    );
}

#[tokio::test]
async fn slack_post_message_permalink_is_best_effort() {
    let shim = curl_shim();
    let registry = slack_registry(slack_tool_with(
        shim.path(),
        r#"{"ok":true,"channel":"C1","ts":"1717171717.000100"}"#,
        r#"{"ok":false,"error":"message_not_found"}"#,
    ));

    let outcome = registry
        .invoke(
            "slack__tool__slack_post_message",
            json!({"channel": "C1", "text": "hi"}),
        )
        .await
        .unwrap();
    // The message was delivered; a permalink lookup that doesn't answer is
    // null, not a failed post. The key is still present, so a template
    // reference to it never dangles.
    assert!(!outcome.is_error, "{:?}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({"channel": "C1", "ts": "1717171717.000100", "permalink": null})
    );
}

#[tokio::test]
async fn slack_post_message_without_a_token_never_calls_slack() {
    let shim = curl_shim();
    let mut doc = slack_tool_with(shim.path(), "{}", "{}");
    let ToolKind::Exec { env, .. } = &mut doc.kind else {
        unreachable!()
    };
    // Explicitly empty rather than absent: the child inherits the parent
    // environment, so a developer's exported token must not decide this.
    env.insert("SLACK_BOT_TOKEN".to_string(), String::new().into());

    let outcome = slack_registry(doc)
        .invoke(
            "slack__tool__slack_post_message",
            json!({"channel": "C1", "text": "hi"}),
        )
        .await
        .unwrap();
    assert!(outcome.is_error, "{:?}", outcome.result);
    assert!(
        outcome.result["stderr"]
            .as_str()
            .unwrap()
            .contains("SLACK_BOT_TOKEN"),
        "{:?}",
        outcome.result
    );
    assert!(
        !shim.path().join("curl.log").exists(),
        "no request should have been attempted"
    );
}

struct EchoDecider {
    requests: std::sync::Mutex<Vec<dive_llm::decision::DecisionRequest>>,
    roles: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl dive_llm::DecisionProvider for EchoDecider {
    async fn decide(
        &self,
        req: dive_llm::decision::DecisionRequest,
    ) -> Result<dive_llm::decision::DecisionResponse, LlmError> {
        self.requests.lock().unwrap().push(req.clone());
        self.roles
            .lock()
            .unwrap()
            .push(crate::usage::CallSite::current().role);
        let answers = req
            .questions
            .iter()
            .map(|(name, question)| {
                let answer = match question {
                    dive_llm::decision::Question::Choice { criteria, .. } => {
                        let choice = criteria.keys().next().unwrap().clone();
                        dive_llm::decision::Answer::Choice {
                            probabilities: criteria
                                .keys()
                                .map(|key| (key.clone(), if *key == choice { 0.8 } else { 0.2 }))
                                .collect(),
                            choice,
                            confidence: 0.6,
                        }
                    }
                    _ => dive_llm::decision::Answer::Likelihood { probability: 0.9 },
                };
                (name.clone(), answer)
            })
            .collect();
        Ok(dive_llm::decision::DecisionResponse {
            model: req.model,
            answers,
            usage: Usage::default(),
        })
    }
}

fn decision_router(with_decider: bool) -> (Arc<ModelRouter>, Arc<EchoDecider>) {
    let decider = Arc::new(EchoDecider {
        requests: std::sync::Mutex::new(Vec::new()),
        roles: std::sync::Mutex::new(Vec::new()),
    });
    let choice = |provider: &str, model: &str, description: Option<&str>| ModelChoice {
        kind: None,
        provider: provider.into(),
        model: model.into(),
        temperature: None,
        description: description.map(str::to_string),
        fallbacks: Vec::new(),
        context_window: None,
        images: None,
    };
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("mock".into(), Arc::new(UnusedChat));
    let mut roles = vec![
        ("default", choice("mock", "m", None)),
        ("nano", choice("mock", "small", Some("small chat model"))),
    ];
    if with_decider {
        roles.push((
            "decider",
            choice("typesafe", "jev-latest", Some("calibrated verdicts")),
        ));
    }
    let router = ModelRouter::with_providers(
        providers,
        ModelRoles::from_iter(
            roles
                .into_iter()
                .map(|(name, choice)| (name.to_string(), choice)),
        ),
    )
    .with_deciders(HashMap::from([(
        "typesafe".to_string(),
        decider.clone() as Arc<dyn dive_llm::DecisionProvider>,
    )]));
    (Arc::new(router), decider)
}

struct UnusedChat;

#[async_trait]
impl ChatProvider for UnusedChat {
    async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, LlmError> {
        unimplemented!()
    }
    async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn builtin_decide_is_listed_only_with_a_decision_model_and_offers_no_choice_of_one() {
    let docs = load_builtin_tools().unwrap();
    let (router, _) = decision_router(false);
    let names: Vec<String> = UserToolRegistry::builtins(docs.clone(), router)
        .tools()
        .await
        .unwrap()
        .into_iter()
        .map(|def| def.name)
        .collect();
    assert_eq!(names, ["builtin__infer", "builtin__reshape"]);

    let (router, _) = decision_router(true);
    let defs = UserToolRegistry::builtins(docs, router)
        .tools()
        .await
        .unwrap();
    let decide = defs.iter().find(|d| d.name == "builtin__decide").unwrap();
    assert_eq!(decide.read_only, Some(true));
    assert!(decide.input_schema["properties"].get("model").is_none());
    let infer = defs.iter().find(|d| d.name == "builtin__infer").unwrap();
    assert_eq!(
        infer.input_schema["properties"]["model"]["enum"],
        json!(["chat", "default", "judge", "nano", "solver"])
    );
}

#[tokio::test]
async fn builtin_decide_answers_caller_questions_about_the_state() {
    let docs = load_builtin_tools().unwrap();
    let (router, decider) = decision_router(true);
    let registry = UserToolRegistry::builtins(docs, router);
    let outcome = registry
        .invoke(
            "builtin__decide",
            json!({
                "state": {"ticket": "payouts failing"},
                "questions": {
                    "urgent": {"type": "likelihood", "instructions": "Is it urgent?"},
                    "team": {"type": "choice", "instructions": "Which team?", "criteria": {"billing": "Payments", "technical": "Bugs"}}
                }
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{}", outcome.result);
    assert_eq!(
        outcome.result,
        json!({
            "urgent": {"type": "likelihood", "probability": 0.9},
            "team": {"type": "choice", "choice": "billing", "confidence": 0.6, "probabilities": {"billing": 0.8, "technical": 0.2}}
        })
    );
    let requests = decider.requests.lock().unwrap();
    assert_eq!(requests[0].model, "jev-latest");
    assert_eq!(requests[0].state, json!({"ticket": "payouts failing"}));
}

#[tokio::test]
async fn builtin_decide_meters_its_call_under_the_decision_role_not_the_tool_name() {
    let docs = load_builtin_tools().unwrap();
    let (router, decider) = decision_router(true);
    let registry = UserToolRegistry::builtins(docs, router);
    let outcome = crate::usage::CallSite::role("builtin__decide")
        .at("E1")
        .scope(registry.invoke(
            "builtin__decide",
            json!({
                "state": {"ticket": "payouts failing"},
                "questions": {"urgent": {"type": "likelihood", "instructions": "Is it urgent?"}}
            }),
        ))
        .await
        .unwrap();
    assert!(!outcome.is_error, "{}", outcome.result);
    assert_eq!(*decider.roles.lock().unwrap(), ["decider"]);
}

#[tokio::test]
async fn a_user_decision_tool_renders_its_state_and_questions_from_input() {
    let tool = doc(r#"
name: triage
description: Route a ticket.
kind: decision
state: { ticket: "{{input.ticket}}", source: "{{input.source}}" }
questions:
  team:
    type: choice
    instructions: "Which team handles a {{input.source}} ticket?"
    criteria: { billing: Payments, technical: Bugs }
input_schema:
  type: object
  required: [ticket, source]
  properties:
    ticket: { type: string }
    source: { type: string }
"#);
    let (router, decider) = decision_router(true);
    let registry = UserToolRegistry::new(vec![tool], router);
    let outcome = registry
        .invoke(
            "user__triage",
            json!({"ticket": "refund please", "source": "email"}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.result["team"]["choice"], json!("billing"));
    let requests = decider.requests.lock().unwrap();
    assert_eq!(
        requests[0].state,
        json!({"ticket": "refund please", "source": "email"})
    );
    assert!(matches!(
        &requests[0].questions["team"],
        dive_llm::decision::Question::Choice { instructions, .. } if instructions == "Which team handles a email ticket?"
    ));
}

#[test]
fn decision_tools_are_checked_at_load() {
    let parse = |yaml: &str| {
        let doc = parse_tool_source(yaml)?;
        validate_tool(&doc)
    };
    let err = parse("name: t\ndescription: d\nkind: decision\n").unwrap_err();
    assert!(
        err.contains("`questions` or `caller_questions: true`"),
        "{err}"
    );
    let err = parse(
        "name: t\ndescription: d\nkind: decision\nquestions: { q: { type: guess, instructions: x } }\n",
    )
    .unwrap_err();
    assert!(err.contains("likelihood, choice, or score"), "{err}");
    let err = parse(
        "name: t\ndescription: d\nkind: decision\nquestions: { q: { type: likelihood, instructions: \"{{steps.E0}}\" } }\n",
    )
    .unwrap_err();
    assert!(err.contains("{{input.*}}"), "{err}");
    let err = parse("name: t\ndescription: d\nkind: decision\nprompt: x\ncaller_questions: true\n")
        .unwrap_err();
    assert!(err.contains("`prompt`"), "{err}");
}

#[tokio::test]
async fn a_decision_tool_on_a_chat_role_fails_naming_the_role() {
    let tool = doc(r#"
name: check
description: d
kind: decision
model: nano
questions: { q: { type: likelihood, instructions: "Is it?" } }
"#);
    let (router, _) = decision_router(true);
    let registry = UserToolRegistry::new(vec![tool], router);
    let error = registry.invoke("user__check", json!({})).await.unwrap_err();
    assert!(
        error.to_string().contains("'nano' is a chat model"),
        "{error}"
    );
}

#[test]
fn a_builtin_override_replaces_the_pack_tool_it_names() {
    let builtins = load_builtin_tools().unwrap();
    let mut mine = builtins
        .iter()
        .find(|d| d.name == "reshape")
        .unwrap()
        .clone();
    mine.description = "my reshape".to_string();
    mine.path = Some(std::path::PathBuf::from("/tools/builtin/reshape.yaml"));
    let merged = apply_tool_overrides(builtins.clone(), vec![mine]).unwrap();
    assert_eq!(merged.len(), builtins.len());
    let reshape = merged.iter().find(|d| d.name == "reshape").unwrap();
    assert_eq!(reshape.description, "my reshape");
}

#[test]
fn a_builtin_override_without_a_matching_pack_tool_is_an_error() {
    let builtins = load_builtin_tools().unwrap();
    let mut stray = doc("name: summarize\ndescription: s\nkind: exec\ncommand: echo\n");
    stray.path = Some(std::path::PathBuf::from("/tools/builtin/summarize.yaml"));
    let err = apply_tool_overrides(builtins, vec![stray]).unwrap_err();
    assert!(err.starts_with("/tools/builtin/summarize.yaml"), "{err}");
    assert!(err.contains("no built-in tool named 'summarize'"), "{err}");
}

#[test]
fn a_top_level_tool_named_like_a_pack_tool_stays_a_user_tool() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("reshape.yaml"),
        "name: reshape\ndescription: mine\nkind: exec\ncommand: echo\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join(BUILTIN_OVERRIDE_DIR)).unwrap();
    let dirs = vec![dir.path().to_path_buf()];
    let user = load_user_tools(&dirs).unwrap();
    assert_eq!(
        user.len(),
        1,
        "the builtin/ subdirectory is not read as user tools"
    );
    let overrides = load_user_tools(&builtin_override_dirs(&dirs)).unwrap();
    assert!(overrides.is_empty());
}

#[test]
fn coercion_leaves_strings_where_the_schema_allows_them() {
    let schema = json!({
        "type": "object",
        "required": ["note", "done"],
        "properties": {
            "note": {"type": "string"},
            "items": {"type": "array"},
            "done": {"type": "boolean", "default": false},
        },
    });
    let coerced = crate::user_tools::coerce_to_schema(
        json!({"note": "{\"a\": 1}", "items": "[1, 2]"}),
        &schema,
    );
    assert_eq!(
        coerced,
        json!({"note": "{\"a\": 1}", "items": [1, 2], "done": false})
    );
}

#[test]
fn coercion_pairs_parameter_names_with_their_values() {
    let schema = json!({
        "type": "object",
        "required": ["step", "planComplete"],
        "properties": {
            "step": {"type": "object"},
            "planComplete": {"type": "boolean", "default": false},
        },
    });
    let coerced = crate::user_tools::coerce_to_schema(
        json!({"$PARAMETER_NAME": "step", "$PARAMETER_NAME2": {"id": "E5"}}),
        &schema,
    );
    assert_eq!(
        coerced,
        json!({"step": {"id": "E5"}, "planComplete": false})
    );
}

#[test]
fn coercion_reads_a_json_string_with_trailing_parameter_tags() {
    let schema = json!({
        "type": "object",
        "required": ["step", "planComplete"],
        "properties": {
            "step": {"type": "object"},
            "planComplete": {"type": "boolean", "default": false},
        },
    });
    let coerced = crate::user_tools::coerce_to_schema(
        json!({"step": "{\"id\": \"E0\"}\n<parameter name=\"planComplete\">true"}),
        &schema,
    );
    assert_eq!(coerced, json!({"step": {"id": "E0"}, "planComplete": true}));
}

#[test]
fn coercion_recovers_parent_fields_leaked_into_a_json_string() {
    let schema = json!({
        "type": "object",
        "required": ["step", "planComplete"],
        "properties": {
            "step": {"type": "object"},
            "planComplete": {"type": "boolean", "default": false},
        },
    });
    let coerced = crate::user_tools::coerce_to_schema(
        json!({"step": "{\"id\": \"E1\"},\"planComplete\":true}"}),
        &schema,
    );
    assert_eq!(coerced, json!({"step": {"id": "E1"}, "planComplete": true}));
}

#[test]
fn an_env_reference_can_carry_a_default_for_settings() {
    let doc = crate::user_tools::parse_tool_source(
        "name: search\ndescription: x\nkind: exec\ncommand: echo\nenv:\n  URL: \"${DIVE_TEST_NEVER_SET_URL:-https://search.example}\"\n  KEY: \"${DIVE_TEST_NEVER_SET_KEY}\"\n",
    )
    .unwrap();
    assert_eq!(
        crate::user_tools::unset_env_vars(&doc, &crate::secrets::EnvSource),
        ["DIVE_TEST_NEVER_SET_KEY"],
        "a reference with a default never counts as unset"
    );
    let supplied = crate::user_tools::with_env_values(
        &doc,
        &std::collections::BTreeMap::from([(
            "DIVE_TEST_NEVER_SET_KEY".to_string(),
            "k".to_string(),
        )]),
    );
    let crate::user_tools::ToolKind::Exec { env, .. } = &supplied.kind else {
        panic!("exec tool");
    };
    assert_eq!(env["KEY"].value(), "k");
    assert_eq!(
        env["URL"].value(),
        "${DIVE_TEST_NEVER_SET_URL:-https://search.example}"
    );
}

#[tokio::test]
async fn pack_exec_tools_see_their_pack_directory_and_user_tools_do_not() {
    let doc = parse_tool_source(
        "name: where\ndescription: d\nkind: exec\ncommand: sh\nargs: [\"-c\", 'printf %s \"${DIVE_PACK_DIR-unset}\"']\noutput: text\n",
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pack =
        UserToolRegistry::pack("example", vec![doc.clone()], router()).with_pack_dir(dir.path());
    let outcome = pack
        .invoke("example__tool__where", json!({}))
        .await
        .unwrap();
    assert_eq!(
        outcome.result,
        json!({"text": dir.path().display().to_string()})
    );

    let user = UserToolRegistry::new(vec![doc], router());
    let outcome = user.invoke("user__where", json!({})).await.unwrap();
    assert_eq!(outcome.result, json!({"text": "unset"}));
}

struct Truncating {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl ChatProvider for Truncating {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let structured = req
            .response_schema
            .is_some()
            .then(|| json!({"ranked": [{"url": "https://example.com/a"}]}));
        Ok(ChatResponse {
            thinking: Vec::new(),
            content: structured
                .is_none()
                .then(|| "a long partial answer".to_string()),
            tool_calls: vec![],
            structured,
            stop_reason: StopReason::MaxTokens,
            usage: Usage {
                output_tokens: 16000,
                output_image_tokens: 0,
                ..Default::default()
            },
        })
    }
    async fn chat_stream(&self, _req: ChatRequest) -> Result<EventStream, LlmError> {
        unimplemented!()
    }
}

fn truncating_router() -> (Arc<ModelRouter>, Arc<Truncating>, Arc<Truncating>) {
    let chat = Arc::new(Truncating {
        calls: Default::default(),
    });
    let fixer = Arc::new(Truncating {
        calls: Default::default(),
    });
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("chat".into(), chat.clone());
    providers.insert("fixer".into(), fixer.clone());
    let choice = |provider: &str| ModelChoice {
        kind: None,
        provider: provider.into(),
        model: "m".into(),
        temperature: None,
        description: None,
        fallbacks: Vec::new(),
        context_window: None,
        images: None,
    };
    let router = Arc::new(ModelRouter::with_providers(
        providers,
        ModelRoles::from([("default", choice("chat")), ("repair", choice("fixer"))]),
    ));
    (router, chat, fixer)
}

#[tokio::test]
async fn a_structured_infer_that_hits_max_tokens_fails_without_repair() {
    let (router, chat, fixer) = truncating_router();
    let registry = UserToolRegistry::builtins(load_builtin_tools().unwrap(), router);
    let err = registry
        .invoke(
            "builtin__infer",
            json!({
                "instruction": "Rank the links",
                "output_schema": {
                    "type": "object",
                    "required": ["ranked", "urls"],
                    "properties": {
                        "ranked": {"type": "array"},
                        "urls": {"type": "array"}
                    }
                },
                "on_mismatch": "return"
            }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("output limit of 16000 tokens before finishing the structured result"),
        "{err}"
    );
    assert_eq!(chat.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(fixer.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_plain_text_infer_that_hits_max_tokens_returns_its_text() {
    let (router, _, fixer) = truncating_router();
    let registry = UserToolRegistry::builtins(load_builtin_tools().unwrap(), router);
    let outcome = registry
        .invoke("builtin__infer", json!({"instruction": "Summarize X"}))
        .await
        .unwrap();
    assert_eq!(outcome.result["text"], "a long partial answer");
    assert_eq!(fixer.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[derive(Default)]
struct MemoryFiles {
    blobs: std::sync::Mutex<HashMap<String, Vec<u8>>>,
    dir: std::sync::OnceLock<tempfile::TempDir>,
}

#[async_trait]
impl crate::files::BlobStore for MemoryFiles {
    async fn put(
        &self,
        bytes: Vec<u8>,
        mime: &str,
    ) -> Result<crate::files::StoredFile, crate::files::FileError> {
        let hash = format!(
            "{:064x}",
            bytes.iter().map(|b| *b as u64).sum::<u64>() + bytes.len() as u64
        );
        let blob = crate::files::BlobRef::new(hash, mime);
        let size = bytes.len() as u64;
        self.blobs.lock().unwrap().insert(blob.to_string(), bytes);
        Ok(crate::files::StoredFile {
            blob,
            mime: mime.to_string(),
            bytes: size,
            width: None,
            height: None,
        })
    }

    async fn get(&self, blob: &crate::files::BlobRef) -> Result<Vec<u8>, crate::files::FileError> {
        self.blobs
            .lock()
            .unwrap()
            .get(&blob.to_string())
            .cloned()
            .ok_or_else(|| crate::files::FileError(format!("no file {blob}")))
    }

    async fn delete(&self, _blob: &crate::files::BlobRef) -> Result<(), crate::files::FileError> {
        Ok(())
    }

    async fn locate(
        &self,
        blob: &crate::files::BlobRef,
    ) -> Result<crate::files::FileLocation, crate::files::FileError> {
        self.materialize(blob)
            .await
            .map(crate::files::FileLocation::Path)
    }

    async fn materialize(
        &self,
        blob: &crate::files::BlobRef,
    ) -> Result<std::path::PathBuf, crate::files::FileError> {
        let dir = self.dir.get_or_init(|| tempfile::tempdir().unwrap());
        let path = dir.path().join(blob.file_name());
        std::fs::write(&path, self.get(blob).await?).unwrap();
        Ok(path)
    }
}

struct Painter {
    seen: std::sync::Mutex<Vec<dive_llm::ImageRequest>>,
}

#[async_trait]
impl dive_llm::ImageProvider for Painter {
    fn options(&self, _model: &str) -> dive_llm::ImageOptions {
        dive_llm::ImageOptions {
            aspect_ratios: vec!["1:1", "16:9"],
            sizes: vec!["1K"],
            max_images: 2,
        }
    }

    async fn generate(
        &self,
        req: dive_llm::ImageRequest,
    ) -> Result<dive_llm::ImageResponse, LlmError> {
        let mut seen = self.seen.lock().unwrap();
        seen.push(req.clone());
        Ok(dive_llm::ImageResponse {
            parts: vec![
                dive_llm::Part::Text("done".into()),
                dive_llm::Part::Image(dive_llm::ImageData {
                    mime: "image/png".into(),
                    bytes: format!("png {}", seen.len()).into_bytes(),
                }),
            ],
            usage: Usage::default(),
        })
    }
}

fn image_registry() -> (
    UserToolRegistry,
    Arc<Painter>,
    Arc<MemoryFiles>,
    tempfile::TempDir,
) {
    let painter = Arc::new(Painter {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let router = ModelRouter::with_providers(
        HashMap::new(),
        ModelRoles::from([(
            "image",
            ModelChoice {
                kind: None,
                provider: "gemini".into(),
                model: "nano".into(),
                temperature: None,
                description: None,
                fallbacks: Vec::new(),
                context_window: None,
                images: None,
            },
        )]),
    )
    .with_images(HashMap::from([(
        "gemini".to_string(),
        painter.clone() as Arc<dyn dive_llm::ImageProvider>,
    )]));
    let files = Arc::new(MemoryFiles::default());
    let workspace = tempfile::tempdir().unwrap();
    let registry = UserToolRegistry::builtins(load_builtin_tools().unwrap(), Arc::new(router))
        .with_files(files.clone())
        .with_workspace(workspace.path());
    (registry, painter, files, workspace)
}

#[tokio::test]
async fn generate_image_stores_each_image_and_saves_it_into_the_workspace() {
    let (registry, painter, files, workspace) = image_registry();
    let reference = crate::files::BlobStore::put(files.as_ref(), b"cat".to_vec(), "image/jpeg")
        .await
        .unwrap();
    let outcome = registry
        .invoke(
            "builtin__generate_image",
            json!({
                "prompt": "the cat, in a hat",
                "images": [reference.blob.to_string()],
                "aspect_ratio": "16:9",
                "count": 2,
                "path": "site/hero.png"
            }),
        )
        .await
        .unwrap();
    assert!(!outcome.is_error, "{}", outcome.result);

    let seen = painter.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].model, "nano");
    assert_eq!(seen[0].aspect_ratio.as_deref(), Some("16:9"));
    assert_eq!(
        seen[0].parts,
        vec![
            dive_llm::Part::Text("the cat, in a hat".into()),
            dive_llm::Part::Image(dive_llm::ImageData {
                mime: "image/jpeg".into(),
                bytes: b"cat".to_vec()
            })
        ]
    );

    let images = outcome.result["images"].as_array().unwrap();
    assert_eq!(images.len(), 2);
    assert_eq!(images[0]["path"], "site/hero.png");
    assert_eq!(images[1]["path"], "site/hero-2.png");
    assert_eq!(outcome.result["text"], "done\ndone");
    let first = crate::files::BlobRef::parse(images[0]["ref"].as_str().unwrap()).unwrap();
    let stored = crate::files::BlobStore::get(files.as_ref(), &first)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(workspace.path().join("site/hero.png")).unwrap(),
        stored
    );
    assert!(workspace.path().join("site/hero-2.png").is_file());
}

#[tokio::test]
async fn generate_image_returns_tool_errors_the_model_can_fix() {
    let (registry, painter, _, _) = image_registry();
    let call = |input: Value| registry.invoke("builtin__generate_image", input);

    let outcome = call(json!({"prompt": "x", "size": "4K"})).await.unwrap();
    assert!(outcome.is_error);
    assert_eq!(
        outcome.result["error"],
        "size \"4K\" isn't supported: nano supports \"1K\""
    );
    let outcome = call(json!({"prompt": "x", "images": ["cat.png"]}))
        .await
        .unwrap();
    assert!(outcome.result["error"]
        .as_str()
        .unwrap()
        .contains("isn't a file ref"));
    let outcome = call(json!({"prompt": "x", "path": "../escape.png"}))
        .await
        .unwrap();
    assert!(outcome.result["error"]
        .as_str()
        .unwrap()
        .contains("stay inside the workspace"));
    let outcome = call(json!({"prompt": "x", "model": "chat"})).await.unwrap();
    assert!(outcome.is_error);
    assert!(painter.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn generate_image_is_only_offered_when_an_image_role_resolves() {
    async fn offered(registry: &UserToolRegistry) -> bool {
        registry
            .tools()
            .await
            .unwrap()
            .into_iter()
            .any(|tool| tool.name == "builtin__generate_image")
    }
    let (registry, _, _, _) = image_registry();
    assert!(offered(&registry).await);
    let without = UserToolRegistry::builtins(load_builtin_tools().unwrap(), router());
    assert!(!offered(&without).await);
}

#[tokio::test]
async fn exec_tools_take_blob_inputs_as_paths_and_store_files_written_to_dive_out() {
    let tool = doc(r#"
name: thumb
description: copy an image
kind: exec
command: sh
args: ["-c", "cp \"$0\" \"$DIVE_OUT/copy.png\" && printf '{\"ok\": true}'", "{{input.image}}"]
input_schema:
  type: object
  required: [image]
  properties:
    image: { type: string, format: blob }
"#);
    let files = Arc::new(MemoryFiles::default());
    let original = crate::files::BlobStore::put(files.as_ref(), b"pixels".to_vec(), "image/png")
        .await
        .unwrap();
    let registry = registry(vec![tool]).with_files(files.clone());
    let outcome = registry
        .invoke("user__thumb", json!({"image": original.blob.to_string()}))
        .await
        .unwrap();
    assert!(!outcome.is_error, "{}", outcome.result);
    assert_eq!(outcome.result["ok"], true);
    let listed = outcome.result["files"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["mime"], "image/png");
    let copy = crate::files::BlobRef::parse(listed[0]["ref"].as_str().unwrap()).unwrap();
    assert_eq!(
        crate::files::BlobStore::get(files.as_ref(), &copy)
            .await
            .unwrap(),
        b"pixels".to_vec()
    );

    let outcome = registry
        .invoke("user__thumb", json!({"image": "not-a-ref"}))
        .await
        .unwrap();
    assert!(outcome.is_error);
    assert_eq!(
        outcome.result["error"],
        "input.image must be a file ref like blob:sha256-….png"
    );
}

#[test]
fn a_saved_image_takes_the_extension_of_its_type() {
    let cases = [
        ("site/hero.png", 0, "image/jpeg", "site/hero.jpg"),
        ("site/hero.jpeg", 1, "image/jpeg", "site/hero-2.jpeg"),
        ("site/hero.png", 2, "image/png", "site/hero-3.png"),
        ("site/hero", 0, "image/webp", "site/hero.webp"),
    ];
    for (path, index, mime, expected) in cases {
        assert_eq!(
            saved_path(std::path::Path::new(path), index, mime),
            std::path::PathBuf::from(expected),
            "{path} as {mime}"
        );
    }
}
