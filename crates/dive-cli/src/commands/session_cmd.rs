//! `dive session` — list/show/rm (`dive thread` is a hidden alias). Opens
//! the store directly; no providers or MCP servers needed.

use crate::cli::SessionCommand;
use crate::commands::outcome::{report, Outcome};
use crate::host::{open_files, open_store};
use anyhow::Result;
use dive_core::context::{fold, Folded, SubagentCall};
use dive_core::store::{Entry, EntryBody, Page, SessionFilter, SessionKind};
use dive_core::Store;
use dive_llm::types::ChatMessage;
use dive_runtime::ops::session as ops;
use dive_runtime::runtime::Workspace;
use std::sync::Arc;

pub async fn run(command: SessionCommand) -> Result<()> {
    let config = crate::host::load_config()?.config;
    let store = open_store(&config)?;

    match command {
        SessionCommand::List { all, kind, json } => {
            let project = Workspace::current().project();
            let kind = kind.as_deref().and_then(SessionKind::parse);
            report(list(&store, all, kind, project.as_deref()).await?, json)
        }
        SessionCommand::Show { id, state, json } => report(show(&store, &id, state).await?, json),
        SessionCommand::Rm { id, json } => {
            let files = open_files(&config)?;
            report(rm(&store, &files, &id).await?, json)
        }
    }
}

