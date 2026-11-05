//! In-memory `Store`: ephemeral runtime state for CI jobs and tests.
//! Nothing survives the process.

use crate::session::{self as ops, Clock};
use dive_core::store::{
    check_batch, Attachment, CancelOutcome, Entry, EntryBody, FenceError, Lease, NewEntry,
    NewSession, Page, Reaped, Renewal, RunEnded, RunMeta, SessionFilter, SessionMeta, SessionPage,
    Span, StartError, StartRun, Store, StoreError, ThreadEntry, ThreadMeta, ToolShape,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

pub struct MemoryStore {
    inner: Mutex<Inner>,
    clock: Clock,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            clock: ops::wall_clock(),
        }
    }
}

#[derive(Default)]
struct Inner {
    threads: HashMap<String, (ThreadMeta, Vec<ThreadEntry>)>,
    shapes: HashMap<String, ToolShape>,
    spans: HashMap<String, Vec<Span>>,
    sessions: HashMap<String, Held>,
    runs: HashMap<String, RunMeta>,
}

struct Held {
    meta: SessionMeta,
    entries: Vec<Entry>,
    spans: Vec<Span>,
}

impl Held {
    fn last_seq(&self) -> u64 {
        self.entries.last().map_or(0, |entry| entry.seq)
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_clock(clock: Clock) -> Self {
        Self {
            inner: Mutex::default(),
            clock,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }
}

fn no_session(id: &str) -> StoreError {
    StoreError(format!("no session {id}"))
}

fn no_run(id: &str) -> StoreError {
    StoreError(format!("no run {id}"))
}

impl Inner {
    fn held(&mut self, lease: &Lease) -> Result<(&mut Held, &mut RunMeta), FenceError> {
        let held = self
            .sessions
            .get_mut(&lease.session)
            .ok_or(FenceError::LeaseLost)?;
        let run = self.runs.get_mut(&lease.run).ok_or(FenceError::LeaseLost)?;
        ops::fence(&held.meta, lease)?;
        Ok((held, run))
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[async_trait::async_trait]
impl Store for MemoryStore {
    async fn create_thread(
        &self,
        title: &str,
        owner: &str,
        project: Option<&str>,
    ) -> Result<ThreadMeta, StoreError> {
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let now = now_ms();
        let meta = ThreadMeta {
            id: id.clone(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
            owner: owner.to_string(),
            active: owner.to_string(),
            attachments: Vec::new(),
            project: project.map(str::to_string),
        };
        self.inner
            .lock()
            .unwrap()
            .threads
            .insert(id, (meta.clone(), Vec::new()));
        Ok(meta)
    }

    async fn get_thread(&self, id: &str) -> Result<Option<ThreadMeta>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .threads
            .get(id)
            .map(|(meta, _)| meta.clone()))
    }

    async fn latest_thread(&self) -> Result<Option<ThreadMeta>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .threads
            .values()
            .max_by_key(|(meta, _)| meta.updated_at)
            .map(|(meta, _)| meta.clone()))
    }

    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, StoreError> {
        let mut threads: Vec<ThreadMeta> = self
            .inner
            .lock()
            .unwrap()
            .threads
            .values()
            .map(|(meta, _)| meta.clone())
            .collect();
        threads.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
        Ok(threads)
    }

    async fn delete_thread(&self, id: &str) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.spans.remove(id);
        Ok(inner.threads.remove(id).is_some())
    }

