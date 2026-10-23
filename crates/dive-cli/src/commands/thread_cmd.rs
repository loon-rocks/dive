//! `dive thread` — list/show/rm. Opens the store directly; no providers
//! or MCP servers needed.

use crate::cli::ThreadCommand;
use crate::commands::outcome::{report, Outcome};
use crate::host::open_store;
use anyhow::Result;
use dive_core::{EntryBody, Store, ThreadEntry};
use dive_llm::types::ChatMessage;
use dive_runtime::ops::thread as ops;
use dive_runtime::runtime::Workspace;
use std::sync::Arc;

pub async fn run(command: ThreadCommand) -> Result<()> {
    let config = crate::host::load_config()?.config;
    let store = open_store(&config)?;

    match command {
        ThreadCommand::List { all, json } => {
            let project = Workspace::current().project();
            report(list(&store, all, project.as_deref()).await?, json)
        }
        ThreadCommand::Show { id, state, json } => report(show(&store, &id, state).await?, json),
        ThreadCommand::Rm { id, json } => report(rm(&store, &id).await?, json),
    }
}

async fn list(store: &Arc<dyn Store>, all: bool, project: Option<&str>) -> Result<Outcome> {
    let listed = if all {
        ops::list(store.as_ref()).await?
    } else {
        ops::list_in(store.as_ref(), project).await?
    };
    let body = serde_json::to_value(&listed)?;
    if listed.threads.is_empty() {
        let note = match (all, project) {
            (false, Some(_)) => {
                "no threads yet in this project — run `dive ask` or `dive chat`, or `dive thread list --all` for every thread"
            }
            (false, None) => {
                "no threads yet outside a project — run `dive ask` or `dive chat`, or `dive thread list --all` for every thread"
            }
            (true, _) => "no threads yet — run `dive ask` or `dive chat`",
        };
        return Ok(Outcome::raw(String::new(), body).with_note(note));
    }
    let text = listed
        .threads
        .iter()
        .map(|thread| {
            format!(
                "{}  {}  {:>3} msgs  {}\n",
                thread.id,
                format_time(thread.updated_at),
                thread.message_count,
                thread.title,
            )
        })
        .collect::<String>();
    Ok(Outcome::raw(text, body))
}

async fn show(store: &Arc<dyn Store>, id: &str, state: bool) -> Result<Outcome> {
    let detail = ops::show(store.as_ref(), id).await?;
    let body = serde_json::to_value(&detail)?;
    // `--state` asks for the raw runtime state, so it prints the entries
    // verbatim rather than the conversation transcript.
    if state {
        let text = format!("{}\n", serde_json::to_string_pretty(&detail.entries)?);
        return Ok(Outcome::raw(text, body));
    }
    let meta = &detail.summary;
    let mut text = format!(
        "{} — {} ({} messages, updated {}; owner {}, active {})\n\n",
        meta.id,
        meta.title,
        meta.message_count,
        format_time(meta.updated_at),
        meta.owner,
        meta.active,
    );
    for entry in &detail.entries {
        text.push_str(&render_entry(entry));
        text.push('\n');
    }
    Ok(Outcome::raw(text, body))
}

async fn rm(store: &Arc<dyn Store>, id: &str) -> Result<Outcome> {
    let deleted = ops::delete(store.as_ref(), id).await?;
    Ok(Outcome::raw(
        format!("deleted {}\n", deleted.deleted),
        serde_json::to_value(&deleted)?,
    ))
}

fn format_time(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "?".to_string())
}

fn render_entry(entry: &ThreadEntry) -> String {
    match &entry.body {
        EntryBody::Message { message } => render_message(&entry.author, message),
        EntryBody::Handoff {
            from, to, message, ..
        } => format!("  ⇢ {from} hands off to {to}: {message}\n"),
        EntryBody::SubagentRun {
            agent,
            caller,
            messages,
            final_,
            ..
        } => format!(
            "  ↳ {caller} ran {agent} ({} messages{})\n",
            messages.len(),
            if *final_ {
                ""
            } else {
                ", stopped before answering"
            }
        ),
    }
}

fn render_message(author: &str, message: &ChatMessage) -> String {
    match message {
        ChatMessage::User { content } => format!("{author}> {content}\n"),
        ChatMessage::Assistant {
            content,
            tool_calls,
            // Opaque provider reasoning — never rendered to a human.
            thinking: _,
        } => {
            let mut out = String::new();
            for call in tool_calls {
                let args = serde_json::to_string(&call.arguments).unwrap_or_default();
                let preview: String = args.chars().take(100).collect();
                out.push_str(&format!("  → {} {}\n", call.name, preview));
            }
            if let Some(text) = content {
                if !text.is_empty() {
                    out.push_str(&format!("{author}> {text}\n"));
                }
            }
            out
        }
        ChatMessage::ToolResult {
            content, is_error, ..
        } => {
            let rendered = serde_json::to_string(content).unwrap_or_default();
            let preview: String = rendered.chars().take(160).collect();
            let marker = if *is_error { "✗" } else { "✓" };
            format!("  {marker} {preview}\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_store::MemoryStore;

    fn titles(outcome: &Outcome) -> Vec<String> {
        let mut titles: Vec<String> = outcome.body["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|thread| thread["title"].as_str().unwrap().to_string())
            .collect();
        titles.sort();
        titles
    }

    #[tokio::test]
    async fn thread_list_shows_the_current_project_by_default_and_every_thread_with_all() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        store.create_thread("global", "chat", None).await.unwrap();
        store
            .create_thread("site", "chat", Some("/work/site"))
            .await
            .unwrap();
        store
            .create_thread("other", "chat", Some("/work/other"))
            .await
            .unwrap();

        let in_site = list(&store, false, Some("/work/site")).await.unwrap();
        assert_eq!(titles(&in_site), ["site"]);
        let global = list(&store, false, None).await.unwrap();
        assert_eq!(titles(&global), ["global"]);
        let all = list(&store, true, Some("/work/site")).await.unwrap();
        assert_eq!(titles(&all), ["global", "other", "site"]);
        let empty = list(&store, false, Some("/work/none")).await.unwrap();
        assert!(titles(&empty).is_empty());
    }
}
