use dive_core::trace::{Span, SpanKind};
use dive_core::EventSink;
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;

const MAX_LINES: usize = 80;
const SUMMARY_CHARS: usize = 240;
pub const FOCUS_MARK: &str = "  ← the focus";
const PLUMBING: [&str; 7] = [
    "builtin__reshape",
    "builtin__flatten",
    "builtin__dedupe",
    "builtin__take",
    "builtin__sort",
    "map",
    "reduce",
];

pub fn summary(value: &Value) -> String {
    if let Some(branch) = value.get("branch").and_then(Value::as_str) {
        let mut text = format!("took the {branch} branch");
        if let Some(verdict) = value.get("verdict").and_then(Value::as_bool) {
            text.push_str(&format!(", verdict {verdict}"));
        }
        if let Some(probability) = value.get("probability").and_then(Value::as_f64) {
            text.push_str(&format!(", probability {probability:.2}"));
        }
        return text;
    }
    let text = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    clip(&text)
}

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SUMMARY_CHARS {
        return flat;
    }
    format!("{}…", flat.chars().take(SUMMARY_CHARS).collect::<String>())
}

fn capped(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > MAX_LINES {
        let more = lines.len() - MAX_LINES;
        lines.truncate(MAX_LINES);
        lines.push(format!("({more} more steps not shown)"));
    }
    lines
}

