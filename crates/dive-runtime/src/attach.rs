use crate::attachment::AttachmentHost;
use crate::evidence::{from_call, Editable, EvidenceAttachment};
use crate::workbench::{ArtifactKind, NetworkAttachment};
use async_trait::async_trait;
use dive_core::agent::doc::AgentSet;
use dive_core::pipeline::Pipeline;
use dive_core::tools::{ToolDef, ToolError, ToolOutcome, ToolRegistry};
use dive_core::{EventSink, Store};
use serde_json::{json, Value};
use std::sync::Arc;

pub const CREATE_NETWORK: &str = "builtin__create_network";

pub const OPEN_NETWORK: &str = "builtin__open_network";

pub const ATTACH_EVIDENCE: &str = "builtin__attach_evidence";

pub const ATTACH_TOOLS: [&str; 3] = [CREATE_NETWORK, OPEN_NETWORK, ATTACH_EVIDENCE];

pub struct AttachTools {
    pub network: Arc<NetworkAttachment>,
    pub evidence: Arc<EvidenceAttachment>,
    pub store: Arc<dyn Store>,
    pub agents: Arc<AgentSet>,
    pub pipeline: Arc<Pipeline>,
    pub events: Arc<dyn EventSink>,
}

fn def(name: &str, description: &str, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
        output_schema: None,
        output_example: None,
        read_only: None,
        file_input: Default::default(),
    }
}

fn done(result: Value) -> ToolOutcome {
    ToolOutcome {
        result,
        is_error: false,
    }
}

fn failed(error: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        result: json!({ "error": error.into() }),
        is_error: true,
    }
}

impl AttachTools {
    fn changed(&self, host: &dyn AttachmentHost) {
        self.events.attachment_changed(host.kind(), &host.view());
    }

    async fn editable(&self) -> Editable {
        let tools = self
            .pipeline
            .registry
            .tools()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|def| def.name)
            .filter(|name| {
                name.starts_with("user__")
                    || name.contains("__tool__")
                    || (name.starts_with("builtin__") && !ATTACH_TOOLS.contains(&name.as_str()))
            })
            .collect();
        Editable {
            agents: self.agents.iter().map(|doc| doc.name).collect(),
            plans: self
                .pipeline
                .plans
                .iter()
                .map(|plan| plan.identifier.clone())
                .collect(),
            tools,
        }
    }

    fn create_network(&self) -> ToolOutcome {
        if !self.network.load() {
            return done(json!({
                "created": false,
                "note": "this thread already has a network draft; keep working on it"
            }));
        }
        self.changed(self.network.as_ref());
        done(json!({ "created": true }))
    }

    async fn open_network(&self, input: &Value) -> ToolOutcome {
        let Some(kind) = input["kind"].as_str().and_then(ArtifactKind::parse) else {
            return failed("kind must be agent, plan or tool");
        };
        let Some(name) = input["name"].as_str().filter(|name| !name.is_empty()) else {
            return failed("name is required: the agent, plan or tool to open");
        };
        match self.network.open_item(kind, name).await {
            Ok(opened) => {
                self.changed(self.network.as_ref());
                done(json!({ "opened": opened }))
            }
            Err(error) => failed(error),
        }
    }

    async fn attach_evidence(&self, input: &Value) -> ToolOutcome {
        let (Some(thread), Some(call)) = (input["thread"].as_str(), input["call"].as_str()) else {
            return failed("thread and call are required: the thread id and the call's id in it");
        };
        let editable = self.editable().await;
        match from_call(&editable, self.store.as_ref(), thread, call).await {
            Ok(evidence) => {
                let item = format!("{} {}", evidence.item.kind.label(), evidence.item.name);
                self.evidence.load(evidence);
                self.changed(self.evidence.as_ref());
                done(json!({ "attached": item }))
            }
            Err(error) => failed(error),
        }
    }
}

#[async_trait]
impl ToolRegistry for AttachTools {
    async fn tools(&self) -> Result<Vec<ToolDef>, ToolError> {
        Ok(vec![
            def(
                CREATE_NETWORK,
                "Starts a blank network draft in this thread, for building new agents, plans and tools. Call it before drafting when the thread has no network draft. Every agent in the thread then sees the draft.",
                json!({"type": "object", "properties": {}}),
            ),
            def(
                OPEN_NETWORK,
                "Opens an existing agent, plan or tool in this thread's network draft so it can be changed, starting the draft if there isn't one.",
                json!({
                    "type": "object",
                    "required": ["kind", "name"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["agent", "plan", "tool"], "description": "What to open"},
                        "name": {"type": "string", "description": "Its name: an agent name, a plan identifier, or a tool name"}
                    }
                }),
            ),
            def(
                ATTACH_EVIDENCE,
                "Brings one call from another thread into this thread as evidence: the agent, plan or tool that was called, its input, and its output or error. Use it to work on why a run went wrong.",
                json!({
                    "type": "object",
                    "required": ["thread", "call"],
                    "properties": {
                        "thread": {"type": "string", "description": "The id of the thread the call is in"},
                        "call": {"type": "string", "description": "The call's id in that thread"}
                    }
                }),
            ),
        ])
    }

    async fn invoke(&self, name: &str, input: Value) -> Result<ToolOutcome, ToolError> {
        match name {
            CREATE_NETWORK => Ok(self.create_network()),
            OPEN_NETWORK => Ok(self.open_network(&input).await),
            ATTACH_EVIDENCE => Ok(self.attach_evidence(&input).await),
            other => Err(ToolError::Unknown(other.to_string())),
        }
    }
}
