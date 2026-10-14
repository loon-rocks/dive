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
    assert_eq!(whoami.pack.as_ref().unwrap().name, "example");
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
        set.add(pack);
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
            origin: Default::default(),
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
    set.add(pack);
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

fn loaded_excluding(exclude: &[&str]) -> (tempfile::TempDir, LoadedPack) {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    let exclude: Vec<String> = exclude.iter().map(|name| name.to_string()).collect();
    let pack = load_pack(source("example", dir.path()), &[], &exclude).unwrap();
    (dir, pack)
}

#[test]
fn the_entry_exclude_list_narrows_the_loaded_interface() {
    let (_dir, pack) = loaded_excluding(&["tool__echo", "agent__*"]);
    let public: Vec<String> = pack
        .items
        .iter()
        .filter(|item| item.public)
        .map(PackItem::catalog_name)
        .collect();
    assert_eq!(public, ["example__plan__outer", "example__plan__whoami"]);
}

#[test]
fn the_static_check_takes_the_origin_from_the_plan_file() {
    let (_dir, pack) = loaded_excluding(&["tool__echo"]);
    let catalog = catalog_for(&pack);
    let whoami = pack
        .plans
        .iter()
        .find(|doc| doc.identifier == "example__plan__whoami")
        .unwrap();
    let own = crate::pipeline::catalog::resolve_plan_tools(whoami, &catalog);
    assert!(own.is_ok(), "{:?}", own.errors);

    let mut user = whoami.clone();
    user.identifier = "snoop".to_string();
    user.pack = None;
    let outside = crate::pipeline::catalog::resolve_plan_tools(&user, &catalog);
    assert!(
        outside
            .errors
            .iter()
            .any(|error| error.contains("example__tool__echo") && error.contains("pack 'example'")),
        "{:?}",
        outside.errors
    );
}

#[tokio::test]
async fn an_excluded_tool_is_refused_to_user_plans_and_open_to_its_pack() {
    let (_dir, pack) = loaded_excluding(&["tool__echo"]);
    let (pipeline, _) = pipeline_over(&[pack], Vec::new(), Vec::new());
    let refused = pipeline
        .run_explicit(
            "run",
            steps(json!([{"id": "a", "toolName": "example__tool__echo", "input": {"name": "x"}}])),
            crate::pipeline::Finish::Silent,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("pack 'example'"), "{refused}");

    let call = pipeline.call_plan("example__plan__outer", json!({})).await;
    assert!(!call.is_error, "{}", call.result);
    assert_eq!(call.result, json!({"name": "example"}));

    let whoami = pipeline
        .plans
        .iter()
        .find(|doc| doc.identifier == "example__plan__whoami")
        .unwrap()
        .clone();
    let direct = pipeline
        .run_explicit_in(
            "run",
            whoami.steps.clone(),
            whoami.finish.clone(),
            Some(json!({})),
            whoami.pack.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(direct.structured.unwrap()["name"], "example");
}

#[tokio::test]
async fn the_agent_toolbox_routes_pack_items_behind_the_visibility_check() {
    let (_dir, pack) = loaded_excluding(&["plan__outer"]);
    let plans = pack.plans.clone();
    let (pipeline, _) = pipeline_over(&[pack], Vec::new(), vec![answer("helped")]);
    let pipeline = Arc::new(pipeline);
    let toolbox =
        crate::toolbox::AgentToolbox::new(pipeline.registry.clone(), pipeline.clone(), plans);
    let names: Vec<String> = toolbox
        .tools()
        .await
        .unwrap()
        .into_iter()
        .map(|def| def.name)
        .collect();
    assert!(names.contains(&"example__plan__whoami".to_string()));
    assert!(!names.contains(&"example__plan__outer".to_string()));

    let plan = toolbox
        .invoke("example__plan__whoami", json!({}))
        .await
        .unwrap();
    assert!(!plan.is_error, "{}", plan.result);
    assert_eq!(plan.result["name"], "example");

    let agent = toolbox
        .invoke("example__agent__helper", json!({"prompt": "help"}))
        .await
        .unwrap();
    assert!(!agent.is_error, "{}", agent.result);

    let hidden = toolbox
        .invoke("example__plan__outer", json!({}))
        .await
        .unwrap();
    assert!(hidden.is_error);
    assert!(hidden.result.to_string().contains("pack 'example'"));
}

fn pack_requiring(requires: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    write(
        dir.path(),
        "pack.toml",
        &format!("{MANIFEST}\n[requires]\n{requires}\n"),
    );
    dir
}

fn other_os() -> &'static str {
    if std::env::consts::OS == "linux" {
        "macos"
    } else {
        "linux"
    }
}