fn text_attribute<'a>(span: &'a Span, key: &str) -> &'a str {
    span.attribute(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

pub fn from_spans(spans: &[Span], call: &str) -> Vec<String> {
    spans
        .iter()
        .find(|span| text_attribute(span, "gen_ai.tool.call.id") == call)
        .map(|root| outline(spans, &root.span_id, None))
        .unwrap_or_default()
}

pub fn outline(spans: &[Span], root: &str, focus: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    walk(spans, root, 0, focus, &mut lines);
    match lines.iter().position(|line| line.ends_with(FOCUS_MARK)) {
        Some(at) if at >= MAX_LINES => {
            let from = at - MAX_LINES / 2;
            let mut kept = vec![format!("({from} earlier steps not shown)")];
            kept.extend(lines.drain(from..));
            capped(kept)
        }
        _ => capped(lines),
    }
}

fn speaks(spans: &[Span], span: &Span) -> bool {
    span.kind == SpanKind::Model
        && span
            .parent_span_id
            .as_deref()
            .and_then(|parent| spans.iter().find(|other| other.span_id == parent))
            .is_some_and(|parent| matches!(parent.kind, SpanKind::Agent | SpanKind::Turn))
}

fn line(span: &Span, depth: usize) -> String {
    let status = if span.is_error() { "failed" } else { "ok" };
    let took = span
        .ms()
        .map(|ms| format!(" in {ms}ms"))
        .unwrap_or_default();
    if span.kind == SpanKind::Model {
        let role = text_attribute(span, "dive.role");
        return format!(
            "{}model call, {} role ({}){took}",
            "  ".repeat(depth),
            if role.is_empty() { "chat" } else { role },
            span.name,
        );
    }
    let path = text_attribute(span, "dive.step.path");
    let output = span.output.as_ref().map(summary).unwrap_or_default();
    format!(
        "{}{}{} {status}{took}: {output}",
        "  ".repeat(depth),
        if path.is_empty() {
            String::new()
        } else {
            format!("{path} ")
        },
        span.name,
    )
}

fn walk(spans: &[Span], parent: &str, depth: usize, focus: Option<&str>, lines: &mut Vec<String>) {
    let mut children: Vec<&Span> = spans
        .iter()
        .filter(|span| span.parent_span_id.as_deref() == Some(parent))
        .collect();
    children.sort_by_key(|span| span.start_time_unix_nano);
    for span in children {
        let shown = (matches!(span.kind, SpanKind::Plan | SpanKind::Agent | SpanKind::Tool)
            && !PLUMBING.contains(&span.name.as_str()))
            || speaks(spans, span)
            || focus == Some(span.span_id.as_str());
        if shown {
            let mut text = line(span, depth);
            if focus == Some(span.span_id.as_str()) {
                text.push_str(FOCUS_MARK);
            }
            lines.push(text);
        }
        walk(
            spans,
            &span.span_id,
            depth + usize::from(shown),
            focus,
            lines,
        );
    }
}

#[derive(Default)]
pub struct StepLog {
    lines: Mutex<Vec<String>>,
}

impl StepLog {
    pub fn lines(&self) -> Vec<String> {
        capped(self.lines.lock().unwrap().clone())
    }
}

impl EventSink for StepLog {
    fn step_finished(
        &self,
        call_stack: &[String],
        path: &str,
        tool: &str,
        result: &Value,
        is_error: bool,
        elapsed: Duration,
    ) {
        if PLUMBING.contains(&tool) {
            return;
        }
        let at = call_stack
            .iter()
            .map(String::as_str)
            .chain([path])
            .collect::<Vec<_>>()
            .join(" › ");
        let status = if is_error { "failed" } else { "ok" };
        self.lines.lock().unwrap().push(format!(
            "{at} {tool} {status} in {}ms: {}",
            elapsed.as_millis(),
            summary(result)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_core::trace::Status;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn span(id: &str, parent: Option<&str>, name: &str, kind: SpanKind, at: u64) -> Span {
        Span {
            trace_id: "t".into(),
            span_id: id.into(),
            parent_span_id: parent.map(str::to_string),
            name: name.into(),
            kind,
            start_time_unix_nano: at,
            end_time_unix_nano: Some(at + 2_000_000),
            status: Status::default(),
            attributes: BTreeMap::new(),
            input: None,
            output: None,
        }
    }

    #[test]
    fn a_call_s_spans_become_an_outline_of_what_ran_and_which_branch() {
        let mut root = span("r", Some("turn"), "plan__research", SpanKind::Tool, 1);
        root.attributes
            .insert("gen_ai.tool.call.id".into(), json!("toolu_1"));
        let mut quick = span("q", Some("r"), "plan__research_quick", SpanKind::Plan, 2);
        quick
            .attributes
            .insert("dive.step.path".into(), json!("depth/quick/quick_run"));
        quick.output = Some(json!({"result": "The sources do not establish its race name"}));
        let reshape = span("x", Some("q"), "builtin__reshape", SpanKind::Tool, 3);
        let mut read = span("p", Some("x"), "user__crawl4ai_read", SpanKind::Tool, 4);
        read.output = Some(json!({"markdown": "# 2026 calendar", "success": true}));
        let model = span("m", Some("r"), "decision", SpanKind::Model, 5);
        let mut deep = span("d", Some("r"), "plan__research_deep", SpanKind::Plan, 6);
        deep.attributes.insert(
            "dive.step.path".into(),
            json!("depth/quick/quick_check/else/esc_run"),
        );
        let other = span("o", Some("turn"), "plan__other", SpanKind::Tool, 7);
        let spans = vec![root, quick, reshape, read, model, deep, other];

        let lines = from_spans(&spans, "toolu_1");
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with("depth/quick/quick_run plan__research_quick ok in 2ms"));
        assert!(lines[0].contains("do not establish its race name"));
        assert!(
            lines[1].starts_with("  user__crawl4ai_read ok"),
            "{lines:?}"
        );
        assert!(lines[2].starts_with("depth/quick/quick_check/else/esc_run plan__research_deep"));
        assert!(from_spans(&spans, "toolu_missing").is_empty());
    }

    #[test]
    fn the_step_log_names_each_gate_s_branch_and_skips_plumbing() {
        let log = StepLog::default();
        log.step_finished(
            &["research".into()],
            "depth/quick/quick_check",
            "route",
            &json!({"branch": "else", "verdict": false, "probability": 0.31, "result": {}}),
            false,
            Duration::from_millis(40),
        );
        log.step_finished(&[], "rows", "map", &json!([]), false, Duration::ZERO);
        log.step_finished(
            &["research".into(), "research_quick".into()],
            "answer",
            "builtin__infer",
            &json!({"text": "x ".repeat(400)}),
            false,
            Duration::from_millis(900),
        );
        let lines = log.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "research › depth/quick/quick_check route ok in 40ms: took the else branch, verdict false, probability 0.31"
        );
        assert!(
            lines[1].starts_with("research › research_quick › answer builtin__infer ok in 900ms")
        );
        assert!(lines[1].ends_with('…'));
    }
}
