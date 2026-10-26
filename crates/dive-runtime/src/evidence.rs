use crate::attachment::{AttachmentHost, EVIDENCE_ATTACHMENT};
use crate::runtime::Runtime;
use crate::workbench::ArtifactKind;
use anyhow::Result;
use async_trait::async_trait;
use dive_core::trace::{Span, SpanKind};
use dive_core::{Attachment, EntryBody, NewEntry, Store, ThreadEntry, ToolRegistry};
use dive_llm::types::ChatMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::any::Any;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

const SHOWN_CHARS: usize = 6000;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Item {
    pub kind: ArtifactKind,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub thread: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    pub item: Item,
    #[serde(default)]
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trace: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub turn: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<Focus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Focus {
    pub span: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<Item>,
    #[serde(default)]
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Focus {
    pub fn label(&self) -> String {
        if let Some(model) = &self.model {
            return format!("the {model} model call");
        }
        match self.step.as_deref() {
            Some(path) => format!(
                "step `{}` ({}, path `{path}`)",
                path.rsplit('/').next().unwrap_or(path),
                self.name
            ),
            None => format!("the call to {}", self.name),
        }
    }

    fn section(&self) -> String {
        let mut section = format!(
            "\n### The focus\nThe user asked to improve {}, inside this call. Start the diagnosis there, and make the restate's problem about it. A change there often needs changes upstream or downstream (a step that feeds it, a step that reads what it returns): make them, and put each in the sketch.\n",
            self.label()
        );
        if let Some(item) = &self.item {
            section.push_str(&format!(
                "It is the {} {}, attached too. To test a fix that is only in it, call workbench__test_network at it with the input below; otherwise test the evidence's item.\n",
                item.kind.label(),
                item.name
            ));
        }
        if !self.input.is_null() {
            section.push_str(&format!("\n#### Its input\n{}\n", shown(&self.input)));
        }
        match (&self.error, &self.output) {
            (Some(error), _) => section.push_str(&format!("\n#### Its error\n{error}\n")),
            (None, Some(output)) => {
                section.push_str(&format!("\n#### Its output\n{}\n", shown(output)))
            }
            (None, None) => {}
        }
        section
    }
}

impl Evidence {
    pub fn record(&self) -> Attachment {
        let mut record = Attachment::new(EVIDENCE_ATTACHMENT);
        if let Ok(Value::Object(fields)) = serde_json::to_value(self) {
            record.fields = fields;
        }
        record
    }

    pub fn recall(recorded: &Attachment) -> Option<Self> {
        serde_json::from_value(Value::Object(recorded.fields.clone())).ok()
    }

    pub fn section(&self) -> String {
        let mut section = if self.turn {
            format!(
                "## The evidence\nThis edit is about one whole turn of the agent {}, from thread {}: the user didn't point at a call, so find which item is at fault from how the turn ran, and attach it before you sketch. Test the edits against the user's message.\n",
                self.item.name, self.thread
            )
        } else {
            format!(
                "## The evidence\nThis edit is about one call of the {} {}, from thread {}{}. Test the edits against its input.\n",
                self.item.kind.label(),
                self.item.name,
                self.thread,
                self.call
                    .as_deref()
                    .map(|call| format!(" (call {call})"))
                    .unwrap_or_default()
            )
        };
        let (asked, answered) = if self.turn {
            ("The user's message", "The reply")
        } else {
            ("Input", "Output")
        };
        section.push_str(&format!("\n### {asked}\n{}\n", shown(&self.input)));
        match (&self.error, &self.output) {
            (Some(error), _) => section.push_str(&format!("\n### Error\n{error}\n")),
            (None, Some(output)) => {
                section.push_str(&format!("\n### {answered}\n{}\n", shown(output)))
            }
            (None, None) => section.push_str("\n(no output was recorded)\n"),
        }
        if let Some(focus) = &self.focus {
            section.push_str(&focus.section());
        }
        if self.trace.is_empty() && self.item.kind != ArtifactKind::Tool {
            section.push_str(
                "\n### How it ran\n(no trace of the steps inside this call was recorded)\n",
            );
        } else if !self.trace.is_empty() {
            section.push_str("\n### How it ran\nEvery plan, agent and tool call inside it, and each model call an agent made, in order, with its step path (a route's branch is part of the path: depth/quick/quick_check/else/esc_run took `quick`, then `else`) and the start of its output:\n");
            if self.focus.is_some() {
                section.push_str(&format!(
                    "The line ending `{}` is the call the user asked about.\n",
                    crate::run_trace::FOCUS_MARK.trim()
                ));
            }
            for line in &self.trace {
                section.push_str(&format!("- {line}\n"));
            }
        }
        section
    }
}

fn shown(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    let total = text.chars().count();
    if total <= SHOWN_CHARS {
        return text;
    }
    format!(
        "{}\n(the first {SHOWN_CHARS} of {total} characters)",
        text.chars().take(SHOWN_CHARS).collect::<String>()
    )
}

#[derive(Debug, Default)]
pub struct Editable {
    pub agents: BTreeSet<String>,
    pub plans: BTreeSet<String>,
    pub tools: BTreeSet<String>,
}

impl Editable {
    pub fn of(runtime: &Runtime) -> Self {
        let mut tools = BTreeSet::new();
        if let Ok(docs) = runtime.tool_docs() {
            tools.extend(docs.user.iter().map(|doc| format!("user__{}", doc.name)));
            for (source, pack_docs) in &docs.packs {
                tools.extend(
                    pack_docs
                        .iter()
                        .map(|doc| format!("{}__tool__{}", source.name, doc.name)),
                );
            }
            tools.extend(
                docs.builtins
                    .iter()
                    .map(|doc| format!("builtin__{}", doc.name)),
            );
        }
        Self {
            agents: runtime.agent_set().iter().map(|doc| doc.name).collect(),
            plans: crate::ops::plan::list(runtime)
                .plans
                .iter()
                .map(|plan| plan.identifier.clone())
                .collect(),
            tools,
        }
    }

    pub fn item(&self, call: &str) -> Option<Item> {
        if let Some(name) = call.strip_prefix("agent__") {
            return self.agents.contains(name).then(|| Item {
                kind: ArtifactKind::Agent,
                name: name.to_string(),
            });
        }
        if let Some(name) = call.strip_prefix("plan__") {
            return self.plans.contains(name).then(|| Item {
                kind: ArtifactKind::Plan,
                name: name.to_string(),
            });
        }
        self.tools.contains(call).then(|| Item {
            kind: ArtifactKind::Tool,
            name: call.strip_prefix("user__").unwrap_or(call).to_string(),
        })
    }
}

fn parsed(content: &Value) -> Value {
    match content {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(value @ (Value::Object(_) | Value::Array(_))) => value,
            _ => content.clone(),
        },
        other => other.clone(),
    }
}

fn error_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| content.to_string()),
        other => other.to_string(),
    }
}

