//! `dive mcp` — list/tools/test/probe.

use super::listing::render_tool_listing;
use crate::cli::McpCommand;
use crate::commands::outcome::{report, Outcome};
use anyhow::{bail, Result};
use dive_mcp::{McpManager, SignIn, SignInScope};
use dive_runtime::ops;
use dive_runtime::runtime::{system_sign_ins, Workspace};
use std::io::IsTerminal;

pub async fn run(command: McpCommand) -> Result<()> {
    let loaded = crate::host::load_config()?;
    let workspace = Workspace::current();
    let manager = McpManager::new(loaded.config.mcp.clone()).with_sign_ins(system_sign_ins(
        dive_runtime::secrets::project_id(&workspace),
    ));

    let result = match command {
        McpCommand::List { json } => list(&loaded.config, &manager).map(|o| (o, json)),
        McpCommand::Login {
            server,
            project,
            global,
            json,
        } => login(&manager, &workspace, &server, scope(project, global))
            .await
            .map(|o| (o, json)),
        McpCommand::Logout {
            server,
            project,
            global,
            json,
        } => logout(&manager, &workspace, &server, scope(project, global))
            .await
            .map(|o| (o, json)),
        McpCommand::Tools { server, json } => tools(&manager, server).await.map(|o| (o, json)),
        McpCommand::Test { server, json } => test(&manager, &server).await.map(|o| (o, json)),
        McpCommand::Serve { .. } => unreachable!("serve is dispatched before the manager opens"),
        McpCommand::Probe { .. } => bail!("probe lands with the shape cache (phase 4)"),
    };
    manager.shutdown().await;
    let (outcome, json) = result?;
    report(outcome, json)
}

fn scope(project: bool, global: bool) -> Option<SignInScope> {
    match (project, global) {
        (true, _) => Some(SignInScope::Project),
        (_, true) => Some(SignInScope::Global),
        _ => None,
    }
}

fn scope_name(scope: SignInScope) -> &'static str {
    match scope {
        SignInScope::Project => "this project",
        SignInScope::Global => "every project",
    }
}

fn describe(sign_in: &SignIn) -> String {
    let when = chrono::DateTime::from_timestamp(sign_in.signed_in_at as i64, 0)
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%b %-d, %H:%M")
                .to_string()
        })
        .unwrap_or_default();
    let access = if sign_in.access.is_empty() {
        String::new()
    } else {
        format!(", access: {}", sign_in.access.join(" "))
    };
    format!(
        "signed in for {} since {when}{access}",
        scope_name(sign_in.scope)
    )
}

async fn login(
    manager: &McpManager,
    workspace: &Workspace,
    server: &str,
    scope: Option<SignInScope>,
) -> Result<Outcome> {
    let open = |url: &str| {
        eprintln!("Opening {server}'s sign-in page. If no browser opens, visit:\n  {url}");
        open_browser(url);
    };
    let cancelled = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let signed = ops::mcp::sign_in(manager, workspace, server, scope, open, cancelled).await?;
    Ok(Outcome::raw(
        format!("{server}: {}\n", describe(&signed)),
        serde_json::to_value(&signed)?,
    ))
}

async fn logout(
    manager: &McpManager,
    workspace: &Workspace,
    server: &str,
    scope: Option<SignInScope>,
) -> Result<Outcome> {
    let out = ops::mcp::sign_out(manager, workspace, server, scope).await?;
    let body = serde_json::to_value(&out)?;
    if !out.removed {
        return Ok(Outcome::rejected(body).with_note(format!(
            "{server} has no sign-in for {}",
            scope_name(out.scope)
        )));
    }
    let after = match &out.now {
        Some(now) => format!("; now {}", describe(now)),
        None => String::new(),
    };
    Ok(Outcome::raw(
        format!(
            "{server}: signed out for {}{after}\n",
            scope_name(out.scope)
        ),
        body,
    ))
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(error) = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tracing::debug!(error = %error, "couldn't open a browser");
    }
}

fn list(config: &dive_config::Config, manager: &McpManager) -> Result<Outcome> {
    let listed = ops::mcp::list(config, manager.sign_ins().as_ref());
    let body = serde_json::to_value(&listed)?;
    if listed.servers.is_empty() {
        return Ok(Outcome::raw(String::new(), body)
            .with_note("no MCP servers configured — add [mcp.<name>] sections to your config"));
    }
    let text = listed
        .servers
        .iter()
        .map(|s| {
            let signed = s
                .sign_in
                .as_ref()
                .map(|sign_in| format!("\t{}", describe(sign_in)))
                .unwrap_or_default();
            format!("{}\t{}: {}{signed}\n", s.name, s.transport, s.target)
        })
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
