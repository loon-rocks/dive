pub mod agents;
pub mod artifact;
mod effects;
pub mod event;
pub mod fs_tools;
pub mod runner;
pub mod sink;
pub mod tools;

pub use agents::Start;
pub use artifact::{ArtifactKind, ArtifactView};
pub use effects::{save_draft, Workbench};
pub use event::{GateKind, WorkbenchEvent};
pub use runner::{DebugControls, UiDecision};
pub use tools::DraftState;

use crate::runtime::{PipelineHooks, Runtime};
use anyhow::{Context, Result};
use graph_core::pipeline::doc::{load_plan_doc, PlanDoc};
use graph_core::{CompositeRegistry, EventSink, ExcludingRegistry, ToolRegistry};
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

        let draft = Arc::new(std::sync::Mutex::new(DraftState::new(doc.clone())));

        let agent_sink: Arc<dyn EventSink> =
            crate::telemetry::tee(Arc::new(sink::ChannelSink::agent(tx.clone())), exporter);
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
        let plans_dir = runtime.plans_dir();
        let debug = Arc::new(DebugControls::default());
        let workbench_tools = Arc::new(tools::WorkbenchTools::new(
            draft.clone(),
            pipeline.clone(),
            plans_dir.clone(),
            debug.clone(),
            tx.clone(),
        ));
        let fs_tools = Arc::new(
            fs_tools::FsTools::new(root.into())
                .context("failed to resolve the workbench project directory")?,
        );
        let registry: Arc<dyn ToolRegistry> = Arc::new(CompositeRegistry::new(vec![
            Arc::new(tools::ComposedCapture::new(
                visible_catalog.clone(),
                draft.clone(),
            )),
            workbench_tools,
            fs_tools,
        ]));
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
        conversation.context = Some(agents::context_hook(draft.clone(), pipeline.clone()));
        conversation.handoff_guard = Some(artifact::handoff_guard(
            draft.clone(),
            pipeline.clone(),
            tx.clone(),
        ));
        conversation.progress_tools = tools::progress_tools();
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

        let workbench = Workbench {
            active: std::sync::Mutex::new(agents::starting_agent(&start, doc.as_ref()).to_string()),
            conversation,
            pipeline,
            history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            draft,
            catalog: visible_catalog,
            store,
            plans_dir,
            debug,
            tx,
        };
        if let Some((kind, name)) = opening {
            artifact::open_existing(
                &workbench.draft,
                &workbench.pipeline,
                &workbench.tx,
                kind,
                &name,
            )
            .await
            .map_err(anyhow::Error::msg)?;
        }
        Ok(workbench)
    }
}

fn resolve_doc(runtime: &Runtime, name_or_path: &str) -> Result<PlanDoc> {
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
