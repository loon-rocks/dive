use super::*;
use dive_config::{ModelChoice, ModelRoles};
use dive_llm::decision::DecisionResponse;
use dive_llm::types::{ChatRequest, ChatResponse, EventStream, Usage};
use dive_llm::{ChatProvider, DecisionProvider, LlmError};
use std::collections::HashMap;
use std::sync::Mutex;

struct WeightDecider {
    requests: Mutex<Vec<DecisionRequest>>,
}

#[async_trait]
impl DecisionProvider for WeightDecider {
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, LlmError> {
        self.requests.lock().unwrap().push(req.clone());
        let answers = req
            .questions
            .iter()
            .map(|(name, question)| {
                let Question::Choice { criteria, .. } = question else {
                    unreachable!()
                };
                let weights: BTreeMap<String, f64> = criteria
                    .iter()
                    .map(|(key, text)| (key.clone(), text.parse::<f64>().unwrap_or(0.5)))
                    .collect();
                let total: f64 = weights.values().sum();
                let probabilities: BTreeMap<String, f64> = weights
                    .into_iter()
                    .map(|(key, weight)| (key, weight / total))
                    .collect();
                let choice = probabilities
                    .iter()
                    .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                    .unwrap()
                    .0
                    .clone();
                (
                    name.clone(),
                    Answer::Choice {
                        choice,
                        confidence: 0.5,
                        probabilities,
                    },
                )
            })
            .collect();
        Ok(DecisionResponse {
            model: req.model,
            answers,
            usage: Usage::default(),
        })
    }
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

fn tools(with_decider: bool) -> (ListTools, Arc<WeightDecider>) {
    let decider = Arc::new(WeightDecider {
        requests: Mutex::new(Vec::new()),
    });
    let choice = |provider: &str, model: &str| ModelChoice {
        kind: None,
        provider: provider.into(),
        model: model.into(),
        temperature: None,
        description: None,
        fallbacks: Vec::new(),
        context_window: None,
    };
    let mut roles = vec![("default".to_string(), choice("mock", "m"))];
    if with_decider {
        roles.push(("decider".to_string(), choice("typesafe", "jev-latest")));
    }
    let providers: HashMap<String, Arc<dyn ChatProvider>> = HashMap::from([(
        "mock".to_string(),
        Arc::new(UnusedChat) as Arc<dyn ChatProvider>,
    )]);
    let router = ModelRouter::with_providers(providers, ModelRoles::from_iter(roles))
        .with_deciders(HashMap::from([(
            "typesafe".to_string(),
            decider.clone() as Arc<dyn DecisionProvider>,
        )]));
    (ListTools::new(Arc::new(router)), decider)
}

async fn call(name: &str, input: Value) -> ToolOutcome {
    tools(true).0.invoke(name, input).await.unwrap()
}

#[tokio::test]
async fn flatten_splices_nested_lists_to_the_given_depth() {
    let out = call(FLATTEN_TOOL, json!({"items": [[1, 2], [3, [4]], 5]})).await;
    assert_eq!(out.result, json!({"items": [1, 2, 3, [4], 5]}));
    let out = call(FLATTEN_TOOL, json!({"items": [[1, [2, [3]]]], "depth": 3})).await;
    assert_eq!(out.result, json!({"items": [1, 2, 3]}));
}

#[tokio::test]
async fn dedupe_keeps_the_first_of_each_key_and_every_item_missing_it() {
    let items = json!([
        {"url": "a", "n": 1},
        {"url": "b", "n": 2},
        {"url": "a", "n": 3},
        {"n": 4},
        {"n": 5}
    ]);
    let out = call(DEDUPE_TOOL, json!({"items": items, "by": "url"})).await;
    assert_eq!(
        out.result,
        json!({"removed": 1, "items": [{"url": "a", "n": 1}, {"url": "b", "n": 2}, {"n": 4}, {"n": 5}]})
    );
    let out = call(DEDUPE_TOOL, json!({"items": ["x", "y", "x"]})).await;
    assert_eq!(out.result, json!({"removed": 1, "items": ["x", "y"]}));
}

#[tokio::test]
async fn sort_is_stable_typed_and_puts_missing_keys_last() {
    let items = json!([
        {"id": 1, "meta": {"score": 10}},
        {"id": 2},
        {"id": 3, "meta": {"score": 9}},
        {"id": 4, "meta": {"score": 10}}
    ]);
    let ids = |out: ToolOutcome| -> Vec<i64> {
        out.result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_i64().unwrap())
            .collect()
    };
    let asc = call(SORT_TOOL, json!({"items": items, "by": "meta.score"})).await;
    assert_eq!(ids(asc), [3, 1, 4, 2]);
    let desc = call(
        SORT_TOOL,
        json!({"items": items, "by": "meta.score", "order": "desc"}),
    )
    .await;
    assert_eq!(ids(desc), [1, 4, 3, 2]);
    let dates = call(
        SORT_TOOL,
        json!({"items": ["2026-10-02", "2025-12-31", "2026-01-15"]}),
    )
    .await;
    assert_eq!(
        dates.result,
        json!({"items": ["2025-12-31", "2026-01-15", "2026-10-02"]})
    );
}

