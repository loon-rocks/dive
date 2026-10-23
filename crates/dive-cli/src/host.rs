use anyhow::Result;
use dive_runtime::runtime::{self, Runtime, RuntimeOptions, Workspace};
use std::io::IsTerminal;
use std::sync::Arc;

pub fn options() -> RuntimeOptions {
    RuntimeOptions {
        storage_backend: dive_config::legacy::var("DIVE_STORAGE"),
        capture_content: crate::telemetry::captures_content(),
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
    Ok(Runtime::with_packs(loaded.config, packs, options())?
        .in_project(Workspace::current().project()))
}

pub fn open_store(config: &dive_config::Config) -> Result<Arc<dyn dive_core::Store>> {
    runtime::open_store(config, &options())
}

pub fn open_files(config: &dive_config::Config) -> Result<Arc<dyn dive_core::files::BlobStore>> {
    runtime::open_files(config, &options())
}
