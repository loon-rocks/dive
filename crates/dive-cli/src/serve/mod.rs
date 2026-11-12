pub mod auth;
pub mod dev_key;
pub mod error;
pub mod limits;
pub mod openapi;
pub mod plans;
pub mod runs;
pub mod shutdown;
pub mod stream;
#[cfg(test)]
mod tests;

use anyhow::{bail, Context, Result};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use dive_core::Store;
use dive_runtime::ops::run::RunControl;
use dive_runtime::runtime::{Mode, Runtime, RuntimeOptions, Workspace};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinSet;

pub const DOCS_ENV: &str = "DIVE_SERVE_DOCS";
const DEV_DEPLOYMENT: &str = "serve";
const DEV_ENVIRONMENT: &str = "development";
const DELTA_BUFFER: usize = 256;
const REAP_BATCH: usize = 100;

pub struct ServeArgs {
    pub listen: Option<String>,
    pub dir: Option<PathBuf>,
    pub dev_key: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Delta {
    pub text: String,
}

pub struct AppState {
    pub runtime: Arc<Runtime>,
    pub store: Arc<dyn Store>,
    pub verifier: auth::Verifier,
    pub limits: Arc<limits::Limits>,
    pub heartbeat: Duration,
    pub poll: Duration,
    pub docs: bool,
    draining: AtomicBool,
    tasks: Mutex<JoinSet<()>>,
    live: Mutex<HashMap<String, Live>>,
}

struct Live {
    control: RunControl,
    deltas: broadcast::Sender<Delta>,
}

impl AppState {
    pub fn new(
        runtime: Arc<Runtime>,
        store: Arc<dyn Store>,
        verifier: auth::Verifier,
        serve: &dive_config::ServeConfig,
        docs: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            store,
            verifier,
            limits: limits::Limits::new(serve.max_runs, serve.max_runs_per_tenant),
            heartbeat: Duration::from_secs(serve.heartbeat_secs.max(1)),
            poll: dive_runtime::ops::run::FOLLOW_POLL,
            docs,
            draining: AtomicBool::new(false),
            tasks: Mutex::new(JoinSet::new()),
            live: Mutex::new(HashMap::new()),
        })
    }

    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    pub fn ready(&self) -> bool {
        !self.draining() && !self.limits.at_capacity()
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Live>> {
        self.live.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn tasks(&self) -> std::sync::MutexGuard<'_, JoinSet<()>> {
        self.tasks.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn track(&self, run: &str, control: RunControl, deltas: broadcast::Sender<Delta>) {
        let mut live = self.live();
        if self.draining() {
            control.interrupt();
        }
        live.insert(run.to_string(), Live { control, deltas });
    }

    pub fn forget(&self, run: &str) {
        self.live().remove(run);
    }

    pub fn cancel_here(&self, run: &str, by: &str) {
        if let Some(live) = self.live().get(run) {
            live.control.cancel(by);
        }
    }

    pub fn deltas(&self, run: &str) -> Option<broadcast::Receiver<Delta>> {
        self.live().get(run).map(|live| live.deltas.subscribe())
    }

    pub fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks();
        while tasks.try_join_next().is_some() {}
        tasks.spawn(task);
    }

    pub fn drain(&self) {
        let live = self.live();
        if self.draining.swap(true, Ordering::SeqCst) {
            return;
        }
        for run in live.values() {
            run.control.interrupt();
        }
    }

    pub fn force(&self) {
        for run in self.live().values() {
            run.control.force();
        }
    }

    pub async fn wait_for_runs(&self) {
        loop {
            let mut tasks = std::mem::take(&mut *self.tasks());
            if tasks.is_empty() {
                return;
            }
            while tasks.join_next().await.is_some() {}
        }
    }

    pub async fn reap(&self) {
        match self.store.reap_expired(REAP_BATCH).await {
            Ok(reaped) => {
                for done in reaped {
                    tracing::info!(run = %done.run, session = %done.session, status = done.status.as_str(), "reaped a run whose process stopped");
                }
            }
            Err(error) => tracing::warn!(error = %error, "couldn't reap expired runs"),
        }
    }
}

pub fn delta_channel() -> broadcast::Sender<Delta> {
    broadcast::channel(DELTA_BUFFER).0
}

pub struct DeltaSink {
    deltas: broadcast::Sender<Delta>,
}

impl DeltaSink {
    pub fn new(deltas: broadcast::Sender<Delta>) -> Self {
        Self { deltas }
    }

    fn send(&self, text: &str) {
        if !text.is_empty() {
            let _ = self.deltas.send(Delta {
                text: text.to_string(),
            });
        }
    }
}

impl dive_core::EventSink for DeltaSink {
    fn text_delta(&self, text: &str) {
        self.send(text);
    }

