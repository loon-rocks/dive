use crate::attachment::{AttachmentHost, EVIDENCE_ATTACHMENT};
use crate::runtime::Runtime;
use crate::workbench::ArtifactKind;
use anyhow::Result;
use async_trait::async_trait;
use dive_core::{Attachment, EntryBody, NewEntry, Store, ToolRegistry};
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
    let (asked, author) = asked.ok_or_else(|| format!("thread {thread} has no call {call}"))?;
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
    Ok(Evidence {
        thread: thread.to_string(),
        call: Some(call.to_string()),
        item,
        input: asked.arguments,
        output: (!is_error).then(|| parsed(&content)),
        error: is_error.then(|| error_text(&content)),
    })
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
        let mut long = evidence();
        long.error = None;
        long.output = Some(json!("x".repeat(SHOWN_CHARS + 10)));
        assert!(long.section().contains(&format!(
            "(the first {SHOWN_CHARS} of {} characters)",
            SHOWN_CHARS + 10
        )));
    }
}
