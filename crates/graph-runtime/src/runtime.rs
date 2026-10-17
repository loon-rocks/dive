//! Shared wiring: config → providers → MCP registry → store → agent.

use anyhow::{Context, Result};
use graph_core::agent::conversation::{Conversation, ConversationTurn};
use graph_core::pipeline::{doc::LoadedPlans, ExecutionGate, Interlocutor, Pipeline, ToolCatalog};
use graph_core::toolbox::AgentToolbox;
use graph_core::usage::UsageLedger;
use graph_core::user_tools::UserToolRegistry;
use graph_core::{CompositeRegistry, EventSink, NewEntry, Store, ThreadMeta, ToolRegistry};
use graph_llm::ModelRouter;
use graph_mcp::McpManager;
use graph_store::{FileStore, MemoryStore, RecordingRegistry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The interactive hooks a host can hand a [`Pipeline`]. Both are
/// optional and independent: `graph plan run` on a terminal installs an
/// interlocutor and no gate, the workbench installs both, and CI installs
/// neither.
#[derive(Default, Clone)]
pub struct PipelineHooks {
    pub gate: Option<Arc<dyn ExecutionGate>>,
    pub interlocutor: Option<Arc<dyn Interlocutor>>,
    pub cancel: Option<crate::cancel::CancelHandle>,
}

pub struct Runtime {
    pub config: graph_config::Config,
    pub registry: Arc<McpManager>,
    router: Arc<ModelRouter>,
    /// Tally for every model call this runtime's router serves — plan steps,
    /// agent rounds, prompt tools, and the chat loop alike. Drain it with
    /// [`UsageLedger::take`] at the end of a run or turn.
    pub usage: Arc<UsageLedger>,
    /// One warning per skipped plan file per command, even though several
    /// components (pipeline, toolbox, commands) each load the catalog.
    plans_warned: std::sync::atomic::AtomicBool,
    drafted: Arc<graph_core::pipeline::Drafted>,
    pub packs: Arc<crate::packs::InstalledPacks>,
    options: RuntimeOptions,
}

impl Runtime {
    pub fn model_kind_of(&self, role: &str) -> Option<graph_config::ModelKind> {
        self.router.kind_of_role(role)
    }

    pub fn open(workspace: &Workspace, options: RuntimeOptions) -> Result<Self> {
        let loaded = load_config(workspace)?;
        let packs = load_packs(&loaded)?;
        Self::with_packs(loaded.config, packs, options)
    }

    /// Build a runtime over an already-resolved config.
    ///
    /// The MCP server uses this to serve a *deliberately narrowed* config —
    /// see `mcp_server::project`.
    pub fn with_packs(
        config: graph_config::Config,
        packs: crate::packs::InstalledPacks,
        options: RuntimeOptions,
    ) -> Result<Self> {
        let router = ModelRouter::from_config(&config)?;
        Self::with_router(config, packs, options, router)
    }

    pub fn with_router(
        config: graph_config::Config,
        packs: crate::packs::InstalledPacks,
        options: RuntimeOptions,
        router: ModelRouter,
    ) -> Result<Self> {
        let loaded = graph_config::LoadedConfig {
            config,
            sources: Vec::new(),
            layers: Vec::new(),
            pack_layers: Vec::new(),
        };
        // The meter has to be installed before anything resolves a provider —
        // resolution is when providers get wrapped, so a meter added later
        // would silently miss whatever was already handed out.
        let usage = Arc::new(
            UsageLedger::new(loaded.config.pricing.clone())
                .with_content_capture(options.capture_content),
        );
        let router = router.with_meter(usage.clone());
        let registry = Arc::new(McpManager::new(loaded.config.mcp.clone()));
        let drafted = Arc::new(
            graph_core::pipeline::Drafted::new(
                loaded
                    .config
                    .tools
                    .paths
                    .first()
                    .map(|p| graph_config::expand_tilde(p)),
                graph_core::agent::doc::agent_dirs().into_iter().next(),
            )
            .with_tool_dirs(
                loaded
                    .config
                    .tools
                    .paths
                    .iter()
                    .map(|p| graph_config::expand_tilde(p))
                    .collect(),
            )
            .with_secrets(Arc::new(graph_core::secrets::in_project(
                loaded.config.project.id.clone(),
            ))),
        );
        Ok(Self {
            config: loaded.config,
            registry,
            router: Arc::new(router),
            usage,
            plans_warned: std::sync::atomic::AtomicBool::new(false),
            drafted,
            packs: Arc::new(packs),
            options,
        })
    }

    /// Gracefully close MCP connections. Call before returning from a
    /// command; skipping it orphans stdio MCP servers (their async-Drop
    /// cleanup never runs once the tokio runtime starts shutting down).
    pub async fn shutdown(&self) {
        self.registry.shutdown().await;
    }

    /// Open the configured runtime-state store.
    pub fn store(&self) -> Result<Arc<dyn Store>> {
        open_store(&self.config, &self.options)
    }

    pub fn options(&self) -> &RuntimeOptions {
        &self.options
    }

    /// Base tool catalog (MCP servers + built-in tools + user-defined
    /// tools + installed packs), wrapped with shape recording.
    pub fn recording_registry(&self, store: &Arc<dyn Store>) -> Result<Arc<dyn ToolRegistry>> {
        let docs = self.tool_docs()?;
        let mut sources: Vec<Arc<dyn ToolRegistry>> = vec![
            Arc::new(graph_core::pipeline::DraftedTools::new(
                self.drafted.clone(),
                self.router.clone(),
            )) as Arc<dyn ToolRegistry>,
            self.registry.clone() as Arc<dyn ToolRegistry>,
            Arc::new(
                UserToolRegistry::builtins(docs.builtins, self.router.clone())
                    .with_secrets(self.secrets()),
            ),
            Arc::new(
                UserToolRegistry::new(docs.user, self.router.clone()).with_secrets(self.secrets()),
            ),
        ];
        for (source, tools) in docs.packs {
            sources.push(Arc::new(
                UserToolRegistry::pack(&source.name, tools, self.router.clone())
                    .with_secrets(self.secrets()),
            ));
        }
        let base: Arc<dyn ToolRegistry> = Arc::new(CompositeRegistry::new(sources));
        Ok(Arc::new(RecordingRegistry::new(base, store.clone())))
    }

    pub fn secrets(&self) -> Arc<dyn graph_core::secrets::SecretSource> {
        self.drafted.secrets.clone()
    }

    /// The directories user tools load from (`[tools].paths`, expanded).
    fn tool_dirs(&self) -> Vec<std::path::PathBuf> {
        self.config
            .tools
            .paths
            .iter()
            .map(|p| graph_config::expand_tilde(p))
            .collect()
    }

    pub fn tool_docs(&self) -> Result<graph_core::user_tools::ToolDocs> {
        let dirs = self.tool_dirs();
        let builtins = graph_core::user_tools::load_builtin_tools().map_err(anyhow::Error::msg)?;
        let overrides = graph_core::user_tools::load_user_tools(
            &graph_core::user_tools::builtin_override_dirs(&dirs),
        )
        .map_err(anyhow::Error::msg)?;
        let user = graph_core::user_tools::load_user_tools(&dirs).map_err(anyhow::Error::msg)?;
        Ok(graph_core::user_tools::ToolDocs {
            builtins: graph_core::user_tools::apply_tool_overrides(builtins, overrides)
                .map_err(anyhow::Error::msg)?,
            user,
            packs: self.packs.tool_docs(),
        })
    }

    /// The loadable-tool catalog for catalog-aware plan validation: what a
    /// plan step can actually resolve to at run time. Built from config
    /// alone — MCP servers are included by *name*, never connected to.
    pub fn tool_catalog(
        &self,
        plan_docs: &[graph_core::pipeline::doc::PlanDoc],
    ) -> Result<ToolCatalog> {
        let docs = self.tool_docs()?;
        let agent_set = self.agent_set();
        let mut input_schemas = std::collections::BTreeMap::new();
        let mut named = |prefix: &str, docs: Vec<graph_core::user_tools::UserToolDoc>| {
            docs.into_iter()
                .map(|doc| {
                    let name = format!("{prefix}{}", doc.name);
                    if let Some(schema) = doc.input_schema {
                        input_schemas.insert(name.clone(), schema);
                    }
                    name
                })
                .collect()
        };
        let builtin_tools = named(graph_core::user_tools::BUILTIN_TOOL_PREFIX, docs.builtins);
        let user_tools = named(graph_core::user_tools::USER_TOOL_PREFIX, docs.user);
        for (source, tools) in docs.packs {
            let prefix = graph_core::packs::catalog_name(
                &source.name,
                graph_core::packs::ItemKind::Tool,
                "",
            );
            named(&prefix, tools);
        }
        for doc in plan_docs {
            if let Some(schema) = &doc.input_schema {
                input_schemas.insert(doc.tool_name(), schema.clone());
            }
        }
        for doc in agent_set.iter() {
            input_schemas.insert(doc.tool_name(), doc.subagent_input_schema());
        }
        Ok(ToolCatalog {
            builtin_tools,
            user_tools,
            plans: plan_docs
                .iter()
                .filter(|doc| doc.pack.is_none())
                .map(|d| d.identifier.clone())
                .collect(),
            mcp_servers: self.config.mcp.keys().cloned().collect(),
            agents: agent_set
                .iter()
                .filter(|doc| doc.source.pack().is_none())
                .map(|doc| doc.name.clone())
                .collect(),
            input_schemas,
            packs: self.packs.pack_set(),
        })
    }

    pub fn agent_set(&self) -> graph_core::agent::doc::AgentSet {
        self.agent_set_for(None)
    }

    pub fn agent_set_for(&self, attachment: Option<&str>) -> graph_core::agent::doc::AgentSet {
        let mut sources = graph_core::agent::doc::BUILTINS.to_vec();
        sources.extend(
            attachment
                .map(crate::attachment::agent_sources)
                .unwrap_or_default(),
        );
        let (set, errors) =
            graph_core::agent::doc::AgentSet::load(&sources, &graph_core::agent::doc::agent_dirs());
        for error in errors {
            tracing::warn!("skipping agent file — {error}");
        }
        set.with_packs(self.packs.agents().cloned())
            .with_drafted(self.drafted.agents())
    }

    pub fn drafted(&self) -> Arc<graph_core::pipeline::Drafted> {
        self.drafted.clone()
    }

    /// Where new plan files are written: the first configured
    /// `[plans].paths` entry. None when nothing is configured, which makes
    /// "where would this go?" a caller-visible error rather than a guess.
    pub fn plans_dir(&self) -> Option<std::path::PathBuf> {
        self.config
            .plans
            .paths
            .first()
            .map(|p| graph_config::expand_tilde(p))
    }

    /// The file holding the plan with this identifier, anywhere in the
    /// configured plan directories — *including* plans the catalog refuses:
    /// ones hidden by unconfigured `requires_servers`, and ones that failed to
    /// load at all.
    ///
    /// Authoring has to be able to open a plan the runtime won't run, because
    /// that is precisely the plan someone needs to edit. Tries the
    /// conventional `<identifier>.yaml` name first, then reads the identifier
    /// out of every plan file in the directory.
    pub fn find_plan_file(&self, identifier: &str) -> Option<std::path::PathBuf> {
        let holds_it = |path: &std::path::Path| {
            graph_core::pipeline::authoring::on_disk_identifier(path).as_deref() == Some(identifier)
        };
        for dir in self
            .config
            .plans
            .paths
            .iter()
            .map(|p| graph_config::expand_tilde(p))
        {
            for extension in ["yaml", "yml"] {
                let candidate = dir.join(format!("{identifier}.{extension}"));
                if holds_it(&candidate) {
                    return Some(candidate);
                }
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            // Sorted so a duplicate identifier resolves the same way the
            // catalog loader resolves it: first file wins, deterministically.
            let mut candidates: Vec<std::path::PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    matches!(
                        path.extension().and_then(|e| e.to_str()),
                        Some("yaml" | "yml")
                    )
                })
                .collect();
            candidates.sort();
            if let Some(found) = candidates.into_iter().find(|path| holds_it(path)) {
                return Some(found);
            }
        }
        None
    }

    /// The plan catalog, kept to documents whose `requires_servers` are all
    /// configured. Files that fail to load stay in `skipped` and are warned
    /// about here — a broken plan never takes the command down.
    pub fn plan_docs(&self) -> LoadedPlans {
        let dirs: Vec<std::path::PathBuf> = self
            .config
            .plans
            .paths
            .iter()
            .map(|p| graph_config::expand_tilde(p))
            .collect();
        let mut loaded = graph_core::pipeline::doc::load_plan_docs(&dirs);
        graph_core::pipeline::doc::add_builtin_plans(
            &mut loaded,
            graph_core::pipeline::doc::BUILTIN_PLANS,
        );
        for doc in self.packs.plans() {
            if loaded.docs.iter().any(|d| d.identifier == doc.identifier) {
                loaded
                    .skipped
                    .push(graph_core::pipeline::doc::DocError::Duplicate {
                        path: doc.source_label(),
                        identifier: doc.identifier.clone(),
                    });
            } else {
                loaded.docs.push(doc.clone());
            }
        }
        if !self
            .plans_warned
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            for error in &loaded.skipped {
                tracing::warn!("skipping plan file — {error}");
            }
        }
        let (visible, hidden): (Vec<_>, Vec<_>) = loaded.docs.drain(..).partition(|doc| {
            doc.requires_servers
                .iter()
                .all(|server| self.config.mcp.contains_key(server))
        });
        loaded.docs = visible;
        for doc in hidden {
            tracing::info!(
                plan = doc.identifier,
                "hidden: required MCP server not configured"
            );
            loaded.hidden.push(graph_core::pipeline::doc::HiddenPlan {
                missing_servers: doc
                    .requires_servers
                    .iter()
                    .filter(|server| !self.config.mcp.contains_key(*server))
                    .cloned()
                    .collect(),
                identifier: doc.identifier,
            });
        }
        loaded
    }

    /// The plan pipeline over the base registry (shape-recording MCP +
    /// user tools).
    pub async fn pipeline(
        &self,
        store: &Arc<dyn Store>,
        events: Arc<dyn EventSink>,
    ) -> Result<Arc<Pipeline>> {
        self.pipeline_with(store, events, PipelineHooks::default())
            .await
    }

    /// A pipeline carrying whichever interactive hooks the caller can
    /// serve: an [`ExecutionGate`](graph_core::pipeline::ExecutionGate)
    /// intercepting tool calls, an
    /// [`Interlocutor`](graph_core::pipeline::Interlocutor) answering
    /// `ask` steps, or neither (the CI shape).
    pub async fn pipeline_with(
        &self,
        store: &Arc<dyn Store>,
        events: Arc<dyn EventSink>,
        hooks: PipelineHooks,
    ) -> Result<Arc<Pipeline>> {
        let PipelineHooks {
            gate,
            interlocutor,
            cancel,
        } = hooks;
        let mut base = self.recording_registry(store)?;
        let mut gate = gate;
        if let Some(cancel) = &cancel {
            base = cancel.track(base);
            gate = Some(cancel.gate(gate));
        }
        let user_context = user_context_text(&self.config.user);
        let plans = self.plan_docs().docs;
        let catalog = self.tool_catalog(&plans)?;
        Ok(Arc::new(Pipeline {
            router: self.router.clone(),
            registry: base,
            events,
            plans: Arc::new(plans),
            call_stack: Vec::new(),
            store: Some(store.clone()),
            gate,
            interlocutor,
            catalog: Some(Arc::new(catalog)),
            user_context,
            current_date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            max_attempts: self.config.settings.planning_attempts.max(1),
            max_agent_iterations: self.config.settings.max_agent_iterations,
            usage: self.usage.clone(),
            agents: Arc::new(self.agent_set()),
            agent_depth: 0,
            always_loaded: Arc::new(self.config.tools.always_loaded.clone()),
            drafted: self.drafted.clone(),
            packs: Arc::new(self.packs.pack_set()),
            origin: Default::default(),
        }))
    }

    pub async fn conversation(
        &self,
        store: &Arc<dyn Store>,
        events: Arc<dyn EventSink>,
        hooks: PipelineHooks,
    ) -> Result<Conversation> {
        let cancel = hooks.cancel.clone();
        let pipeline = self.pipeline_with(store, events.clone(), hooks).await?;
        let catalog: Arc<dyn ToolRegistry> =
            self.toolbox_over(store, &pipeline, cancel.as_ref())?;
        Ok(self.conversation_over(pipeline.agents.clone(), catalog, pipeline, events))
    }

    pub fn conversation_over(
        &self,
        agents: Arc<graph_core::agent::doc::AgentSet>,
        catalog: Arc<dyn ToolRegistry>,
        pipeline: Arc<Pipeline>,
        events: Arc<dyn EventSink>,
    ) -> Conversation {
        let now = chrono::Local::now()
            .format("%A, %B %e %Y, %H:%M %Z")
            .to_string();
        let mut prompt_overrides = std::collections::BTreeMap::new();
        if let Some(prompt) = deprecated_prompt(
            &agents,
            graph_core::agent::doc::CHAT_AGENT,
            "chat",
            self.config.prompts.chat.as_deref(),
            graph_core::prompts::DEFAULT_CHAT_PROMPT,
        ) {
            prompt_overrides.insert(
                graph_core::agent::doc::CHAT_AGENT.to_string(),
                graph_core::prompts::chat_system_prompt(&self.config.user, &now, Some(prompt)),
            );
        }
        Conversation {
            agents,
            catalog,
            pipeline,
            events,
            session: std::collections::BTreeMap::from([
                ("date", now),
                ("user", session_user(&self.config.user)),
            ]),
            prompt_overrides,
            context: None,
            default_max_iterations: self.config.settings.max_agent_iterations,
            progress_tools: Vec::new(),
            stop_tools: Vec::new(),
            handoff_guard: None,
        }
    }

    /// The agent's full tool catalog: MCP + user tools + plan tools +
    /// plan_and_execute.
    pub async fn toolbox(
        &self,
        store: &Arc<dyn Store>,
        events: Arc<dyn EventSink>,
    ) -> Result<Arc<AgentToolbox>> {
        self.toolbox_with(store, events, PipelineHooks::default())
            .await
    }

    pub async fn toolbox_with(
        &self,
        store: &Arc<dyn Store>,
        events: Arc<dyn EventSink>,
        hooks: PipelineHooks,
    ) -> Result<Arc<AgentToolbox>> {
        let cancel = hooks.cancel.clone();
        let pipeline = self.pipeline_with(store, events, hooks).await?;
        self.toolbox_over(store, &pipeline, cancel.as_ref())
    }

    pub(crate) fn toolbox_over(
        &self,
        store: &Arc<dyn Store>,
        pipeline: &Arc<Pipeline>,
        cancel: Option<&crate::cancel::CancelHandle>,
    ) -> Result<Arc<AgentToolbox>> {
        let mut base = self.recording_registry(store)?;
        if let Some(cancel) = cancel {
            base = cancel.track(base);
        }
        let plans = pipeline.plans.as_ref().clone();
        Ok(Arc::new(AgentToolbox::new(base, pipeline.clone(), plans)))
    }
}

