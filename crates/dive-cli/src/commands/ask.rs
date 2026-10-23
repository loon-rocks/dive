//! `graph ask` — one agent turn, persisted to a thread.

use anyhow::{bail, Result};
use dive_core::NullSink;
use dive_runtime::chat::ChatStart;
use std::io::{IsTerminal, Read};
use std::sync::Arc;

pub struct AskArgs {
    pub message: Option<String>,
    pub agent: Option<String>,
    pub thread: Option<Option<String>>,
    pub json: bool,
    pub no_stream: bool,
}

pub async fn run(args: AskArgs) -> Result<()> {
    let message = resolve_message(args.message)?;

    if let Some(name) = args
        .agent
        .as_deref()
        .filter(|name| crate::workbench::agents::is_workbench_only(name))
    {
        bail!("{name} runs only in the workbench: start it with `graph wb`");
    }
    let runtime = crate::host::runtime()?;
    let store = runtime.store()?;
    let mut start = ChatStart::resolve(&runtime, store, args.agent.as_deref(), args.thread).await?;

    let stream_text = !args.json && !args.no_stream;
    let thread_id = match start.thread() {
        Some(meta) => meta.id.clone(),
        None => start.create_thread(&message).await?.id.clone(),
    };
    let run = crate::telemetry::RunInfo::conversation("ask", Some(thread_id.clone()))
        .user(runtime.config.user.name.as_deref())
        .input(serde_json::Value::String(message.clone()));
    let events: Arc<dyn dive_core::EventSink> = if args.json {
        // Quiet unless JSONL events were explicitly requested.
        if std::env::var("GRAPH_EVENTS").as_deref() == Ok("jsonl") {
            crate::output::make_sink(true, false, run)
        } else {
            crate::telemetry::attach(Arc::new(NullSink), run)
        }
    } else {
        crate::output::make_sink(!stream_text, false, run)
    };
    // A conversation already has the user's attention: a plan called as
    // plan__* from here can put an `ask` step's question to them.
    let hooks = dive_runtime::runtime::PipelineHooks {
        interlocutor: crate::interlocutor::tty(),
        ..Default::default()
    };
    let mut session = start.open(&runtime, events, hooks, None).await?;

    let result = session.send(&message).await;
    runtime.shutdown().await;
    let report = result?;
    if let Some(error) = report.persist_error {
        return Err(error);
    }
    let outcome = report.turn;
    let usage = report.usage;

    if args.json {
        let envelope = serde_json::json!({
            "content": outcome.text,
            "tool_calls_made": outcome.tool_calls_made,
            "tools_used": outcome.tools_used,
            "usage": usage,
            "thread_id": thread_id,
            "agent": outcome.active,
        });
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        if stream_text {
            println!();
        } else {
            println!("{}", outcome.text);
        }
        // Not under GRAPH_EVENTS=jsonl: stderr is the machine feed there,
        // and the `usage_summary` event already carries this.
        if !usage.is_empty() && !crate::output::jsonl_events() {
            eprintln!("\x1b[2m{}\x1b[0m", usage.summary());
        }
        eprintln!(
            "\x1b[2mresume with `graph chat --thread {}`\x1b[0m",
            thread_id
        );
    }
    Ok(())
}

/// Use the argument, appending piped stdin as an input block when present.
fn resolve_message(message: Option<String>) -> Result<String> {
    let mut piped = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_to_string(&mut piped)?;
    }
    let piped = piped.trim();
    match (message, piped.is_empty()) {
        (Some(m), true) => Ok(m),
        (Some(m), false) => Ok(format!("{m}\n\n<input>\n{piped}\n</input>")),
        (None, false) => Ok(piped.to_string()),
        (None, true) => bail!("no message given — pass one as an argument or pipe via stdin"),
    }
}
