use crate::cli::SecretCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::{Context, Result};
use graph_runtime::runtime::Workspace;
use graph_runtime::secrets::{self as ops, SecretDetail, SecretList, SecretStatus, Secrets};
use serde_json::json;
use std::io::{IsTerminal, Read};

pub fn run(command: SecretCommand) -> Result<()> {
    let secrets = Secrets::system();
    let workspace = Workspace::current();
    match command {
        SecretCommand::Set { name, json } => {
            let value = read_value(&name)?;
            report(set(&secrets, &name, &value)?, json)
        }
        SecretCommand::Rm { name, json } => report(rm(&secrets, &name)?, json),
        SecretCommand::List { json } => report(list(&workspace, &secrets)?, json),
        SecretCommand::Show { name, json } => report(show(&workspace, &secrets, &name)?, json),
    }
}

fn read_value(name: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password(format!("Value for {name} (input hidden): "))
            .context("reading the value from the terminal");
    }
    let mut value = String::new();
    std::io::stdin()
        .read_to_string(&mut value)
        .context("reading the value from stdin")?;
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    Ok(value)
}

pub fn set(secrets: &Secrets, name: &str, value: &str) -> Result<Outcome> {
    let saved = ops::set(secrets, name, value)?;
    let mut outcome = Outcome::ok(json!({
        "ok": true,
        "name": saved.name,
        "stored": true,
        "shadowedByEnv": saved.shadowed_by_env,
    }));
    if saved.shadowed_by_env {
        outcome = outcome.with_note(format!(
            "stored {name}, but the environment also sets it and its value wins"
        ));
    } else {
        outcome = outcome.with_note(format!("stored {name} in the OS credential store"));
    }
    Ok(outcome)
}

pub fn rm(secrets: &Secrets, name: &str) -> Result<Outcome> {
    let removed = ops::remove(secrets, name)?;
    if !removed.removed {
        return Ok(Outcome::rejected(json!({
            "error": format!("no stored secret named {name}"),
            "name": name,
            "removed": false,
        })));
    }
    Ok(
        Outcome::ok(json!({"ok": true, "name": name, "removed": true}))
            .with_note(format!("removed {name} from the OS credential store")),
    )
}

pub fn list(workspace: &Workspace, secrets: &Secrets) -> Result<Outcome> {
    let listed = ops::list(workspace, secrets);
    let body = serde_json::to_value(&listed)?;
    let text = render_list(&listed);
    let outcome = Outcome::raw(text, body);
    let mut notes = Vec::new();
    if listed.secrets.is_empty() {
        notes.push(
            "no ${NAME} references in config.toml, user tools or installed packs".to_string(),
        );
    }
    if let Some(reason) = &listed.store.reason {
        notes.push(format!("{reason}; names resolve from the environment only"));
    }
    notes.extend(listed.problems.iter().cloned());
    Ok(if notes.is_empty() {
        outcome
    } else {
        outcome.with_note(notes.join("\n"))
    })
}

fn render_list(listed: &SecretList) -> String {
    let width = listed
        .secrets
        .iter()
        .map(|entry| entry.name.len())
        .max()
        .unwrap_or(0);
    let status_width = SecretStatus::EnvShadowsStore.label().len();
    let mut text = String::new();
    for entry in &listed.secrets {
        let places: Vec<String> = entry
            .references
            .iter()
            .map(|reference| reference.describe())
            .collect();
        text.push_str(&format!(
            "{:width$}  {:status_width$}  {}\n",
            entry.name,
            entry.status.label(),
            places.join("; "),
        ));
    }
    text
}

pub fn show(workspace: &Workspace, secrets: &Secrets, name: &str) -> Result<Outcome> {
    let detail = ops::show(workspace, secrets, name)?;
    let body = serde_json::to_value(&detail)?;
    let outcome = Outcome::raw(render_detail(&detail), body);
    Ok(match &detail.store.reason {
        Some(reason) => outcome.with_note(format!(
            "{reason}; {name} resolves from the environment only"
        )),
        None => outcome,
    })
}

fn render_detail(detail: &SecretDetail) -> String {
    let mut text = format!("{}: {}\n", detail.name, detail.status.label());
    if let Some(value) = &detail.env_value {
        text.push_str(&format!("  env:   {value}\n"));
    }
    if let Some(value) = &detail.store_value {
        text.push_str(&format!("  store: {value}\n"));
    }
    if detail.references.is_empty() {
        text.push_str("  not referenced by config.toml, user tools or installed packs\n");
    } else {
        text.push_str("  referenced by:\n");
        for reference in &detail.references {
            text.push_str(&format!("    {}\n", reference.describe()));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_core::secrets::MemorySecrets;
    use std::sync::Arc;

    fn secrets() -> Secrets {
        Secrets::new(
            Arc::new(MemorySecrets::env()),
            Arc::new(MemorySecrets::store().with("STORED_KEY", "sk-0123456789")),
        )
    }

    #[test]
    fn removing_an_unknown_name_is_a_rejection_not_an_error() {
        let outcome = rm(&secrets(), "NEVER_STORED").unwrap();
        assert!(outcome.rejected);
        assert_eq!(outcome.body["error"], "no stored secret named NEVER_STORED");
        let outcome = rm(&secrets(), "STORED_KEY").unwrap();
        assert!(!outcome.rejected);
        assert_eq!(outcome.body["removed"], true);
    }

    #[test]
    fn show_renders_a_masked_value_and_never_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = show(&Workspace::new(dir.path()), &secrets(), "STORED_KEY").unwrap();
        let text = outcome.raw.unwrap();
        assert!(text.starts_with("STORED_KEY: store\n"), "{text}");
        assert!(text.contains("****6789"));
        assert!(!text.contains("sk-0123456789"));
        assert!(!outcome.body.to_string().contains("sk-0123456789"));
    }

    #[test]
    fn set_reports_when_the_environment_shadows_the_stored_value() {
        let secrets = Secrets::new(
            Arc::new(MemorySecrets::env().with("BOTH", "env-value")),
            Arc::new(MemorySecrets::store()),
        );
        let outcome = set(&secrets, "BOTH", "stored-value").unwrap();
        assert_eq!(outcome.body["shadowedByEnv"], true);
        assert!(outcome.body["note"].as_str().unwrap().contains("wins"));
    }
}