fn user_context_text(user: &graph_config::UserConfig) -> String {
    let mut out = String::new();
    if let Some(name) = &user.name {
        out.push_str(&format!("Name: {name}\n"));
    }
    if let Some(context) = &user.context {
        out.push_str(context);
    }
    if out.is_empty() {
        out.push_str("(none provided)");
    }
    out
}

/// Open the configured runtime-state store. Backend selection:
/// `RuntimeOptions::storage_backend` (`file` | `memory`) wins over
/// `[storage].backend`; the default is plain files under `data_dir`.
pub fn open_store(
    config: &graph_config::Config,
    options: &RuntimeOptions,
) -> Result<Arc<dyn Store>> {
    let backend = match options.storage_backend.as_deref() {
        Some("memory") => graph_config::StorageBackend::Memory,
        Some("file") => graph_config::StorageBackend::File,
        Some(other) => anyhow::bail!("GRAPH_STORAGE must be 'file' or 'memory', got '{other}'"),
        None => config.storage.backend,
    };
    match backend {
        graph_config::StorageBackend::Memory => Ok(Arc::new(MemoryStore::new())),
        graph_config::StorageBackend::File => {
            let root = graph_config::expand_tilde(&config.settings.data_dir);
            Ok(Arc::new(
                FileStore::open(&root).map_err(anyhow::Error::msg)?,
            ))
        }
    }
}

