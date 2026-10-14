use super::*;
use crate::agent::doc::AgentSet;
use crate::pipeline::catalog::{resolve_plan_tools_deep, resolve_step_tools, ToolCatalog};
use crate::pipeline::doc::PlanDoc;
use crate::pipeline::{Pipeline, Plan};
use crate::tools::ToolRegistry;
use crate::user_tools::UserToolRegistry;
use crate::NullSink;
use async_trait::async_trait;
use graph_config::{ModelChoice, ModelRoles};
use graph_llm::types::{ChatRequest, ChatResponse, EventStream, StopReason, Usage};
use graph_llm::{ChatProvider, LlmError};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

const MANIFEST: &str = "version = 1\nname = \"example\"\ndescription = \"files\"\n";

const ECHO_TOOL: &str = r#"version: 2
name: echo
description: Echoes a name
kind: exec
command: echo
args: ['{"name": "{{input.name}}", "args": "$pack__tool__echo $HOME"}']
input_schema:
  type: object
  required: [name]
  properties:
    name: { type: string }
"#;

const SELF_PLAN: &str = r#"version: 2
identifier: whoami
name: Who am I
description: Echoes the installed pack name
input_schema:
  type: object
  properties: {}
steps:
  - id: a
    tool_name: $pack__tool__echo
    input:
      name: "{{pack.name}}"
finish:
  output:
    name: "{{a.name}}"
    args: "{{a.args}}"
"#;

const OUTER_PLAN: &str = r#"version: 2
identifier: outer
name: Outer
description: Calls the pack's own plan
steps:
  - id: inner
    tool_name: $pack__plan__whoami
    input: {}
finish:
  output:
    name: "{{inner.name}}"
"#;

const HELPER_AGENT: &str = r#"version: 1
name: helper
description: Helps with files
model: chat
tools: [$pack__tool__echo, $pack__plan__whoami]
system_prompt: You work for the {{pack.name}} pack; call {{pack.name}}__tool__echo.
"#;

fn write(dir: &Path, path: &str, content: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn example_pack(dir: &Path) {
    write(dir, "pack.toml", MANIFEST);
    write(dir, "README.md", "# example\n");
    write(dir, "tools/echo.yaml", ECHO_TOOL);
    write(dir, "plans/whoami.yaml", SELF_PLAN);
    write(dir, "plans/outer.yaml", OUTER_PLAN);
    write(dir, "agents/helper.yaml", HELPER_AGENT);
    write(dir, "plans/migrations/migrate_v2.yaml", "not: [valid");
    write(dir, "tools/migrations/migrate_v2_scan.yaml", "not: [valid");
    write(dir, "tests/migrations/v2/plans/x.yaml", "not: [valid");
}

fn source(name: &str, dir: &Path) -> PackSource {
    PackSource {
        name: name.to_string(),
        tag: Some("v1.0.0".to_string()),
        layer: LayerKind::Project,
        dir: dir.to_path_buf(),
    }
}

fn loaded(name: &str) -> (tempfile::TempDir, LoadedPack) {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    let pack = load_pack(source(name, dir.path()), &[], &[]).unwrap();
    (dir, pack)
}

#[test]
fn a_pack_loads_its_items_under_catalog_names_and_skips_migrations() {
    let (_dir, pack) = loaded("example");
    let names: Vec<String> = pack.items.iter().map(PackItem::catalog_name).collect();
    assert_eq!(
        names,
        [
            "example__tool__echo",
            "example__plan__outer",
            "example__plan__whoami",
            "example__agent__helper"
        ]
    );
    assert!(pack.items.iter().all(|item| item.public));
    assert_eq!(pack.tools[0].name, "echo");
    let whoami = pack
        .plans
        .iter()
        .find(|doc| doc.identifier == "example__plan__whoami")
        .unwrap();
    assert_eq!(whoami.steps[0].tool_name, "example__tool__echo");
    assert_eq!(whoami.tool_name(), "example__plan__whoami");
    assert_eq!(whoami.pack_name(), Some("example"));
    assert_eq!(whoami.source_label(), "pack example v1.0.0 (project)");
    let helper = &pack.agents[0];
    assert_eq!(helper.name, "example__agent__helper");
    assert_eq!(helper.tool_name(), "example__agent__helper");
    assert_eq!(
        helper.tools,
        ["example__tool__echo", "example__plan__whoami"]
    );
    assert_eq!(helper.source.describe(), "pack example v1.0.0 (project)");
    let crate::user_tools::ToolKind::Exec { args, .. } = &pack.tools[0].kind else {
        panic!("echo is an exec tool");
    };
    assert!(args[0].contains("$pack__tool__echo $HOME"));
}

#[test]
fn the_installed_name_drives_every_resolved_name() {
    let (_dir, pack) = loaded("files");
    assert!(pack
        .plans
        .iter()
        .any(|doc| doc.identifier == "files__plan__outer"
            && doc.steps[0].tool_name == "files__plan__whoami"));
}

#[test]
fn a_catalog_name_over_64_characters_is_refused_with_the_remedy() {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    let long = "a".repeat(36);
    write(
        dir.path(),
        &format!("agents/{long}.yaml"),
        &HELPER_AGENT.replace("name: helper", &format!("name: {long}")),
    );
    let pack_name = "abcdefghijklmnopqrst";
    let errors = load_pack(source(pack_name, dir.path()), &[], &[]).unwrap_err();
    let joined = errors.join("\n");
    assert!(
        joined.contains(&format!("{pack_name}__agent__{long} (65)")),
        "{joined}"
    );
    assert!(joined.contains("--as"), "{joined}");

    std::fs::remove_file(dir.path().join(format!("agents/{long}.yaml"))).unwrap();
    let fits = "a".repeat(35);
    write(
        dir.path(),
        &format!("agents/{fits}.yaml"),
        &HELPER_AGENT.replace("name: helper", &format!("name: {fits}")),
    );
    assert!(load_pack(source(pack_name, dir.path()), &[], &[]).is_ok());
}

#[test]
fn reserved_and_colliding_names_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    let errors = load_pack(source("plan", dir.path()), &[], &[]).unwrap_err();
    assert!(errors.join("\n").contains("reserved"), "{errors:?}");
    let servers = std::collections::BTreeSet::from(["linear".to_string()]);
    let error = check_installed_name("linear", &servers).unwrap_err();
    assert!(error.contains("[mcp.linear]"), "{error}");
    assert!(check_installed_name("example", &servers).is_ok());
    assert!(check_installed_name("bad__name", &servers).is_err());
}

