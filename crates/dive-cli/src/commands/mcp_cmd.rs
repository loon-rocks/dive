//! `graph mcp` — list/tools/test/probe.

use super::listing::render_tool_listing;
use crate::cli::McpCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::{bail, Result};
use dive_mcp::McpManager;
use dive_runtime::ops;
use std::io::IsTerminal;

pub async fn run(command: McpCommand) -> Result<()> {
    let loaded = crate::host::load_config()?;
    let manager = McpManager::new(loaded.config.mcp.clone());

    let result = match command {
        McpCommand::List { json } => list(&loaded.config).map(|o| (o, json)),
        McpCommand::Tools { server, json } => tools(&manager, server).await.map(|o| (o, json)),
        McpCommand::Test { server, json } => test(&manager, &server).await.map(|o| (o, json)),
        McpCommand::Serve { .. } => unreachable!("serve is dispatched before the manager opens"),
        McpCommand::Probe { .. } => bail!("probe lands with the shape cache (phase 4)"),
    };
    manager.shutdown().await;
    let (outcome, json) = result?;
    report(outcome, json)
}

fn list(config: &dive_config::Config) -> Result<Outcome> {
    let listed = ops::mcp::list(config);
    let body = serde_json::to_value(&listed)?;
    if listed.servers.is_empty() {
        return Ok(Outcome::raw(String::new(), body)
            .with_note("no MCP servers configured — add [mcp.<name>] sections to your config"));
    }
    let text = listed
        .servers
        .iter()
        .map(|s| format!("{}\t{}: {}\n", s.name, s.transport, s.target))
        .collect::<String>();
    Ok(Outcome::raw(text, body))
}

async fn tools(manager: &McpManager, server: Option<String>) -> Result<Outcome> {
    let listing = ops::mcp::tools(manager, server.as_deref()).await?;
    let body = serde_json::to_value(&listing)?;
    if listing.defs.is_empty() {
        return Ok(Outcome::raw(String::new(), body).with_note("no tools exposed"));
    }
    Ok(Outcome::raw(
        render_tool_listing(&listing.defs, std::io::stdout().is_terminal()),
        body,
    ))
}

async fn test(manager: &McpManager, server: &str) -> Result<Outcome> {
    let tested = ops::mcp::test(manager, server).await?;
    let text = format!(
        "ok: '{server}' initialized in {:.2?} — {} tools \
         ({} declare output schemas)\n",
        tested.elapsed, tested.tools, tested.declare_output_schema,
    );
    Ok(Outcome::raw(text, serde_json::to_value(&tested)?))
}