    async fn append_entries(
        &self,
        thread_id: &str,
        entries: &[NewEntry],
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let (meta, stored) = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| StoreError(format!("no thread {thread_id}")))?;
        let at = now_ms();
        for entry in entries {
            let seq = stored.len() as u64;
            stored.push(entry.clone().into_thread_entry(seq, at));
        }
        meta.message_count = stored
            .iter()
            .filter(|entry| matches!(entry.body, EntryBody::Message { .. }))
            .count() as i64;
        meta.updated_at = at;
        Ok(())
    }

    async fn load_entries(&self, thread_id: &str) -> Result<Vec<ThreadEntry>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .threads
            .get(thread_id)
            .map(|(_, entries)| entries.clone())
            .unwrap_or_default())
    }

    async fn append_spans(&self, thread_id: &str, spans: &[Span]) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.threads.contains_key(thread_id) {
            return Err(StoreError(format!("no thread {thread_id}")));
        }
        inner
            .spans
            .entry(thread_id.to_string())
            .or_default()
            .extend(spans.iter().cloned());
        Ok(())
    }

    async fn load_spans(&self, thread_id: &str) -> Result<Vec<Span>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .spans
            .get(thread_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn set_active_agent(&self, thread_id: &str, agent: &str) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let (meta, _) = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| StoreError(format!("no thread {thread_id}")))?;
        meta.active = agent.to_string();
        meta.updated_at = now_ms();
        Ok(())
    }

    async fn set_attachments(
        &self,
        thread_id: &str,
        attachments: &[Attachment],
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let (meta, _) = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| StoreError(format!("no thread {thread_id}")))?;
        meta.attachments = attachments.to_vec();
        meta.updated_at = now_ms();
        Ok(())
    }

    async fn record_tool_shape(
        &self,
        tool: &str,
        schema: &Value,
        example: &Value,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.shapes.entry(tool.to_string()).or_insert(ToolShape {
            tool: tool.to_string(),
            schema: schema.clone(),
            example: example.clone(),
            seen_count: 0,
        });
        entry.schema = schema.clone();
        entry.example = example.clone();
        entry.seen_count += 1;
        Ok(())
    }

    async fn tool_shapes(&self) -> Result<Vec<ToolShape>, StoreError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .shapes
            .values()
            .cloned()
            .collect())
    }

    async fn create_session(&self, new: NewSession) -> Result<SessionMeta, StoreError> {
        let meta = new.meta(ops::new_session_id(), self.now());
        self.lock().sessions.insert(
            meta.id.clone(),
            Held {
                meta: meta.clone(),
                entries: Vec::new(),
                spans: Vec::new(),
            },
        );
        Ok(meta)
    }

    async fn get_session(&self, id: &str) -> Result<Option<SessionMeta>, StoreError> {
        Ok(self.lock().sessions.get(id).map(|held| held.meta.clone()))
    }

    async fn list_sessions(
        &self,
        filter: &SessionFilter,
        page: Page,
    ) -> Result<SessionPage, StoreError> {
        let sessions = self
            .lock()
            .sessions
            .values()
            .filter(|held| filter.matches(&held.meta))
            .map(|held| held.meta.clone())
            .collect();
        Ok(page.of(sessions))
    }

    async fn delete_session(&self, id: &str) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        inner.runs.retain(|_, run| run.session != id);
        Ok(inner.sessions.remove(id).is_some())
    }

    async fn set_session_agent(&self, lease: &Lease, agent: &str) -> Result<(), FenceError> {
        let now = self.now();
        let mut inner = self.lock();
        let (held, _) = inner.held(lease)?;
        held.meta.active_agent = agent.to_string();
        held.meta.updated_at = held.meta.updated_at.max(now);
        Ok(())
    }

    async fn set_session_attachments(
        &self,
        id: &str,
        attachments: &[Attachment],
    ) -> Result<(), StoreError> {
        let now = self.now();
        let mut inner = self.lock();
        let held = inner.sessions.get_mut(id).ok_or_else(|| no_session(id))?;
        held.meta.attachments = attachments.to_vec();
        held.meta.updated_at = held.meta.updated_at.max(now);
        Ok(())
    }

    async fn start_run(&self, start: StartRun) -> Result<Lease, StartError> {
        let now = self.now();
        let mut inner = self.lock();
        if let Some(existing) = inner.runs.get(&start.run_id) {
            return Err(StartError::Duplicate {
                run: existing.id.clone(),
                session: existing.session.clone(),
            });
        }
        let held = inner
            .sessions
            .get_mut(&start.session)
            .ok_or_else(|| no_session(&start.session))?;
        let last = held.last_seq();
        let started = ops::start(&mut held.meta, last, &start, now)?;
        held.entries.push(started.entry);
        inner.runs.insert(started.run.id.clone(), started.run);
        Ok(started.lease)
    }

    async fn append(&self, lease: &Lease, entries: &[Entry]) -> Result<(), FenceError> {
        let mut inner = self.lock();
        let (held, _) = inner.held(lease)?;
        check_batch(lease, held.last_seq(), entries)?;
        held.entries.extend(entries.iter().cloned());
        Ok(())
    }

    async fn renew(&self, lease: &Lease) -> Result<Renewal, FenceError> {
        let now = self.now();
        let mut inner = self.lock();
        let (held, run) = inner.held(lease)?;
        ops::renew(&mut held.meta, run, lease, now)
    }

    async fn end_run(&self, lease: &Lease, ended: RunEnded) -> Result<(), FenceError> {
        let now = self.now();
        let mut inner = self.lock();
        let (held, run) = inner.held(lease)?;
        let messages = ops::messages_of(&held.entries, &run.id);
        let last = held.last_seq();
        let entry = ops::finish(&mut held.meta, run, last, messages, ended, now);
        held.entries.push(entry);
        Ok(())
    }

    async fn request_cancel(&self, run: &str, by: &str) -> Result<CancelOutcome, StoreError> {
        let now = self.now();
        let mut inner = self.lock();
        let Inner { sessions, runs, .. } = &mut *inner;
        let meta = runs.get_mut(run).ok_or_else(|| no_run(run))?;
        let held = sessions
            .get_mut(&meta.session)
            .ok_or_else(|| no_session(&meta.session))?;
        let messages = ops::messages_of(&held.entries, run);
        let last = held.last_seq();
        let cancel = ops::cancel(&mut held.meta, meta, last, messages, by, now);
        let outcome = cancel.outcome();
        if let ops::Cancel::End(entries) = cancel {
            held.entries.extend(entries);
        }
        Ok(outcome)
    }

    async fn reap_expired(&self, limit: usize) -> Result<Vec<Reaped>, StoreError> {
        let now = self.now();
        let mut inner = self.lock();
        let Inner { sessions, runs, .. } = &mut *inner;
        let mut reaped = Vec::new();
        for held in sessions.values_mut() {
            if reaped.len() >= limit {
                break;
            }
            let Some(run) = held
                .meta
                .active_run
                .as_ref()
                .and_then(|id| runs.get_mut(id))
            else {
                continue;
            };
            let messages = ops::messages_of(&held.entries, &run.id);
            let last = held.last_seq();
            if let Some((entry, done)) = ops::reap(&mut held.meta, run, last, messages, now) {
                held.entries.push(entry);
                reaped.push(done);
            }
        }
        Ok(reaped)
    }

    async fn get_run(&self, id: &str) -> Result<Option<RunMeta>, StoreError> {
        Ok(self.lock().runs.get(id).cloned())
    }

    async fn session_entries(
        &self,
        session: &str,
        after: Option<u64>,
    ) -> Result<Vec<Entry>, StoreError> {
        let after = after.unwrap_or(0);
        Ok(self
            .lock()
            .sessions
            .get(session)
            .map(|held| {
                held.entries
                    .iter()
                    .filter(|entry| entry.seq > after)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn append_session_spans(&self, session: &str, spans: &[Span]) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let held = inner
            .sessions
            .get_mut(session)
            .ok_or_else(|| no_session(session))?;
        held.spans.extend(spans.iter().cloned());
        Ok(())
    }

    async fn session_spans(&self, session: &str) -> Result<Vec<Span>, StoreError> {
        Ok(self
            .lock()
            .sessions
            .get(session)
            .map(|held| held.spans.clone())
            .unwrap_or_default())
    }
}
