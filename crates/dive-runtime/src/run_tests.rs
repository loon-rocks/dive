use crate::chat::ChatStart;
use crate::packs::InstalledPacks;
use crate::runtime::{PipelineHooks, Runtime, RuntimeOptions};
use async_trait::async_trait;
use dive_config::{ModelChoice, ModelRoles};
use dive_core::pipeline::{Finish, Plan};
use dive_core::run::{Run, RunToken};
use dive_core::usage::{ModelCallEvent, UsageReport};
use dive_core::{EventSink, RunStart};
use dive_llm::types::{ChatRequest, ChatResponse, EventStream, StopReason, StreamEvent, Usage};
use dive_llm::{ChatProvider, LlmError, ModelRouter};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LEAK: &str = "tok_LEAKCHECK";

struct Marked;

impl Marked {
    async fn answer(&self, request: &ChatRequest) -> ChatResponse {
        tokio::task::yield_now().await;
        let seen = format!("{:?}", request.messages);
        let input_tokens = ["charlie", "bravo", "alpha"]
            .iter()
            .zip([100, 10, 1])
            .find(|(marker, _)| seen.contains(**marker))
            .map(|(_, tokens)| tokens)
            .unwrap_or(1000);
        tokio::task::yield_now().await;
        ChatResponse {
            content: Some("{}".to_string()),
            tool_calls: Vec::new(),
            thinking: Vec::new(),
            structured: Some(json!({})),
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens,
                output_tokens: 1,
                ..Default::default()
            },
        }
    }
}

#[async_trait]
impl ChatProvider for Marked {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        Ok(self.answer(&request).await)
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<EventStream, LlmError> {
        let response = self.answer(&request).await;
        Ok(futures::stream::iter(vec![Ok(StreamEvent::Completed(response))]).boxed())
    }
}

fn runtime() -> Runtime {
    let mut config = dive_config::Config::default();
    config.tools.paths = Vec::new();
    config.plans.paths = Vec::new();
    let provider: Arc<dyn ChatProvider> = Arc::new(Marked);
    let router = ModelRouter::with_providers(
        HashMap::from([("marked".to_string(), provider)]),
        ModelRoles::from([(
            "default",
            ModelChoice {
                kind: None,
                provider: "marked".to_string(),
                model: "marked".to_string(),
                temperature: None,
                description: None,
                fallbacks: Vec::new(),
                context_window: None,
                images: None,
            },
        )]),
    );
    let options = RuntimeOptions {
        storage_backend: Some("memory".to_string()),
        ..Default::default()
    };
    Runtime::with_router(config, InstalledPacks::default(), options, router)
        .expect("the test runtime opens")
}

#[derive(Default)]
struct Recorded {
    calls: Mutex<Vec<ModelCallEvent>>,
    lines: Mutex<Vec<String>>,
}

impl Recorded {
    fn inputs(&self) -> Vec<u64> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.usage.input_tokens)
            .collect()
    }

    fn line(&self, line: String) {
        self.lines.lock().unwrap().push(line);
    }
}

impl EventSink for Recorded {
    fn text_delta(&self, text: &str) {
        self.line(text.to_string());
    }
    fn model_call(&self, call: &ModelCallEvent) {
        self.line(format!("{call:?} {:?}", call.span()));
        self.calls.lock().unwrap().push(call.clone());
    }
    fn usage_summary(&self, report: &UsageReport) {
        self.line(format!("{report:?}"));
    }
    fn run_finished(&self, output: &Value, is_error: bool) {
        self.line(format!("{output} {is_error}"));
    }
    fn run_started(&self, run: &RunStart) {
        self.line(format!("{run:?}"));
    }
    fn tool_started(&self, name: &str, args: &Value) {
        self.line(format!("{name} {args}"));
    }
    fn tool_finished(&self, name: &str, elapsed: Duration, is_error: bool) {
        self.line(format!("{name} {elapsed:?} {is_error}"));
    }
    fn step_started(&self, call_stack: &[String], path: &str, tool: &str, input: &Value) {
        self.line(format!("{call_stack:?} {path} {tool} {input}"));
    }
    fn step_finished(
        &self,
        call_stack: &[String],
        path: &str,
        tool: &str,
        result: &Value,
        is_error: bool,
        elapsed: Duration,
    ) {
        self.line(format!(
            "{call_stack:?} {path} {tool} {result} {is_error} {elapsed:?}"
        ));
    }
}

fn agent_plan(prompts: &[&str]) -> Plan {
    let steps: Vec<Value> = prompts
        .iter()
        .enumerate()
        .map(|(index, prompt)| {
            json!({"id": format!("E{index}"), "toolName": "agent", "input": {
                "prompt": prompt,
                "outputSchema": {"type": "object"},
            }})
        })
        .collect();
    serde_json::from_value(Value::Array(steps)).expect("the plan parses")
}

