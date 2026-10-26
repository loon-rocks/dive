use crate::pipeline::AGENT_TOOL_PREFIX;
use crate::EventSink;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

tokio::task_local! {
    static FRAME: Frame;
}

static FRAMES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Turn,
    Run,
    Agent,
    Plan,
    Tool,
    Model,
}

impl SpanKind {
    pub fn of_tool(name: &str) -> Self {
        if name.starts_with(AGENT_TOOL_PREFIX) {
            SpanKind::Agent
        } else if name == "plan_and_execute"
            || name.starts_with(crate::toolbox::PLAN_TOOL_PREFIX)
            || crate::packs::is_plan_name(name)
        {
            SpanKind::Plan
        } else {
            SpanKind::Tool
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusCode {
    #[default]
    Unset,
    Ok,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub code: StatusCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Status {
    pub fn is_unset(&self) -> bool {
        self.code == StatusCode::Unset && self.message.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub trace_id: String,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub name: String,
    pub kind: SpanKind,
    pub start_time_unix_nano: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time_unix_nano: Option<u64>,
    #[serde(default, skip_serializing_if = "Status::is_unset")]
    pub status: Status,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

impl Span {
    pub fn is_error(&self) -> bool {
        self.status.code == StatusCode::Error
    }

    pub fn ms(&self) -> Option<u64> {
        self.end_time_unix_nano
            .map(|end| end.saturating_sub(self.start_time_unix_nano) / 1_000_000)
    }

    pub fn attribute(&self, key: &str) -> Option<&Value> {
        self.attributes.get(key)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Content {
    pub keep: bool,
    pub clip: Option<usize>,
}

impl Content {
    pub fn kept(&self, value: &Value) -> Option<Value> {
        if !self.keep {
            return None;
        }
        Some(match self.clip {
            Some(budget) => clipped(value, budget),
            None => value.clone(),
        })
    }
}

fn clip_strings(value: &mut Value, cap: usize) {
    match value {
        Value::String(text) if text.len() > cap => {
            let mut cut = cap;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let dropped = text.len() - cut;
            text.truncate(cut);
            text.push_str(&format!("…[{dropped} bytes truncated]"));
        }
        Value::Array(items) => items.iter_mut().for_each(|item| clip_strings(item, cap)),
        Value::Object(map) => map.values_mut().for_each(|item| clip_strings(item, cap)),
        _ => {}
    }
}

pub fn clipped(value: &Value, budget: usize) -> Value {
    let size = |value: &Value| serde_json::to_string(value).map_or(0, |text| text.len());
    let bytes = size(value);
    if bytes <= budget {
        return value.clone();
    }
    let mut cap = budget / 2;
    while cap >= 64 {
        let mut clipped = value.clone();
        clip_strings(&mut clipped, cap);
        if size(&clipped) <= budget {
            return clipped;
        }
        cap /= 2;
    }
    let text = serde_json::to_string(value).unwrap_or_default();
    let mut cut = (budget / 2).min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    serde_json::json!({
        "truncated": true,
        "bytes": bytes,
        "preview": &text[..cut],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Root,
    Tool,
    Step,
    Model,
}

#[derive(Clone)]
pub struct Frame(Arc<Node>);

struct Node {
    id: u64,
    kind: FrameKind,
    parent: Option<Frame>,
    recorder: Option<Arc<Recorder>>,
    span_id: Option<String>,
    span: Mutex<Option<Span>>,
    filled: Mutex<bool>,
}

struct Recorder {
    trace_id: String,
    events: Arc<dyn EventSink>,
    content: Content,
    closed: Mutex<Vec<Span>>,
}

impl Recorder {
    fn finish(&self, span: Span) {
        self.events.span_finished(&span);
        self.closed.lock().unwrap().push(span);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub id: u64,
    pub kind: FrameKind,
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        })
}

fn new_span_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

pub fn new_trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn current() -> Option<Frame> {
    FRAME.try_with(Clone::clone).ok()
}

fn span_id_of(frame: &Frame) -> Option<String> {
    let mut frame = Some(frame.clone());
    while let Some(current) = frame {
        if let Some(id) = &current.0.span_id {
            return Some(id.clone());
        }
        frame = current.0.parent.clone();
    }
    None
}

pub fn current_span_id() -> Option<String> {
    current().as_ref().and_then(span_id_of)
}

pub fn content() -> Content {
    current()
        .and_then(|frame| frame.0.recorder.clone())
        .map(|recorder| recorder.content)
        .unwrap_or_default()
}

pub fn lineage() -> Vec<FrameInfo> {
    let mut out = Vec::new();
    let mut frame = current();
    while let Some(current) = frame {
        out.push(FrameInfo {
            id: current.0.id,
            kind: current.0.kind,
        });
        frame = current.0.parent.clone();
    }
    out
}

#[derive(Debug, Clone)]
pub struct Open {
    pub kind: SpanKind,
    pub name: String,
    pub input: Option<Value>,
    pub attributes: BTreeMap<String, Value>,
}

impl Open {
    pub fn new(kind: SpanKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            input: None,
            attributes: BTreeMap::new(),
        }
    }

    pub fn tool(name: &str) -> Self {
        let kind = SpanKind::of_tool(name);
        let operation = match kind {
            SpanKind::Agent => "invoke_agent",
            _ => "execute_tool",
        };
        let open = Self::new(kind, name)
            .attr("gen_ai.operation.name", operation)
            .attr("gen_ai.tool.name", name);
        match name.strip_prefix(AGENT_TOOL_PREFIX) {
            Some(agent) => open.attr("gen_ai.agent.name", agent),
            None => open,
        }
    }

    pub fn input(mut self, input: &Value) -> Self {
        self.input = Some(input.clone());
        self
    }

    pub fn attr(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.attributes.insert(key.to_string(), value.into());
        self
    }
}

pub struct Root {
    pub open: Open,
    pub events: Arc<dyn EventSink>,
    pub content: Content,
    pub trace_id: Option<String>,
}

fn open_frame(kind: FrameKind, open: Option<Open>, recorder: Option<Arc<Recorder>>) -> Frame {
    let parent = current();
    let recorder = recorder.or_else(|| parent.as_ref().and_then(|p| p.0.recorder.clone()));
    let span = match (&recorder, open) {
        (Some(recorder), Some(open)) => {
            let span = Span {
                trace_id: recorder.trace_id.clone(),
                span_id: new_span_id(),
                parent_span_id: parent.as_ref().and_then(span_id_of),
                name: open.name,
                kind: open.kind,
                start_time_unix_nano: now_nanos(),
                end_time_unix_nano: None,
                status: Status::default(),
                attributes: open.attributes,
                input: open.input.and_then(|input| recorder.content.kept(&input)),
                output: None,
            };
            recorder.events.span_started(&span);
            Some(span)
        }
        _ => None,
    };
    Frame(Arc::new(Node {
        id: FRAMES.fetch_add(1, Ordering::Relaxed) + 1,
        kind,
        parent,
        recorder,
        span_id: span.as_ref().map(|span| span.span_id.clone()),
        span: Mutex::new(span),
        filled: Mutex::new(false),
    }))
}

pub struct Traced {
    pub elapsed: Duration,
    frame: Frame,
}

impl Traced {
    pub fn ms(&self) -> u64 {
        u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX)
    }

    pub fn span_id(&self) -> Option<String> {
        self.frame.0.span_id.clone()
    }

    pub fn close(&self, is_error: bool, output: Option<&Value>) {
        let Some(recorder) = self.frame.0.recorder.clone() else {
            return;
        };
        let Some(mut span) = self.frame.0.span.lock().unwrap().take() else {
            return;
        };
        span.end_time_unix_nano = Some(now_nanos());
        span.status = Status {
            code: if is_error {
                StatusCode::Error
            } else {
                StatusCode::Ok
            },
            message: output
                .filter(|_| is_error)
                .and_then(|output| output.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        if let Some(output) = output {
            span.output = recorder.content.kept(output);
        }
        recorder.finish(span);
    }

    pub fn spans(&self) -> Vec<Span> {
        let Some(recorder) = &self.frame.0.recorder else {
            return Vec::new();
        };
        let nested = self.frame.0.parent.as_ref().is_some_and(|parent| {
            parent
                .0
                .recorder
                .as_ref()
                .is_some_and(|theirs| Arc::ptr_eq(theirs, recorder))
        });
        if nested {
            return Vec::new();
        }
        let mut spans = std::mem::take(&mut *recorder.closed.lock().unwrap());
        spans.sort_by_key(|span| span.start_time_unix_nano);
        spans
    }
}

async fn scoped<F: Future>(frame: Frame, future: F) -> (F::Output, Traced) {
    let started = Instant::now();
    let output = FRAME.scope(frame.clone(), future).await;
    (
        output,
        Traced {
            elapsed: started.elapsed(),
            frame,
        },
    )
}

pub async fn span<F: Future>(kind: FrameKind, open: Open, future: F) -> (F::Output, Traced) {
    scoped(open_frame(kind, Some(open), None), future).await
}

pub async fn root<F: Future>(root: Root, future: F) -> (F::Output, Traced) {
    let inherited = current().and_then(|frame| frame.0.recorder.clone());
    let recorder = inherited.unwrap_or_else(|| {
        Arc::new(Recorder {
            trace_id: root.trace_id.unwrap_or_else(new_trace_id),
            events: root.events,
            content: root.content,
            closed: Mutex::new(Vec::new()),
        })
    });
    scoped(
        open_frame(FrameKind::Root, Some(root.open), Some(recorder)),
        future,
    )
    .await
}

#[derive(Debug, Clone)]
pub struct ModelSpan {
    pub name: String,
    pub elapsed: Duration,
    pub attributes: BTreeMap<String, Value>,
}

pub fn model(call: ModelSpan) {
    let Some(frame) = current() else {
        return;
    };
    let Some(recorder) = frame.0.recorder.clone() else {
        return;
    };
    if frame.0.kind == FrameKind::Model {
        let mut filled = frame.0.filled.lock().unwrap();
        if !*filled {
            if let Some(span) = frame.0.span.lock().unwrap().as_mut() {
                *filled = true;
                span.name = call.name;
                span.attributes.extend(call.attributes);
                return;
            }
        }
    }
    let end = now_nanos();
    let elapsed = u64::try_from(call.elapsed.as_nanos()).unwrap_or(u64::MAX);
    recorder.finish(Span {
        trace_id: recorder.trace_id.clone(),
        span_id: new_span_id(),
        parent_span_id: span_id_of(&frame),
        name: call.name,
        kind: SpanKind::Model,
        start_time_unix_nano: end.saturating_sub(elapsed),
        end_time_unix_nano: Some(end),
        status: Status {
            code: StatusCode::Ok,
            message: None,
        },
        attributes: call.attributes,
        input: None,
        output: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Default)]
    struct Seen(Mutex<Vec<(bool, Span)>>);

    impl EventSink for Seen {
        fn span_started(&self, span: &Span) {
            self.0.lock().unwrap().push((false, span.clone()));
        }

        fn span_finished(&self, span: &Span) {
            self.0.lock().unwrap().push((true, span.clone()));
        }
    }

    fn turn(events: Arc<Seen>, content: Content) -> Root {
        Root {
            open: Open::new(SpanKind::Turn, "chat"),
            events,
            content,
            trace_id: None,
        }
    }

    fn model_call(name: &str) -> ModelSpan {
        ModelSpan {
            name: name.to_string(),
            elapsed: Duration::from_millis(5),
            attributes: BTreeMap::from([("dive.role".to_string(), json!(name))]),
        }
    }

    fn kept() -> Content {
        Content {
            keep: true,
            clip: None,
        }
    }

    #[tokio::test]
    async fn nested_and_concurrent_calls_parent_on_their_own_frames() {
        let events = Arc::new(Seen::default());
        let ((), turn) = root(turn(events.clone(), kept()), async {
            let ((), outer) = span(
                FrameKind::Tool,
                Open::tool("agent__tool_drafter").input(&json!({"node": "x"})),
                async {
                    model(model_call("drafter"));
                    let inner = |name: &'static str| async move {
                        let ((), traced) = span(FrameKind::Step, Open::tool(name), async {
                            tokio::task::yield_now().await;
                            model(model_call(name));
                        })
                        .await;
                        traced.close(false, Some(&json!({"ok": name})));
                    };
                    futures::join!(inner("a"), inner("b"));
                },
            )
            .await;
            outer.close(false, Some(&json!({"status": "drafted"})));
        })
        .await;
        turn.close(false, None);
        let spans = turn.spans();
        assert_eq!(spans.len(), 7);
        let by_name = |name: &str| spans.iter().find(|span| span.name == name).unwrap();
        let top = by_name("chat");
        assert_eq!(top.parent_span_id, None);
        let drafter = by_name("agent__tool_drafter");
        assert_eq!(drafter.kind, SpanKind::Agent);
        assert_eq!(drafter.parent_span_id.as_ref(), Some(&top.span_id));
        assert_eq!(drafter.input, Some(json!({"node": "x"})));
        assert_eq!(
            drafter.attribute("gen_ai.agent.name"),
            Some(&json!("tool_drafter"))
        );
        assert_eq!(
            by_name("drafter").parent_span_id.as_ref(),
            Some(&drafter.span_id)
        );
        for name in ["a", "b"] {
            let step = spans
                .iter()
                .find(|span| span.name == name && span.kind == SpanKind::Tool)
                .unwrap();
            assert_eq!(step.parent_span_id.as_ref(), Some(&drafter.span_id));
            let call = spans
                .iter()
                .find(|span| span.name == name && span.kind == SpanKind::Model)
                .unwrap();
            assert_eq!(call.parent_span_id.as_ref(), Some(&step.span_id));
            assert_eq!((&call.input, &call.output), (&None, &None));
        }
        assert!(spans.iter().all(|span| span.trace_id == top.trace_id));
        let seen = events.0.lock().unwrap();
        let started = seen.iter().filter(|(done, _)| !done).count();
        assert_eq!(started, 4, "model calls outside a model frame only finish");
    }

    #[tokio::test]
    async fn a_model_frame_takes_its_call_and_further_calls_nest_under_it() {
        let ((), turn) = root(turn(Arc::new(Seen::default()), kept()), async {
            let ((), frame) = span(
                FrameKind::Model,
                Open::new(SpanKind::Model, "chat"),
                async {
                    model(model_call("first"));
                    model(model_call("repair"));
                },
            )
            .await;
            frame.close(false, None);
        })
        .await;
        turn.close(false, None);
        let spans = turn.spans();
        let first = spans.iter().find(|span| span.name == "first").unwrap();
        assert_eq!(first.kind, SpanKind::Model);
        assert_eq!(first.attribute("dive.role"), Some(&json!("first")));
        let repair = spans.iter().find(|span| span.name == "repair").unwrap();
        assert_eq!(repair.parent_span_id.as_ref(), Some(&first.span_id));
    }

    async fn tool_input(content: Content, input: &Value) -> Option<Value> {
        let ((), turn) = root(turn(Arc::new(Seen::default()), content), async {
            let ((), traced) = span(FrameKind::Tool, Open::tool("t").input(input), async {}).await;
            traced.close(false, None);
        })
        .await;
        turn.spans()[0].input.clone()
    }

    #[tokio::test]
    async fn content_is_dropped_unless_kept_and_clipped_only_when_asked() {
        let input = json!({"text": "x".repeat(4000)});
        assert_eq!(tool_input(Content::default(), &input).await, None);
        assert_eq!(tool_input(kept(), &input).await, Some(input.clone()));
        let clip = Content {
            keep: true,
            clip: Some(1000),
        };
        let clipped = tool_input(clip, &input).await.unwrap();
        assert!(serde_json::to_string(&clipped).unwrap().len() <= 1000);
        assert!(clipped["text"]
            .as_str()
            .unwrap()
            .contains("bytes truncated"));
    }

    #[tokio::test]
    async fn a_root_inside_a_trace_is_a_child_span_and_hands_back_nothing() {
        let events = Arc::new(Seen::default());
        let ((), outer) = root(turn(events.clone(), kept()), async {
            let ((), inner) = root(turn(events.clone(), kept()), async {
                model(model_call("inner"));
            })
            .await;
            inner.close(false, None);
            assert!(inner.spans().is_empty());
        })
        .await;
        outer.close(false, None);
        let spans = outer.spans();
        assert_eq!(spans.len(), 3);
        let roots = spans
            .iter()
            .filter(|span| span.parent_span_id.is_none())
            .count();
        assert_eq!(roots, 1);
    }

    #[tokio::test]
    async fn outside_any_trace_frames_still_nest_but_nothing_is_kept() {
        model(model_call("chat"));
        assert!(lineage().is_empty());
        let ((), traced) = span(FrameKind::Tool, Open::tool("x"), async {
            assert_eq!(lineage().len(), 1);
            assert_eq!(lineage()[0].kind, FrameKind::Tool);
            model(model_call("chat"));
        })
        .await;
        traced.close(false, None);
        assert_eq!(traced.span_id(), None);
        assert!(traced.spans().is_empty());
    }

    #[tokio::test]
    async fn a_failed_call_carries_its_error_as_the_status_message() {
        let ((), turn) = root(turn(Arc::new(Seen::default()), kept()), async {
            let ((), traced) = span(FrameKind::Tool, Open::tool("t"), async {}).await;
            traced.close(true, Some(&json!({"error": "no such file"})));
        })
        .await;
        let spans = turn.spans();
        assert!(spans[0].is_error());
        assert_eq!(spans[0].status.message.as_deref(), Some("no such file"));
    }

    #[test]
    fn spans_round_trip_without_empty_fields() {
        let span = Span {
            trace_id: new_trace_id(),
            span_id: new_span_id(),
            parent_span_id: None,
            name: "plan__digest".to_string(),
            kind: SpanKind::Plan,
            start_time_unix_nano: 1,
            end_time_unix_nano: Some(2_000_001),
            status: Status::default(),
            attributes: BTreeMap::new(),
            input: None,
            output: None,
        };
        let text = serde_json::to_string(&span).unwrap();
        for absent in ["parent_span_id", "status", "attributes", "input", "output"] {
            assert!(!text.contains(absent), "{text}");
        }
        assert_eq!(serde_json::from_str::<Span>(&text).unwrap(), span);
        assert_eq!(span.ms(), Some(2));
        assert_eq!(span.trace_id.len(), 32);
        assert_eq!(span.span_id.len(), 16);
    }

    #[test]
    fn tool_names_decide_the_span_kind() {
        assert_eq!(SpanKind::of_tool("agent__tool_drafter"), SpanKind::Agent);
        assert_eq!(SpanKind::of_tool("plan__digest"), SpanKind::Plan);
        assert_eq!(SpanKind::of_tool("plan_and_execute"), SpanKind::Plan);
        assert_eq!(SpanKind::of_tool("linear__list_issues"), SpanKind::Tool);
    }
}
