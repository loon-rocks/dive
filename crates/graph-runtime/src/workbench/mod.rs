pub mod agents;
pub mod artifact;
pub mod attachment;
mod effects;
pub mod event;
pub mod fs_tools;
pub mod network;
pub mod replay;
pub mod runner;
pub mod secrets;
pub mod sink;
pub mod tools;

pub use agents::Start;
pub use artifact::ArtifactKind;
pub use attachment::{NetworkAttachment, NetworkAttachmentView, NetworkSetup, NetworkState};
pub use effects::{save_draft, Workbench};
pub use event::{GateKind, WorkbenchEvent};
pub use network::{
    Design, Designs, EdgeKind, Fact, FactState, Interview, NetworkView, NodeSecret, NodeStatus,
    NodeView, Restate, TestRun,
};
pub use runner::{DebugControls, UiDecision};
pub use secrets::{SecretState, TestSecret};
pub use tools::DraftState;

use crate::attachment::AttachmentHost;
use crate::runtime::{PipelineHooks, Runtime};
use anyhow::{Context, Result};
use graph_core::pipeline::doc::{load_plan_doc, PlanDoc};
use graph_core::{EventSink, ExcludingRegistry, ToolRegistry};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

pub const WORKBENCH_SYSTEM_PROMPT: &str = include_str!("prompts/system.md").trim_ascii_end();

impl Workbench {
    pub async fn open(
        runtime: &Runtime,
        start: Start,
        root: impl Into<PathBuf>,
        tx: UnboundedSender<WorkbenchEvent>,
    ) -> Result<Self> {
        let doc = match &start {
            Start::Plan(Some(arg)) => Some(resolve_doc(runtime, arg)?),
            _ => None,
        };
        let opening = match &start {
            Start::Agent(Some(name)) => Some((ArtifactKind::Agent, name.clone())),
            Start::Tool(Some(name)) => Some((ArtifactKind::Tool, name.clone())),
            _ => None,
        };
        let store = runtime.store()?;

        let user = runtime.config.user.name.as_deref();
        let session = Some(uuid::Uuid::new_v4().simple().to_string());
        let exporter = crate::telemetry::exporter(
            crate::telemetry::RunInfo::conversation("workbench", session).user(user),
        );
        let run_sink: Arc<dyn EventSink> = crate::telemetry::tee(
            Arc::new(sink::ChannelSink::plan_run(tx.clone())),
            exporter.clone(),
        );
        runtime.usage.attach_events(run_sink.clone());
        let pipeline = runtime.pipeline(&store, run_sink).await?;
        let plans_dir = runtime.plans_dir();
        let root: PathBuf = root.into();
        let attachment = Arc::new(NetworkAttachment::new(NetworkSetup {
            pipeline: pipeline.clone(),
            tx: tx.clone(),
            plans_dir: plans_dir.clone(),
            root: root.clone(),
            doc: doc.clone(),
        })?);
        let mut agent_sinks: Vec<Arc<dyn EventSink>> =
            vec![Arc::new(sink::ChannelSink::agent(tx.clone()))];
        agent_sinks.extend(attachment.observer());
        let agent_sink: Arc<dyn EventSink> =
            crate::telemetry::tee(Arc::new(graph_core::TeeSink::new(agent_sinks)), exporter);
        let hooks = PipelineHooks {
            interlocutor: Some(Arc::new(runner::UiInterlocutor::new(tx.clone()))),
            ..Default::default()
        };
        let toolbox = runtime
            .toolbox_with(&store, agent_sink.clone(), hooks)
            .await?;
        let visible_catalog: Arc<dyn ToolRegistry> = Arc::new(ExcludingRegistry::new(
            toolbox.clone() as Arc<dyn ToolRegistry>,
            vec!["plan_and_execute".to_string()],
        ));
        let registry = attachment.tools(toolbox as Arc<dyn ToolRegistry>);
        let (agent_set, errors) = graph_core::agent::doc::AgentSet::load(
            &agents::builtin_sources(),
            &graph_core::agent::doc::agent_dirs(),
        );
        let agent_set = agent_set.with_drafted(runtime.drafted().agents());
        for error in errors {
            tracing::warn!("skipping agent file — {error}");
        }
        let mut conversation =
            runtime.conversation_over(Arc::new(agent_set), registry, pipeline.clone(), agent_sink);
        conversation.context = Some(crate::attachment::context_hook(
            conversation.agents.clone(),
            vec![attachment.clone() as Arc<dyn AttachmentHost>],
        ));
        if let Some(prompt) = crate::runtime::deprecated_prompt(
            &conversation.agents,
            agents::PLAN_EDITOR,
            "workbench",
            runtime.config.prompts.workbench.as_deref(),
            WORKBENCH_SYSTEM_PROMPT,
        ) {
            if let Some(doc) = conversation.agents.get(agents::PLAN_EDITOR) {
                if let Ok(rendered) = graph_core::agent::doc::render_system_prompt(
                    &doc,
                    &graph_core::agent::doc::global_fragments(),
                    &conversation.session,
                ) {
                    conversation.prompt_overrides.insert(
                        agents::PLAN_EDITOR.to_string(),
                        format!("{prompt}\n\n{rendered}"),
                    );
                }
            }
        }
        attachment.wire(&mut conversation);
        let (resumed, history) = match &start {
            Start::Resume(id) => {
                let meta = store
                    .get_thread(id)
                    .await?
                    .with_context(|| format!("no thread '{id}'"))?;
                let history = crate::runtime::load_history(store.as_ref(), id).await?;
                attachment.restore(&history);
                if let Some(recorded) = meta
                    .attachments
                    .iter()
                    .find(|recorded| recorded.kind == attachment.kind())
                {
                    attachment.recall(recorded);
                }
                (Some(meta), history)
            }
            _ => (None, Vec::new()),
        };
        let starting = resumed
            .as_ref()
            .map(|meta| meta.active.clone())
            .unwrap_or_else(|| agents::starting_agent(&start, doc.as_ref()).to_string());
        let workbench = Workbench {
            active: std::sync::Mutex::new(starting),
            thread: std::sync::Mutex::new(resumed.map(|meta| meta.id)),
            conversation,
            pipeline,
            history: Arc::new(tokio::sync::Mutex::new(history)),
            draft: attachment.draft.clone(),
            catalog: visible_catalog,
            store,
            plans_dir,
            debug: attachment.debug.clone(),
            tx,
            root,
            project: runtime.project().map(str::to_string),
            attachment,
        };
        if let Some((kind, name)) = opening {
            workbench
                .attachment
                .open_existing(kind, &name)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        Ok(workbench)
    }
}

pub(crate) fn resolve_doc(runtime: &Runtime, name_or_path: &str) -> Result<PlanDoc> {
    let path = std::path::Path::new(name_or_path);
    if path.exists() {
        return load_plan_doc(path).context("failed to load plan file");
    }
    let loaded = runtime.plan_docs();
    loaded
        .docs
        .iter()
        .find(|d| d.identifier == name_or_path)
        .cloned()
        .with_context(|| match loaded.skip_reason(name_or_path) {
            Some(reason) => format!("plan '{name_or_path}' failed to load — {reason}"),
            None => format!("'{name_or_path}' is neither a file nor a known plan identifier"),
        })
}
