use crate::tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use crate::usage::CallSite;
use async_trait::async_trait;
use dive_config::{ModelKind, Role};
use dive_llm::decision::{Answer, DecisionRequest, Question};
use dive_llm::ModelRouter;
use serde::Deserialize;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub const FLATTEN_TOOL: &str = "builtin__flatten";
pub const DEDUPE_TOOL: &str = "builtin__dedupe";
pub const SORT_TOOL: &str = "builtin__sort";
pub const TAKE_TOOL: &str = "builtin__take";
pub const RANK_TOOL: &str = "builtin__rank";

pub const LIST_TOOLS: [&str; 5] = [FLATTEN_TOOL, DEDUPE_TOOL, SORT_TOOL, TAKE_TOOL, RANK_TOOL];

const CHOICE_OPTIONS: usize = 254;

const NONE_OPTION: &str = "none";

pub fn is_list_tool(name: &str) -> bool {
    LIST_TOOLS.contains(&name)
}

pub struct ListTools {
    router: Arc<ModelRouter>,
}

impl ListTools {
    pub fn new(router: Arc<ModelRouter>) -> Self {
        Self { router }
    }
}

#[async_trait]
impl ToolRegistry for ListTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        let decision = self.router.has_decision_models();
        Ok(list_tool_defs()
            .into_iter()
            .filter(|def| decision || def.name != RANK_TOOL)
            .collect())
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        let result = match name {
            FLATTEN_TOOL => flatten(input),
            DEDUPE_TOOL => dedupe(input),
            SORT_TOOL => sort(input),
            TAKE_TOOL => take(input),
            RANK_TOOL => self.rank(input).await,
            _ => return Err(ToolError::Unknown(name.to_string())),
        };
        Ok(match result {
            Ok(result) => ToolOutcome {
                result,
                is_error: false,
            },
            Err(error) => ToolOutcome {
                result: json!({ "error": error }),
                is_error: true,
            },
        })
    }
}

fn items_schema() -> Value {
    json!({"type": "array", "description": "The list, usually a template like {{E1.results}}"})
}

fn by_schema(what: &str) -> Value {
    json!({"type": "string", "description": format!("Dot path into each item to {what}, e.g. `url` or `meta.date`")})
}

fn list_result() -> Value {
    json!({"type": "object", "required": ["items"], "properties": {"items": {"type": "array"}}})
}

pub fn list_tool_defs() -> Vec<ToolDef> {
    let def = |name: &str, description: &str, input_schema: Value, output_schema: Value| ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
        output_schema: Some(output_schema),
        output_example: None,
        read_only: Some(true),
        file_input: Default::default(),
    };
    vec![
        def(
            FLATTEN_TOOL,
            "Flatten a list of lists into one list, in code with no model: \
             [[a, b], [c]] becomes [a, b, c]. Use it after a `map` whose items each \
             return a list, before deduping, sorting or ranking. `depth` is how many \
             levels to splice (default 1); elements that aren't lists are kept as they are.",
            json!({
                "type": "object",
                "required": ["items"],
                "properties": {
                    "items": items_schema(),
                    "depth": {"type": "integer", "description": "Levels to flatten; default 1"}
                }
            }),
            list_result(),
        ),
        def(
            DEDUPE_TOOL,
            "Remove duplicates from a list, in code with no model, keeping the first \
             item of each key in the original order. `by` is the field to compare \
             (e.g. `url`); without it whole items are compared. Items missing the \
             field are all kept. Returns the items and how many were removed.",
            json!({
                "type": "object",
                "required": ["items"],
                "properties": {
                    "items": items_schema(),
                    "by": by_schema("compare")
                }
            }),
            json!({
                "type": "object",
                "required": ["items", "removed"],
                "properties": {"items": {"type": "array"}, "removed": {"type": "integer"}}
            }),
        ),
        def(
            SORT_TOOL,
            "Sort a list by a field, in code with no model: a stable sort where \
             numbers compare as numbers and strings as text (ISO dates sort \
             correctly). `by` is the field (without it the items themselves are \
             compared), `order` is `asc` (default) or `desc`; items missing the field \
             go last. To order items by relevance to a question, use builtin__rank.",
            json!({
                "type": "object",
                "required": ["items"],
                "properties": {
                    "items": items_schema(),
                    "by": by_schema("sort on"),
                    "order": {"type": "string", "enum": ["asc", "desc"], "description": "Default asc"}
                }
            }),
            list_result(),
        ),
        def(
            TAKE_TOOL,
            "Keep the first `n` items of a list, in code with no model: the top-k cut \
             after builtin__sort. A list shorter than `n` is returned whole.",
            json!({
                "type": "object",
                "required": ["items", "n"],
                "properties": {
                    "items": items_schema(),
                    "n": {"type": "integer", "minimum": 0, "description": "How many items to keep"}
                }
            }),
            list_result(),
        ),
        def(
            RANK_TOOL,
            "Rank a whole list by relevance in one decision-model call: the items are \
             the options of one choice question, `state` is the context they're judged \
             against (the question, the request), and `instructions` is the question \
             (\"Which result best answers the question?\"). Returns `items` sorted by \
             probability (cut to `top_k`) and `ranked` ([{item, probability}]). \
             `describe` is the field shown as each option (e.g. `snippet`); without it \
             the whole item is shown. With `none` (e.g. \"None of these answer the \
             question\") the choice can say nothing fits: `no_fit` is true when it beats \
             every item. Lists over 254 items are ranked in chunks, then the chunk \
             winners against each other. Never use an LLM to rank, sort or cut a list.",
            json!({
                "type": "object",
                "required": ["state", "items", "instructions"],
                "properties": {
                    "state": {"description": "The context the items are judged against, e.g. {\"question\": \"{{input.question}}\"}"},
                    "items": items_schema(),
                    "instructions": {"type": "string", "description": "The choice question"},
                    "describe": by_schema("show as the option"),
                    "top_k": {"type": "integer", "minimum": 1, "description": "Keep only the best k"},
                    "none": {"type": "string", "description": "Text of a 'nothing fits' option"},
                    "model": {"type": "string", "description": "The decision model role; default decider"}
                }
            }),
            json!({
                "type": "object",
                "required": ["items", "ranked"],
                "properties": {
                    "items": {"type": "array"},
                    "ranked": {"type": "array", "items": {"type": "object"}},
                    "none": {"type": "number"},
                    "no_fit": {"type": "boolean"}
                }
            }),
        ),
    ]
}