pub async fn from_call(
    editable: &Editable,
    store: &dyn Store,
    thread: &str,
    call: &str,
) -> Result<Evidence, String> {
    let entries = store
        .load_entries(thread)
        .await
        .map_err(|error| format!("couldn't read thread {thread}: {error}"))?;
    let mut asked = None;
    let mut answered = None;
    for entry in &entries {
        let EntryBody::Message { message } = &entry.body else {
            continue;
        };
        match message {
            ChatMessage::Assistant { tool_calls, .. } => {
                if let Some(found) = tool_calls.iter().find(|tool| tool.id == call) {
                    asked = Some((found.clone(), entry.author.clone()));
                }
            }
            ChatMessage::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == call => answered = Some((content.clone(), *is_error)),
            _ => {}
        }
    }
    let Some((asked, author)) = asked else {
        return from_span(editable, store, thread, call).await;
    };
    let (content, is_error) = answered.ok_or_else(|| {
        format!(
            "{} hasn't returned yet, so there's nothing to look at",
            asked.name
        )
    })?;
    let item = editable
        .item(&asked.name)
        .or_else(|| editable.item(&format!("agent__{author}")))
        .ok_or_else(|| format!("{} isn't an agent, plan or tool Dive can edit", asked.name))?;
    let spans = store.load_spans(thread).await.unwrap_or_default();
    Ok(Evidence {
        thread: thread.to_string(),
        call: Some(call.to_string()),
        item,
        input: asked.arguments,
        output: (!is_error).then(|| parsed(&content)),
        error: is_error.then(|| error_text(&content)),
        trace: crate::run_trace::from_spans(&spans, call),
        turn: false,
        focus: None,
    })
}

