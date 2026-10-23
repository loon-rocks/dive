//! The engine→UI bridge: an [`EventSink`] that forwards agent and plan
//! events into the workbench's message channel.

use super::event::WorkbenchEvent;
use dive_core::EventSink;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

enum SinkKind {
    /// Chat agent turns: assistant text and tool activity.
    AgentTurn,
    /// Plan runs: step events and the solver stream.
    PlanRun,
}

pub struct ChannelSink {
    tx: UnboundedSender<WorkbenchEvent>,
    kind: SinkKind,
}

impl ChannelSink {
    pub fn agent(tx: UnboundedSender<WorkbenchEvent>) -> Self {
        Self {
            tx,
            kind: SinkKind::AgentTurn,
        }
    }

    pub fn plan_run(tx: UnboundedSender<WorkbenchEvent>) -> Self {
        Self {
            tx,
            kind: SinkKind::PlanRun,
        }
    }

    fn send(&self, msg: WorkbenchEvent) {
        let _ = self.tx.send(msg);
    }
}

/// A display path for a step event: nested plan frames prefix the step
/// path, and only unprefixed paths — the run plan's own steps and their
/// body sub-steps — map onto plan-pane rows.
fn display_path(call_stack: &[String], path: &str) -> (String, bool) {
    if call_stack.is_empty() {
        (path.to_string(), true)
    } else {
        (format!("{}→{path}", call_stack.join("→")), false)
    }
}

impl EventSink for ChannelSink {
    fn text_delta(&self, text: &str) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(WorkbenchEvent::AgentDelta(text.to_string()));
        }
    }

    fn handoff(&self, from: &str, to: &str, back: bool) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            let verb = if back {
                "handed back to"
            } else {
                "handed off to"
            };
            self.send(WorkbenchEvent::ActiveAgent {
                name: to.to_string(),
                note: Some(format!("⇢ {from} {verb} {to}")),
            });
        }
    }

    fn subagent_started(&self, run: u64, agent: &str, _caller: &str, input: &Value) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(WorkbenchEvent::SubagentStarted {
                run,
                agent: agent.to_string(),
                input: input.clone(),
            });
        }
    }

    fn subagent_finished(
        &self,
        run: u64,
        agent: &str,
        output: &Value,
        is_error: bool,
        elapsed: Duration,
    ) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(WorkbenchEvent::SubagentFinished {
                run,
                agent: agent.to_string(),
                output: output.clone(),
                is_error,
                seconds: elapsed.as_secs_f64(),
            });
        }
    }

    fn subagent_tool_started(&self, run: u64, name: &str, _args: &Value) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(WorkbenchEvent::SubagentTool {
                run,
                name: name.to_string(),
                finished: None,
            });
        }
    }

    fn subagent_tool_finished(&self, run: u64, name: &str, _elapsed: Duration, is_error: bool) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(WorkbenchEvent::SubagentTool {
                run,
                name: name.to_string(),
                finished: Some(!is_error),
            });
        }
    }

    fn tool_started(&self, name: &str, _args: &Value) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(WorkbenchEvent::AgentToolStarted(name.to_string()));
        }
    }

    fn tool_finished(&self, name: &str, elapsed: Duration, is_error: bool) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(WorkbenchEvent::AgentToolFinished {
                name: name.to_string(),
                is_error,
                elapsed,
            });
        }
    }

    fn model_call(&self, call: &dive_core::usage::ModelCallEvent) {
        if matches!(self.kind, SinkKind::AgentTurn) {
            self.send(WorkbenchEvent::AgentModelCall(Box::new(call.clone())));
        }
    }

    fn planning(&self) {
        if matches!(self.kind, SinkKind::PlanRun) {
            self.send(WorkbenchEvent::Planning);
        }
    }

    fn synthesizing(&self) {
        if matches!(self.kind, SinkKind::PlanRun) {
            self.send(WorkbenchEvent::Synthesizing);
        }
    }

    /// Both kinds report it: a chat turn spends tokens too, and the workbench
    /// is where plans are iterated on, so what a draft costs to run is part of
    /// the feedback loop.
    ///
    /// Only the end-of-run total — one line per inference would bury the run
    /// log on a multi-round agent step, and the log is read by a human.
    fn usage_summary(&self, report: &dive_core::usage::UsageReport) {
        self.send(WorkbenchEvent::RunUsage(report.summary()));
    }

    fn solver_delta(&self, text: &str) {
        if matches!(self.kind, SinkKind::PlanRun) {
            self.send(WorkbenchEvent::SolverDelta(text.to_string()));
        }
    }

    fn step_started(&self, call_stack: &[String], path: &str, tool: &str, input: &Value) {
        if matches!(self.kind, SinkKind::PlanRun) {
            let (path, in_plan) = display_path(call_stack, path);
            self.send(WorkbenchEvent::StepStarted {
                path,
                tool: tool.to_string(),
                input: input.clone(),
                in_plan,
            });
        }
    }

    fn step_finished(
        &self,
        call_stack: &[String],
        path: &str,
        _tool: &str,
        result: &Value,
        is_error: bool,
        _elapsed: Duration,
    ) {
        if matches!(self.kind, SinkKind::PlanRun) {
            let (path, in_plan) = display_path(call_stack, path);
            self.send(WorkbenchEvent::StepFinished {
                path,
                result: result.clone(),
                is_error,
                in_plan,
            });
        }
    }
}

