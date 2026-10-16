use crate::cli::SecretCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::{Context, Result};
use graph_runtime::runtime::Workspace;
use graph_runtime::secrets::{self as ops, Layer, SecretDetail, SecretList, SecretScope, Secrets};
use serde_json::json;
use std::io::{IsTerminal, Read};

pub fn run(command: SecretCommand) -> Result<()> {
    let secrets = Secrets::system();
    let workspace = Workspace::current();
    match command {
        SecretCommand::Set {
            name,
            project,
            json,
        } => {
            let value = read_value(&name)?;
            report(
                set(&secrets, &workspace, &name, &value, scope(project))?,
                json,
            )
        }
        SecretCommand::Rm {
            name,
            project,
            json,
        } => report(rm(&secrets, &workspace, &name, scope(project))?, json),
        SecretCommand::List { json } => report(list(&workspace, &secrets)?, json),
        SecretCommand::Show { name, json } => report(show(&workspace, &secrets, &name)?, json),
    }
}

fn scope(project: bool) -> SecretScope {
    if project {
        SecretScope::Project
    } else {
        SecretScope::Shared
    }
}

fn where_stored(scope: SecretScope) -> &'static str {
    match scope {
        SecretScope::Project => "for this project",
        SecretScope::Shared => "for every project",
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

pub fn set(
    secrets: &Secrets,
    workspace: &Workspace,
    name: &str,
    value: &str,
    scope: SecretScope,
) -> Result<Outcome> {
    let saved = ops::set(secrets, workspace, name, value, scope)?;
    let note = match saved.shadowed_by {
        Some(layer) => format!(
            "stored {name} {}, but the {} value wins",
            where_stored(scope),
            describe_layer(layer)
        ),
        None => format!(
            "stored {name} {} in the OS credential store",
            where_stored(scope)
        ),
    };
    Ok(Outcome::ok(json!({
        "ok": true,
        "name": saved.name,
        "stored": true,
        "scope": saved.scope,
        "project": saved.project,
        "shadowedBy": saved.shadowed_by,
    }))
    .with_note(note))
}

fn describe_layer(layer: Layer) -> &'static str {
    match layer {
        Layer::Env => "environment's",
        Layer::Project => "project's",
        Layer::Shared => "shared",
    }
}

pub fn rm(
    secrets: &Secrets,
    workspace: &Workspace,
    name: &str,
    scope: SecretScope,
) -> Result<Outcome> {
    let removed = ops::remove(secrets, workspace, name, scope)?;
    if !removed.removed {
        let error = match scope {
            SecretScope::Project => format!("no secret named {name} stored for this project"),
            SecretScope::Shared => format!("no stored secret named {name}"),
        };
        return Ok(Outcome::rejected(json!({
            "error": error,
            "name": name,
            "scope": scope,
            "removed": false,
        })));
    }
    Ok(
        Outcome::ok(json!({"ok": true, "name": name, "scope": scope, "removed": true})).with_note(
            format!(
                "removed {name} {} from the OS credential store",
                where_stored(scope)
            ),
        ),
    )
}