fn parse<T: for<'de> Deserialize<'de>>(tool: &str, input: Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|e| format!("invalid {tool} input: {e}"))
}

fn lookup<'a>(item: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .filter(|part| !part.is_empty())
        .try_fold(item, |value, part| match value {
            Value::Object(map) => map.get(part),
            Value::Array(items) => part.parse::<usize>().ok().and_then(|i| items.get(i)),
            _ => None,
        })
        .filter(|value| !value.is_null())
}

fn key_of<'a>(item: &'a Value, by: Option<&str>) -> Option<&'a Value> {
    match by {
        Some(path) => lookup(item, path),
        None => Some(item),
    }
}

#[derive(Deserialize)]
struct FlattenInput {
    items: Vec<Value>,
    #[serde(default = "one")]
    depth: usize,
}

fn one() -> usize {
    1
}

fn splice(items: Vec<Value>, depth: usize, out: &mut Vec<Value>) {
    for item in items {
        match item {
            Value::Array(inner) if depth > 0 => splice(inner, depth - 1, out),
            other => out.push(other),
        }
    }
}

fn flatten(input: Value) -> Result<Value, String> {
    let input: FlattenInput = parse("flatten", input)?;
    let mut items = Vec::new();
    splice(input.items, input.depth, &mut items);
    Ok(json!({ "items": items }))
}

#[derive(Deserialize)]
struct DedupeInput {
    items: Vec<Value>,
    #[serde(default)]
    by: Option<String>,
}

fn dedupe(input: Value) -> Result<Value, String> {
    let input: DedupeInput = parse("dedupe", input)?;
    let total = input.items.len();
    let mut seen = HashSet::new();
    let items: Vec<Value> = input
        .items
        .into_iter()
        .filter(|item| match key_of(item, input.by.as_deref()) {
            Some(key) => seen.insert(key.to_string()),
            None => true,
        })
        .collect();
    Ok(json!({ "removed": total - items.len(), "items": items }))
}

#[derive(Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Order {
    #[default]
    Asc,
    Desc,
}

#[derive(Deserialize)]
struct SortInput {
    items: Vec<Value>,
    #[serde(default)]
    by: Option<String>,
    #[serde(default)]
    order: Order,
}

fn type_rank(value: &Value) -> u8 {
    match value {
        Value::Number(_) => 0,
        Value::String(_) => 1,
        Value::Bool(_) => 2,
        _ => 3,
    }
}

fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