#[test]
fn a_pack_needing_a_newer_graph_or_another_os_is_refused_by_name() {
    let dir = pack_requiring("graph = \">=99\"");
    let errors = load_pack(source("example", dir.path()), &[], &[]).unwrap_err();
    assert_eq!(
        errors,
        [format!(
            "pack 'example': requires.graph: needs graph >=99; this is graph {}",
            env!("CARGO_PKG_VERSION")
        )]
    );

    let dir = pack_requiring(&format!("os = [\"{}\"]", other_os()));
    let errors = load_pack(source("example", dir.path()), &[], &[]).unwrap_err();
    assert_eq!(
        errors,
        [format!(
            "pack 'example': requires.os: needs {}; this machine runs {}",
            other_os(),
            std::env::consts::OS
        )]
    );

    let dir = pack_requiring(&format!(
        "graph = \">=0.1\"\nos = [\"{}\"]",
        std::env::consts::OS
    ));
    assert!(load_pack(source("example", dir.path()), &[], &[]).is_ok());
}

const MISSING: &str =
    "binaries = [\"graph-pack-no-such-binary\"]\nenv = [\"GRAPH_PACK_TEST_NEVER_SET\"]";

#[test]
fn a_pack_missing_a_binary_or_env_var_loads_with_the_gaps_recorded() {
    let dir = pack_requiring(MISSING);
    let pack = load_pack(source("example", dir.path()), &[], &[]).unwrap();
    assert_eq!(
        pack.needs.unmet,
        [
            "requires.binaries: graph-pack-no-such-binary not found on PATH",
            "requires.env: GRAPH_PACK_TEST_NEVER_SET not set (or empty)",
        ]
    );
    let mut set = PackSet::default();
    set.add(&pack);
    assert_eq!(set.needs["example"], pack.needs);
}

#[test]
fn mcp_requirements_record_the_literal_server_tools_plans_and_agents_name() {
    let dir = pack_requiring("[requires.mcp.everything]\nserver = \"mcp-servers/everything\"");
    write(
        dir.path(),
        "plans/say.yaml",
        r#"version: 2
identifier: say
name: Say
description: Echoes through the server
steps:
  - id: a
    tool_name: everything__echo
    input:
      message: hi
  - id: b
    tool_name: linear__list_issues
    input: {}
finish:
  output:
    said: "{{a.text}}"
"#,
    );
    write(
        dir.path(),
        "agents/talker.yaml",
        "version: 1\nname: talker\ndescription: Talks\nmodel: chat\ntools: [everything__echo, everything__add, \"everything__*\"]\nsystem_prompt: Talk.\n",
    );
    let pack = load_pack(source("example", dir.path()), &[], &[]).unwrap();
    let need = &pack.needs.mcp["everything"];
    assert_eq!(
        need.requirement.server.as_deref(),
        Some("mcp-servers/everything")
    );
    let uses: Vec<(&str, &str, Option<Value>)> = need
        .uses
        .iter()
        .map(|found| {
            (
                found.site.as_str(),
                found.tool.as_str(),
                found.input.clone().map(Value::Object),
            )
        })
        .collect();
    assert_eq!(
        uses,
        [
            (
                "plan 'example__plan__say' step a",
                "everything__echo",
                Some(json!({"message": "hi"}))
            ),
            (
                "agent 'example__agent__talker' tools",
                "everything__echo",
                None
            ),
            (
                "agent 'example__agent__talker' tools",
                "everything__add",
                None
            ),
        ]
    );
}