async fn list(
    store: &Arc<dyn Store>,
    all: bool,
    kind: Option<SessionKind>,
    project: Option<&str>,
) -> Result<Outcome> {
    let filter = SessionFilter {
        kind,
        project: (!all).then(|| project.map(str::to_string)),
        ..SessionFilter::default()
    };
    let listed = ops::list(store.as_ref(), &filter, Page::ALL).await?;
    let body = serde_json::to_value(&listed)?;
    if listed.sessions.is_empty() {
        let note = match (all, project) {
            (false, Some(_)) => {
                "no sessions yet in this project — run `dive ask`, `dive chat` or `dive plan run`, or `dive session list --all` for every session"
            }
            (false, None) => {
                "no sessions yet outside a project — run `dive ask`, `dive chat` or `dive plan run`, or `dive session list --all` for every session"
            }
            (true, _) => "no sessions yet — run `dive ask`, `dive chat` or `dive plan run`",
        };
        return Ok(Outcome::raw(String::new(), body).with_note(note));
    }
    let text = listed
        .sessions
        .iter()
        .map(|session| {
            format!(
                "{}  {}  {:<4}  {:>3} msgs  {}\n",
                session.id,
                format_time(session.updated_at),
                session.kind.as_str(),
                session.message_count,
                session.title,
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
        "{} — {} ({}, {} messages, updated {}; started with {}, active {})\n\n",
        meta.id,
        meta.title,
        meta.kind.as_str(),
        meta.message_count,
        format_time(meta.updated_at),
        meta.agent,
        meta.active,
    );
    text.push_str(&transcript(&detail.entries));
    Ok(Outcome::raw(text, body))
}

fn transcript(entries: &[Entry]) -> String {
    let mut text = String::new();
    for item in fold(entries) {
        match item {
            Folded::Entry(entry) => text.push_str(&render_entry(entry)),
            Folded::Subagent(call) => text.push_str(&render_subagent(&call)),
        }
    }
    text
}

async fn rm(
    store: &Arc<dyn Store>,
    files: &Arc<dyn dive_core::files::BlobStore>,
    id: &str,
) -> Result<Outcome> {
    let deleted = ops::delete(store.as_ref(), files.as_ref(), id).await?;
    let removed = match deleted.files_removed {
        0 => String::new(),
        1 => " and 1 file only it used".to_string(),
        n => format!(" and {n} files only it used"),
    };
    Ok(Outcome::raw(
        format!("deleted {}{removed}\n", deleted.deleted),
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

fn render_subagent(call: &SubagentCall<'_>) -> String {
    format!(
        "  ↳ {} ran {} ({} messages{})\n",
        call.caller,
        call.agent,
        call.messages.len(),
        if call.final_ {
            ""
        } else {
            ", stopped before answering"
        }
    )
}

fn render_entry(entry: &Entry) -> String {
    match &entry.body {
        EntryBody::Message { message } => format!("{}\n", render_message(&entry.author, message)),
        EntryBody::Handoff {
            from, to, message, ..
        } => format!("  ⇢ {from} hands off to {to}: {message}\n\n"),
        EntryBody::RunEnded(ended) => match &ended.error {
            Some(error) => format!("  ✗ run {}: {error}\n\n", ended.status.as_str()),
            None if ended.status != dive_core::store::RunStatus::Completed => {
                format!("  ✗ run {}\n\n", ended.status.as_str())
            }
            None => String::new(),
        },
        EntryBody::CancelRequested { by } if !by.is_empty() => {
            format!("  ✗ {by} asked to cancel\n\n")
        }
        EntryBody::CancelRequested { .. } => "  ✗ cancel requested\n\n".to_string(),
        _ => String::new(),
    }
}

fn render_message(author: &str, message: &ChatMessage) -> String {
    match message {
        ChatMessage::User { content, .. } => format!("{author}> {content}\n"),
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
    use dive_core::run::Identity;
    use dive_core::store::NewSession;
    use dive_store::MemoryStore;

    fn titles(outcome: &Outcome) -> Vec<String> {
        let mut titles: Vec<String> = outcome.body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["title"].as_str().unwrap().to_string())
            .collect();
        titles.sort();
        titles
    }

    async fn create(store: &Arc<dyn Store>, new: NewSession) {
        store.create_session(new).await.unwrap();
    }

    #[tokio::test]
    async fn session_list_shows_the_current_project_by_default_every_session_with_all_and_one_kind_with_kind(
    ) {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let who = Identity::default();
        create(&store, NewSession::chat("global", "chat", &who)).await;
        create(
            &store,
            NewSession::chat("site", "chat", &who).in_project(Some("/work/site")),
        )
        .await;
        create(
            &store,
            NewSession::chat("other", "chat", &who).in_project(Some("/work/other")),
        )
        .await;
        create(
            &store,
            NewSession::plan("digest", "digest", &who).in_project(Some("/work/site")),
        )
        .await;

        let in_site = list(&store, false, None, Some("/work/site")).await.unwrap();
        assert_eq!(titles(&in_site), ["digest", "site"]);
        let global = list(&store, false, None, None).await.unwrap();
        assert_eq!(titles(&global), ["global"]);
        let all = list(&store, true, None, Some("/work/site")).await.unwrap();
        assert_eq!(titles(&all), ["digest", "global", "other", "site"]);
        let plans = list(&store, true, Some(SessionKind::Plan), None)
            .await
            .unwrap();
        assert_eq!(titles(&plans), ["digest"]);
        let empty = list(&store, false, None, Some("/work/none")).await.unwrap();
        assert!(titles(&empty).is_empty());
    }

    fn fixture(version: &str) -> tempfile::TempDir {
        let from = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../dive-store/tests/fixtures")
            .join(version);
        let dir = tempfile::tempdir().unwrap();
        let mut pending = vec![from.clone()];
        while let Some(at) = pending.pop() {
            for entry in std::fs::read_dir(&at).unwrap().filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let to = dir.path().join(path.strip_prefix(&from).unwrap());
                    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
                    std::fs::copy(&path, to).unwrap();
                }
            }
        }
        dir
    }

    #[tokio::test]
    async fn a_migrated_version_4_session_shows_its_messages_handoffs_and_subagent_calls() {
        let dir = fixture("v4");
        let store: Arc<dyn Store> = Arc::new(dive_store::FileStore::open(dir.path()).unwrap());
        let shown = show(&store, "5ab000000001", false).await.unwrap();
        let text = shown.raw.unwrap();
        assert!(text.contains("user> draft the release plan"), "{text}");
        assert!(
            text.contains("⇢ front_desk hands off to plan_author: draft it"),
            "{text}"
        );
        assert!(text.contains("→ agent__plan_refiner"), "{text}");
        assert!(
            text.contains("↳ plan_author ran plan_refiner (2 messages)"),
            "{text}"
        );
        assert!(
            text.contains("plan_author> The plan now ends with a changelog step."),
            "{text}"
        );
        assert!(text.contains("user> ship it"), "{text}");
        assert_eq!(shown.body["spans"].as_array().unwrap().len(), 2);
        assert_eq!(shown.body["runs"].as_array().unwrap().len(), 2);
    }
}