#[tokio::test]
async fn take_keeps_the_first_n() {
    let out = call(TAKE_TOOL, json!({"items": [1, 2, 3], "n": 2})).await;
    assert_eq!(out.result, json!({"items": [1, 2]}));
    let out = call(TAKE_TOOL, json!({"items": [1], "n": 5})).await;
    assert_eq!(out.result, json!({"items": [1]}));
}

#[tokio::test]
async fn a_list_that_is_not_a_list_is_a_tool_error() {
    let out = call(SORT_TOOL, json!({"items": "nope"})).await;
    assert!(out.is_error);
    assert!(out.result["error"]
        .as_str()
        .unwrap()
        .contains("invalid sort input"));
}

#[tokio::test]
async fn rank_asks_one_choice_over_the_items_with_the_context_as_state() {
    let (tools, decider) = tools(true);
    let items = json!([
        {"url": "a", "weight": "1"},
        {"url": "b", "weight": "6"},
        {"url": "c", "weight": "3"}
    ]);
    let out = tools
        .invoke(
            RANK_TOOL,
            json!({
                "state": {"question": "q"},
                "items": items,
                "instructions": "Which answers it?",
                "describe": "weight",
                "top_k": 2
            }),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.result);
    assert_eq!(
        out.result["items"],
        json!([{"url": "b", "weight": "6"}, {"url": "c", "weight": "3"}])
    );
    assert_eq!(out.result["ranked"][0]["probability"], json!(0.6));
    assert!(out.result.get("none").is_none());
    let requests = decider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].state, json!({"question": "q"}));
    let Question::Choice { criteria, .. } = &requests[0].questions["rank"] else {
        panic!()
    };
    let mut shown: Vec<&str> = criteria.values().map(String::as_str).collect();
    shown.sort();
    assert_eq!(shown, ["1", "3", "6"]);
    assert!(criteria.keys().all(|key| key.len() == 3));
}

#[tokio::test]
async fn rank_shows_the_whole_item_without_describe_and_reports_no_fit() {
    let (tools, decider) = tools(true);
    let out = tools
        .invoke(
            RANK_TOOL,
            json!({
                "state": "q",
                "items": [{"t": "x"}, {"t": "y"}],
                "instructions": "Which?",
                "none": "Nothing fits"
            }),
        )
        .await
        .unwrap();
    assert_eq!(out.result["none"], json!(0.333));
    assert_eq!(out.result["no_fit"], json!(false));
    let requests = decider.requests.lock().unwrap();
    let Question::Choice { criteria, .. } = &requests[0].questions["rank"] else {
        panic!()
    };
    assert_eq!(criteria["none"], "Nothing fits");
    assert!(criteria.values().any(|text| text == r#"{"t":"x"}"#));
}

#[tokio::test]
async fn rank_chunks_long_lists_then_ranks_the_winners() {
    let (tools, decider) = tools(true);
    let items: Vec<Value> = (0..600)
        .map(|i| json!({"id": i, "w": if i == 599 { "100" } else { "1" }}))
        .collect();
    let out = tools
        .invoke(
            RANK_TOOL,
            json!({"state": "q", "items": items, "instructions": "Which?", "describe": "w"}),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.result);
    assert_eq!(out.result["items"].as_array().unwrap().len(), 600);
    assert_eq!(out.result["items"][0]["id"], json!(599));
    let ranked = out.result["ranked"].as_array().unwrap();
    assert_eq!(
        ranked
            .iter()
            .filter(|entry| entry["probability"].is_null())
            .count(),
        600 - 252
    );
    let requests = decider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in requests.iter() {
        let Question::Choice { criteria, .. } = &request.questions["rank"] else {
            panic!()
        };
        assert!(criteria.len() <= 255);
    }
}

#[tokio::test]
async fn rank_needs_a_decision_model_and_is_hidden_without_one() {
    let (tools, _) = tools(false);
    let names: Vec<String> = tools
        .tools()
        .await
        .unwrap()
        .into_iter()
        .map(|def| def.name)
        .collect();
    assert!(!names.contains(&RANK_TOOL.to_string()));
    let out = tools
        .invoke(
            RANK_TOOL,
            json!({"state": "q", "items": [1], "instructions": "Which?"}),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.result["error"]
        .as_str()
        .unwrap()
        .contains("[models.decider]"));
}