#[test]
fn a_broken_pack_file_or_a_bad_reference_refuses_the_pack() {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    write(
        dir.path(),
        "plans/bad.yaml",
        &SELF_PLAN
            .replace("identifier: whoami", "identifier: bad")
            .replace("$pack__tool__echo", "$pack__widget__echo"),
    );
    let errors = load_pack(source("example", dir.path()), &[], &[]).unwrap_err();
    assert!(errors[0].starts_with("pack 'example': "), "{errors:?}");
    assert!(errors[0].contains("$pack__widget__echo"), "{errors:?}");
}

#[test]
fn pack_tools_keep_the_input_only_template_rule() {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    write(
        dir.path(),
        "tools/named.yaml",
        &ECHO_TOOL
            .replace("name: echo", "name: named")
            .replace("{{input.name}}", "{{pack.name}}"),
    );
    let errors = load_pack(source("example", dir.path()), &[], &[]).unwrap_err();
    assert!(errors.join("\n").contains("{{input.*}}"), "{errors:?}");
}

#[test]
fn the_pack_root_is_only_for_pack_files() {
    let user: PlanDoc = SELF_PLAN
        .replace("$pack__tool__echo", "user__echo")
        .parse()
        .unwrap();
    let error = crate::pipeline::doc::validate_doc(&user).unwrap_err();
    assert!(error.contains("pack"), "{error}");

    let with_step: PlanDoc = r#"version: 2
identifier: legacy
name: Legacy
description: A user plan with a step called pack
steps:
  - id: pack
    tool_name: user__echo
    input: { name: x }
  - id: b
    tool_name: user__echo
    input: { name: "{{pack.name}}" }
finish:
  output:
    name: "{{b.name}}"
"#
    .parse()
    .unwrap();
    crate::pipeline::doc::validate_doc(&with_step).unwrap();
    assert!(crate::pipeline::authoring::static_problems(&with_step).is_empty());

    let (_dir, pack) = loaded("example");
    let mut clash = pack.plans[1].clone();
    clash.steps[0].id = "pack".to_string();
    assert!(crate::pipeline::doc::validate_doc(&clash)
        .unwrap_err()
        .contains("duplicate step id 'pack'"));
}