fn source_label(in_use: Option<Layer>, set: impl Fn(Layer) -> bool) -> String {
    let Some(layer) = in_use else {
        return "missing".to_string();
    };
    let shadowed: Vec<&str> = Layer::ALL
        .into_iter()
        .filter(|other| *other > layer && set(*other))
        .map(Layer::label)
        .collect();
    if shadowed.is_empty() {
        layer.label().to_string()
    } else {
        format!("{} (shadows {})", layer.label(), shadowed.join(", "))
    }
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
    let sources: Vec<String> = listed
        .secrets
        .iter()
        .map(|entry| {
            source_label(entry.in_use, |layer| match layer {
                Layer::Env => entry.env_value.is_some(),
                Layer::Project => entry.project_value.is_some(),
                Layer::Shared => entry.shared_value.is_some(),
            })
        })
        .collect();
    let status_width = sources.iter().map(String::len).max().unwrap_or(0);
    let mut text = String::new();
    for (entry, source) in listed.secrets.iter().zip(&sources) {
        let places: Vec<String> = entry
            .references
            .iter()
            .map(|reference| reference.describe())
            .collect();
        text.push_str(&format!(
            "{:width$}  {:status_width$}  {}\n",
            entry.name,
            source,
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
    let source = source_label(detail.in_use, |layer| detail.value(layer).is_some());
    let mut text = format!("{}: {source}\n", detail.name);
    for layer in Layer::ALL {
        let marker = if detail.in_use == Some(layer) {
            "*"
        } else {
            " "
        };
        let value = match (layer, detail.value(layer)) {
            (_, Some(value)) => value.clone(),
            (Layer::Project, None) if detail.project.is_none() => {
                "not set (no [project] id yet)".to_string()
            }
            (_, None) => "not set".to_string(),
        };
        text.push_str(&format!("{marker} {:8} {value}\n", layer.label()));
    }
    if let Some(error) = &detail.read_error {
        text.push_str(&format!("  couldn't read the credential store: {error}\n"));
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
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path());
        let outcome = rm(&secrets(), &workspace, "NEVER_STORED", SecretScope::Shared).unwrap();
        assert!(outcome.rejected);
        assert_eq!(outcome.body["error"], "no stored secret named NEVER_STORED");
        let outcome = rm(&secrets(), &workspace, "STORED_KEY", SecretScope::Shared).unwrap();
        assert!(!outcome.rejected);
        assert_eq!(outcome.body["removed"], true);
    }

    #[test]
    fn show_renders_a_masked_value_and_never_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = show(&Workspace::new(dir.path()), &secrets(), "STORED_KEY").unwrap();
        let text = outcome.raw.unwrap();
        assert!(text.starts_with("STORED_KEY: shared\n"), "{text}");
        assert!(text.contains("* shared   ****6789"), "{text}");
        assert!(text.contains("  env      not set"), "{text}");
        assert!(!text.contains("sk-0123456789"));
        assert!(!outcome.body.to_string().contains("sk-0123456789"));
    }

    #[test]
    fn set_reports_when_the_environment_shadows_the_stored_value() {
        let secrets = Secrets::new(
            Arc::new(MemorySecrets::env().with("BOTH", "env-value")),
            Arc::new(MemorySecrets::store()),
        );
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path());
        let outcome = set(
            &secrets,
            &workspace,
            "BOTH",
            "stored-value",
            SecretScope::Shared,
        )
        .unwrap();
        assert_eq!(outcome.body["shadowedBy"], "env");
        assert!(outcome.body["note"].as_str().unwrap().contains("wins"));
    }

    #[test]
    fn show_lists_every_layer_and_marks_the_project_value_in_use() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".graph")).unwrap();
        std::fs::write(
            dir.path().join(".graph/config.toml"),
            "version = 5\n[project]\nid = \"p1\"\n",
        )
        .unwrap();
        let secrets = Secrets::new(
            Arc::new(MemorySecrets::env()),
            Arc::new(
                MemorySecrets::store()
                    .with("SLACK_BOT_TOKEN", "xoxb-shared-1111")
                    .with_project("SLACK_BOT_TOKEN", "p1", "xoxb-project-2222"),
            ),
        );
        let outcome = show(&Workspace::new(dir.path()), &secrets, "SLACK_BOT_TOKEN").unwrap();
        let text = outcome.raw.unwrap();
        assert!(
            text.starts_with("SLACK_BOT_TOKEN: project (shadows shared)\n"),
            "{text}"
        );
        assert!(text.contains("  env      not set\n"), "{text}");
        assert!(text.contains("* project  ****2222\n"), "{text}");
        assert!(text.contains("  shared   ****1111\n"), "{text}");
        assert_eq!(outcome.body["inUse"], "project");
    }
}