#[tokio::test]
async fn two_plan_runs_and_a_chat_turn_at_once_each_report_only_their_own_usage_and_events() {
    let runtime = runtime();
    let store = runtime.store().unwrap();
    let (sink_a, sink_b, sink_chat) = (
        Arc::new(Recorded::default()),
        Arc::new(Recorded::default()),
        Arc::new(Recorded::default()),
    );
    let pipeline_a = runtime.pipeline(&store, sink_a.clone()).await.unwrap();
    let pipeline_b = runtime.pipeline(&store, sink_b.clone()).await.unwrap();
    let run_a = runtime.run(sink_a.clone());
    let run_b = runtime.run(sink_b.clone());
    let conversation = runtime
        .conversation(&store, sink_chat.clone(), PipelineHooks::default())
        .await
        .unwrap();
    let mut chat = ChatStart::resolve(&runtime, store.clone(), None, None)
        .await
        .unwrap()
        .over(conversation, sink_chat.clone(), None, None)
        .await
        .unwrap();

    let (a, b, turn) = tokio::join!(
        Box::pin(run_a.scope(pipeline_a.run_explicit(
            "q",
            agent_plan(&["alpha"]),
            Finish::Silent,
            None
        ))),
        Box::pin(run_b.scope(pipeline_b.run_explicit(
            "q",
            agent_plan(&["bravo", "bravo"]),
            Finish::Silent,
            None
        ))),
        Box::pin(chat.send("charlie")),
    );
    a.unwrap();
    b.unwrap();
    let turn = turn.unwrap();

    assert_eq!(
        (run_a.usage().calls, run_a.usage().total.input_tokens),
        (1, 1)
    );
    assert_eq!(
        (run_b.usage().calls, run_b.usage().total.input_tokens),
        (2, 20)
    );
    assert_eq!((turn.usage.calls, turn.usage.total.input_tokens), (1, 100));
    assert_eq!(sink_a.inputs(), [1]);
    assert_eq!(sink_b.inputs(), [10, 10]);
    assert_eq!(sink_chat.inputs(), [100]);
    assert!(runtime.usage.unscoped().is_empty());
}

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Lines;
    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

#[tokio::test]
async fn a_runs_token_appears_in_none_of_its_entries_events_or_log_lines() {
    let logs = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let runtime = runtime();
    let store = runtime.store().unwrap();
    let sink = Arc::new(Recorded::default());
    let run = Arc::new(Run::new(runtime.identity(), sink.clone()).with_token(RunToken::new(LEAK)));
    let pipeline = runtime.pipeline(&store, sink.clone()).await.unwrap();
    let conversation = runtime
        .conversation(&store, sink.clone(), PipelineHooks::default())
        .await
        .unwrap();
    let meta =
        crate::ops::session::tests::chat_session(store.as_ref(), "leak", None, Vec::new()).await;
    let start = dive_core::store::StartRun {
        run_id: run.id.to_string(),
        session: meta.id.clone(),
        kind: dive_core::store::SessionKind::Chat,
        holder: crate::writer::holder().to_string(),
        token_expires_at: None,
        started: dive_core::store::RunStarted {
            input: serde_json::json!("hello"),
            identity: run.identity.clone(),
            trace_id: None,
            plan: None,
            synthetic: false,
        },
    };
    let writer = crate::writer::RunWriter::start(store.clone(), start, None)
        .await
        .unwrap();
    let mut conversation = conversation;
    conversation.entries = Some(writer.hook());
    let (planned, turn) = Box::pin(run.scope(async {
        let planned = pipeline
            .run_explicit("q", agent_plan(&["alpha"]), Finish::Silent, None)
            .await;
        let turn = conversation.run_turn(&[], "chat", "hello").await;
        (planned, turn)
    }))
    .await;
    planned.unwrap();
    let turn = turn.unwrap();
    store.append_spans(&meta.id, &turn.spans).await.unwrap();
    writer
        .finish(dive_core::store::RunEnded::completed(None))
        .await
        .unwrap();
    tracing::info!(run = ?run, "finished");

    let usage = run.usage();
    assert_eq!(usage.calls, 2);
    let stored = store.load_entries(&meta.id, None).await.unwrap();
    let spans = store.load_spans(&meta.id).await.unwrap();
    let everything = [
        serde_json::to_string(&stored).unwrap(),
        serde_json::to_string(&spans).unwrap(),
        serde_json::to_string(&usage).unwrap(),
        serde_json::to_string(&run.identity).unwrap(),
        format!("{run:?}"),
        sink.lines.lock().unwrap().join("\n"),
        String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned(),
    ];
    assert!(!everything[6].is_empty(), "the log capture saw nothing");
    for (index, text) in everything.iter().enumerate() {
        assert!(!text.contains(LEAK), "the token leaked into output {index}");
    }
}
