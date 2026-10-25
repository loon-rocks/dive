//! `dive chat` — interactive REPL with persistent threads.

use anyhow::{bail, Result};
use dive_core::agent::conversation::view;
use dive_runtime::chat::{ChatSession, ChatStart};
use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};
use std::sync::Arc;

pub async fn run(agent: Option<String>, thread: Option<Option<String>>) -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("chat needs an interactive terminal — use `dive ask` for scripted queries");
    }
    let runtime = crate::host::runtime()?;
    let store = runtime.store()?;
    let start = ChatStart::resolve(&runtime, store, agent.as_deref(), thread).await?;
    let run =
        crate::telemetry::RunInfo::conversation("chat", start.thread().map(|meta| meta.id.clone()))
            .user(runtime.config.user.name.as_deref());
    let events: Arc<dyn dive_core::EventSink> = crate::output::make_sink(false, false, run);
    // A conversation already has the user's attention: a plan called as
    // plan__* from here can put an `ask` step's question to them.
    let hooks = dive_runtime::runtime::PipelineHooks {
        interlocutor: crate::interlocutor::tty(),
        ..Default::default()
    };
    let mut session = start
        .open(&runtime, events, hooks, Some("chat".to_string()))
        .await?;

    if let Some(meta) = session.thread() {
        eprintln!(
            "continuing thread {} — {} (with {})",
            meta.id,
            meta.title,
            session.active()
        );
    }

    let mut editor = Reedline::create();
    eprintln!(
        "dive chat with {} — /quit to exit, /agent to see or switch agents, /state to inspect, /thread for the thread id",
        session.active()
    );

    loop {
        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic(prompt_label(session.active())),
            DefaultPromptSegment::Empty,
        );
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if let Some(command) = line.strip_prefix('/') {
                    if slash(&mut session, command).await? {
                        break;
                    }
                    continue;
                }
                let before = session.active().to_string();
                match session.send(&line).await {
                    Ok(report) => {
                        if let Some(e) = &report.thread_error {
                            eprintln!("warning: failed to create thread: {e}");
                        }
                        println!();
                        if !report.usage.is_empty() && !crate::output::jsonl_events() {
                            eprintln!("\x1b[2m{}\x1b[0m", report.usage.summary());
                        }
                        if report.turn.active != before {
                            eprintln!("\x1b[2mnow talking with {}\x1b[0m", report.turn.active);
                        }
                        if let Some(e) = &report.persist_error {
                            eprintln!("warning: failed to persist turn: {e}");
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => break,
            Err(e) => {
                runtime.shutdown().await;
                bail!("readline failed: {e}");
            }
        }
    }
    runtime.shutdown().await;
    if let Some(meta) = session.thread() {
        eprintln!("resume with `dive chat --thread {}`", meta.id);
    }
    Ok(())
}

fn prompt_label(active: &str) -> String {
    if active == dive_core::agent::doc::CHAT_AGENT {
        "dive".to_string()
    } else {
        format!("dive:{active}")
    }
}

/// Returns true when the session should end.
async fn slash(session: &mut ChatSession, command: &str) -> Result<bool> {
    let mut words = command.split_whitespace();
    let active = session.active().to_string();
    match (words.next().unwrap_or_default(), words.next()) {
        ("quit" | "exit" | "q", _) => Ok(true),
        ("state", _) => {
            let messages = view(&active, session.history().iter());
            println!("{}", serde_json::to_string_pretty(&messages)?);
            Ok(false)
        }
        ("thread", _) => {
            match session.thread() {
                Some(meta) => println!(
                    "{} — {} (owner {}, active {active})",
                    meta.id, meta.title, meta.owner
                ),
                None => println!("no thread yet (created after the first turn)"),
            }
            Ok(false)
        }
        ("agent", None) => {
            println!("talking with {active}");
            for doc in session.conversation().agents.iter() {
                let marker = if doc.name == active { "*" } else { " " };
                println!("{marker} {} — {}", doc.name, doc.description);
            }
            Ok(false)
        }
        ("agent", Some(name)) => {
            match session.conversation().agent(name) {
                Ok(doc) => {
                    session.set_active(&doc.name).await?;
                    println!("now talking with {}", session.active());
                }
                Err(e) => eprintln!("{e}"),
            }
            Ok(false)
        }
        (other, _) => {
            eprintln!(
                "unknown command: /{other} (try /quit, /agent, /agent <name>, /state, /thread)"
            );
            Ok(false)
        }
    }
}
