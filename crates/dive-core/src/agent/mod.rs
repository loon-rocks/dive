//! The ReAct agent loop: model ↔ tools until a final text answer.

pub mod conversation;
pub mod deferral;
pub mod doc;
mod events;
pub mod task;

pub use events::{EventSink, NullSink, RunStart, TeeSink};

use crate::store::{EntryUsage, Stamp};
use crate::tools::{ToolOutcome, ToolRegistry};
use futures::StreamExt;
use dive_llm::types::{
    ChatMessage, ChatRequest, ChatResponse, StopReason, StreamEvent, ToolCall, ToolSpec, Usage,
};
use dive_llm::ChatProvider;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

pub struct Agent {
    pub provider: Arc<dyn ChatProvider>,
    pub registry: Arc<dyn ToolRegistry>,
    pub events: Arc<dyn EventSink>,
    pub model: String,
    pub temperature: Option<f32>,
    pub system_prompt: String,
    pub max_iterations: u32,
    /// Tool names that count as genuine progress. When a batch executes one
    /// of these successfully, the iteration budget resets: `max_iterations`
    /// then bounds *consecutive non-progress* iterations rather than the whole
    /// turn. Empty (the `ask`/`chat` default) keeps the plain hard cap. The
    /// workbench sets it to its mutating edit tools so a long fix-forward loop
    /// — edit, validate, run, repeat — isn't starved mid-repair.
    pub progress_tools: Vec<String>,
    pub stop_tools: Vec<String>,
    pub standing_tools: Vec<String>,
    pub call_site: crate::usage::CallSite,
}

/// Failures of the agent loop (`ask`/`chat`/workbench turns). The plan
/// pipeline never runs the agent loop, so these sit outside its
/// `EmptyData`/`BadPath` error taxonomy: they surface directly to the user
/// as a failed turn (rolled back in the workbench and `chat`, nonzero exit
/// in `ask`) and never trigger replanning.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] dive_llm::LlmError),
    #[error("agent exceeded {0} tool iterations without reaching an answer")]
    MaxIterations(u32),
    #[error(
        "the model hit its output-token limit before producing any visible text \
         (the whole budget went to internal reasoning) — raise max_tokens or \
         simplify the request"
    )]
    MaxTokensNoOutput,
    #[error("model stream ended without completing a response")]
    IncompleteStream,
}

#[derive(Debug)]
pub struct TurnOutcome {
    pub text: String,
    pub usage: Usage,
    pub tool_calls_made: u32,
    /// Names of tools invoked this turn, in first-use order, deduplicated.
    pub tools_used: Vec<String>,
    pub stopped_by: Option<String>,
    pub stamps: Vec<Stamp>,
}

impl Agent {
    /// Run one user turn: loop model → tools → model until the model answers
    /// with text and no tool calls. Appends every intermediate message to
    /// `messages` so the caller owns the full history.
    pub async fn run_turn(
        &self,
        messages: &mut Vec<ChatMessage>,
    ) -> Result<TurnOutcome, AgentError> {
        let tools = self.tool_specs().await;
        let mut usage = Usage::default();
        let mut tool_calls_made = 0u32;
        let mut tools_used: Vec<String> = Vec::new();
        let mut stamps: Vec<Stamp> = Vec::new();
        // `max_iterations` bounds *consecutive non-progress* iterations, not
        // the whole turn: a successful progress-tool call resets `stall` (see
        // `progress_tools`). With no progress tools configured, nothing ever
        // resets it and this is exactly the old hard cap. `iteration` stays a
        // monotonic count for the event sink's UI.
        let mut stall = 0u32;
        let mut iteration = 0u32;

        loop {
            if stall >= self.max_iterations {
                return Err(AgentError::MaxIterations(self.max_iterations));
            }
            if iteration > 0 {
                self.events.iteration(iteration);
            }
            iteration += 1;
            // `stream_once` opens *and* drains the stream, so one scope over
            // the call covers the terminal event that carries the counts.
            let called = Instant::now();
            let response = self
                .call_site
                .clone()
                .scope(self.stream_once(messages, &tools))
                .await?;
            usage.add(&response.usage);
            stamps.push(Stamp {
                ms: Some(millis(called.elapsed())),
                usage: Some(EntryUsage::new(&self.model, &response.usage)),
                calls: Vec::new(),
            });

            messages.push(ChatMessage::Assistant {
                content: response.content.clone(),
                tool_calls: response.tool_calls.clone(),
                // Carried forward so the model keeps its own reasoning
                // across rounds instead of re-deriving it each time.
                thinking: response.thinking.clone(),
            });

            if response.tool_calls.is_empty() {
                let text = response.content.unwrap_or_default();
                // A max_tokens stop with no text means the whole output
                // budget went to (invisible) thinking — silence here reads
                // as "nothing happened"; make it a diagnosable failure.
                if text.is_empty() && response.stop_reason == StopReason::MaxTokens {
                    return Err(AgentError::MaxTokensNoOutput);
                }
                return Ok(TurnOutcome {
                    text,
                    usage,
                    tool_calls_made,
                    tools_used,
                    stopped_by: None,
                    stamps,
                });
            }

            tool_calls_made += response.tool_calls.len() as u32;
            for call in &response.tool_calls {
                if !tools_used.contains(&call.name) {
                    tools_used.push(call.name.clone());
                }
            }
            let (results, tool_stamps): (Vec<ChatMessage>, Vec<Stamp>) = self
                .execute_calls(&response.tool_calls)
                .await
                .into_iter()
                .unzip();
            // A successful progress-tool call in this batch refreshes the
            // budget; otherwise the stall counter advances toward the cap.
            let made_progress = !self.progress_tools.is_empty()
                && response.tool_calls.iter().zip(&results).any(|(call, msg)| {
                    self.progress_tools.contains(&call.name) && !is_error_result(msg)
                });
            stall = if made_progress { 0 } else { stall + 1 };
            let stopped_by = response
                .tool_calls
                .iter()
                .zip(&results)
                .find(|(call, msg)| self.stop_tools.contains(&call.name) && !is_error_result(msg))
                .map(|(call, _)| call.name.clone());
            messages.extend(results);
            stamps.extend(tool_stamps);
            if stopped_by.is_some() {
                return Ok(TurnOutcome {
                    text: response.content.unwrap_or_default(),
                    usage,
                    tool_calls_made,
                    tools_used,
                    stopped_by,
                    stamps,
                });
            }
        }
    }

