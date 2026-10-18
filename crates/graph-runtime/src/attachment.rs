use anyhow::{bail, Result};
use async_trait::async_trait;
use graph_core::agent::conversation::{ContextHook, Conversation, HandoffGuard};
use graph_core::agent::doc::{AgentDoc, AgentSet};
use graph_core::{Attachment, EventSink, NewEntry, Store, ToolRegistry};
use serde_json::Value;
use std::any::Any;
use std::sync::Arc;

pub use graph_core::store::{
    ATTACHMENT_KINDS, EVIDENCE_ATTACHMENT, NETWORK_ATTACHMENT, PLAN_ATTACHMENT,
};

pub const THREAD_KINDS: &[&str] = &[NETWORK_ATTACHMENT, EVIDENCE_ATTACHMENT];

#[async_trait]
pub trait AttachmentHost: Send + Sync {
    fn kind(&self) -> &'static str;

    fn kinds(&self) -> &'static [&'static str];

    fn sections(&self, kind: &str) -> Vec<String>;

    fn context(&self, agent: &AgentDoc) -> Vec<String> {
        self.kinds()
            .iter()
            .filter(|kind| agent.reads(kind))
            .flat_map(|kind| self.sections(kind))
            .collect()
    }

    fn tools(&self, catalog: Arc<dyn ToolRegistry>) -> Arc<dyn ToolRegistry>;

    fn wire(&self, _conversation: &mut Conversation) {}

    fn observer(&self) -> Option<Arc<dyn EventSink>> {
        None
    }

    fn restore(&self, history: &[NewEntry]);

    fn view(&self) -> Value;

    fn record(&self) -> Attachment;

    async fn act(&self, action: &str, input: Value) -> Result<Value, String>;

    fn as_any(&self) -> &dyn Any;
}

pub fn agent_sources(kind: &str) -> Vec<&'static str> {
    match kind {
        NETWORK_ATTACHMENT | EVIDENCE_ATTACHMENT => {
            crate::workbench::agents::WORKBENCH_AGENTS.to_vec()
        }
        _ => Vec::new(),
    }
}

pub fn agent_sources_for(kinds: &[String]) -> Vec<&'static str> {
    let mut sources: Vec<&'static str> = Vec::new();
    for source in kinds.iter().flat_map(|kind| agent_sources(kind)) {
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    sources
}

pub async fn record(store: &dyn Store, thread_id: &str, attachments: &[Attachment]) -> Result<()> {
    for (at, attachment) in attachments.iter().enumerate() {
        if attachments[..at]
            .iter()
            .any(|earlier| earlier.kind == attachment.kind)
        {
            bail!(
                "a thread carries one attachment of each kind: it has two {} attachments",
                attachment.kind
            );
        }
    }
    store.set_attachments(thread_id, attachments).await?;
    Ok(())
}

pub fn add_handoff_guard(slot: &mut Option<HandoffGuard>, guard: HandoffGuard) {
    *slot = Some(match slot.take() {
        Some(earlier) => {
            Arc::new(move |from: &str, to: &str| earlier(from, to).or_else(|| guard(from, to)))
        }
        None => guard,
    });
}

pub fn add_tools(tools: &mut Vec<String>, added: impl IntoIterator<Item = String>) {
    for tool in added {
        if !tools.contains(&tool) {
            tools.push(tool);
        }
    }
}

pub fn context_hook(agents: Arc<AgentSet>, hosts: Vec<Arc<dyn AttachmentHost>>) -> ContextHook {
    Arc::new(move |agent: &str| match agents.get(agent) {
        Some(doc) => hosts.iter().flat_map(|host| host.context(&doc)).collect(),
        None => Vec::new(),
    })
}

pub fn thread_kinds(declared: &[String]) -> Vec<String> {
    let mut kinds: Vec<String> = Vec::new();
    for kind in declared {
        if THREAD_KINDS.contains(&kind.as_str()) && !kinds.contains(kind) {
            kinds.push(kind.clone());
        }
    }
    kinds
}

