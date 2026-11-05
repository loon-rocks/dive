use crate::layout::{
    json_lines, read_jsonl, run_bytes, write_session, ENTRIES_FILE, RUNS_DIR, SESSIONS_DIR,
    SPANS_FILE,
};
use dive_core::run::Identity;
use dive_core::store::{
    Attachment, Entry, EntryBody, NewEntry, RunEnded, RunMeta, RunStarted, RunStatus, SessionKind,
    SessionMeta, StoreError, ThreadEntry, RUN_AUTHOR, USER_AUTHOR,
};
use dive_llm::types::ChatMessage;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

const TMP_DIR: &str = "sessions.v5-tmp";

const LOCAL_TENANT: &str = "local";

const LEGACY_AGENT: &str = "chat";

pub fn unknown_identity() -> Identity {
    Identity {
        user: "unknown".to_string(),
        tenant: LOCAL_TENANT.to_string(),
        deployment: "unknown".to_string(),
        environment: "local".to_string(),
    }
}

#[derive(Deserialize)]
struct ThreadFile {
    id: String,
    title: String,
    created_at: i64,
    updated_at: i64,
    #[serde(default)]
    message_count: i64,
    #[serde(default = "legacy_agent")]
    owner: String,
    #[serde(default = "legacy_agent")]
    active: String,
    #[serde(default)]
    attachments: Vec<Attachment>,
    #[serde(default)]
    project: Option<String>,
}

fn legacy_agent() -> String {
    LEGACY_AGENT.to_string()
}

fn failed(what: &str, path: &Path, e: impl std::fmt::Display) -> StoreError {
    StoreError(format!("{what} {}: {e}", path.display()))
}

pub fn sessions_v4(root: &Path, identity: &Identity) -> Result<(), StoreError> {
    let tmp = root.join(TMP_DIR);
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).map_err(|e| failed("removing", &tmp, e))?;
    }
    let sessions = tmp.join(SESSIONS_DIR);
    let runs = tmp.join(RUNS_DIR);
    for dir in [&sessions, &runs] {
        std::fs::create_dir_all(dir).map_err(|e| failed("creating", dir, e))?;
    }
    let threads = root.join("threads");
    if threads.is_dir() {
        let dirs = std::fs::read_dir(&threads).map_err(|e| failed("reading", &threads, e))?;
        let mut dirs: Vec<_> = dirs.filter_map(Result::ok).map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            if dir.join("meta.json").is_file() {
                thread(&dir, &sessions, &runs, identity)?;
            }
        }
    }
    for name in [SESSIONS_DIR, RUNS_DIR] {
        let into = root.join(name);
        if into.exists() {
            std::fs::remove_dir_all(&into).map_err(|e| failed("removing", &into, e))?;
        }
        let from = tmp.join(name);
        std::fs::rename(&from, &into).map_err(|e| failed("moving", &from, e))?;
    }
    std::fs::remove_dir(&tmp).map_err(|e| failed("removing", &tmp, e))
}

fn thread(dir: &Path, sessions: &Path, runs: &Path, identity: &Identity) -> Result<(), StoreError> {
    let meta_path = dir.join("meta.json");
    let raw = std::fs::read_to_string(&meta_path).map_err(|e| failed("reading", &meta_path, e))?;
    let meta: ThreadFile =
        serde_json::from_str(&raw).map_err(|e| failed("corrupt", &meta_path, e))?;
    let old = read_jsonl::<ThreadEntry>(&dir.join(ENTRIES_FILE))?.unwrap_or_default();
    let stamp = Identity {
        tenant: LOCAL_TENANT.to_string(),
        ..identity.clone()
    };
    let (entries, run_rows) = synthetic_runs(&meta.id, &old, &stamp);
    let session = SessionMeta {
        id: meta.id.clone(),
        kind: SessionKind::Chat,
        title: meta.title,
        tenant: stamp.tenant.clone(),
        owner: stamp.user.clone(),
        deployment: stamp.deployment.clone(),
        environment: stamp.environment.clone(),
        agent: meta.owner,
        active_agent: meta.active,
        plan: None,
        attachments: meta.attachments,
        project: meta.project,
        origin_session: None,
        origin_run: None,
        active_run: None,
        lease_holder: None,
        lease_generation: 0,
        lease_expires_at: None,
        message_count: u64::try_from(meta.message_count).unwrap_or(0),
        created_at: meta.created_at,
        updated_at: meta.updated_at,
    };
    let into = sessions.join(&session.id);
    std::fs::create_dir_all(&into).map_err(|e| failed("creating", &into, e))?;
    write_session(&into, &session)?;
    if !entries.is_empty() {
        let path = into.join(ENTRIES_FILE);
        std::fs::write(&path, json_lines(&entries, "entry")?)
            .map_err(|e| failed("writing", &path, e))?;
    }
    let spans = dir.join(SPANS_FILE);
    if spans.is_file() {
        let to = into.join(SPANS_FILE);
        std::fs::copy(&spans, &to).map_err(|e| failed("copying", &spans, e))?;
    }
    for run in run_rows {
        let path = runs.join(format!("{}.json", run.id));
        std::fs::write(&path, run_bytes(&run)?).map_err(|e| failed("writing", &path, e))?;
    }
    Ok(())
}