/// Resolve which existing thread to continue, if any.
///
/// `None` → new thread; `Some(None)` (bare `--thread`) → most recent thread,
/// or a new one when none exist yet; `Some(Some(id))` → that thread or error.
pub async fn resolve_thread(
    store: &dyn Store,
    thread: Option<Option<String>>,
) -> Result<Option<ThreadMeta>> {
    match thread {
        None => Ok(None),
        Some(Some(id)) => {
            let meta = store
                .get_thread(&id)
                .await?
                .with_context(|| format!("no thread with id {id} (see `graph thread list`)"))?;
            Ok(Some(meta))
        }
        Some(None) => Ok(store.latest_thread().await?),
    }
}

pub fn deprecated_prompt<'a>(
    agents: &graph_core::agent::doc::AgentSet,
    agent: &str,
    field: &str,
    value: Option<&'a str>,
    shipped_default: &str,
) -> Option<&'a str> {
    let value = value.filter(|value| value.trim() != shipped_default.trim())?;
    let builtin = agents
        .get(agent)
        .is_some_and(|doc| doc.source == graph_core::agent::doc::AgentSource::Builtin);
    if !builtin {
        return None;
    }
    tracing::warn!(
        "[prompts].{field} is deprecated: run `graph agent show {agent}`, save it as \
         ./.graph/agents/{agent}.yaml, and edit its system_prompt instead"
    );
    Some(value)
}