#[derive(Default)]
struct CountingGate {
    calls: Mutex<usize>,
}

#[async_trait]
impl crate::pipeline::ExecutionGate for CountingGate {
    async fn before_tool(
        &self,
        _ctx: crate::pipeline::GateContext<'_>,
    ) -> crate::pipeline::GateDecision {
        *self.calls.lock().unwrap() += 1;
        crate::pipeline::GateDecision::Proceed
    }
}

fn reaching_pack_plan() -> Plan {
    steps(json!([
        {"id": "first", "toolName": "user__echo", "input": {"name": "first"}},
        {"id": "p", "toolName": "plan__wrap", "input": {}},
    ]))
}

fn wrap_plan() -> PlanDoc {
    r#"version: 2
identifier: wrap
name: Wrap
description: Calls the pack plan
steps:
  - id: w
    tool_name: example__plan__whoami
    input: {}
finish:
  output:
    name: "{{w.name}}"
"#
    .parse()
    .unwrap()
}

#[tokio::test]
async fn a_run_that_can_reach_a_pack_with_unmet_requirements_fails_before_its_first_step() {
    let dir = pack_requiring(MISSING);
    let pack = load_pack(source("example", dir.path()), &[], &[]).unwrap();
    let (mut pipeline, _) = pipeline_over(&[pack], vec![wrap_plan()], Vec::new());
    let gate = Arc::new(CountingGate::default());
    pipeline.gate = Some(gate.clone());

    let error = pipeline
        .run_explicit(
            "run",
            reaching_pack_plan(),
            crate::pipeline::Finish::Silent,
            None,
        )
        .await
        .err()
        .unwrap();
    let crate::pipeline::PipelineError::UnmetRequirements(message) = &error else {
        panic!("expected unmet requirements, got {error}");
    };
    assert_eq!(
        message,
        "pack 'example' has unmet requirements: requires.binaries: graph-pack-no-such-binary not found on PATH; requires.env: GRAPH_PACK_TEST_NEVER_SET not set (or empty)"
    );
    assert_eq!(*gate.calls.lock().unwrap(), 0);

    let direct = pipeline.call_plan("example__plan__outer", json!({})).await;
    assert!(direct.is_error);
    assert!(
        direct
            .result
            .to_string()
            .contains("graph-pack-no-such-binary"),
        "{}",
        direct.result
    );
    assert_eq!(*gate.calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_run_that_cannot_reach_the_pack_runs() {
    let dir = pack_requiring(MISSING);
    let pack = load_pack(source("example", dir.path()), &[], &[]).unwrap();
    let (mut pipeline, _) = pipeline_over(&[pack], vec![wrap_plan()], Vec::new());
    let gate = Arc::new(CountingGate::default());
    pipeline.gate = Some(gate.clone());
    pipeline
        .run_explicit(
            "run",
            steps(json!([{"id": "only", "toolName": "user__echo", "input": {"name": "x"}}])),
            crate::pipeline::Finish::Silent,
            None,
        )
        .await
        .ok()
        .unwrap();
    assert_eq!(*gate.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_run_reaching_a_pack_whose_mcp_server_is_not_configured_fails_naming_it() {
    let dir = pack_requiring(
        "[requires.mcp.everything]\nserver = \"mcp-servers/everything\"\nurl = \"https://example.com/mcp\"",
    );
    let pack = load_pack(source("example", dir.path()), &[], &[]).unwrap();
    let (pipeline, _) = pipeline_over(&[pack], vec![wrap_plan()], Vec::new());
    let error = pipeline
        .run_explicit(
            "run",
            reaching_pack_plan(),
            crate::pipeline::Finish::Silent,
            None,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "pack 'example' has unmet requirements: requires.mcp.everything: no MCP server is configured under [mcp.everything] (url = \"https://example.com/mcp\")"
    );
}

const MIGRATE_PLAN: &str = r#"version: 2
identifier: migrate_v2
name: Migrate
description: Calls the pack's migration tool
input_schema:
  type: object
  properties:
    root: { type: string }
    apply: { type: boolean }
steps:
  - id: a
    tool_name: $pack__tool__rewrite
    input:
      name: "{{input.root}}"
finish:
  output:
    name: "{{a.name}}"
"#;

#[test]
fn migrations_load_only_on_request_as_private_items_under_the_installed_name() {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    write(dir.path(), "plans/migrations/migrate_v2.yaml", MIGRATE_PLAN);
    std::fs::remove_file(dir.path().join("tools/migrations/migrate_v2_scan.yaml")).unwrap();
    write(
        dir.path(),
        "tools/migrations/rewrite.yaml",
        &ECHO_TOOL.replace("name: echo", "name: rewrite"),
    );
    let mut pack = load_pack(source("files", dir.path()), &[], &[]).unwrap();
    assert!(pack.migrations.is_empty());
    assert!(!pack.items.iter().any(|item| item.name == "rewrite"));

    assert!(pack.load_migrations().is_empty());
    assert_eq!(
        pack.migrations,
        ["files__tool__rewrite", "files__plan__migrate_v2"]
    );
    assert!(pack.is_migration("files__plan__migrate_v2"));
    assert!(!pack.is_migration("files__plan__whoami"));
    let migration = pack
        .items
        .iter()
        .find(|item| item.catalog_name() == "files__plan__migrate_v2")
        .unwrap();
    assert!(!migration.public);
    let plan = pack
        .plans
        .iter()
        .find(|doc| doc.identifier == "files__plan__migrate_v2")
        .unwrap();
    assert_eq!(plan.steps[0].tool_name, "files__tool__rewrite");

    write(dir.path(), "tools/migrations/echo.yaml", ECHO_TOOL);
    let mut pack = load_pack(source("files", dir.path()), &[], &[]).unwrap();
    let problems = pack.load_migrations();
    assert!(
        problems[0].contains("migration tool 'echo' has the same name as a tool in tools/"),
        "{problems:?}"
    );
}

#[test]
fn inspecting_a_broken_pack_keeps_what_loaded_and_lists_every_problem() {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    write(dir.path(), "pack.toml", "version = 1\nname = \"example\"\n");
    write(
        dir.path(),
        "plans/bad.yaml",
        &SELF_PLAN
            .replace("identifier: whoami", "identifier: bad")
            .replace("$pack__tool__echo", "$pack__widget__echo"),
    );
    let (pack, problems) = inspect_pack(source("example", dir.path()), &[], &[]);
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].contains("description"), "{problems:?}");
    assert!(problems[1].contains("$pack__widget__echo"), "{problems:?}");
    assert_eq!(pack.items.len(), 4);
    assert_eq!(pack.manifest.name, "example");
}

#[test]
fn literal_self_names_are_found_in_plans_migrations_and_agent_lists_but_not_in_prose() {
    let dir = tempfile::tempdir().unwrap();
    example_pack(dir.path());
    write(
        dir.path(),
        "plans/literal.yaml",
        &OUTER_PLAN
            .replace("identifier: outer", "identifier: literal")
            .replace("$pack__plan__whoami", "example__plan__whoami"),
    );
    write(
        dir.path(),
        "plans/migrations/migrate_v2.yaml",
        &MIGRATE_PLAN.replace("$pack__tool__rewrite", "example__tool__rewrite"),
    );
    write(
        dir.path(),
        "agents/literal.yaml",
        &HELPER_AGENT
            .replace("name: helper", "name: literal")
            .replace("$pack__tool__echo", "example__tool__echo"),
    );
    let problems = literal_self_names(dir.path(), "example");
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(
        problems[0].contains(
            "literal.yaml: step inner: names its own pack literally as 'example__plan__whoami'; write '$pack__plan__whoami'"
        ),
        "{problems:?}"
    );
    assert!(
        problems[1].contains("migrate_v2.yaml: step a:"),
        "{problems:?}"
    );
    assert!(
        problems[2].contains(
            "agents/literal.yaml: tools: names its own pack literally as 'example__tool__echo'"
        ),
        "{problems:?}"
    );
    assert!(literal_self_names(dir.path(), "files").is_empty());
}
