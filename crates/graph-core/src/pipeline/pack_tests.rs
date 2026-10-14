use super::catalog::{resolve_plan_tools, resolve_plan_tools_deep};
use super::doc::PlanDoc;
use super::*;
use crate::agent::doc::{parse_agent_source, AgentSet, AgentSource};
use crate::packs::{split_catalog_name, ItemKind, Origin, PackItem, PackSet, PackSource};
use crate::tools::{ToolDef, ToolError};
use crate::NullSink;
use async_trait::async_trait;
use graph_config::{ModelChoice, ModelRoles};
use graph_llm::types::{ChatResponse, EventStream, StopReason, Usage};
use graph_llm::{ChatProvider, LlmError};
use std::collections::BTreeSet;
use std::sync::Mutex;

const PRIVATE_TOOL: &str = "example__tool__file_info";

struct Recorder {
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl ChatProvider for Recorder {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.requests.lock().unwrap().push(req);
        Ok(ChatResponse {
            content: Some("done".into()),
            tool_calls: vec![],
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
        let response = self.chat(req).await?;
        Ok(
            futures::stream::iter(vec![Ok(graph_llm::types::StreamEvent::Completed(response))])
                .boxed(),
        )
    }
}

#[derive(Default)]
struct Tools {
    invoked: Mutex<Vec<String>>,
}

#[async_trait]
impl ToolRegistry for Tools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok([PRIVATE_TOOL, "t__echo"]
            .iter()
            .map(|name| ToolDef {
                name: name.to_string(),
                description: format!("{name} does a thing"),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
            })
            .collect())
    }

    async fn invoke(&self, name: &str, _input: Value) -> Result<ToolOutcome, ToolError> {
        self.invoked.lock().unwrap().push(name.to_string());
        Ok(ToolOutcome {
            result: json!({"from": name}),
            is_error: false,
        })
    }
}

fn packs() -> PackSet {
    let mut set = PackSet::default();
    for (pack, kind, name, public) in [
        ("example", ItemKind::Plan, "describe_file", true),
        ("example", ItemKind::Tool, "file_info", false),
        ("example", ItemKind::Agent, "reviewer", true),
        ("other", ItemKind::Plan, "peek", true),
    ] {
        set.insert(PackItem {
            pack: pack.to_string(),
            kind,
            name: name.to_string(),
            public,
        });
    }
    set
}

fn source_of(name: &str) -> Option<PackSource> {
    split_catalog_name(name).map(|(pack, _, _)| PackSource {
        name: pack.to_string(),
        tag: Some("v1.0.0".to_string()),
        layer: graph_config::pack::LayerKind::Project,
        dir: std::path::PathBuf::from(".graph/packs").join(pack),
    })
}

fn plan_doc(identifier: &str, tool: &str) -> PlanDoc {
    let mut doc = str::parse::<PlanDoc>(&format!(
        r#"
identifier: {identifier}
name: {identifier}
description: calls a tool
steps:
  - id: E0
    tool_name: {tool}
    input: {{}}
output:
  got: "{{{{E0}}}}"
"#
    ))
    .unwrap();
    doc.pack = source_of(identifier);
    doc
}

fn docs() -> Vec<PlanDoc> {
    vec![
        plan_doc("snoop", PRIVATE_TOOL),
        plan_doc("example__plan__describe_file", PRIVATE_TOOL),
        plan_doc("wrapper", "example__plan__describe_file"),
        plan_doc("other__plan__peek", PRIVATE_TOOL),
    ]
}

fn agent(name: &str) -> crate::agent::doc::AgentDoc {
    let mut doc = parse_agent_source(&format!(
        "name: {name}\ndescription: looks at files\nmodel: default\ntools: [\"*\"]\nsystem_prompt: Look.\n"
    ))
    .unwrap();
    if let Some(source) = source_of(name) {
        doc.source = AgentSource::Pack {
            source,
            path: std::path::PathBuf::from("agents/reviewer.yaml"),
        };
    }
    doc
}

