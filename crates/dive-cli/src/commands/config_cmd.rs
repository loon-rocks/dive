//! `graph config` — show/init/path/check/migrate.

use crate::cli::ConfigCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::Result;
use dive_runtime::ops;
use dive_runtime::runtime::Workspace;
use serde_json::json;

pub async fn run(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show { json } => report(show()?, json),
        ConfigCommand::Path { json } => report(path()?, json),
        ConfigCommand::Init { global, force, .. } => report(init(global, force)?, false),
        ConfigCommand::Check { json } => report(check().await?, json),
        ConfigCommand::Migrate { global, json } => report(migrate(global)?, json),
    }
}

fn show() -> Result<Outcome> {
    let loaded = crate::host::load_config()?;
    let shown = ops::config::show(&loaded)?;
    let body = serde_json::to_value(&shown)?;
    if loaded.sources.is_empty() {
        eprintln!("# no config files found — showing defaults (run `graph config init`)");
    } else {
        for source in &loaded.sources {
            eprintln!("# merged from {}", source.display());
        }
    }
    Ok(Outcome::raw(shown.toml, body))
}

fn path() -> Result<Outcome> {
    let paths = ops::config::path(&Workspace::current());
    let text = paths
        .files
        .iter()
        .map(|file| format!("{}\t{}\n", file.path, file.marker()))
        .collect::<String>();
    Ok(Outcome::raw(text, serde_json::to_value(&paths)?))
}

async fn check() -> Result<Outcome> {
    let verdict = ops::config::check(&Workspace::current()).await;
    let body = serde_json::to_value(&verdict)?;
    Ok(if verdict.ok {
        Outcome::raw(verdict.text(), body)
    } else {
        Outcome::rejected(body)
    })
}

fn migrate(global: bool) -> Result<Outcome> {
    let migrated = ops::config::migrate(&Workspace::current(), global)?;
    let mut outcome = Outcome::ok(json!({
        "ok": true,
        "savedTo": migrated.path.display().to_string(),
        "from": migrated.from,
        "to": migrated.to,
        "changed": migrated.changed,
        "notes": migrated.notes,
    }));
    if !migrated.changed {
        outcome = outcome.with_note(format!("already at config version {}", migrated.to));
    } else if !migrated.notes.is_empty() {
        outcome = outcome.with_note(migrated.notes.join("; "));
    }
    Ok(outcome)
}

fn init(global: bool, force: bool) -> Result<Outcome> {
    let created = ops::config::init(&Workspace::current(), global, force)?;
    Ok(Outcome::ok(serde_json::to_value(&created)?))
}