    fn solver_delta(&self, text: &str) {
        self.send(text);
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(readyz))
        .route("/v1/plans", get(plans::list))
        .route("/v1/plans/{plan}", get(plans::show))
        .route("/v1/plans/{plan}/runs", post(plans::start))
        .route("/v1/runs/{run}", get(runs::show))
        .route("/v1/runs/{run}/entries", get(stream::entries))
        .route("/v1/runs/{run}/cancel", post(runs::cancel));
    if state.docs {
        router = router
            .route("/openapi.json", get(openapi::deployment))
            .route("/docs", get(openapi::page));
    }
    router.with_state(state)
}

async fn readyz(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> StatusCode {
    if state.ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub fn workspace(dir: Option<&Path>) -> Result<Workspace> {
    let root = match dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().context("can't read the current directory")?,
    };
    if !root.is_dir() {
        bail!("{} isn't a directory", root.display());
    }
    Ok(Workspace::new(root))
}

fn verifier(
    serve: &dive_config::ServeConfig,
    dev_key: Option<&dev_key::DevKey>,
    project: &Path,
) -> Result<auth::Verifier> {
    if let Some(key) = dev_key {
        let issuer = serve.issuer.as_deref().unwrap_or(dev_key::DEV_ISSUER);
        let audience = serve.audience.as_deref().unwrap_or(dev_key::DEV_AUDIENCE);
        let mut verifier = auth::Verifier::new(issuer, audience)
            .trust_pem(key.public_pem().as_bytes(), "the development key")?;
        for path in &serve.public_keys {
            verifier = verifier.trust_file(&resolve(project, path))?;
        }
        return Ok(verifier);
    }
    if serve.public_keys.is_empty() {
        bail!("[serve].public_keys is empty; list the PEM keys that verify run tokens, or pass --dev-key");
    }
    let issuer = serve
        .issuer
        .as_deref()
        .context("[serve].issuer is required unless you pass --dev-key")?;
    let audience = serve
        .audience
        .as_deref()
        .context("[serve].audience is required unless you pass --dev-key")?;
    let mut verifier = auth::Verifier::new(issuer, audience);
    for path in &serve.public_keys {
        verifier = verifier.trust_file(&resolve(project, path))?;
    }
    Ok(verifier)
}

fn resolve(project: &Path, path: &str) -> PathBuf {
    let path = dive_config::expand_tilde(Path::new(path));
    if path.is_absolute() {
        path
    } else {
        project.join(path)
    }
}

pub async fn run(args: ServeArgs) -> Result<()> {
    if args.dev_key {
        dev_key::refuse_in_production(std::env::var("DIVE_ENV").ok().as_deref())?;
    }
    let workspace = workspace(args.dir.as_deref())?;
    let mut runtime = Runtime::open(
        &workspace,
        RuntimeOptions {
            storage_backend: dive_config::legacy::var("DIVE_STORAGE"),
            capture_content: false,
            mode: Mode::Serve,
            ..RuntimeOptions::default()
        },
    )?;
    let configured = runtime.config.serve.clone();
    if configured.is_none() && !args.dev_key {
        bail!(
            "{} has no [serve] section; add one, or pass --dev-key",
            workspace.root().join(".dive/config.toml").display()
        );
    }
    let serve = configured.unwrap_or_default();
    let key = if args.dev_key {
        dev_key::refuse_in_production(runtime.config.identity.environment.as_deref())?;
        let identity = &mut runtime.config.identity;
        identity
            .deployment
            .get_or_insert_with(|| DEV_DEPLOYMENT.to_string());
        identity
            .environment
            .get_or_insert_with(|| DEV_ENVIRONMENT.to_string());
        let data_dir = dive_config::expand_tilde(&runtime.config.settings.data_dir);
        Some(dev_key::DevKey::load_or_create(&data_dir)?)
    } else {
        None
    };
    dive_runtime::identity::serve(&runtime.config.identity, "-", "-")?;
    let verifier = verifier(&serve, key.as_ref(), workspace.root())?;
    let store = runtime.store()?;
    let runtime = Arc::new(runtime);
    let listen = args.listen.unwrap_or_else(|| serve.listen.clone());
    let docs = std::env::var(DOCS_ENV).is_ok_and(|value| !value.is_empty() && value != "0");
    let state = AppState::new(runtime.clone(), store, verifier, &serve, docs);
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("can't listen on {listen}"))?;
    eprintln!(
        "dive serve listening on http://{}",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or(listen)
    );
    if key.is_some() {
        eprintln!("trusting the development key; get a token with `dive serve token`");
    }
    shutdown::serve(
        listener,
        state,
        Duration::from_secs(serve.shutdown_grace_secs),
    )
    .await;
    runtime.shutdown().await;
    Ok(())
}
