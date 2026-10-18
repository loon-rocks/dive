use crate::attachment::{AttachmentHost, EVIDENCE_ATTACHMENT};
use crate::workbench::ArtifactKind;
use anyhow::Result;
use async_trait::async_trait;
use graph_core::{Attachment, NewEntry, ToolRegistry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::any::Any;
use std::sync::Arc;

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
        let mut section = format!(
            "## The evidence\nThis edit is about one call of the {} {}, from thread {}{}. Test the edits against its input.\n",
            self.item.kind.label(),
            self.item.name,
            self.thread,
            self.call
                .as_deref()
                .map(|call| format!(" (call {call})"))
                .unwrap_or_default()
        );
        section.push_str(&format!("\n### Input\n{}\n", shown(&self.input)));
        match (&self.error, &self.output) {
            (Some(error), _) => section.push_str(&format!("\n### Error\n{error}\n")),
            (None, Some(output)) => section.push_str(&format!("\n### Output\n{}\n", shown(output))),
            (None, None) => section.push_str("\n(no output was recorded)\n"),
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

pub struct EvidenceAttachment {
    evidence: Evidence,
}

impl EvidenceAttachment {
    pub fn new(evidence: Evidence) -> Self {
        Self { evidence }
    }

    pub fn evidence(&self) -> &Evidence {
        &self.evidence
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
        match kind {
            EVIDENCE_ATTACHMENT => vec![self.evidence.section()],
            _ => Vec::new(),
        }
    }

    fn tools(&self, catalog: Arc<dyn ToolRegistry>) -> Arc<dyn ToolRegistry> {
        catalog
    }

    fn restore(&self, _history: &[NewEntry]) {}

    fn view(&self) -> Value {
        serde_json::to_value(&self.evidence).unwrap_or_default()
    }

    fn record(&self) -> Attachment {
        self.evidence.record()
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
        let mut long = evidence();
        long.error = None;
        long.output = Some(json!("x".repeat(SHOWN_CHARS + 10)));
        assert!(long.section().contains(&format!(
            "(the first {SHOWN_CHARS} of {} characters)",
            SHOWN_CHARS + 10
        )));
    }
}