    async fn tool_specs(&self) -> Vec<ToolSpec> {
        match self.registry.tools().await {
            Ok(defs) => deferral::specs(defs, &self.standing_tools),
            Err(e) => {
                tracing::warn!(error = %e, "tool discovery failed; running without tools");
                Vec::new()
            }
        }
    }

    async fn stream_once(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, AgentError> {
        let request = ChatRequest {
            model: self.model.clone(),
            system: self.system_prompt.clone(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            temperature: self.temperature,
            tool_references: deferral::references(messages),
            ..Default::default()
        };
        let mut stream = self.provider.chat_stream(request).await?;
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta(text) => self.events.text_delta(&text),
                StreamEvent::ToolCallStarted { .. } => {}
                StreamEvent::Completed(response) => return Ok(response),
            }
        }
        Err(AgentError::IncompleteStream)
    }

    /// Execute one round of tool calls concurrently, preserving call order
    /// in the returned messages.
    async fn execute_calls(&self, calls: &[ToolCall]) -> Vec<(ChatMessage, Stamp)> {
        let futures = calls.iter().map(|call| async {
            let (outcome, traced) = crate::trace::tool(crate::trace::FrameKind::Tool, async {
                self.events.tool_started(&call.name, &call.arguments);
                let started = Instant::now();
                let outcome = self
                    .registry
                    .invoke(&call.name, call.arguments.clone())
                    .await
                    .unwrap_or_else(|e| ToolOutcome {
                        result: json!({ "error": e.to_string() }),
                        is_error: true,
                    });
                self.events
                    .tool_finished(&call.name, started.elapsed(), outcome.is_error);
                outcome
            })
            .await;
            let ms = traced.ms();
            let inner = traced.finish(&call.name, outcome.is_error);
            (
                ChatMessage::ToolResult {
                    tool_call_id: call.id.clone(),
                    content: outcome.result,
                    is_error: outcome.is_error,
                },
                Stamp {
                    ms: Some(ms),
                    usage: None,
                    calls: inner,
                },
            )
        });
        futures::future::join_all(futures).await
    }
}

