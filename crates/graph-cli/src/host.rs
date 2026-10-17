use anyhow::Result;
use graph_runtime::runtime::{self, Runtime, RuntimeOptions, Workspace};
use std::io::IsTerminal;
use std::sync::Arc;

pub fn options() -> RuntimeOptions {
    RuntimeOptions {
        storage_backend: std::env::var("GRAPH_STORAGE").ok(),
        capture_content: crate::telemetry::captures_content(),
    }
}

pub fn load_config() -> Result<graph_config::LoadedConfig> {
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
    Ok(Runtime::with_packs(loaded.config, packs, options())?
        .in_project(Workspace::current().project()))
}

pub fn open_store(config: &graph_config::Config) -> Result<Arc<dyn graph_core::Store>> {
    runtime::open_store(config, &options())
}