struct Scripted {
    responses: Mutex<Vec<ChatResponse>>,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl ChatProvider for Scripted {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.requests.lock().unwrap().push(req);
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Err(LlmError::Parse("script exhausted".into()));
        }
        Ok(responses.remove(0))
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        use futures::StreamExt;
        let response = self.chat(req).await?;
        Ok(
            futures::stream::iter(vec![Ok(graph_llm::types::StreamEvent::Completed(response))])
                .boxed(),
        )
    }
}

fn answer(text: &str) -> ChatResponse {
    ChatResponse {
        content: Some(text.to_string()),
        tool_calls: vec![],
        thinking: Vec::new(),
        structured: None,
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

fn pipeline_over(
    packs: &[LoadedPack],
    user_plans: Vec<PlanDoc>,
    responses: Vec<ChatResponse>,
) -> (Pipeline, Arc<Scripted>) {
    let provider = Arc::new(Scripted {
        responses: Mutex::new(responses),
        requests: Mutex::new(Vec::new()),
    });
    let mut providers: std::collections::HashMap<String, Arc<dyn ChatProvider>> =
        std::collections::HashMap::new();
    providers.insert("mock".to_string(), provider.clone());
    let router = Arc::new(graph_llm::ModelRouter::with_providers(
        providers,
        ModelRoles::from([(
            "default",
            ModelChoice {
                provider: "mock".into(),
                model: "m".into(),
                temperature: None,
                description: None,
                fallbacks: Vec::new(),
                context_window: None,
            },
        )]),
    ));
    let user_tools = vec![crate::user_tools::parse_tool_source(ECHO_TOOL).unwrap()];
    let mut sources: Vec<Arc<dyn ToolRegistry>> =
        vec![Arc::new(UserToolRegistry::new(user_tools, router.clone()))];
    let mut set = PackSet::default();
    let mut plans = user_plans;
    let mut agents = Vec::new();
    for pack in packs {
        sources.push(Arc::new(UserToolRegistry::pack(
            &pack.source.name,
            pack.tools.clone(),
            router.clone(),
        )));
        set.install(&pack.source.name);
        for item in &pack.items {
            set.insert(item.clone());
        }
        plans.extend(pack.plans.iter().cloned());
        agents.extend(pack.agents.iter().cloned());
    }
    (
        Pipeline {
            router,
            registry: Arc::new(crate::CompositeRegistry::new(sources)),
            events: Arc::new(NullSink),
            plans: Arc::new(plans),
            call_stack: Vec::new(),
            store: None,
            gate: None,
            interlocutor: None,
            catalog: None,
            user_context: String::new(),
            current_date: "2026-10-13".into(),
            max_attempts: 1,
            max_agent_iterations: 5,
            usage: Arc::new(crate::usage::UsageLedger::unpriced()),
            agents: Arc::new(AgentSet::default().with_packs(agents)),
            agent_depth: 0,
            always_loaded: Default::default(),
            drafted: Default::default(),
            packs: Arc::new(set),
        },
        provider,
    )
}

fn steps(value: Value) -> Plan {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn pack_plans_render_the_pack_root_and_route_to_their_own_tools() {
    let (_dir, pack) = loaded("example");
    let (pipeline, _) = pipeline_over(&[pack], Vec::new(), Vec::new());
    let call = pipeline.call_plan("example__plan__outer", json!({})).await;
    assert!(!call.is_error, "{}", call.result);
    assert_eq!(call.result, json!({"name": "example"}));

    let direct = pipeline.call_plan("example__plan__whoami", json!({})).await;
    assert_eq!(direct.result["args"], "$pack__tool__echo $HOME");
}

#[tokio::test]
async fn user_plans_route_pack_names_by_their_second_segment() {
    let (_dir, pack) = loaded("example");
    let (pipeline, provider) = pipeline_over(&[pack], Vec::new(), vec![answer("helped")]);
    let outcome = pipeline
        .run_explicit(
            "run",
            steps(json!([
                {"id": "t", "toolName": "example__tool__echo", "input": {"name": "direct"}},
                {"id": "p", "toolName": "example__plan__whoami", "input": {}},
                {"id": "g", "toolName": "example__agent__helper", "input": {"prompt": "help"}},
            ])),
            crate::pipeline::Finish::Output(
                serde_json::from_value(json!({
                    "tool": "{{t.name}}",
                    "plan": "{{p.name}}",
                    "agent": "{{g.result}}",
                }))
                .unwrap(),
            ),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.structured,
        Some(json!({"tool": "direct", "plan": "example", "agent": "helped"}))
    );
    let requests = provider.requests.lock().unwrap();
    assert!(requests[0]
        .system
        .contains("You work for the example pack; call example__tool__echo."));
    let offered: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(offered, ["example__tool__echo", "example__plan__whoami"]);
}

#[tokio::test]
async fn generic_prefixes_never_alias_pack_items() {
    let (_dir, pack) = loaded("example");
    let (pipeline, _) = pipeline_over(&[pack], Vec::new(), Vec::new());
    for alias in [
        "plan__example__plan__whoami",
        "agent__example__agent__helper",
    ] {
        let error = pipeline
            .run_explicit(
                "run",
                steps(json!([{"id": "a", "toolName": alias, "input": {"prompt": "x"}}])),
                crate::pipeline::Finish::Silent,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("plan failed at step a"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn a_user_step_named_pack_still_runs() {
    let user: PlanDoc = r#"version: 2
identifier: legacy
name: Legacy
description: A user plan with a step called pack
steps:
  - id: pack
    tool_name: user__echo
    input: { name: from-step }
  - id: b
    tool_name: user__echo
    input: { name: "{{pack.name}}" }
finish:
  output:
    name: "{{b.name}}"
"#
    .parse()
    .unwrap();
    let (_dir, pack) = loaded("example");
    let (pipeline, _) = pipeline_over(&[pack], vec![user], Vec::new());
    let call = pipeline.call_plan("legacy", json!({})).await;
    assert_eq!(call.result, json!({"name": "from-step"}));
}

fn catalog_for(pack: &LoadedPack) -> ToolCatalog {
    let mut set = PackSet::default();
    set.install(&pack.source.name);
    for item in &pack.items {
        set.insert(item.clone());
    }
    ToolCatalog {
        builtin_tools: Default::default(),
        user_tools: ["user__echo".to_string()].into(),
        plans: Default::default(),
        mcp_servers: ["linear".to_string()].into(),
        agents: Default::default(),
        input_schemas: [(
            "example__tool__echo".to_string(),
            pack.tools[0].input_schema.clone().unwrap(),
        )]
        .into(),
        packs: set,
    }
}

#[test]
fn the_static_check_routes_pack_names() {
    let (_dir, pack) = loaded("example");
    let catalog = catalog_for(&pack);
    let errors = resolve_step_tools(
        &steps(json!([
            {"id": "a", "toolName": "example__tool__echo", "input": {"name": "x"}},
            {"id": "b", "toolName": "example__plan__whoami", "input": {}},
            {"id": "c", "toolName": "example__agent__helper", "input": {"prompt": "x"}},
            {"id": "d", "toolName": "example__tool__missing", "input": {}},
            {"id": "e", "toolName": "example__agent__nobody", "input": {}},
            {"id": "f", "toolName": "plan__example__plan__whoami", "input": {}},
            {"id": "g", "toolName": "example__tool__echo", "input": {}},
            {"id": "h", "toolName": "agent", "input": {"prompt": "x", "tools": ["example__plan__*", "example__widget__*"]}},
            {"id": "i", "toolName": "linear__list_issues", "input": {}},
        ])),
        &catalog,
    );
    assert_eq!(errors.len(), 5, "{errors:#?}");
    assert!(errors[0].contains("step d: unknown tool 'example__tool__missing'"));
    assert!(errors[1].contains("step e: unknown agent 'example__agent__nobody'"));
    assert!(errors[2].contains("step f: unknown plan 'example__plan__whoami'"));
    assert!(errors[3].contains("step g: input for example__tool__echo"));
    assert!(errors[4].contains("'example__widget__*' matches nothing in pack 'example'"));
}

#[test]
fn the_deep_check_follows_pack_plan_calls() {
    let (_dir, pack) = loaded("example");
    let catalog = catalog_for(&pack);
    let mut broken = pack.clone();
    let whoami = broken
        .plans
        .iter_mut()
        .find(|doc| doc.identifier == "example__plan__whoami")
        .unwrap();
    whoami.steps[0].tool_name = "example__tool__gone".to_string();
    let outer = broken
        .plans
        .iter()
        .find(|doc| doc.identifier == "example__plan__outer")
        .unwrap();
    let check = resolve_plan_tools_deep(outer, &broken.plans, &catalog);
    assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
    assert!(check.errors[0].starts_with("plan 'example__plan__whoami': step a: unknown tool"));
}