fn millis(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// True when a tool-result message carried an error. `execute_calls` returns
/// results in call order, so this reads the flag per call for progress
/// accounting.
fn is_error_result(message: &ChatMessage) -> bool {
    matches!(message, ChatMessage::ToolResult { is_error, .. } if *is_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolDef, ToolError};
    use async_trait::async_trait;
    use dive_llm::types::{EventStream, StopReason};
    use dive_llm::LlmError;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Provider that plays back a fixed sequence of responses.
    struct ScriptedProvider {
        responses: Mutex<Vec<ChatResponse>>,
        seen_requests: Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                seen_requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ChatProvider for ScriptedProvider {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.seen_requests.lock().unwrap().push(req);
            Ok(self.responses.lock().unwrap().remove(0))
        }

        async fn chat_stream(&self, req: ChatRequest) -> Result<EventStream, LlmError> {
            let response = {
                self.seen_requests.lock().unwrap().push(req);
                self.responses.lock().unwrap().remove(0)
            };
            let events = vec![Ok(StreamEvent::Completed(response))];
            Ok(futures::stream::iter(events).boxed())
        }
    }

    struct EchoRegistry {
        invocations: Mutex<Vec<(String, Value)>>,
        error_mode: bool,
    }

    #[async_trait]
    impl ToolRegistry for EchoRegistry {
        async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
            Ok(vec![ToolDef {
                name: "test__echo".into(),
                description: "echo".into(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                output_example: None,
                read_only: Some(true),
            }])
        }

        async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
            self.invocations
                .lock()
                .unwrap()
                .push((name.to_string(), input.clone()));
            Ok(ToolOutcome {
                result: json!({"echoed": input}),
                is_error: self.error_mode,
            })
        }
    }

    fn agent(responses: Vec<ChatResponse>, error_mode: bool) -> (Agent, Arc<EchoRegistry>) {
        agent_with_progress(responses, error_mode, Vec::new())
    }

    fn agent_with_progress(
        responses: Vec<ChatResponse>,
        error_mode: bool,
        progress_tools: Vec<String>,
    ) -> (Agent, Arc<EchoRegistry>) {
        let registry = Arc::new(EchoRegistry {
            invocations: Mutex::new(Vec::new()),
            error_mode,
        });
        let agent = Agent {
            provider: Arc::new(ScriptedProvider::new(responses)),
            registry: registry.clone(),
            events: Arc::new(NullSink),
            model: "test-model".into(),
            temperature: None,
            system_prompt: "test".into(),
            max_iterations: 3,
            progress_tools,
            stop_tools: Vec::new(),
            standing_tools: Vec::new(),
            call_site: crate::usage::CallSite::role("chat"),
        };
        (agent, registry)
    }

    fn text_response(text: &str) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
            tool_calls: vec![],
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        }
    }

    fn tool_response(calls: Vec<(&str, Value)>) -> ChatResponse {
        ChatResponse {
            content: None,
            thinking: Vec::new(),
            tool_calls: calls
                .into_iter()
                .enumerate()
                .map(|(i, (name, arguments))| ToolCall {
                    id: format!("call-{i}"),
                    name: name.to_string(),
                    arguments,
                })
                .collect(),
            structured: None,
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn answers_directly_without_tools() {
        let (agent, registry) = agent(vec![text_response("hi there")], false);
        let mut messages = vec![ChatMessage::User {
            content: "hello".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.text, "hi there");
        assert_eq!(outcome.tool_calls_made, 0);
        assert_eq!(messages.len(), 2);
        assert!(registry.invocations.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn every_appended_message_gets_a_stamp_in_order() {
        let mut first = tool_response(vec![
            ("test__echo", json!({"n": 1})),
            ("test__echo", json!({"n": 2})),
        ]);
        first.usage = Usage {
            input_tokens: 100,
            output_tokens: 7,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 40,
        };
        let (agent, _) = agent(vec![first, text_response("done")], false);
        let mut messages = vec![ChatMessage::User {
            content: "hello".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        let appended = &messages[1..];
        assert_eq!(outcome.stamps.len(), appended.len());
        for (message, stamp) in appended.iter().zip(&outcome.stamps) {
            assert!(stamp.ms.is_some());
            match message {
                ChatMessage::Assistant { .. } => {
                    assert_eq!(stamp.usage.as_ref().unwrap().model, "test-model")
                }
                _ => assert!(stamp.usage.is_none()),
            }
        }
        assert_eq!(
            outcome.stamps[0].usage,
            Some(EntryUsage {
                model: "test-model".into(),
                input_tokens: 100,
                output_tokens: 7,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 40,
            })
        );
        assert_eq!(outcome.stamps[3].usage.as_ref().unwrap().input_tokens, 10);
    }

    #[tokio::test]
    async fn max_tokens_with_no_text_is_a_diagnosable_error() {
        // The whole output budget went to invisible thinking: no tool
        // calls, no text, stop_reason max_tokens.
        let empty = ChatResponse {
            content: None,
            tool_calls: vec![],
            thinking: Vec::new(),
            structured: None,
            stop_reason: StopReason::MaxTokens,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 8192,
                ..Default::default()
            },
        };
        let (subject, _registry) = agent(vec![empty], false);
        let mut messages = vec![ChatMessage::User {
            content: "hello".into(),
        }];
        let err = subject.run_turn(&mut messages).await.unwrap_err();
        assert!(matches!(err, AgentError::MaxTokensNoOutput));
        assert!(err.to_string().contains("max_tokens"), "{err}");

        // A truncated-but-nonempty answer still returns normally.
        let mut truncated = text_response("partial answ");
        truncated.stop_reason = StopReason::MaxTokens;
        let (subject, _registry) = agent(vec![truncated], false);
        let mut messages = vec![ChatMessage::User {
            content: "hello".into(),
        }];
        let outcome = subject.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.text, "partial answ");
    }

    #[tokio::test]
    async fn executes_tool_round_then_answers() {
        let (agent, registry) = agent(
            vec![
                tool_response(vec![("test__echo", json!({"q": "x"}))]),
                text_response("done"),
            ],
            false,
        );
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.text, "done");
        assert_eq!(outcome.tool_calls_made, 1);
        assert_eq!(
            outcome.usage.input_tokens, 20,
            "usage accumulates across rounds"
        );
        // History: user, assistant(tool_calls), tool result, assistant(text).
        assert_eq!(messages.len(), 4);
        let invocations = registry.invocations.lock().unwrap();
        assert_eq!(
            invocations[0],
            ("test__echo".to_string(), json!({"q": "x"}))
        );
        assert!(matches!(
            &messages[2],
            ChatMessage::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn parallel_calls_produce_ordered_results() {
        let (agent, _) = agent(
            vec![
                tool_response(vec![
                    ("test__echo", json!({"n": 1})),
                    ("test__echo", json!({"n": 2})),
                ]),
                text_response("both"),
            ],
            false,
        );
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        agent.run_turn(&mut messages).await.unwrap();
        let ids: Vec<_> = messages
            .iter()
            .filter_map(|m| match m {
                ChatMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call-0", "call-1"]);
    }

    #[tokio::test]
    async fn tool_errors_flow_back_as_error_results() {
        let (agent, _) = agent(
            vec![
                tool_response(vec![("test__echo", json!({}))]),
                text_response("recovered"),
            ],
            true,
        );
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.text, "recovered");
        assert!(matches!(
            &messages[2],
            ChatMessage::ToolResult { is_error: true, .. }
        ));
    }

    #[tokio::test]
    async fn max_iterations_stops_runaway_loops() {
        let responses = (0..3)
            .map(|_| tool_response(vec![("test__echo", json!({}))]))
            .collect();
        let (agent, _) = agent(responses, false);
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let err = agent.run_turn(&mut messages).await.unwrap_err();
        assert!(matches!(err, AgentError::MaxIterations(3)));
    }

    #[tokio::test]
    async fn a_successful_progress_call_resets_the_iteration_budget() {
        // max_iterations is 3, but a fix-forward loop that keeps landing a
        // successful edit should run well past that. Interleave a progress
        // call every third round; the run reaches its final answer at round 7,
        // which a plain hard cap would have cut off at round 3.
        let mut responses = Vec::new();
        for i in 0..7 {
            let tool = if i % 3 == 2 {
                "test__edit"
            } else {
                "test__echo"
            };
            responses.push(tool_response(vec![(tool, json!({}))]));
        }
        responses.push(text_response("shipped"));
        let (agent, _) = agent_with_progress(responses, false, vec!["test__edit".to_string()]);
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.text, "shipped");
        assert_eq!(outcome.tool_calls_made, 7);
    }

    #[tokio::test]
    async fn a_successful_stop_tool_ends_the_turn_after_recording_every_result() {
        let responses = vec![
            tool_response(vec![
                ("test__echo", json!({"n": 1})),
                ("test__handoff", json!({})),
            ]),
            text_response("never reached"),
        ];
        let (mut agent, _) = agent(responses, false);
        agent.stop_tools = vec!["test__handoff".to_string()];
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.stopped_by.as_deref(), Some("test__handoff"));
        let results = messages
            .iter()
            .filter(|m| matches!(m, ChatMessage::ToolResult { .. }))
            .count();
        assert_eq!(results, 2, "every tool_use keeps its tool_result");
        assert!(matches!(
            messages.last(),
            Some(ChatMessage::ToolResult { .. })
        ));
    }

    #[tokio::test]
    async fn a_failing_stop_tool_does_not_end_the_turn() {
        let responses = vec![
            tool_response(vec![("test__handoff", json!({}))]),
            text_response("answered instead"),
        ];
        let (mut agent, _) = agent(responses, true);
        agent.stop_tools = vec!["test__handoff".to_string()];
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let outcome = agent.run_turn(&mut messages).await.unwrap();
        assert_eq!(outcome.stopped_by, None);
        assert_eq!(outcome.text, "answered instead");
    }

    #[tokio::test]
    async fn a_failing_progress_call_does_not_reset_the_budget() {
        // The edit tool is named a progress tool, but every call errors, so it
        // is not real progress: the stall counter still trips the cap.
        let responses = (0..3)
            .map(|_| tool_response(vec![("test__edit", json!({}))]))
            .collect();
        let (agent, _) = agent_with_progress(responses, true, vec!["test__edit".to_string()]);
        let mut messages = vec![ChatMessage::User {
            content: "go".into(),
        }];
        let err = agent.run_turn(&mut messages).await.unwrap_err();
        assert!(matches!(err, AgentError::MaxIterations(3)));
    }
}