fn sort(input: Value) -> Result<Value, String> {
    let input: SortInput = parse("sort", input)?;
    let by = input.by.as_deref();
    let mut items = input.items;
    items.sort_by(|a, b| match (key_of(a, by), key_of(b, by)) {
        (Some(a), Some(b)) => match input.order {
            Order::Asc => compare(a, b),
            Order::Desc => compare(b, a),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
    Ok(json!({ "items": items }))
}

#[derive(Deserialize)]
struct TakeInput {
    items: Vec<Value>,
    n: usize,
}

fn take(input: Value) -> Result<Value, String> {
    let input: TakeInput = parse("take", input)?;
    let items: Vec<Value> = input.items.into_iter().take(input.n).collect();
    Ok(json!({ "items": items }))
}

#[derive(Deserialize)]
struct RankInput {
    #[serde(default)]
    state: Value,
    items: Vec<Value>,
    instructions: String,
    #[serde(default)]
    describe: Option<String>,
    #[serde(default)]
    top_k: Option<usize>,
    #[serde(default)]
    none: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

struct Round {
    probabilities: Vec<f64>,
    none: Option<f64>,
}

fn shuffled(items: &[Value]) -> Vec<usize> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for item in items {
        item.to_string().hash(&mut hasher);
    }
    let mut seed = hasher.finish() | 1;
    let mut order: Vec<usize> = (0..items.len()).collect();
    for i in (1..order.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        order.swap(i, (seed % (i as u64 + 1)) as usize);
    }
    order
}

fn option_text(item: &Value, describe: Option<&str>) -> String {
    let shown = describe.and_then(|path| lookup(item, path)).unwrap_or(item);
    match shown {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn chunks(order: &[usize]) -> Vec<Vec<usize>> {
    let count = order.len().div_ceil(CHOICE_OPTIONS);
    let len = order.len().div_ceil(count);
    order.chunks(len).map(<[usize]>::to_vec).collect()
}

impl ListTools {
    async fn rank(&self, input: Value) -> Result<Value, String> {
        let input: RankInput = parse("rank", input)?;
        let role = input
            .model
            .clone()
            .unwrap_or_else(|| Role::Decider.as_str().to_string());
        if self.router.kind_of_role(&role) != Some(ModelKind::Decision) {
            return Err(format!(
                "builtin__rank needs a decision model: '{role}' isn't one; configure [models.decider]"
            ));
        }
        let order = shuffled(&input.items);
        let mut probabilities: Vec<Option<f64>> = vec![None; input.items.len()];
        let mut none = None;
        if order.len() <= CHOICE_OPTIONS {
            let round = self
                .choose(&role, &input, &order, input.none.as_deref())
                .await?;
            for (&i, p) in order.iter().zip(round.probabilities) {
                probabilities[i] = Some(p);
            }
            none = round.none;
        } else if !order.is_empty() {
            let chunks = chunks(&order);
            if chunks.len() > CHOICE_OPTIONS {
                return Err(format!(
                    "builtin__rank takes at most {} items",
                    CHOICE_OPTIONS * CHOICE_OPTIONS
                ));
            }
            let quota = CHOICE_OPTIONS / chunks.len();
            let rounds = chunks
                .iter()
                .map(|chunk| self.choose(&role, &input, chunk, None));
            let mut finalists = Vec::new();
            for (chunk, round) in chunks.iter().zip(futures::future::join_all(rounds).await) {
                let mut scored: Vec<(usize, f64)> =
                    chunk.iter().copied().zip(round?.probabilities).collect();
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
                finalists.extend(scored.into_iter().take(quota).map(|(i, _)| i));
            }
            let round = self
                .choose(&role, &input, &finalists, input.none.as_deref())
                .await?;
            for (&i, p) in finalists.iter().zip(round.probabilities) {
                probabilities[i] = Some(p);
            }
            none = round.none;
        }
        let mut ranked: Vec<usize> = order.clone();
        ranked.sort_by(|&a, &b| match (probabilities[a], probabilities[b]) {
            (Some(a), Some(b)) => b.partial_cmp(&a).unwrap_or(Ordering::Equal),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        });
        if let Some(k) = input.top_k {
            ranked.truncate(k);
        }
        let best = probabilities.iter().flatten().copied().fold(0.0, f64::max);
        let mut out = json!({
            "items": ranked.iter().map(|&i| input.items[i].clone()).collect::<Vec<_>>(),
            "ranked": ranked
                .iter()
                .map(|&i| json!({"item": input.items[i], "probability": probabilities[i].map(round)}))
                .collect::<Vec<_>>(),
        });
        if let Some(none) = none {
            out["none"] = json!(round(none));
            out["no_fit"] = json!(none > best);
        }
        Ok(out)
    }

    async fn choose(
        &self,
        role: &str,
        input: &RankInput,
        indexes: &[usize],
        none: Option<&str>,
    ) -> Result<Round, String> {
        let width = (indexes.len() + 1).to_string().len().max(3);
        let key = |position: usize| format!("{:0width$}", position + 1);
        let mut criteria: BTreeMap<String, String> = indexes
            .iter()
            .enumerate()
            .map(|(position, &i)| {
                (
                    key(position),
                    option_text(&input.items[i], input.describe.as_deref()),
                )
            })
            .collect();
        if let Some(none) = none {
            criteria.insert(NONE_OPTION.to_string(), none.to_string());
        }
        let request = DecisionRequest {
            model: String::new(),
            state: input.state.clone(),
            questions: BTreeMap::from([(
                "rank".to_string(),
                Question::Choice {
                    instructions: input.instructions.clone(),
                    criteria,
                },
            )]),
            images: Default::default(),
        };
        let response = CallSite::as_role(role, self.router.decide_named(Some(role), request))
            .await
            .map_err(|e| format!("decision model '{role}' failed: {e}"))?;
        let Some(Answer::Choice { probabilities, .. }) = response.answers.get("rank") else {
            return Err(format!("decision model '{role}' did not answer the choice"));
        };
        Ok(Round {
            probabilities: (0..indexes.len())
                .map(|position| probabilities.get(&key(position)).copied().unwrap_or(0.0))
                .collect(),
            none: none.map(|_| probabilities.get(NONE_OPTION).copied().unwrap_or(0.0)),
        })
    }
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[cfg(test)]
#[path = "list_tools_tests.rs"]
mod tests;