pub struct NetworkProgress {
    pub draft: super::tools::SharedDraft,
    pub tx: UnboundedSender<WorkbenchEvent>,
    pub builders: &'static [&'static str],
    pub runs: std::sync::Mutex<std::collections::HashMap<u64, String>>,
}

impl EventSink for NetworkProgress {
    fn subagent_started(&self, run: u64, agent: &str, _caller: &str, input: &Value) {
        if !self.builders.contains(&agent) {
            return;
        }
        if let Some(node) = input["node"].as_str() {
            self.runs.lock().unwrap().insert(run, node.to_string());
            self.draft
                .lock()
                .unwrap()
                .network
                .builder_started(node, agent);
            super::artifact::publish(&self.draft, &self.tx);
        }
    }

    fn subagent_finished(
        &self,
        run: u64,
        agent: &str,
        output: &Value,
        is_error: bool,
        _elapsed: Duration,
    ) {
        if !self.builders.contains(&agent) {
            return;
        }
        let node = self.runs.lock().unwrap().remove(&run).or_else(|| {
            self.draft
                .lock()
                .unwrap()
                .network
                .nodes
                .iter()
                .find(|node| {
                    node.builder.as_deref() == Some(agent)
                        && node.status == super::network::NodeStatus::Drafting
                })
                .map(|node| node.name.clone())
        });
        if let Some(node) = node {
            self.draft
                .lock()
                .unwrap()
                .network
                .builder_finished(&node, agent, output, is_error);
            super::artifact::publish(&self.draft, &self.tx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_llm::types::Usage;
    use serde_json::json;

    fn report() -> dive_core::usage::UsageReport {
        dive_core::usage::UsageReport {
            calls: 2,
            total: Usage {
                input_tokens: 4_000,
                output_tokens: 250,
                ..Default::default()
            },
            cost_usd: Some(0.42),
            ..Default::default()
        }
    }

    /// Both kinds report usage: a chat turn spends tokens as surely as a plan
    /// run, and the workbench is where a draft's cost gets discovered.
    #[test]
    fn both_sink_kinds_report_the_run_total() {
        for make in [ChannelSink::plan_run, ChannelSink::agent] {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            make(tx).usage_summary(&report());
            match rx.try_recv().expect("a usage message") {
                WorkbenchEvent::RunUsage(summary) => {
                    assert_eq!(summary, "2 calls · 4k in / 250 out · $0.42")
                }
                _ => panic!("expected WorkbenchEvent::RunUsage"),
            }
        }
    }

    #[test]
    fn agent_turns_report_model_calls_and_tool_durations() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ChannelSink::agent(tx);
        sink.model_call(&dive_core::usage::ModelCallEvent {
            kind: dive_config::ModelKind::Chat,
            site: "agent:network_drafter".to_string(),
            role: "chat".to_string(),
            provider: "anthropic".to_string(),
            model: "haiku".to_string(),
            usage: Usage::default(),
            elapsed: Duration::from_millis(1200),
            cost_usd: None,
            input: None,
            output: None,
        });
        sink.tool_finished("workbench__set_sketch", Duration::from_millis(30), false);
        match rx.try_recv().expect("a model call") {
            WorkbenchEvent::AgentModelCall(call) => {
                assert_eq!(call.site, "agent:network_drafter");
                assert_eq!(call.elapsed, Duration::from_millis(1200));
            }
            _ => panic!("expected WorkbenchEvent::AgentModelCall"),
        }
        match rx.try_recv().expect("a finished tool") {
            WorkbenchEvent::AgentToolFinished { elapsed, .. } => {
                assert_eq!(elapsed, Duration::from_millis(30))
            }
            _ => panic!("expected WorkbenchEvent::AgentToolFinished"),
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ChannelSink::plan_run(tx).tool_finished("x", Duration::ZERO, false);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_builder_finishes_on_the_node_it_was_started_for_whatever_name_it_reports() {
        use crate::workbench::artifact::ArtifactKind;
        use crate::workbench::network::NodeStatus;
        let draft = std::sync::Arc::new(std::sync::Mutex::new(
            crate::workbench::tools::DraftState::new(None),
        ));
        draft
            .lock()
            .unwrap()
            .network
            .put_draft(
                ArtifactKind::Tool,
                "crawl4ai_crawl",
                "name: crawl4ai_crawl".into(),
                Vec::new(),
            )
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let progress = NetworkProgress {
            draft: draft.clone(),
            tx,
            builders: &["tool_drafter"],
            runs: Default::default(),
        };
        progress.subagent_started(
            7,
            "tool_drafter",
            "network_editor",
            &json!({"node": "crawl4ai_crawl"}),
        );
        progress.subagent_finished(
            7,
            "tool_drafter",
            &json!({"name": "crawl4ai", "status": "drafted", "summary": "sends the token"}),
            false,
            Duration::ZERO,
        );
        let network = &draft.lock().unwrap().network;
        let node = network.node(ArtifactKind::Tool, "crawl4ai_crawl").unwrap();
        assert_eq!(node.status, NodeStatus::Drafted);
        assert_eq!(node.note.as_deref(), Some("tool_drafter · sends the token"));
        assert!(network.node(ArtifactKind::Tool, "crawl4ai").is_none());
    }
}