pub async fn load_history(store: &dyn Store, thread_id: &str) -> Result<Vec<NewEntry>> {
    Ok(store
        .load_entries(thread_id)
        .await?
        .into_iter()
        .map(NewEntry::from)
        .collect())
}

pub async fn persist_turn(
    store: &dyn Store,
    meta: &ThreadMeta,
    active: &str,
    turn: &ConversationTurn,
) -> Result<()> {
    store.append_entries(&meta.id, &turn.entries).await?;
    if turn.active != active {
        store.set_active_agent(&meta.id, &turn.active).await?;
    }
    Ok(())
}

pub fn starting_agent(
    thread: Option<&ThreadMeta>,
    requested: Option<&str>,
    known: &dyn Fn(&str) -> bool,
) -> Result<String> {
    match (thread, requested) {
        (Some(meta), Some(agent)) if agent != meta.owner => anyhow::bail!(
            "thread {} belongs to the {} agent — continue it without naming an agent \
             (it resumes with {}), or start a new thread for {agent}",
            meta.id,
            meta.owner,
            meta.active
        ),
        (Some(_), Some(owner)) => Ok(owner.to_string()),
        (Some(meta), None) if !known(&meta.active) && known(&meta.owner) => {
            tracing::warn!(
                "thread {} was last with the {} agent, which isn't available here; resuming with its owner, {}",
                meta.id,
                meta.active,
                meta.owner
            );
            Ok(meta.owner.clone())
        }
        (Some(meta), None) => Ok(meta.active.clone()),
        (None, Some(agent)) => Ok(agent.to_string()),
        (None, None) => Ok(graph_core::agent::doc::CHAT_AGENT.to_string()),
    }
}

