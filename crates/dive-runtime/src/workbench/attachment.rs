use super::artifact::{self, ArtifactKind, SaveRequest, Saved, Scope, Tester};
use super::event::{GateKind, WorkbenchEvent};
use super::network::NetworkView;
use super::runner::{DebugControls, UiDecision};
use super::tools::{DraftState, SharedDraft};
use crate::attachment::{AttachmentHost, NETWORK_ATTACHMENT, PLAN_ATTACHMENT};
use anyhow::{Context, Result};
use async_trait::async_trait;
use dive_core::agent::conversation::Conversation;
use dive_core::pipeline::authoring;
use dive_core::pipeline::doc::PlanDoc;
use dive_core::pipeline::Pipeline;
use dive_core::{
    Attachment, CompositeRegistry, EventSink, ExcludingRegistry, NewEntry, TeeSink, ToolRegistry,
};
use serde::Serialize;
use serde_json::Value;
use std::any::Any;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub const SAVE: &str = "save";

pub const DISCARD: &str = "discard";

pub const OPENED: &str = "opened";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkState {
    #[default]
    Open,
    Saved,
    Discarded,
}

impl NetworkState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Saved => "saved",
            Self::Discarded => "discarded",
        }
    }

    fn parse(text: Option<&str>) -> Self {
        match text {
            Some("saved") => Self::Saved,
            Some("discarded") => Self::Discarded,
            _ => Self::Open,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Lifecycle {
    state: NetworkState,
    saved: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanPane {
    pub identifier: String,
    pub name: String,
    pub dirty: bool,
    pub yaml: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NetworkAttachmentView {
    pub state: NetworkState,
    pub saved: Option<String>,
    pub network: NetworkView,
    pub plan: Option<PlanPane>,
}

pub struct NetworkSetup {
    pub pipeline: Arc<Pipeline>,
    pub tx: UnboundedSender<WorkbenchEvent>,
    pub plans_dir: Option<PathBuf>,
    pub root: PathBuf,
    pub doc: Option<PlanDoc>,
}

pub struct NetworkAttachment {
    pub draft: SharedDraft,
    pub pipeline: Arc<Pipeline>,
    pub debug: Arc<DebugControls>,
    pub plans_dir: Option<PathBuf>,
    pub root: PathBuf,
    tx: UnboundedSender<WorkbenchEvent>,
    tester: Tester,
    tools: Arc<dyn ToolRegistry>,
    lifecycle: Arc<Mutex<Lifecycle>>,
    opened: Mutex<Vec<(ArtifactKind, String)>>,
    loaded: Arc<AtomicBool>,
}

impl NetworkAttachment {
    pub fn new(setup: NetworkSetup) -> Result<Self> {
        let NetworkSetup {
            pipeline,
            tx,
            plans_dir,
            root,
            doc,
        } = setup;
        let mut state = DraftState::new(doc);
        state.network.secrets = Some(pipeline.drafted.secrets.clone());
        let draft = Arc::new(Mutex::new(state));
        let debug = Arc::new(DebugControls::default());
        let tester: Tester = Arc::new(std::sync::OnceLock::new());
        let loaded = Arc::new(AtomicBool::new(true));
        let workbench_tools = Arc::new(
            super::tools::WorkbenchTools::new(
                draft.clone(),
                pipeline.clone(),
                plans_dir.clone(),
                debug.clone(),
                tx.clone(),
            )
            .with_tester(tester.clone())
            .with_root(root.clone())
            .with_loaded(loaded.clone()),
        );
        let fs_tools = Arc::new(
            super::fs_tools::FsTools::new(root.clone())
                .context("failed to resolve the workbench project directory")?,
        );
        let tools: Arc<dyn ToolRegistry> =
            Arc::new(CompositeRegistry::new(vec![workbench_tools, fs_tools]));
        Ok(Self {
            draft,
            pipeline,
            debug,
            plans_dir,
            root,
            tx,
            tester,
            tools,
            lifecycle: Arc::new(Mutex::new(Lifecycle::default())),
            opened: Mutex::new(Vec::new()),
            loaded,
        })
    }

    pub fn unloaded(self) -> Self {
        self.loaded.store(false, Ordering::SeqCst);
        self
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    pub fn load(&self) -> bool {
        !self.loaded.swap(true, Ordering::SeqCst)
    }

    pub fn network(&self) -> NetworkView {
        artifact::view(&self.draft)
    }

    pub fn plan(&self) -> Option<PlanDoc> {
        self.draft.lock().unwrap().doc.clone()
    }

    pub fn state(&self) -> NetworkState {
        self.lifecycle.lock().unwrap().state.clone()
    }

    pub fn recall(&self, recorded: &Attachment) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        lifecycle.state = NetworkState::parse(recorded.field("state"));
        lifecycle.saved = recorded.field("saved").map(str::to_string);
        *self.opened.lock().unwrap() = recorded
            .fields
            .get(OPENED)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some((
                            ArtifactKind::parse(item["kind"].as_str()?)?,
                            item["name"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    pub fn opened(&self) -> Vec<(ArtifactKind, String)> {
        self.opened.lock().unwrap().clone()
    }

    pub async fn open_item(&self, kind: ArtifactKind, name: &str) -> Result<String, String> {
        let opened = self.open_existing(kind, name).await?;
        self.load();
        let mut items = self.opened.lock().unwrap();
        if !items
            .iter()
            .any(|(known, known_name)| *known == kind && known_name == name)
        {
            items.push((kind, name.to_string()));
        }
        Ok(opened)
    }

    pub fn attachment_view(&self) -> NetworkAttachmentView {
        view_of(&self.draft, &self.lifecycle)
    }

    pub async fn open_existing(&self, kind: ArtifactKind, name: &str) -> Result<String, String> {
        artifact::open_existing(&self.draft, &self.pipeline, &self.tx, kind, name).await
    }

    pub async fn save(&self, scope: Option<&str>, skip: Vec<String>) -> Result<Saved, String> {
        let saved = if self.network().is_empty() {
            super::save_draft(&self.draft, self.plans_dir.as_deref()).map(|path| Saved {
                saved: path,
                files: Vec::new(),
                untested: true,
            })
        } else {
            artifact::save_network(
                &self.draft,
                &self.pipeline,
                &self.tx,
                SaveRequest {
                    plans_dir: self.plans_dir.as_deref(),
                    root: Some(&self.root),
                    scope: Scope::parse(scope),
                    skip,
                    untested: true,
                    confirm: false,
                },
            )
            .await
        }?;
        let mut lifecycle = self.lifecycle.lock().unwrap();
        lifecycle.state = NetworkState::Saved;
        lifecycle.saved = Some(saved.saved.clone());
        Ok(saved)
    }

    pub fn discard(&self) -> Option<String> {
        let discarded = artifact::discard_network(&self.draft, &self.tx);
        if discarded.is_some() {
            let mut lifecycle = self.lifecycle.lock().unwrap();
            lifecycle.state = NetworkState::Discarded;
            lifecycle.saved = None;
        }
        discarded
    }

    pub fn forward(&self, rx: UnboundedReceiver<WorkbenchEvent>, events: Arc<dyn EventSink>) {
        tokio::spawn(forward(
            rx,
            events,
            self.draft.clone(),
            self.lifecycle.clone(),
        ));
    }
}

fn view_of(draft: &SharedDraft, lifecycle: &Mutex<Lifecycle>) -> NetworkAttachmentView {
    let network = artifact::view(draft);
    let plan = {
        let state = draft.lock().unwrap();
        state.doc.as_ref().map(|doc| PlanPane {
            identifier: doc.identifier.clone(),
            name: doc.name.clone(),
            dirty: state.dirty,
            yaml: authoring::to_yaml(doc).ok(),
        })
    };
    let lifecycle = lifecycle.lock().unwrap().clone();
    let settled = network.is_empty() || (network.is_edit() && network.unsaved() == 0);
    let state = if settled {
        lifecycle.state
    } else {
        NetworkState::Open
    };
    NetworkAttachmentView {
        saved: (state == NetworkState::Saved)
            .then_some(lifecycle.saved)
            .flatten(),
        state,
        network,
        plan,
    }
}

async fn forward(
    mut rx: UnboundedReceiver<WorkbenchEvent>,
    events: Arc<dyn EventSink>,
    draft: SharedDraft,
    lifecycle: Arc<Mutex<Lifecycle>>,
) {
    while let Some(event) = rx.recv().await {
        match event {
            WorkbenchEvent::NetworkChanged(_)
            | WorkbenchEvent::DraftReplaced { .. }
            | WorkbenchEvent::Saved(_) => {
                let view = view_of(&draft, &lifecycle);
                events.attachment_changed(
                    NETWORK_ATTACHMENT,
                    &serde_json::to_value(view).unwrap_or_default(),
                );
            }
            WorkbenchEvent::GateAsk { kind, reply, .. } => {
                let _ = reply.send(match kind {
                    GateKind::Ask { .. } => UiDecision::Abort,
                    GateKind::BeforeCall | GateKind::OnError { .. } => UiDecision::Proceed {
                        continue_mode: true,
                    },
                });
            }
            _ => {}
        }
    }
}

struct LifecycleObserver(Arc<Mutex<Lifecycle>>);

impl EventSink for LifecycleObserver {
    fn tool_finished(&self, name: &str, _elapsed: Duration, is_error: bool) {
        if is_error {
            return;
        }
        let state = match name {
            artifact::SAVE_NETWORK => NetworkState::Saved,
            artifact::DISCARD_NETWORK => NetworkState::Discarded,
            _ => return,
        };
        let mut lifecycle = self.0.lock().unwrap();
        lifecycle.state = state;
        lifecycle.saved = None;
    }
}

#[async_trait]
impl AttachmentHost for NetworkAttachment {
    fn kind(&self) -> &'static str {
        NETWORK_ATTACHMENT
    }

    fn kinds(&self) -> &'static [&'static str] {
        &[NETWORK_ATTACHMENT, PLAN_ATTACHMENT]
    }

    fn sections(&self, kind: &str) -> Vec<String> {
        match kind {
            NETWORK_ATTACHMENT => vec![
                super::agents::model_roles(&self.pipeline),
                artifact::context_section(&self.draft),
            ],
            PLAN_ATTACHMENT => match self.plan() {
                Some(doc) => vec![super::agents::current_draft(&self.pipeline, &Some(doc))],
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn loaded(&self) -> bool {
        self.is_loaded()
    }

    fn tools(&self, catalog: Arc<dyn ToolRegistry>) -> Arc<dyn ToolRegistry> {
        let visible: Arc<dyn ToolRegistry> = Arc::new(ExcludingRegistry::new(
            catalog,
            vec!["plan_and_execute".to_string()],
        ));
        Arc::new(CompositeRegistry::new(vec![
            Arc::new(super::tools::ComposedCapture::new(
                visible,
                self.draft.clone(),
            )),
            self.tools.clone(),
        ]))
    }

    fn wire(&self, conversation: &mut Conversation) {
        crate::attachment::add_handoff_guard(
            &mut conversation.handoff_guard,
            artifact::handoff_guard(self.draft.clone()),
        );
        crate::attachment::add_tools(
            &mut conversation.progress_tools,
            super::tools::progress_tools(),
        );
        crate::attachment::add_tools(
            &mut conversation.stop_tools,
            artifact::STOP_TOOLS.iter().map(|name| name.to_string()),
        );
        crate::attachment::add_tools(
            &mut conversation.named_standing_tools,
            ["workbench__*".to_string()],
        );
        let mut base = conversation.clone();
        base.catalog = Arc::new(CompositeRegistry::new(Vec::new()));
        base.context = None;
        base.handoff_guard = None;
        let _ = self.tester.set(base);
    }

    fn observer(&self) -> Option<Arc<dyn EventSink>> {
        Some(Arc::new(TeeSink::new(vec![
            Arc::new(super::sink::NetworkProgress {
                draft: self.draft.clone(),
                tx: self.tx.clone(),
                builders: &["tool_drafter", "plan_drafter", "agent_drafter"],
                runs: Default::default(),
            }),
            Arc::new(LifecycleObserver(self.lifecycle.clone())),
        ])))
    }

    fn restore(&self, history: &[NewEntry]) {
        let pipeline = self.pipeline.clone();
        let items =
            move |kind: ArtifactKind, name: &str| artifact::existing(kind, name, &pipeline).ok();
        let mut network = super::replay::network(history, &self.opened(), &items);
        network.secrets = Some(self.pipeline.drafted.secrets.clone());
        self.draft.lock().unwrap().network = network;
    }

    fn view(&self) -> Value {
        serde_json::to_value(self.attachment_view()).unwrap_or_default()
    }

    fn record(&self) -> Attachment {
        let view = self.attachment_view();
        let mut record = Attachment::new(NETWORK_ATTACHMENT).with("state", view.state.label());
        if let Some(saved) = view.saved {
            record = record.with("saved", saved);
        }
        let opened = self.opened();
        if !opened.is_empty() {
            record = record.with(
                OPENED,
                opened
                    .iter()
                    .map(|(kind, name)| serde_json::json!({"kind": kind.label(), "name": name}))
                    .collect::<Vec<_>>(),
            );
        }
        record
    }

    async fn act(&self, action: &str, input: Value) -> Result<Value, String> {
        match action {
            SAVE => {
                let skip = input["skip"]
                    .as_array()
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                self.save(input["scope"].as_str(), skip)
                    .await
                    .map(|saved| serde_json::to_value(saved).unwrap_or_default())
            }
            DISCARD => Ok(self.discard().map(Value::String).unwrap_or(Value::Null)),
            other => Err(format!(
                "the network attachment has no '{other}' action (it has {SAVE} and {DISCARD})"
            )),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
