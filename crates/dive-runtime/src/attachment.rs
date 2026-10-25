use anyhow::{bail, Result};
use async_trait::async_trait;
use dive_core::agent::conversation::{ContextHook, Conversation, HandoffGuard};
use dive_core::{Attachment, EventSink, NewEntry, Store, ToolRegistry};
use serde_json::Value;
use std::any::Any;
use std::sync::Arc;

pub use dive_core::store::{
    ATTACHMENT_KINDS, EVIDENCE_ATTACHMENT, NETWORK_ATTACHMENT, PLAN_ATTACHMENT,
};

pub const THREAD_KINDS: &[&str] = &[NETWORK_ATTACHMENT, EVIDENCE_ATTACHMENT];

#[async_trait]
pub trait AttachmentHost: Send + Sync {
    fn kind(&self) -> &'static str;

    fn kinds(&self) -> &'static [&'static str];

    fn sections(&self, kind: &str) -> Vec<String>;

    fn loaded(&self) -> bool {
        true
    }

    fn context(&self) -> Vec<String> {
        if !self.loaded() {
            return Vec::new();
        }
        self.kinds()
            .iter()
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

pub fn context_hook(hosts: Vec<Arc<dyn AttachmentHost>>) -> ContextHook {
    Arc::new(move |_agent: &str| hosts.iter().flat_map(|host| host.context()).collect())
}

pub fn loaded(hosts: &[Arc<dyn AttachmentHost>]) -> Vec<Arc<dyn AttachmentHost>> {
    hosts.iter().filter(|host| host.loaded()).cloned().collect()
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
    let mut merged: Vec<Attachment> = loaded(hosts).iter().map(|host| host.record()).collect();
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

    struct Unloaded;

    #[async_trait]
    impl AttachmentHost for Unloaded {
        fn kind(&self) -> &'static str {
            EVIDENCE_ATTACHMENT
        }

        fn kinds(&self) -> &'static [&'static str] {
            &[EVIDENCE_ATTACHMENT]
        }

        fn sections(&self, kind: &str) -> Vec<String> {
            vec![format!("## {kind}")]
        }

        fn loaded(&self) -> bool {
            false
        }

        fn tools(&self, catalog: Arc<dyn ToolRegistry>) -> Arc<dyn ToolRegistry> {
            catalog
        }

        fn restore(&self, _history: &[NewEntry]) {}

        fn view(&self) -> Value {
            json!({})
        }

        fn record(&self) -> Attachment {
            Attachment::new(EVIDENCE_ATTACHMENT)
        }

        async fn act(&self, action: &str, _input: Value) -> Result<Value, String> {
            Err(format!("no action {action}"))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn every_agent_sees_what_is_loaded_and_nothing_else() {
        let hook = context_hook(vec![Arc::new(Fake), Arc::new(Unloaded)]);
        assert_eq!(hook("chat"), ["## network", "## plan"]);
        assert_eq!(hook("anyone"), ["## network", "## plan"]);
        let hosts: Vec<Arc<dyn AttachmentHost>> = vec![Arc::new(Unloaded)];
        assert!(records(&hosts, &[]).is_empty());
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

    #[tokio::test]
    async fn a_thread_takes_several_attachments_but_one_of_each_kind() {
        let store = dive_store::MemoryStore::new();
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