fn attribute<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
    span.attribute(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn parent_of<'a>(spans: &'a [Span], span: &Span) -> Option<&'a Span> {
    let parent = span.parent_span_id.as_deref()?;
    spans.iter().find(|other| other.span_id == parent)
}

fn is_caller(span: &Span) -> bool {
    matches!(span.kind, SpanKind::Plan | SpanKind::Agent | SpanKind::Turn)
        || span.name.starts_with("plan__")
        || span.name.starts_with("agent__")
}

fn item_of(editable: &Editable, span: &Span) -> Option<Item> {
    match span.kind {
        SpanKind::Turn => editable.item(&format!(
            "agent__{}",
            attribute(span, "gen_ai.agent.name").unwrap_or(&span.name)
        )),
        SpanKind::Model | SpanKind::Run => None,
        _ => editable.item(&span.name),
    }
}

fn under(spans: &[Span], span: &Span, root: &str) -> bool {
    let mut at = Some(span);
    while let Some(current) = at {
        if current.span_id == root {
            return true;
        }
        at = parent_of(spans, current);
    }
    false
}

fn turn_message(entries: &[ThreadEntry], spans: &[Span], turn: &Span) -> Value {
    let first = entries.iter().position(|entry| {
        entry
            .span
            .as_deref()
            .and_then(|id| spans.iter().find(|span| span.span_id == id))
            .is_some_and(|span| under(spans, span, &turn.span_id))
    });
    entries[..first.unwrap_or(entries.len())]
        .iter()
        .rev()
        .find_map(|entry| match &entry.body {
            EntryBody::Message {
                message: ChatMessage::User { content, .. },
            } if entry.author == "user" => Some(Value::String(content.clone())),
            _ => None,
        })
        .unwrap_or(Value::Null)
}

fn spoken(entries: &[ThreadEntry], span: &Span) -> Option<Value> {
    entries.iter().find_map(|entry| match &entry.body {
        EntryBody::Message {
            message:
                ChatMessage::Assistant {
                    content,
                    tool_calls,
                    ..
                },
        } if entry.span.as_deref() == Some(span.span_id.as_str()) => Some(serde_json::json!({
            "text": content,
            "tool_calls": tool_calls
                .iter()
                .map(|call| serde_json::json!({"name": call.name, "arguments": call.arguments}))
                .collect::<Vec<_>>(),
        })),
        _ => None,
    })
}

fn failure(span: &Span) -> Option<String> {
    span.is_error().then(|| {
        span.status
            .message
            .clone()
            .unwrap_or_else(|| "it failed".to_string())
    })
}

fn of_span(
    spans: &[Span],
    entries: &[ThreadEntry],
    thread: &str,
    span: &Span,
    item: Item,
    focus: Option<Focus>,
) -> Evidence {
    let turn = span.kind == SpanKind::Turn;
    Evidence {
        thread: thread.to_string(),
        call: attribute(span, "gen_ai.tool.call.id").map(str::to_string),
        item,
        input: match &span.input {
            Some(input) => input.clone(),
            None if turn => turn_message(entries, spans, span),
            None => Value::Null,
        },
        output: span
            .output
            .as_ref()
            .filter(|_| !span.is_error())
            .map(parsed),
        error: failure(span),
        trace: crate::run_trace::outline(
            spans,
            &span.span_id,
            focus.as_ref().map(|focus| focus.span.as_str()),
        ),
        turn,
        focus,
    }
}