fn pipeline() -> (Pipeline, Arc<Tools>, Arc<Recorder>) {
    let provider = Arc::new(Recorder {
        requests: Mutex::new(Vec::new()),
    });
    let mut providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::new();
    providers.insert("mock".into(), provider.clone());
    let router = Arc::new(ModelRouter::with_providers(
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
    let tools = Arc::new(Tools::default());
    let pipeline = Pipeline {
        router,
        registry: tools.clone(),
        events: Arc::new(NullSink),
        plans: Arc::new(docs()),
        call_stack: Vec::new(),
        store: None,
        gate: None,
        interlocutor: None,
        catalog: None,
        user_context: String::new(),
        current_date: "2026-10-13".into(),
        max_attempts: 1,
        max_agent_iterations: 3,
        usage: Arc::new(crate::usage::UsageLedger::unpriced()),
        agents: Arc::new(AgentSet::layered(
            vec![agent("example__agent__reviewer"), agent("helper")],
            Vec::new(),
        )),
        agent_depth: 0,
        always_loaded: Default::default(),
        drafted: Default::default(),
        packs: Arc::new(packs()),
        origin: Origin::Outside,
    };
    (pipeline, tools, provider)
}

fn catalog() -> ToolCatalog {
    ToolCatalog {
        plans: docs().into_iter().map(|doc| doc.identifier).collect(),
        packs: packs(),
        ..Default::default()
    }
}

fn find(identifier: &str) -> PlanDoc {
    docs()
        .into_iter()
        .find(|doc| doc.identifier == identifier)
        .unwrap()
}

fn names(defs: &[ToolDef]) -> BTreeSet<String> {
    defs.iter().map(|def| def.name.clone()).collect()
}

#[test]
fn a_user_plan_calling_an_excluded_pack_tool_fails_validation_naming_the_pack() {
    let check = resolve_plan_tools(&find("snoop"), &catalog());
    assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
    assert!(check.errors[0].contains(PRIVATE_TOOL), "{:?}", check.errors);
    assert!(
        check.errors[0].contains("pack 'example'"),
        "{:?}",
        check.errors
    );
}

#[test]
fn a_pack_plan_may_call_its_own_excluded_tool() {
    let check = resolve_plan_tools(&find("example__plan__describe_file"), &catalog());
    assert!(check.is_ok(), "{:?}", check.errors);
}

#[test]
fn a_user_plan_reaches_excluded_tools_through_a_public_pack_plan() {
    let check = resolve_plan_tools_deep(&find("wrapper"), &docs(), &catalog());
    assert!(check.is_ok(), "{:?}", check.errors);
}

#[test]
fn another_packs_plan_cannot_call_a_private_item() {
    let check = resolve_plan_tools(&find("other__plan__peek"), &catalog());
    assert_eq!(check.errors.len(), 1, "{:?}", check.errors);
    assert!(
        check.errors[0].contains("pack 'example'"),
        "{:?}",
        check.errors
    );
}

#[tokio::test]
async fn dispatch_refuses_an_outside_call_to_a_private_item() {
    let (pipeline, tools, _) = pipeline();
    let error = pipeline
        .dispatch(&StepPath::top("E0"), PRIVATE_TOOL, json!({}), &Map::new())
        .await
        .err()
        .unwrap();
    let DispatchError::Failed(message) = error else {
        panic!("expected a tool failure");
    };
    assert!(message.contains(PRIVATE_TOOL), "{message}");
    assert!(message.contains("pack 'example'"), "{message}");
    assert!(tools.invoked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dispatch_lets_the_owning_pack_call_its_private_item() {
    let (mut pipeline, tools, _) = pipeline();
    pipeline.origin = Origin::Pack("example".into());
    let result = pipeline
        .dispatch(&StepPath::top("E0"), PRIVATE_TOOL, json!({}), &Map::new())
        .await
        .ok()
        .unwrap();
    assert_eq!(result, json!({"from": PRIVATE_TOOL}));
    assert_eq!(*tools.invoked.lock().unwrap(), [PRIVATE_TOOL]);
}

#[tokio::test]
async fn a_user_plan_run_is_refused_the_private_tool() {
    let (pipeline, tools, _) = pipeline();
    let call = pipeline.call_plan("snoop", json!({})).await;
    assert!(call.is_error);
    let text = call.result.to_string();
    assert!(text.contains("pack 'example'"), "{text}");
    assert!(tools.invoked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_pack_plan_run_directly_runs_as_its_pack() {
    let doc = find("example__plan__describe_file");
    let (pipeline, tools, _) = pipeline();
    let refused = pipeline
        .run_explicit("q", doc.steps.clone(), doc.finish.clone(), None)
        .await;
    assert!(refused.is_err());
    pipeline
        .run_explicit_in(
            "q",
            doc.steps.clone(),
            doc.finish.clone(),
            None,
            doc.pack.as_ref(),
        )
        .await
        .ok()
        .unwrap();
    assert_eq!(*tools.invoked.lock().unwrap(), [PRIVATE_TOOL]);
}

#[tokio::test]
async fn a_public_pack_plan_called_by_a_user_plan_runs_its_private_tool() {
    let (pipeline, tools, _) = pipeline();
    let call = pipeline.call_plan("wrapper", json!({})).await;
    assert!(!call.is_error, "{:?}", call.result);
    assert_eq!(*tools.invoked.lock().unwrap(), [PRIVATE_TOOL]);
}

#[tokio::test]
async fn another_packs_plan_run_is_refused_the_private_tool() {
    let (pipeline, tools, _) = pipeline();
    let call = pipeline.call_plan("other__plan__peek", json!({})).await;
    assert!(call.is_error);
    assert!(call.result.to_string().contains("pack 'example'"));
    assert!(tools.invoked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_planner_catalog_hides_private_items() {
    let (pipeline, _, _) = pipeline();
    let shown = names(&pipeline.planner_tool_defs().await);
    assert!(!shown.contains(PRIVATE_TOOL), "{shown:?}");
    assert!(shown.contains("t__echo"));
    assert!(shown.contains("example__plan__describe_file"));
}

#[tokio::test]
async fn tool_search_hides_private_items() {
    let (pipeline, _, _) = pipeline();
    let listed = pipeline
        .call_native("builtin__catalog_tools", json!({}))
        .await
        .result;
    let listed: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(!listed.contains(&PRIVATE_TOOL), "{listed:?}");
    assert!(listed.contains(&"t__echo"), "{listed:?}");
    let described = pipeline
        .call_native(
            "builtin__describe_tools",
            json!({"tools": [PRIVATE_TOOL, "t__echo"], "schemas": false}),
        )
        .await
        .result;
    assert_eq!(described["tools"].as_array().unwrap().len(), 1);
    assert_eq!(described["tools"][0]["name"], json!("t__echo"));
    let context = pipeline
        .call_native(
            "builtin__compose_context",
            json!({"found": [{"tools": [PRIVATE_TOOL]}]}),
        )
        .await
        .result;
    assert_eq!(context["tools"], json!("(none beyond the tools above)"));
}

#[tokio::test]
async fn the_agent_toolbox_hides_and_refuses_private_items() {
    let (pipeline, tools, _) = pipeline();
    let pipeline = Arc::new(pipeline);
    let toolbox =
        crate::toolbox::AgentToolbox::new(tools.clone(), pipeline.clone(), docs().clone());
    let shown = names(&toolbox.tools().await.unwrap());
    assert!(!shown.contains(PRIVATE_TOOL), "{shown:?}");
    assert!(shown.contains("t__echo"));
    let outcome = toolbox.invoke(PRIVATE_TOOL, json!({})).await.unwrap();
    assert!(outcome.is_error);
    assert!(outcome.result.to_string().contains("pack 'example'"));
    assert!(tools.invoked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn named_agents_see_private_items_only_inside_their_own_pack() {
    let (pipeline, _, provider) = pipeline();
    let path = StepPath::top("E0");
    pipeline
        .run_subagent(&path, "helper", json!({"prompt": "look"}))
        .await
        .ok()
        .unwrap();
    pipeline
        .run_subagent(&path, "example__agent__reviewer", json!({"prompt": "look"}))
        .await
        .ok()
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    let offered = |index: usize| -> BTreeSet<String> {
        requests[index]
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    };
    assert!(!offered(0).contains(PRIVATE_TOOL), "{:?}", offered(0));
    assert!(offered(0).contains("t__echo"));
    assert!(offered(1).contains(PRIVATE_TOOL), "{:?}", offered(1));
}