pub fn records(hosts: &[Arc<dyn AttachmentHost>], recorded: &[Attachment]) -> Vec<Attachment> {
    let mut merged: Vec<Attachment> = hosts.iter().map(|host| host.record()).collect();
    merged.extend(
        recorded
            .iter()
            .filter(|kept| hosts.iter().all(|host| host.kind() != kept.kind))
            .cloned(),
    );
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::agent::doc::builtin_agents;
    use serde_json::json;

    struct Fake;

    #[async_trait]
    impl AttachmentHost for Fake {
        fn kind(&self) -> &'static str {
            NETWORK_ATTACHMENT
        }

        fn kinds(&self) -> &'static [&'static str] {
            &[NETWORK_ATTACHMENT, PLAN_ATTACHMENT]
        }

        fn sections(&self, kind: &str) -> Vec<String> {
            vec![format!("## {kind}")]
        }

        fn tools(&self, catalog: Arc<dyn ToolRegistry>) -> Arc<dyn ToolRegistry> {
            catalog
        }

        fn restore(&self, _history: &[NewEntry]) {}

        fn view(&self) -> Value {
            json!({})
        }

        fn record(&self) -> Attachment {
            Attachment::new(NETWORK_ATTACHMENT)
        }

        async fn act(&self, action: &str, _input: Value) -> Result<Value, String> {
            Err(format!("no action {action}"))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn agent(name: &str, attachments: &str) -> String {
        format!(
            "name: {name}\ndescription: d\nmodel: chat\ntools: []\n{attachments}system_prompt: p\n"
        )
    }

    #[test]
    fn an_agent_gets_an_attachments_sections_only_for_the_kinds_it_reads() {
        let agents = Arc::new(AgentSet::layered(
            builtin_agents(&[
                &agent("drafter", "attachments: [network]\n"),
                &agent("editor", "attachments: [plan]\n"),
                &agent("both", "attachments: [plan, network]\n"),
                &agent("plain", ""),
            ]),
            Vec::new(),
        ));
        let hook = context_hook(agents, vec![Arc::new(Fake)]);
        assert_eq!(hook("drafter"), ["## network"]);
        assert_eq!(hook("editor"), ["## plan"]);
        assert_eq!(hook("both"), ["## network", "## plan"]);
        assert!(hook("plain").is_empty());
        assert!(hook("nobody").is_empty());
    }

    #[test]
    fn a_thread_takes_every_kind_it_can_host_once() {
        let declared = |kinds: &[&str]| kinds.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        assert_eq!(thread_kinds(&declared(&["network"])), ["network"]);
        assert!(thread_kinds(&declared(&["plan"])).is_empty());
        assert_eq!(thread_kinds(&declared(&["plan", "network"])), ["network"]);
        assert_eq!(
            thread_kinds(&declared(&["network", "evidence", "network"])),
            ["network", "evidence"]
        );
        assert!(thread_kinds(&[]).is_empty());
    }

    #[test]
    fn the_agents_of_several_kinds_load_once() {
        let kinds = vec![
            NETWORK_ATTACHMENT.to_string(),
            EVIDENCE_ATTACHMENT.to_string(),
        ];
        assert_eq!(
            agent_sources_for(&kinds).len(),
            crate::workbench::agents::WORKBENCH_AGENTS.len()
        );
    }

    #[tokio::test]
    async fn a_thread_takes_several_attachments_but_one_of_each_kind() {
        let store = graph_store::MemoryStore::new();
        let meta = store.create_thread("t", "chat", None).await.unwrap();
        let both = [
            Attachment::new(NETWORK_ATTACHMENT),
            Attachment::new(EVIDENCE_ATTACHMENT).with("thread", "a1"),
        ];
        record(&store, &meta.id, &both).await.unwrap();
        let error = record(
            &store,
            &meta.id,
            &[
                Attachment::new(NETWORK_ATTACHMENT),
                Attachment::new(NETWORK_ATTACHMENT),
            ],
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("one attachment of each kind"), "{error}");
        let kept = store.get_thread(&meta.id).await.unwrap().unwrap();
        assert_eq!(kept.attachments, both);
    }

    #[test]
    fn a_second_host_adds_its_guard_and_tools_to_the_first_ones() {
        let mut tools = vec!["a".to_string()];
        add_tools(&mut tools, ["a".to_string(), "b".to_string()]);
        assert_eq!(tools, ["a", "b"]);
        let mut guarded: Option<HandoffGuard> = Some(Arc::new(|from: &str, _: &str| {
            (from == "x").then(|| "first".to_string())
        }));
        add_handoff_guard(
            &mut guarded,
            Arc::new(|from: &str, _: &str| (from == "y").then(|| "second".to_string())),
        );
        let guard = guarded.unwrap();
        assert_eq!(guard("x", "z").as_deref(), Some("first"));
        assert_eq!(guard("y", "z").as_deref(), Some("second"));
        assert_eq!(guard("z", "x"), None);
    }

    #[test]
    fn records_keep_kinds_no_host_serves() {
        let hosts: Vec<Arc<dyn AttachmentHost>> = vec![Arc::new(Fake)];
        let recorded = [
            Attachment::new(NETWORK_ATTACHMENT).with("state", "saved"),
            Attachment::new("future"),
        ];
        assert_eq!(
            records(&hosts, &recorded),
            [
                Attachment::new(NETWORK_ATTACHMENT),
                Attachment::new("future")
            ]
        );
    }
}