fn starts_turn(entry: &ThreadEntry) -> bool {
    entry.author == USER_AUTHOR
        && matches!(
            entry.body,
            EntryBody::Message {
                message: ChatMessage::User { .. }
            }
        )
}

fn user_text(entry: &ThreadEntry) -> Value {
    match &entry.body {
        EntryBody::Message {
            message: ChatMessage::User { content, .. },
        } if entry.author == USER_AUTHOR => Value::String(content.clone()),
        _ => Value::Null,
    }
}

struct Numbered {
    entries: Vec<Entry>,
    next: u64,
}

impl Numbered {
    fn push(&mut self, entry: NewEntry, run: &str, at: i64) -> u64 {
        let seq = self.next;
        self.entries.push(entry.into_entry(seq, run, at));
        self.next += 1;
        seq
    }
}

fn synthetic_runs(
    session: &str,
    old: &[ThreadEntry],
    identity: &Identity,
) -> (Vec<Entry>, Vec<RunMeta>) {
    let mut turns: Vec<&[ThreadEntry]> = Vec::new();
    let mut start = 0;
    for (at, entry) in old.iter().enumerate() {
        if at > start && starts_turn(entry) {
            turns.push(&old[start..at]);
            start = at;
        }
    }
    if start < old.len() {
        turns.push(&old[start..]);
    }
    let mut log = Numbered {
        entries: Vec::new(),
        next: 1,
    };
    let mut runs = Vec::new();
    for (n, turn) in turns.into_iter().enumerate() {
        let Some(first) = turn.first() else {
            continue;
        };
        let last = turn.last().unwrap_or(first);
        let run = format!("{session}-r{}", n + 1);
        let started = RunStarted {
            input: user_text(first),
            identity: identity.clone(),
            trace_id: None,
            plan: None,
            synthetic: true,
        };
        let first_seq = log.push(
            NewEntry::new(RUN_AUTHOR, EntryBody::RunStarted(started)),
            &run,
            first.at,
        );
        for entry in turn {
            carry(&mut log, entry, &run);
        }
        let last_seq = log.push(
            NewEntry::new(
                RUN_AUTHOR,
                EntryBody::RunEnded(RunEnded::new(RunStatus::Completed)),
            ),
            &run,
            last.at,
        );
        runs.push(RunMeta {
            id: run,
            session: session.to_string(),
            kind: SessionKind::Chat,
            status: RunStatus::Completed,
            first_seq,
            last_seq: Some(last_seq),
            trace_id: None,
            cancel_requested: false,
            cancel_by: None,
            tenant: identity.tenant.clone(),
            owner: identity.user.clone(),
            deployment: identity.deployment.clone(),
            environment: identity.environment.clone(),
            token_expires_at: None,
            started_at: first.at,
            ended_at: Some(last.at),
        });
    }
    (log.entries, runs)
}

fn carry(log: &mut Numbered, entry: &ThreadEntry, run: &str) {
    let EntryBody::SubagentRun {
        agent,
        caller,
        input,
        messages,
        output,
        final_,
    } = &entry.body
    else {
        log.push(NewEntry::from(entry.clone()), run, entry.at);
        return;
    };
    let mut opened = NewEntry::new(
        &entry.author,
        EntryBody::AgentStarted {
            agent: agent.clone(),
            caller: caller.clone(),
            input: input.clone(),
        },
    )
    .in_span(entry.span.clone());
    opened.ms = entry.ms;
    let parent = log.push(opened, run, entry.at);
    let path = format!("agent:{agent}");
    for message in messages {
        log.push(
            NewEntry::message(agent, message.clone()).at_path(&path, Some(parent)),
            run,
            entry.at,
        );
    }
    log.push(
        NewEntry::new(
            &entry.author,
            EntryBody::AgentFinished {
                agent: agent.clone(),
                output: output.clone(),
                final_: *final_,
            },
        )
        .at_path("", Some(parent)),
        run,
        entry.at,
    );
}
