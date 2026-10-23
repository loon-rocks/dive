//! `graph tool` — inspect the tool catalog.

use crate::cli::ToolCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::Result;
use dive_core::toolbox::AgentToolbox;
use dive_core::ToolRegistry;
use dive_runtime::ops;
use std::collections::BTreeMap;

pub async fn run(command: ToolCommand) -> Result<()> {
    if let ToolCommand::Migrate { path, json } = command {
        return report(
            crate::commands::plan_cmd::migrated_outcome(ops::tool::migrate(&path)?),
            json,
        );
    }
    if let ToolCommand::Rm { name, json } = command {
        return report(
            crate::commands::remove_cmd::run(ops::remove::ItemKind::Tool, &name)?,
            json,
        );
    }
    let runtime = crate::host::runtime()?;
    let store = runtime.store()?;
    let toolbox = runtime
        .toolbox(&store, std::sync::Arc::new(dive_core::NullSink))
        .await?;
    let docs = runtime.tool_docs()?;
    let result = dispatch(toolbox.as_ref(), command, &docs).await;
    runtime.shutdown().await;
    let (outcome, json) = result?;
    report(outcome, json)
}

async fn dispatch(
    registry: &AgentToolbox,
    command: ToolCommand,
    docs: &dive_core::user_tools::ToolDocs,
) -> Result<(Outcome, bool)> {
    match command {
        ToolCommand::List { json } => Ok((
            list_labeled(registry, &ops::tool::pack_labels(docs)).await?,
            json,
        )),
        ToolCommand::Show { name, json } => Ok((show(registry, docs, &name).await?, json)),
        ToolCommand::Test {
            name,
            input,
            inputs,
            json,
        } => Ok((
            test(registry, &name, input.as_deref(), &inputs).await?,
            json,
        )),
        ToolCommand::Migrate { .. } | ToolCommand::Rm { .. } => {
            unreachable!("handled before the runtime starts")
        }
    }
}

pub(crate) async fn list(registry: &(dyn ToolRegistry + Send + Sync)) -> Result<Outcome> {
    list_labeled(registry, &BTreeMap::new()).await
}

pub(crate) async fn list_labeled(
    registry: &(dyn ToolRegistry + Send + Sync),
    labels: &BTreeMap<String, String>,
) -> Result<Outcome> {
    let listing = ops::tool::list(registry, labels).await?;
    let body = serde_json::to_value(&listing)?;
    if listing.defs.is_empty() {
        return Ok(Outcome::raw(String::new(), body)
            .with_note("no tools available — configure [mcp.*] servers"));
    }
    let text = super::listing::render_tool_listing_labeled(
        &listing.defs,
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
        labels,
    );
    Ok(Outcome::raw(text, body))
}

pub(crate) async fn show(
    registry: &(dyn ToolRegistry + Send + Sync),
    docs: &dive_core::user_tools::ToolDocs,
    name: &str,
) -> Result<Outcome> {
    let detail = ops::tool::show(registry, docs, name).await?;
    let mut text = format!("{}\n\n{}\n\n", detail.name, detail.description);
    text.push_str(&format!(
        "input schema:\n{}\n",
        serde_json::to_string_pretty(&detail.input_schema)?
    ));
    if let Some(schema) = &detail.output_schema {
        text.push_str(&format!(
            "\noutput schema:\n{}\n",
            serde_json::to_string_pretty(schema)?
        ));
    }
    let mut outcome = Outcome::raw(text, serde_json::to_value(&detail)?);
    if let Some(origin) = &detail.origin {
        outcome = outcome.with_note(format!("source: {origin}"));
    }
    Ok(outcome)
}

pub(crate) async fn test(
    registry: &AgentToolbox,
    name: &str,
    input: Option<&str>,
    inputs: &[String],
) -> Result<Outcome> {
    ops::tool::testable(registry, name)?;
    let input = crate::commands::input::resolve_input(input, inputs)?;
    let tested = ops::tool::test(registry, name, input).await?;
    if tested.is_error {
        eprintln!("tool returned an error:");
    }
    let text = format!("{}\n", serde_json::to_string_pretty(&tested.result)?);
    Ok(Outcome::raw(text, serde_json::to_value(&tested)?))
}
