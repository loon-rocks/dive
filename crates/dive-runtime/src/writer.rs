use crate::cancel::CancelHandle;
use dive_core::store::{
    EntryBody, EntryHook, FenceError, Lease, NewEntry, RunEnded, StartError, StartRun, Store,
    LEASE_RENEW_EVERY, LEASE_SELF_STOP_AFTER, RUN_AUTHOR,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub const BATCH_WINDOW: Duration = Duration::from_millis(15);

pub const BATCH_MAX: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Batching {
    pub window: Duration,
    pub max: usize,
}

impl Batching {
    pub const IMMEDIATE: Batching = Batching {
        window: Duration::ZERO,
        max: BATCH_MAX,
    };

    pub const POOLED: Batching = Batching {
        window: BATCH_WINDOW,
        max: BATCH_MAX,
    };
}

pub fn holder() -> &'static str {
    static HOLDER: OnceLock<String> = OnceLock::new();
    HOLDER.get_or_init(|| {
        let host = crate::identity::hostname().unwrap_or_else(|| "localhost".to_string());
        let tag = uuid::Uuid::new_v4().simple().to_string();
        format!("{host}:{}:{}", std::process::id(), &tag[..8])
    })
}

enum Op {
    Entry(Box<dive_core::store::Entry>),
    Agent(String),
    Finish(RunEnded, oneshot::Sender<Result<(), FenceError>>),
}

struct Shared {
    run: String,
    stopped: AtomicBool,
    next: Mutex<u64>,
    tx: mpsc::UnboundedSender<Op>,
    cancel: Option<CancelHandle>,
    saw_cancel: AtomicBool,
}

impl Shared {
    fn push(&self, entry: NewEntry) -> Option<u64> {
        if self.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let seq = *next;
        let at = chrono::Utc::now().timestamp_millis();
        self.tx
            .send(Op::Entry(Box::new(entry.into_entry(seq, &self.run, at))))
            .ok()?;
        *next += 1;
        Some(seq)
    }

    fn stop(&self, why: &str) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            tracing::warn!(run = %self.run, "stopping the run: {why}");
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
        }
    }

    fn cancel_seen(&self, by: Option<String>) {
        if self.saw_cancel.swap(true, Ordering::SeqCst) {
            return;
        }
        self.push(NewEntry::new(
            RUN_AUTHOR,
            EntryBody::CancelRequested {
                by: by.unwrap_or_default(),
            },
        ));
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }
}

pub struct RunWriter {
    shared: Arc<Shared>,
    session: String,
    renewer: JoinHandle<()>,
}

impl RunWriter {
    pub async fn start(
        store: Arc<dyn Store>,
        start: StartRun,
        cancel: Option<CancelHandle>,
    ) -> Result<Self, StartError> {
        Self::start_with(store, start, cancel, Batching::IMMEDIATE).await
    }

    pub async fn start_with(
        store: Arc<dyn Store>,
        start: StartRun,
        cancel: Option<CancelHandle>,
        batching: Batching,
    ) -> Result<Self, StartError> {
        let lease = store.start_run(start).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            run: lease.run.clone(),
            stopped: AtomicBool::new(false),
            next: Mutex::new(lease.next_seq),
            tx,
            cancel,
            saw_cancel: AtomicBool::new(false),
        });
        tokio::spawn(flush(
            Arc::clone(&store),
            lease.clone(),
            Arc::clone(&shared),
            rx,
            batching,
        ));
        let renewer = tokio::spawn(renew(store, lease.clone(), Arc::clone(&shared)));
        Ok(Self {
            shared,
            session: lease.session,
            renewer,
        })
    }

    pub fn run_id(&self) -> &str {
        &self.shared.run
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn stopped(&self) -> bool {
        self.shared.stopped.load(Ordering::SeqCst)
    }

    pub fn cancel_requested(&self) -> bool {
        self.shared.saw_cancel.load(Ordering::SeqCst)
    }

    pub fn push(&self, entry: NewEntry) -> Option<u64> {
        self.shared.push(entry)
    }

    pub fn hook(&self) -> EntryHook {
        let shared = Arc::clone(&self.shared);
        Arc::new(move |entry| shared.push(entry))
    }

    pub fn set_agent(&self, agent: &str) {
        if !self.stopped() {
            let _ = self.shared.tx.send(Op::Agent(agent.to_string()));
        }
    }

    pub async fn finish(self, ended: RunEnded) -> Result<(), FenceError> {
        self.renewer.abort();
        if self.stopped() {
            return Err(FenceError::LeaseLost);
        }
        let (reply, done) = oneshot::channel();
        self.shared
            .tx
            .send(Op::Finish(ended, reply))
            .map_err(|_| FenceError::LeaseLost)?;
        done.await.unwrap_or(Err(FenceError::LeaseLost))
    }
}

