use crate::cli::{LayerArgs, PackCommand};
use crate::commands::outcome::{report, Outcome};
use anyhow::Result;
use graph_runtime::ops::pack::{self as ops, LayerChoice, PackError, PackUpdates};
use graph_runtime::packs::layer::Layer;
use graph_runtime::runtime::Workspace;
use serde_json::json;

pub use graph_runtime::ops::pack::AddRequest;

mod author;
mod major;

pub async fn run(command: PackCommand) -> Result<()> {
    match command {
        PackCommand::Add {
            source,
            as_name,
            reference,
            path,
            layer,
            json,
        } => {
            let request = AddRequest {
                source,
                as_name,
                reference,
                path,
            };
            report(add(&target(&layer)?, &request)?, json)
        }
        PackCommand::List {
            layer,
            offline,
            json,
        } => report(
            list(
                &ops::listed_layers(&Workspace::current(), &choice(&layer))?,
                offline,
            )?,
            json,
        ),
        PackCommand::Update {
            names,
            force,
            major,
            yes,
            layer,
            json,
        } => {
            let layer = target(&layer)?;
            if !major {
                return report(update(&layer, &names, force)?, json);
            }
            let consent = major::Consent {
                yes,
                asker: crate::interlocutor::tty(),
            };
            let config =
                graph_config::load_from(&layer.config_paths, &graph_core::secrets::system())?
                    .config;
            let majors = major::Majors {
                consent: &consent,
                config: &config,
                events: crate::output::make_sink(
                    json,
                    false,
                    crate::telemetry::RunInfo::plan_run("pack update --major"),
                ),
            };
            report(major::update(&layer, &names, force, &majors).await?, json)
        }
        PackCommand::Remove { name, layer, json } => report(remove(&target(&layer)?, &name)?, json),
        PackCommand::New { name, path, json } => report(author::new_pack(&name, &path)?, json),
        PackCommand::Check { path, json } => report(author::check(&path).await?, json),
        PackCommand::Run {
            item,
            input,
            inputs,
            path,
            json,
        } => {
            let input = crate::commands::input::resolve_input(input.as_deref(), &inputs)?;
            report(author::run_item(&path, &item, input, json).await?, json)
        }
        PackCommand::Test { path, json } => report(author::test(&path, json).await?, json),
        PackCommand::Release { bump, path, json } => {
            report(author::release(&path, bump, json).await?, json)
        }
    }
}

fn choice(args: &LayerArgs) -> LayerChoice {
    LayerChoice {
        global: args.global,
        dir: args.dir.clone(),
    }
}

fn target(args: &LayerArgs) -> Result<Layer> {
    ops::target(&Workspace::current(), &choice(args))
}

#[cfg(test)]
fn project_layer(dir: std::path::PathBuf) -> Result<Layer> {
    ops::project_layer(dir)
}

fn refused(error: impl Into<String>, problems: Vec<String>) -> Outcome {
    Outcome::rejected(json!({
        "ok": false,
        "error": error.into(),
        "problems": problems,
    }))
}

fn refusal(error: impl Into<String>) -> Outcome {
    refused(error, Vec::new())
}

fn rejection(error: PackError) -> Result<Outcome> {
    match error {
        PackError::Failed(error) => Err(error),
        refusal => Ok(refused(refusal.to_string(), refusal.problems().to_vec())),
    }
}

pub fn add(layer: &Layer, request: &AddRequest) -> Result<Outcome> {
    match ops::add(layer, request) {
        Ok(installed) => {
            let note = installed.lines.join("\n  ");
            Ok(Outcome::ok(serde_json::to_value(&installed)?).with_note(note))
        }
        Err(error) => rejection(error),
    }
}

fn updates_outcome(updates: PackUpdates) -> Result<Outcome> {
    let body = serde_json::to_value(&updates)?;
    if !updates.ok {
        return Ok(Outcome::rejected(body));
    }
    Ok(Outcome::ok(body).with_note(updates.note))
}

pub fn update(layer: &Layer, names: &[String], force: bool) -> Result<Outcome> {
    updates_outcome(ops::update(layer, names, force)?)
}

pub fn remove(layer: &Layer, name: &str) -> Result<Outcome> {
    match ops::remove(layer, name) {
        Ok(removed) => {
            let note = removed.lines.join("\n  ");
            Ok(Outcome::ok(serde_json::to_value(&removed)?).with_note(note))
        }
        Err(error) => rejection(error),
    }
}

pub fn list(layers: &[Layer], offline: bool) -> Result<Outcome> {
    let listed = ops::list(layers, offline)?;
    let outcome = Outcome::raw(listed.text(), serde_json::to_value(&listed)?);
    if listed.count == 0 {
        return Ok(
            outcome.with_note("no packs installed; add one with `graph pack add <name|url>`")
        );
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests;
