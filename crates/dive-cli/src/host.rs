use anyhow::Result;
use dive_runtime::runtime::{self, Runtime, RuntimeOptions, Workspace};
use std::io::IsTerminal;
use std::sync::Arc;

static ACTING_AS: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

pub fn act_as(identity: Option<String>) {
    let _ = ACTING_AS.set(identity);
}

pub fn options() -> RuntimeOptions {
    RuntimeOptions {
        storage_backend: dive_config::legacy::var("DIVE_STORAGE"),
        capture_content: crate::telemetry::captures_content(),
        acting_as: ACTING_AS.get().cloned().flatten(),
        ..Default::default()
    }
}

pub fn load_config() -> Result<dive_config::LoadedConfig> {
    let loaded = runtime::load_config(&Workspace::current())?;
    if std::io::stderr().is_terminal() {
        for notice in runtime::config_notices(&loaded) {
            eprintln!("{notice}");
        }
    }
    Ok(loaded)
}

pub fn runtime() -> Result<Runtime> {
    let loaded = load_config()?;
    let packs = runtime::load_packs(&loaded)?;
    let workspace = Workspace::current();
    let runtime = Runtime::with_packs(loaded.config, packs, options())?
        .in_project(workspace.project())
        .signing_in_for(&workspace);
    if runtime.options().acting_as.is_some() {
        runtime.origin_as(None)?;
    }
    Ok(runtime)
}

pub fn open_store(config: &dive_config::Config) -> Result<Arc<dyn dive_core::Store>> {
    let identity = dive_runtime::identity::local(
        &config.identity,
        runtime::Host::Cli.as_str(),
        std::env::var("USER").ok(),
        dive_runtime::identity::hostname,
    );
    runtime::open_store(config, &options(), &identity)
}

pub fn open_files(config: &dive_config::Config) -> Result<Arc<dyn dive_core::files::BlobStore>> {
    runtime::open_files(config, &options())
}