fn session_user(user: &graph_config::UserConfig) -> String {
    let mut out = String::new();
    if let Some(name) = &user.name {
        out.push_str(&format!("The user's name is {name}.\n"));
    }
    if let Some(context) = &user.context {
        out.push_str(&format!(
            "\nAbout the user and their environment:\n{context}\n"
        ));
    }
    out
}

/// Derive a thread title from the first user message.
pub fn title_from(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or_default().trim();
    let mut title: String = first_line.chars().take(60).collect();
    if first_line.chars().count() > 60 {
        title.push('…');
    }
    if title.is_empty() {
        title = "untitled".to_string();
    }
    title
}

pub fn load_packs(loaded: &graph_config::LoadedConfig) -> Result<crate::packs::InstalledPacks> {
    let packs = crate::packs::InstalledPacks::for_config(loaded)?;
    packs.warn();
    Ok(packs)
}

pub fn load_config(workspace: &Workspace) -> Result<graph_config::LoadedConfig> {
    graph_config::load_from(&workspace.config_paths(), &graph_core::secrets::system())
}

pub fn config_notices(loaded: &graph_config::LoadedConfig) -> Vec<String> {
    let global = graph_config::expand_tilde(&graph_config::global_config_path());
    let mut notices = Vec::new();
    for layer in &loaded.layers {
        if layer.version < graph_config::CONFIG_FORMAT {
            let flag = if layer.path == global {
                " --global"
            } else {
                ""
            };
            notices.push(format!(
                "{} is config version {}; run `graph config migrate{flag}` to bring it to {}",
                layer.path.display(),
                layer.version,
                graph_config::CONFIG_FORMAT
            ));
        }
        for note in &layer.notes {
            notices.push(format!("{}: {note}", layer.path.display()));
        }
    }
    notices
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn current() -> Self {
        Self::new(".")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_global(&self) -> bool {
        is_global_dir(&self.root)
    }

    pub fn project_dir(&self) -> PathBuf {
        if self.is_global() {
            global_config_dir()
        } else {
            self.root.join(".graph")
        }
    }

    pub fn config_paths(&self) -> Vec<PathBuf> {
        if self.is_global() {
            vec![graph_config::global_config_path()]
        } else {
            vec![
                graph_config::global_config_path(),
                self.root.join(".graph").join("config.toml"),
            ]
        }
    }
}

pub fn global_config_dir() -> PathBuf {
    graph_config::expand_tilde(&graph_config::global_config_path())
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

pub fn is_global_dir(dir: &Path) -> bool {
    let global = global_config_dir();
    match (dir.canonicalize(), global.canonicalize()) {
        (Ok(dir), Ok(global)) => dir == global,
        _ => dir == global,
    }
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeOptions {
    pub storage_backend: Option<String>,
    pub capture_content: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_workspace_loads_the_same_layers_as_the_cli() {
        assert_eq!(
            Workspace::current().config_paths(),
            vec![
                graph_config::global_config_path(),
                graph_config::project_config_path()
            ]
        );
    }

    #[test]
    fn a_workspace_reads_the_project_layer_under_its_root() {
        let workspace = Workspace::new("/work/site");
        assert_eq!(workspace.root(), Path::new("/work/site"));
        assert_eq!(
            workspace.config_paths()[1],
            PathBuf::from("/work/site/.graph/config.toml")
        );
    }

    #[test]
    fn the_global_config_dir_as_a_workspace_has_no_project_layer() {
        let workspace = Workspace::new(global_config_dir());
        assert!(workspace.is_global());
        assert_eq!(
            workspace.config_paths(),
            vec![graph_config::global_config_path()]
        );
        assert_eq!(workspace.project_dir(), global_config_dir());
    }

    #[test]
    fn config_notices_name_old_layers_and_migration_notes() {
        let global = graph_config::expand_tilde(&graph_config::global_config_path());
        let project = PathBuf::from("/work/site/.graph/config.toml");
        let layer = |path: &PathBuf, version: u32, notes: Vec<String>| graph_config::LayerInfo {
            path: path.clone(),
            declared: Some(version),
            version,
            notes,
        };
        let loaded = graph_config::LoadedConfig {
            config: graph_config::Config::default(),
            sources: Vec::new(),
            layers: vec![
                layer(&global, 0, Vec::new()),
                layer(
                    &project,
                    graph_config::CONFIG_FORMAT,
                    vec!["renamed x".to_string()],
                ),
            ],
            pack_layers: Vec::new(),
        };
        let notices = config_notices(&loaded);
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(
            notices[0].contains("graph config migrate --global"),
            "{}",
            notices[0]
        );
        assert_eq!(notices[1], "/work/site/.graph/config.toml: renamed x");
    }

    fn thread(owner: &str, active: &str) -> ThreadMeta {
        ThreadMeta {
            id: "t1".to_string(),
            title: "t".to_string(),
            created_at: 0,
            updated_at: 0,
            message_count: 0,
            owner: owner.to_string(),
            active: active.to_string(),
            attachments: Vec::new(),
            project: None,
        }
    }

    #[test]
    fn a_thread_resumes_with_its_active_agent_and_refuses_other_owners() {
        let meta = thread("router", "poet");
        let all = |_: &str| true;
        assert_eq!(starting_agent(Some(&meta), None, &all).unwrap(), "poet");
        assert_eq!(
            starting_agent(Some(&meta), Some("router"), &all).unwrap(),
            "router",
            "naming the owner resumes with the owner"
        );
        let error = starting_agent(Some(&meta), Some("poet"), &all)
            .unwrap_err()
            .to_string();
        assert!(error.contains("belongs to the router agent"), "{error}");
        assert_eq!(starting_agent(None, Some("poet"), &all).unwrap(), "poet");
        assert_eq!(starting_agent(None, None, &all).unwrap(), "chat");
        let no_poet = |name: &str| name != "poet";
        assert_eq!(
            starting_agent(Some(&meta), None, &no_poet).unwrap(),
            "router",
            "an active agent that isn't available falls back to the owner"
        );
    }
}