pub async fn from_span(
    editable: &Editable,
    store: &dyn Store,
    thread: &str,
    id: &str,
) -> Result<Evidence, String> {
    let spans = store
        .load_spans(thread)
        .await
        .map_err(|error| format!("couldn't read the trace of thread {thread}: {error}"))?;
    let entries = store
        .load_entries(thread)
        .await
        .map_err(|error| format!("couldn't read thread {thread}: {error}"))?;
    let clicked = spans
        .iter()
        .find(|span| span.span_id == id || attribute(span, "gen_ai.tool.call.id") == Some(id))
        .ok_or_else(|| format!("thread {thread} has no call {id}"))?;
    if clicked.end_time_unix_nano.is_none() {
        return Err(format!(
            "{} hasn't finished yet, so there's nothing to look at",
            clicked.name
        ));
    }
    if clicked.kind == SpanKind::Turn {
        let agent = item_of(editable, clicked)
            .ok_or_else(|| format!("the agent {} isn't one Dive can edit", clicked.name))?;
        return Ok(of_span(&spans, &entries, thread, clicked, agent, None));
    }
    let mut focused = clicked;
    while focused.kind == SpanKind::Model {
        match parent_of(&spans, focused) {
            Some(parent) if !matches!(parent.kind, SpanKind::Agent | SpanKind::Turn) => {
                focused = parent
            }
            _ => break,
        }
    }
    let own = item_of(editable, focused);
    let mut caller = parent_of(&spans, focused);
    while let Some(span) = caller.filter(|span| !is_caller(span)) {
        caller = parent_of(&spans, span);
    }
    let within = caller.and_then(|span| item_of(editable, span).map(|item| (span, item)));
    match (within, own) {
        (Some((caller, item)), own) => {
            let model = (focused.kind == SpanKind::Model).then(|| {
                let role = attribute(focused, "dive.role").unwrap_or("chat");
                format!("{role} role ({})", focused.name)
            });
            let focus = Focus {
                span: focused.span_id.clone(),
                name: focused.name.clone(),
                step: attribute(focused, "dive.step.path").map(str::to_string),
                item: own,
                input: focused.input.clone().unwrap_or(Value::Null),
                output: match model {
                    Some(_) => spoken(&entries, focused),
                    None => focused
                        .output
                        .as_ref()
                        .filter(|_| !focused.is_error())
                        .map(parsed),
                },
                error: failure(focused),
                model,
            };
            Ok(of_span(&spans, &entries, thread, caller, item, Some(focus)))
        }
        (None, Some(item)) => Ok(of_span(&spans, &entries, thread, focused, item, None)),
        (None, None) => Err(format!(
            "neither {} nor what called it is an agent, plan or tool Dive can edit",
            focused.name
        )),
    }
}

pub struct EvidenceAttachment {
    evidence: Mutex<Option<Evidence>>,
}

impl EvidenceAttachment {
    pub fn new(evidence: Evidence) -> Self {
        Self {
            evidence: Mutex::new(Some(evidence)),
        }
    }

    pub fn empty() -> Self {
        Self {
            evidence: Mutex::new(None),
        }
    }

    pub fn evidence(&self) -> Option<Evidence> {
        self.evidence.lock().unwrap().clone()
    }

    pub fn load(&self, evidence: Evidence) {
        *self.evidence.lock().unwrap() = Some(evidence);
    }
}