impl Drop for RunWriter {
    fn drop(&mut self) {
        self.renewer.abort();
    }
}

async fn flush(
    store: Arc<dyn Store>,
    lease: Lease,
    shared: Arc<Shared>,
    mut rx: mpsc::UnboundedReceiver<Op>,
    batching: Batching,
) {
    let mut pending: Option<Op> = None;
    loop {
        let op = match pending.take() {
            Some(op) => op,
            None => match rx.recv().await {
                Some(op) => op,
                None => return,
            },
        };
        match op {
            Op::Entry(entry) => {
                let mut batch = vec![*entry];
                if !batching.window.is_zero() {
                    tokio::time::sleep(batching.window).await;
                }
                while batch.len() < batching.max {
                    match rx.try_recv() {
                        Ok(Op::Entry(entry)) => batch.push(*entry),
                        Ok(other) => {
                            pending = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if shared.stopped.load(Ordering::SeqCst) {
                    continue;
                }
                match store.append(&lease, &batch).await {
                    Ok(()) => {}
                    Err(FenceError::LeaseLost) => shared.stop("its lease was lost"),
                    Err(FenceError::Store(e)) => {
                        shared.stop(&format!("an append failed: {e}"));
                    }
                }
            }
            Op::Agent(agent) => {
                if shared.stopped.load(Ordering::SeqCst) {
                    continue;
                }
                match store.set_session_agent(&lease, &agent).await {
                    Ok(()) => {}
                    Err(FenceError::LeaseLost) => shared.stop("its lease was lost"),
                    Err(FenceError::Store(e)) => {
                        tracing::warn!(error = %e, "couldn't record the active agent");
                    }
                }
            }
            Op::Finish(ended, reply) => {
                let result = if shared.stopped.load(Ordering::SeqCst) {
                    Err(FenceError::LeaseLost)
                } else {
                    store.end_run(&lease, ended).await
                };
                shared.stopped.store(true, Ordering::SeqCst);
                let _ = reply.send(result);
                return;
            }
        }
    }
}

async fn renew(store: Arc<dyn Store>, lease: Lease, shared: Arc<Shared>) {
    let mut last_ok = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(LEASE_RENEW_EVERY).await;
        if shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        let deadline = last_ok + LEASE_SELF_STOP_AFTER;
        match tokio::time::timeout_at(deadline, store.renew(&lease)).await {
            Ok(Ok(renewal)) => {
                last_ok = tokio::time::Instant::now();
                if renewal.cancel_requested {
                    shared.cancel_seen(renewal.cancel_by);
                }
            }
            Ok(Err(FenceError::LeaseLost)) => {
                shared.stop("its lease was lost");
                return;
            }
            Ok(Err(FenceError::Store(e))) => {
                tracing::warn!(error = %e, "couldn't renew the run's lease");
                if tokio::time::Instant::now() >= deadline {
                    shared.stop("no renewal succeeded in time");
                    return;
                }
            }
            Err(_) => {
                shared.stop("no renewal succeeded in time");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dive_core::run::Identity;
    use dive_core::store::*;
    use dive_llm::types::ChatMessage;
    use dive_store::MemoryStore;
    use serde_json::{json, Value};

    fn identity() -> Identity {
        Identity {
            user: "ada".into(),
            tenant: "local".into(),
            deployment: "cli".into(),
            environment: "local".into(),
        }
    }

    pub(crate) fn starting(run: &str, session: &str) -> StartRun {
        StartRun {
            run_id: run.to_string(),
            session: session.to_string(),
            kind: SessionKind::Chat,
            holder: holder().to_string(),
            token_expires_at: None,
            started: RunStarted {
                input: json!("hi"),
                identity: identity(),
                trace_id: None,
                plan: None,
                synthetic: false,
            },
        }
    }

    async fn session(store: &dyn Store) -> String {
        store
            .create_session(NewSession::chat("t", "chat", &identity()))
            .await
            .unwrap()
            .id
    }

    fn kind(entry: &Entry) -> String {
        serde_json::to_value(entry).unwrap()["kind"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_writer_numbers_entries_in_arrival_order_and_writes_each_at_once() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let id = session(store.as_ref()).await;
        let writer = RunWriter::start(store.clone(), starting("r1", &id), None)
            .await
            .unwrap();
        let hook = writer.hook();
        assert_eq!(
            hook(NewEntry::message("user", ChatMessage::user("one"))),
            Some(2)
        );
        assert_eq!(
            writer.push(NewEntry::message("chat", ChatMessage::user("two"))),
            Some(3)
        );
        settle().await;
        let written = store.session_entries(&id, None).await.unwrap();
        assert_eq!(written.len(), 3);
        writer.finish(RunEnded::completed(None)).await.unwrap();
        let written = store.session_entries(&id, None).await.unwrap();
        let summary: Vec<(u64, String)> = written
            .iter()
            .map(|entry| (entry.seq, kind(entry)))
            .collect();
        assert_eq!(
            summary,
            [
                (1, "run_started".to_string()),
                (2, "message".to_string()),
                (3, "message".to_string()),
                (4, "run_ended".to_string())
            ]
        );
    }

    struct Unreachable {
        inner: MemoryStore,
        cut: AtomicBool,
    }

    #[async_trait]
    impl Store for Unreachable {
        async fn create_thread(
            &self,
            title: &str,
            owner: &str,
            project: Option<&str>,
        ) -> Result<ThreadMeta, StoreError> {
            self.inner.create_thread(title, owner, project).await
        }
        async fn get_thread(&self, id: &str) -> Result<Option<ThreadMeta>, StoreError> {
            self.inner.get_thread(id).await
        }
        async fn latest_thread(&self) -> Result<Option<ThreadMeta>, StoreError> {
            self.inner.latest_thread().await
        }
        async fn list_threads(&self) -> Result<Vec<ThreadMeta>, StoreError> {
            self.inner.list_threads().await
        }
        async fn delete_thread(&self, id: &str) -> Result<bool, StoreError> {
            self.inner.delete_thread(id).await
        }
        async fn append_entries(&self, id: &str, entries: &[NewEntry]) -> Result<(), StoreError> {
            self.inner.append_entries(id, entries).await
        }
        async fn load_entries(&self, id: &str) -> Result<Vec<ThreadEntry>, StoreError> {
            self.inner.load_entries(id).await
        }
        async fn append_spans(&self, id: &str, spans: &[Span]) -> Result<(), StoreError> {
            self.inner.append_spans(id, spans).await
        }
        async fn load_spans(&self, id: &str) -> Result<Vec<Span>, StoreError> {
            self.inner.load_spans(id).await
        }
        async fn set_active_agent(&self, id: &str, agent: &str) -> Result<(), StoreError> {
            self.inner.set_active_agent(id, agent).await
        }
        async fn set_attachments(&self, id: &str, a: &[Attachment]) -> Result<(), StoreError> {
            self.inner.set_attachments(id, a).await
        }
        async fn record_tool_shape(
            &self,
            tool: &str,
            schema: &Value,
            example: &Value,
        ) -> Result<(), StoreError> {
            self.inner.record_tool_shape(tool, schema, example).await
        }
        async fn tool_shapes(&self) -> Result<Vec<ToolShape>, StoreError> {
            self.inner.tool_shapes().await
        }
        async fn create_session(&self, new: NewSession) -> Result<SessionMeta, StoreError> {
            self.inner.create_session(new).await
        }
        async fn get_session(&self, id: &str) -> Result<Option<SessionMeta>, StoreError> {
            self.inner.get_session(id).await
        }
        async fn list_sessions(
            &self,
            filter: &SessionFilter,
            page: Page,
        ) -> Result<SessionPage, StoreError> {
            self.inner.list_sessions(filter, page).await
        }
        async fn delete_session(&self, id: &str) -> Result<bool, StoreError> {
            self.inner.delete_session(id).await
        }
        async fn set_session_agent(&self, lease: &Lease, agent: &str) -> Result<(), FenceError> {
            self.inner.set_session_agent(lease, agent).await
        }
        async fn set_session_attachments(
            &self,
            id: &str,
            attachments: &[Attachment],
        ) -> Result<(), StoreError> {
            self.inner.set_session_attachments(id, attachments).await
        }
        async fn start_run(&self, start: StartRun) -> Result<Lease, StartError> {
            self.inner.start_run(start).await
        }
        async fn append(&self, lease: &Lease, entries: &[Entry]) -> Result<(), FenceError> {
            if self.cut.load(Ordering::SeqCst) {
                return std::future::pending().await;
            }
            self.inner.append(lease, entries).await
        }
        async fn renew(&self, lease: &Lease) -> Result<Renewal, FenceError> {
            if self.cut.load(Ordering::SeqCst) {
                return std::future::pending().await;
            }
            self.inner.renew(lease).await
        }
        async fn end_run(&self, lease: &Lease, ended: RunEnded) -> Result<(), FenceError> {
            self.inner.end_run(lease, ended).await
        }
        async fn request_cancel(&self, run: &str, by: &str) -> Result<CancelOutcome, StoreError> {
            self.inner.request_cancel(run, by).await
        }
        async fn reap_expired(&self, limit: usize) -> Result<Vec<Reaped>, StoreError> {
            self.inner.reap_expired(limit).await
        }
        async fn get_run(&self, id: &str) -> Result<Option<RunMeta>, StoreError> {
            self.inner.get_run(id).await
        }
        async fn session_entries(
            &self,
            session: &str,
            after: Option<u64>,
        ) -> Result<Vec<Entry>, StoreError> {
            self.inner.session_entries(session, after).await
        }
        async fn append_session_spans(
            &self,
            session: &str,
            spans: &[Span],
        ) -> Result<(), StoreError> {
            self.inner.append_session_spans(session, spans).await
        }
        async fn session_spans(&self, session: &str) -> Result<Vec<Span>, StoreError> {
            self.inner.session_spans(session).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_writer_cut_off_from_the_store_stops_its_run_within_25_seconds_and_writes_nothing_more(
    ) {
        let store = Arc::new(Unreachable {
            inner: MemoryStore::new(),
            cut: AtomicBool::new(false),
        });
        let id = session(store.as_ref()).await;
        let cancel = CancelHandle::new();
        let writer = RunWriter::start(store.clone(), starting("r1", &id), Some(cancel.clone()))
            .await
            .unwrap();
        writer.push(NewEntry::message("user", ChatMessage::user("before")));
        settle().await;
        let started = tokio::time::Instant::now();
        store.cut.store(true, Ordering::SeqCst);
        writer.push(NewEntry::message("chat", ChatMessage::user("stuck")));
        while !cancel.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert!(started.elapsed() <= Duration::from_secs(26));
        }
        assert!(started.elapsed() <= LEASE_SELF_STOP_AFTER + Duration::from_secs(1));
        assert!(writer.stopped());
        store.cut.store(false, Ordering::SeqCst);
        assert_eq!(
            writer.push(NewEntry::message("chat", ChatMessage::user("after"))),
            None
        );
        settle().await;
        assert!(matches!(
            writer.finish(RunEnded::completed(None)).await,
            Err(FenceError::LeaseLost)
        ));
        let written = store.session_entries(&id, None).await.unwrap();
        assert_eq!(written.len(), 2);
    }

    #[tokio::test]
    async fn an_append_that_loses_the_lease_stops_the_run() {
        let now = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
        let clock_now = Arc::clone(&now);
        let store: Arc<dyn Store> = Arc::new(MemoryStore::with_clock(Arc::new(move || {
            clock_now.load(Ordering::SeqCst)
        })));
        let id = session(store.as_ref()).await;
        let cancel = CancelHandle::new();
        let writer = RunWriter::start(store.clone(), starting("r1", &id), Some(cancel.clone()))
            .await
            .unwrap();
        now.fetch_add(LEASE_EXPIRES_AFTER.as_millis() as i64 + 1, Ordering::SeqCst);
        assert_eq!(store.reap_expired(10).await.unwrap().len(), 1);
        writer.push(NewEntry::message("user", ChatMessage::user("late")));
        for _ in 0..100 {
            if cancel.is_cancelled() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(cancel.is_cancelled());
        assert!(writer.stopped());
        let kinds: Vec<String> = store
            .session_entries(&id, None)
            .await
            .unwrap()
            .iter()
            .map(kind)
            .collect();
        assert_eq!(kinds, ["run_started", "run_ended"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stored_cancel_reaches_the_holder_on_its_next_renewal() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let id = session(store.as_ref()).await;
        let cancel = CancelHandle::new();
        let writer = RunWriter::start(store.clone(), starting("r1", &id), Some(cancel.clone()))
            .await
            .unwrap();
        assert_eq!(
            store.request_cancel("r1", "grace").await.unwrap(),
            CancelOutcome::Signalled
        );
        assert!(!cancel.is_cancelled());
        tokio::time::sleep(LEASE_RENEW_EVERY + Duration::from_millis(10)).await;
        settle().await;
        assert!(cancel.is_cancelled());
        assert!(writer.cancel_requested());
        writer
            .finish(RunEnded::new(RunStatus::Cancelled))
            .await
            .unwrap();
        let entries = store.session_entries(&id, None).await.unwrap();
        let kinds: Vec<String> = entries.iter().map(kind).collect();
        assert_eq!(kinds, ["run_started", "cancel_requested", "run_ended"]);
        assert!(matches!(
            &entries[1].body,
            EntryBody::CancelRequested { by } if by == "grace"
        ));
    }
}