#[async_trait]
impl AttachmentHost for EvidenceAttachment {
    fn kind(&self) -> &'static str {
        EVIDENCE_ATTACHMENT
    }

    fn kinds(&self) -> &'static [&'static str] {
        &[EVIDENCE_ATTACHMENT]
    }

    fn sections(&self, kind: &str) -> Vec<String> {
        match (kind, self.evidence()) {
            (EVIDENCE_ATTACHMENT, Some(evidence)) => vec![evidence.section()],
            _ => Vec::new(),
        }
    }

    fn loaded(&self) -> bool {
        self.evidence.lock().unwrap().is_some()
    }

    fn tools(&self, catalog: Arc<dyn ToolRegistry>) -> Arc<dyn ToolRegistry> {
        catalog
    }

    fn restore(&self, _history: &[NewEntry]) {}

    fn view(&self) -> Value {
        serde_json::to_value(self.evidence()).unwrap_or_default()
    }

    fn record(&self) -> Attachment {
        self.evidence()
            .map(|evidence| evidence.record())
            .unwrap_or_else(|| Attachment::new(EVIDENCE_ATTACHMENT))
    }

    async fn act(&self, action: &str, _input: Value) -> Result<Value, String> {
        Err(format!(
            "the evidence attachment is read-only; it has no '{action}' action"
        ))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn evidence() -> Evidence {
        Evidence {
            thread: "a1b2c3d4e5f6".to_string(),
            call: Some("toolu_1".to_string()),
            item: Item {
                kind: ArtifactKind::Plan,
                name: "release_digest".to_string(),
            },
            input: json!({"since": "v0.15.0"}),
            output: None,
            error: Some("step E2: no tool named git_log".to_string()),
            trace: Vec::new(),
            turn: false,
            focus: None,
        }
    }

    #[test]
    fn the_evidence_round_trips_through_the_thread_record() {
        let record = evidence().record();
        assert_eq!(record.kind, "evidence");
        assert_eq!(
            record.fields["item"],
            json!({"kind": "plan", "name": "release_digest"})
        );
        assert_eq!(Evidence::recall(&record), Some(evidence()));
        let empty = EvidenceAttachment::empty();
        assert!(!empty.loaded() && empty.sections(EVIDENCE_ATTACHMENT).is_empty());
        empty.load(evidence());
        assert!(empty.loaded());
        let view = EvidenceAttachment::new(evidence()).view();
        assert_eq!(view["thread"], json!("a1b2c3d4e5f6"));
        assert_eq!(view["error"], json!("step E2: no tool named git_log"));
        assert!(view.get("output").is_none());
    }

    #[test]
    fn the_section_shows_the_input_and_the_error() {
        let section = evidence().section();
        assert!(section.starts_with("## The evidence"));
        assert!(
            section.contains("the plan release_digest, from thread a1b2c3d4e5f6 (call toolu_1)")
        );
        assert!(section.contains("\"since\": \"v0.15.0\""));
        assert!(section.contains("### Error\nstep E2: no tool named git_log"));
        assert!(section.contains("(no trace of the steps inside this call was recorded)"));
        let mut traced = evidence();
        traced.trace = vec!["depth/quick/quick_run plan__research_quick ok: x".to_string()];
        assert!(traced
            .section()
            .contains("- depth/quick/quick_run plan__research_quick ok: x"));
        let mut long = evidence();
        long.error = None;
        long.output = Some(json!("x".repeat(SHOWN_CHARS + 10)));
        assert!(long.section().contains(&format!(
            "(the first {SHOWN_CHARS} of {} characters)",
            SHOWN_CHARS + 10
        )));
    }

    fn traced(
        id: &str,
        parent: Option<&str>,
        name: &str,
        kind: SpanKind,
        at: u64,
        attributes: Value,
    ) -> Span {
        Span {
            trace_id: "t".into(),
            span_id: id.into(),
            parent_span_id: parent.map(str::to_string),
            name: name.into(),
            kind,
            start_time_unix_nano: at,
            end_time_unix_nano: Some(at + 1_000_000),
            status: Default::default(),
            attributes: attributes
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            input: None,
            output: None,
        }
    }

    async fn research_turn() -> (dive_store::MemoryStore, String, Editable) {
        let store = dive_store::MemoryStore::new();
        let thread = store
            .create_thread("race", "research_agent", None)
            .await
            .unwrap()
            .id;
        let mut asked = NewEntry::message(
            "research_agent",
            ChatMessage::Assistant {
                content: None,
                tool_calls: vec![dive_llm::types::ToolCall {
                    id: "c1".into(),
                    name: "plan__research".into(),
                    arguments: json!({"prompt": "next race?"}),
                }],
                thinking: Vec::new(),
            },
        );
        asked.span = Some("m1".into());
        store
            .append_entries(
                &thread,
                &[
                    NewEntry::message("user", ChatMessage::user("When is the next race?")),
                    asked,
                ],
            )
            .await
            .unwrap();
        let mut turn = traced(
            "T",
            None,
            "research_agent",
            SpanKind::Turn,
            1,
            json!({"gen_ai.agent.name": "research_agent"}),
        );
        turn.output = Some(json!("The next race is in São Paulo."));
        let mut plan = traced(
            "P",
            Some("T"),
            "plan__research",
            SpanKind::Plan,
            3,
            json!({"gen_ai.tool.call.id": "c1"}),
        );
        plan.input = Some(json!({"prompt": "next race?"}));
        plan.output = Some(json!({"result": "unknown"}));
        let mut read = traced(
            "R",
            Some("P"),
            "user__crawl4ai_read",
            SpanKind::Tool,
            4,
            json!({"dive.step.path": "depth/quick/read_pages"}),
        );
        read.input = Some(json!({"url": "https://f1.com/calendar"}));
        read.output = Some(json!({"markdown": "626 characters"}));
        let spans = vec![
            turn,
            traced(
                "m1",
                Some("T"),
                "claude-haiku",
                SpanKind::Model,
                2,
                json!({"dive.role": "chat"}),
            ),
            plan,
            read,
            traced(
                "G",
                Some("P"),
                "builtin__infer",
                SpanKind::Tool,
                5,
                json!({"dive.step.path": "depth/quick/extract"}),
            ),
            traced(
                "GM",
                Some("G"),
                "claude-haiku",
                SpanKind::Model,
                6,
                json!({}),
            ),
        ];
        store.append_spans(&thread, &spans).await.unwrap();
        let editable = Editable {
            agents: BTreeSet::from(["research_agent".to_string()]),
            plans: BTreeSet::from(["research".to_string()]),
            tools: BTreeSet::from(["user__crawl4ai_read".to_string()]),
        };
        (store, thread, editable)
    }

    #[tokio::test]
    async fn a_step_s_evidence_is_its_plan_s_call_focused_on_the_step() {
        let (store, thread, editable) = research_turn().await;
        let step = from_span(&editable, &store, &thread, "R").await.unwrap();
        assert_eq!(step.item.name, "research");
        assert_eq!(step.call.as_deref(), Some("c1"));
        assert_eq!(step.input, json!({"prompt": "next race?"}));
        let focus = step.focus.clone().expect("the step is the focus");
        assert_eq!(
            focus.item.map(|item| item.name),
            Some("crawl4ai_read".to_string())
        );
        assert_eq!(focus.input, json!({"url": "https://f1.com/calendar"}));
        assert!(step
            .trace
            .iter()
            .any(|line| line.contains("user__crawl4ai_read")
                && line.ends_with(crate::run_trace::FOCUS_MARK)));
        let section = step.section();
        assert!(section.contains("### The focus\nThe user asked to improve step `read_pages`"));
        assert!(section.contains("It is the tool crawl4ai_read, attached too."));

        let inferred = from_span(&editable, &store, &thread, "GM").await.unwrap();
        let focus = inferred.focus.expect("the infer step is the focus");
        assert_eq!((focus.span.as_str(), focus.item), ("G", None));
        assert!(focus.model.is_none());
    }

    #[tokio::test]
    async fn a_top_level_call_and_a_model_call_are_focused_inside_the_turn() {
        let (store, thread, editable) = research_turn().await;
        let called = from_span(&editable, &store, &thread, "c1").await.unwrap();
        assert!(called.turn);
        assert_eq!(called.item.name, "research_agent");
        assert_eq!(called.input, json!("When is the next race?"));
        assert_eq!(called.focus.unwrap().item.unwrap().name, "research");

        let spoke = from_span(&editable, &store, &thread, "m1").await.unwrap();
        let focus = spoke.focus.clone().expect("the model call is the focus");
        assert_eq!(focus.model.as_deref(), Some("chat role (claude-haiku)"));
        assert_eq!(
            focus.output.unwrap()["tool_calls"][0]["name"],
            json!("plan__research")
        );
        assert!(spoke.trace[0].starts_with("model call, chat role"));
        assert!(spoke.trace[0].ends_with(crate::run_trace::FOCUS_MARK));
        assert!(spoke
            .section()
            .contains("improve the chat role (claude-haiku) model call"));
    }

    #[tokio::test]
    async fn a_whole_turn_is_evidence_about_its_agent_with_no_focus() {
        let (store, thread, editable) = research_turn().await;
        let turn = from_span(&editable, &store, &thread, "T").await.unwrap();
        assert!(turn.turn && turn.focus.is_none());
        assert_eq!(turn.output, Some(json!("The next race is in São Paulo.")));
        assert!(turn
            .section()
            .contains("one whole turn of the agent research_agent"));
        assert!(turn
            .section()
            .contains("### The user's message\nWhen is the next race?"));
        let none = Editable::default();
        assert!(from_span(&none, &store, &thread, "R").await.is_err());
        assert!(from_span(&editable, &store, &thread, "nope").await.is_err());
    }
}
