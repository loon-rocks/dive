//! File-backed `Store`: plain JSON/JSONL files under the data directory.
//!
//! Layout:
//!
//! ```text
//! <root>/FORMAT                       store version
//! <root>/sessions/<id>/session.json   session meta, claim and lease (atomic rename)
//! <root>/sessions/<id>/entries.jsonl  one Entry per line (O_APPEND)
//! <root>/sessions/<id>/spans.jsonl    one finished trace Span per line (O_APPEND)
//! <root>/sessions/<id>/.lock          advisory lock every write takes
//! <root>/runs/<run_id>.json           run row, created only if absent
//! <root>/shapes/<tool>.json           one file per tool shape (atomic rename)
//! <root>/files/<hash>.<ext>           stored files, see `ObjectFiles`
//! ```
//!
//! Concurrency model: whole files are written to a temp file and renamed
//! into place, so readers never observe partial writes. Appends go through
//! `O_APPEND` under an exclusive flock on the session's `.lock` file, after
//! checking the lease generation in `session.json` (the fence). Shape writes
//! are last-writer-wins; `seen_count` is advisory and may lose increments
//! under contention. Advisory locks assume a local filesystem (flock over
//! NFS is unreliable).

use crate::layout::{encode_name, read_session, write_atomic, Layout};
use crate::session::Clock;
use dive_core::run::Identity;
use dive_core::store::{
    Attachment, CancelOutcome, Entry, FenceError, Lease, NewSession, Page, Reaped, Renewal,
    RunEnded, RunMeta, SessionFilter, SessionMeta, SessionPage, Span, StartError, StartRun, Store,
    StoreError, ToolShape,
};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const STORE_FORMAT: u32 = 5;

pub const STORE_FORMAT_OLDEST: u32 = 1;

const FORMAT_MARKER: &str = "FORMAT";

const MIGRATE_LOCK: &str = ".migrate.lock";

pub struct FileStore {
    shapes_dir: PathBuf,
    layout: Layout,
    clock: Clock,
}

#[derive(Serialize, Deserialize)]
struct ShapeFile {
    version: u32,
    tool: String,
    schema: Value,
    example: Value,
    seen_count: i64,
    updated_at: i64,
}

impl FileStore {
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        Self::open_as(root, &crate::migrate::unknown_identity())
    }

    pub fn open_as(root: &Path, identity: &Identity) -> Result<Self, StoreError> {
        Self::open_with(root, identity, crate::session::wall_clock())
    }

    pub fn open_with_clock(root: &Path, clock: Clock) -> Result<Self, StoreError> {
        Self::open_with(root, &crate::migrate::unknown_identity(), clock)
    }

    pub fn open_with(root: &Path, identity: &Identity, clock: Clock) -> Result<Self, StoreError> {
        let shapes_dir = root.join("shapes");
        std::fs::create_dir_all(&shapes_dir)
            .map_err(|e| StoreError(format!("creating {}: {e}", shapes_dir.display())))?;
        check_format_marker(root, identity)?;
        let layout = Layout::new(root);
        layout.create_dirs()?;
        Ok(Self {
            shapes_dir,
            layout,
            clock,
        })
    }

    pub fn reap_now(&self) -> Result<Vec<Reaped>, StoreError> {
        self.layout.reap_expired(usize::MAX, self.now())
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    async fn on_layout<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Layout, i64) -> Result<T, StoreError> + Send + 'static,
    {
        let layout = self.layout.clone();
        let now = self.now();
        blocking(move || work(&layout, now)).await
    }

    async fn fenced<T, F>(&self, work: F) -> Result<T, FenceError>
    where
        T: Send + 'static,
        F: FnOnce(&Layout, i64) -> Result<T, FenceError> + Send + 'static,
    {
        let layout = self.layout.clone();
        let now = self.now();
        tokio::task::spawn_blocking(move || work(&layout, now))
            .await
            .map_err(|e| FenceError::Store(StoreError(format!("store task failed: {e}"))))?
    }
}

async fn blocking<T, F>(work: F) -> Result<T, StoreError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, StoreError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| StoreError(format!("store task failed: {e}")))?
}

pub fn marker_format(root: &Path) -> Result<Option<u32>, StoreError> {
    let path = root.join(FORMAT_MARKER);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError(format!("reading {}: {e}", path.display()))),
    };
    raw.trim()
        .parse::<u32>()
        .ok()
        .filter(|n| *n >= 1)
        .map(Some)
        .ok_or_else(|| {
            StoreError(format!(
                "{} does not hold a store version number (got {:?})",
                path.display(),
                raw.trim()
            ))
        })
}

fn check_format_marker(root: &Path, identity: &Identity) -> Result<(), StoreError> {
    match marker_format(root)? {
        Some(found) => match dive_config::window_problem(
            "store",
            &format_args!("data directory {}", root.display()),
            found,
            STORE_FORMAT_OLDEST,
            STORE_FORMAT,
        ) {
            Some(problem) => Err(StoreError(problem)),
            None if found < STORE_FORMAT => migrate(root, identity),
            None => Ok(()),
        },
        None => match link_new(root, format!("{}\n", unmarked_format(root)?).as_bytes()) {
            Ok(()) => check_format_marker(root, identity),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                check_format_marker(root, identity)
            }
            Err(e) => Err(StoreError(format!(
                "writing {}: {e}",
                root.join(FORMAT_MARKER).display()
            ))),
        },
    }
}

fn unmarked_format(root: &Path) -> Result<u32, StoreError> {
    let threads = root.join("threads");
    match std::fs::read_dir(&threads) {
        Ok(mut dirs) => Ok(if dirs.next().is_some() {
            1
        } else {
            STORE_FORMAT
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(STORE_FORMAT),
        Err(e) => Err(StoreError(format!("reading {}: {e}", threads.display()))),
    }
}

fn migrate(root: &Path, identity: &Identity) -> Result<(), StoreError> {
    let marker = root.join(FORMAT_MARKER);
    let lock_path = root.join(MIGRATE_LOCK);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| StoreError(format!("opening {}: {e}", lock_path.display())))?;
    lock.lock_exclusive()
        .map_err(|e| StoreError(format!("locking {}: {e}", lock_path.display())))?;
    let Some(mut found) = marker_format(root)? else {
        return Err(StoreError(format!("{} disappeared", marker.display())));
    };
    while found < STORE_FORMAT {
        match found {
            1 => crate::migrate::threads_v1(&root.join("threads"))?,
            2 => {}
            3 => move_files_v3(root)?,
            4 => crate::migrate::sessions_v4(root, identity)?,
            other => {
                return Err(StoreError(format!(
                    "no migration from store version {other}"
                )))
            }
        }
        found += 1;
        write_atomic(&marker, format!("{found}\n").as_bytes())?;
    }
    Ok(())
}

fn move_files_v3(root: &Path) -> Result<(), StoreError> {
    move_dir(&root.join("blobs"), &crate::files_dir(root))?;
    move_dir(&root.join("cache").join("blobs"), &crate::cache_dir(root))
}

fn move_dir(old: &Path, new: &Path) -> Result<(), StoreError> {
    let failed = |what: &str, path: &Path, e: std::io::Error| {
        StoreError(format!("{what} {}: {e}", path.display()))
    };
    if !old.is_dir() {
        return Ok(());
    }
    if !new.exists() {
        return std::fs::rename(old, new).map_err(|e| failed("moving", old, e));
    }
    let entries = std::fs::read_dir(old).map_err(|e| failed("reading", old, e))?;
    for entry in entries.flatten() {
        let from = entry.path();
        let to = new.join(entry.file_name());
        if to.exists() {
            std::fs::remove_file(&from).map_err(|e| failed("removing", &from, e))?;
        } else {
            std::fs::rename(&from, &to).map_err(|e| failed("moving", &from, e))?;
        }
    }
    std::fs::remove_dir(old).map_err(|e| failed("removing", old, e))
}

fn link_new(root: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(root)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    std::fs::hard_link(tmp.path(), root.join(FORMAT_MARKER))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[async_trait::async_trait]
impl Store for FileStore {
    async fn create_session(&self, new: NewSession) -> Result<SessionMeta, StoreError> {
        self.on_layout(move |layout, now| layout.create_session(new, now))
            .await
    }

    async fn get_session(&self, id: &str) -> Result<Option<SessionMeta>, StoreError> {
        let dir = self.layout.session_dir(id);
        blocking(move || read_session(&dir)).await
    }

    async fn list_sessions(
        &self,
        filter: &SessionFilter,
        page: Page,
    ) -> Result<SessionPage, StoreError> {
        let filter = filter.clone();
        self.on_layout(move |layout, _| {
            let sessions = layout
                .scan()?
                .into_iter()
                .filter(|meta| filter.matches(meta))
                .collect();
            Ok(page.of(sessions))
        })
        .await
    }

    async fn delete_session(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_string();
        self.on_layout(move |layout, _| layout.delete_session(&id))
            .await
    }

    async fn set_active_agent(&self, lease: &Lease, agent: &str) -> Result<(), FenceError> {
        let lease = lease.clone();
        let agent = agent.to_string();
        self.fenced(move |layout, now| layout.set_agent(&lease, &agent, now))
            .await
    }

    async fn set_attachments(
        &self,
        id: &str,
        attachments: &[Attachment],
    ) -> Result<(), StoreError> {
        let id = id.to_string();
        let attachments = attachments.to_vec();
        self.on_layout(move |layout, now| layout.set_attachments(&id, &attachments, now))
            .await
    }

    async fn start_run(&self, start: StartRun) -> Result<Lease, StartError> {
        let layout = self.layout.clone();
        let now = self.now();
        tokio::task::spawn_blocking(move || layout.start_run(&start, now))
            .await
            .map_err(|e| StartError::Store(StoreError(format!("store task failed: {e}"))))?
    }

    async fn append(&self, lease: &Lease, entries: &[Entry]) -> Result<(), FenceError> {
        if entries.is_empty() {
            return Ok(());
        }
        let lease = lease.clone();
        let entries = entries.to_vec();
        self.fenced(move |layout, _| layout.append(&lease, &entries))
            .await
    }

    async fn renew(&self, lease: &Lease) -> Result<Renewal, FenceError> {
        let lease = lease.clone();
        self.fenced(move |layout, now| layout.renew(&lease, now))
            .await
    }

    async fn end_run(&self, lease: &Lease, ended: RunEnded) -> Result<(), FenceError> {
        let lease = lease.clone();
        self.fenced(move |layout, now| layout.end_run(&lease, ended, now))
            .await
    }

    async fn request_cancel(&self, run: &str, by: &str) -> Result<CancelOutcome, StoreError> {
        let run = run.to_string();
        let by = by.to_string();
        self.on_layout(move |layout, now| layout.request_cancel(&run, &by, now))
            .await
    }

    async fn reap_expired(&self, limit: usize) -> Result<Vec<Reaped>, StoreError> {
        self.on_layout(move |layout, now| layout.reap_expired(limit, now))
            .await
    }

    async fn get_run(&self, id: &str) -> Result<Option<RunMeta>, StoreError> {
        let id = id.to_string();
        self.on_layout(move |layout, _| layout.read_run(&id)).await
    }

    async fn load_entries(
        &self,
        session: &str,
        after: Option<u64>,
    ) -> Result<Vec<Entry>, StoreError> {
        let session = session.to_string();
        self.on_layout(move |layout, _| layout.entries(&session, after))
            .await
    }

    async fn append_spans(&self, session: &str, spans: &[Span]) -> Result<(), StoreError> {
        if spans.is_empty() {
            return Ok(());
        }
        let session = session.to_string();
        let spans = spans.to_vec();
        self.on_layout(move |layout, _| layout.append_spans(&session, &spans))
            .await
    }

    async fn load_spans(&self, session: &str) -> Result<Vec<Span>, StoreError> {
        let session = session.to_string();
        self.on_layout(move |layout, _| layout.spans(&session))
            .await
    }

    async fn record_tool_shape(
        &self,
        tool: &str,
        schema: &Value,
        example: &Value,
    ) -> Result<(), StoreError> {
        let tool = tool.to_string();
        let schema = schema.clone();
        let example = example.clone();
        let path = self.shapes_dir.join(encode_name(&tool, ".json"));
        blocking(move || {
            // Last-writer-wins by design; a corrupt or missing file just
            // starts the count over.
            let seen_count = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<ShapeFile>(&raw).ok())
                .map(|s| s.seen_count)
                .unwrap_or(0);
            let shape = ShapeFile {
                version: STORE_FORMAT,
                tool,
                schema,
                example,
                seen_count: seen_count + 1,
                updated_at: now_ms(),
            };
            let bytes = serde_json::to_vec_pretty(&shape)
                .map_err(|e| StoreError(format!("serializing tool shape: {e}")))?;
            write_atomic(&path, &bytes)
        })
        .await
    }

    async fn tool_shapes(&self) -> Result<Vec<ToolShape>, StoreError> {
        let shapes_dir = self.shapes_dir.clone();
        blocking(move || {
            let entries = match std::fs::read_dir(&shapes_dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(StoreError(format!("reading {}: {e}", shapes_dir.display()))),
            };
            let mut shapes = Vec::new();
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let raw = match std::fs::read_to_string(&path) {
                    Ok(raw) => raw,
                    Err(e) => {
                        tracing::warn!("skipping shape {}: {e}", path.display());
                        continue;
                    }
                };
                match serde_json::from_str::<ShapeFile>(&raw) {
                    Ok(shape) => shapes.push(ToolShape {
                        tool: shape.tool,
                        schema: shape.schema,
                        example: shape.example,
                        seen_count: shape.seen_count,
                    }),
                    Err(e) => tracing::warn!("skipping shape {}: {e}", path.display()),
                }
            }
            shapes.sort_by(|a, b| a.tool.cmp(&b.tool));
            Ok(shapes)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_core::store::{EntryBody, SessionFilter};
    use dive_llm::types::ChatMessage;

    #[test]
    fn open_writes_the_format_marker_once() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(marker_format(dir.path()).unwrap(), None);
        FileStore::open(dir.path()).unwrap();
        assert_eq!(marker_format(dir.path()).unwrap(), Some(STORE_FORMAT));
        let marker = dir.path().join(FORMAT_MARKER);
        let written = std::fs::read_to_string(&marker).unwrap();
        FileStore::open(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), written);
        assert!(!dir.path().join("threads").exists());
    }

    #[test]
    fn concurrent_first_opens_agree_on_one_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || FileStore::open(&root).map(|_| ()))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(marker_format(&root).unwrap(), Some(STORE_FORMAT));
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| ![FORMAT_MARKER, "shapes", "sessions", "runs"].contains(&name.as_str()))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn open_refuses_a_newer_store_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FORMAT_MARKER),
            format!("{}\n", STORE_FORMAT + 1),
        )
        .unwrap();
        let err = FileStore::open(dir.path())
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("is store version {}", STORE_FORMAT + 1)),
            "{err}"
        );
        assert!(
            err.contains(&format!(
                "reads store versions {STORE_FORMAT_OLDEST} to {STORE_FORMAT}"
            )),
            "{err}"
        );
        std::fs::write(dir.path().join(FORMAT_MARKER), "banana\n").unwrap();
        let err = FileStore::open(dir.path())
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not hold a store version number"),
            "{err}"
        );
    }

    fn legacy_thread(root: &Path) -> String {
        let dir = root.join("threads").join("legacy00001");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"version":1,"id":"legacy00001","title":"old","created_at":5,"updated_at":6,"message_count":2}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("messages.jsonl"),
            "{\"kind\":\"user\",\"content\":\"hi\"}\n{\"kind\":\"assistant\",\"content\":\"hello\"}\n",
        )
        .unwrap();
        "legacy00001".to_string()
    }

    async fn authors(store: &FileStore, id: &str) -> Vec<String> {
        store
            .load_entries(id, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| matches!(entry.body, EntryBody::Message { .. }))
            .map(|entry| entry.author)
            .collect()
    }

    #[tokio::test]
    async fn opening_a_version_1_store_converts_its_message_logs_into_a_chat_session() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(FORMAT_MARKER), "1\n").unwrap();
        let id = legacy_thread(root.path());
        let empty = root.path().join("threads").join("empty000001");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(
            empty.join("meta.json"),
            r#"{"version":1,"id":"empty000001","title":"none","created_at":1,"updated_at":1,"message_count":0}"#,
        )
        .unwrap();

        let store = FileStore::open(root.path()).unwrap();
        assert_eq!(marker_format(root.path()).unwrap(), Some(STORE_FORMAT));
        let dir = root.path().join("threads").join(&id);
        assert!(!dir.join("messages.jsonl").exists());
        assert_eq!(authors(&store, &id).await, ["user", "chat"]);
        let meta = store.get_session(&id).await.unwrap().unwrap();
        assert_eq!(
            (meta.agent.as_str(), meta.active_agent.as_str()),
            ("chat", "chat")
        );
        assert_eq!(meta.message_count, 2);
        assert!(store.get_session("empty000001").await.unwrap().is_some());
        let listed = store
            .list_sessions(&SessionFilter::default(), Page::ALL)
            .await
            .unwrap();
        assert_eq!(listed.sessions.len(), 2);
    }

    #[test]
    fn opening_a_version_3_store_moves_its_files_and_their_cache_into_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(FORMAT_MARKER), "3\n").unwrap();
        let blobs = root.path().join("blobs");
        let cached = root.path().join("cache").join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(&cached).unwrap();
        std::fs::write(blobs.join("aaa.png"), "a").unwrap();
        std::fs::write(blobs.join("bbb.png"), "b").unwrap();
        std::fs::write(cached.join("ccc.png"), "c").unwrap();
        let files = crate::files_dir(root.path());
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("bbb.png"), "b").unwrap();

        FileStore::open(root.path()).unwrap();
        assert_eq!(marker_format(root.path()).unwrap(), Some(STORE_FORMAT));
        assert!(!blobs.exists());
        assert!(!cached.exists());
        assert_eq!(std::fs::read_to_string(files.join("aaa.png")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(files.join("bbb.png")).unwrap(), "b");
        let cache = crate::cache_dir(root.path());
        assert_eq!(std::fs::read_to_string(cache.join("ccc.png")).unwrap(), "c");
    }

    #[tokio::test]
    async fn a_store_from_before_the_format_marker_is_migrated_as_version_1() {
        let root = tempfile::tempdir().unwrap();
        let id = legacy_thread(root.path());
        let store = FileStore::open(root.path()).unwrap();
        assert_eq!(marker_format(root.path()).unwrap(), Some(STORE_FORMAT));
        assert_eq!(authors(&store, &id).await, ["user", "chat"]);
    }

    #[tokio::test]
    async fn a_migration_interrupted_after_writing_entries_finishes_once() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(FORMAT_MARKER), "1\n").unwrap();
        let id = legacy_thread(root.path());
        let dir = root.path().join("threads").join(&id);
        let entries = crate::migrate::legacy_entries(&dir, 5).unwrap().unwrap();
        std::fs::write(
            dir.join("entries.jsonl"),
            crate::layout::json_lines(&entries, "thread entry").unwrap(),
        )
        .unwrap();

        let store = FileStore::open(root.path()).unwrap();
        assert!(!dir.join("messages.jsonl").exists());
        assert_eq!(authors(&store, &id).await, ["user", "chat"]);
    }

    #[test]
    fn concurrent_opens_of_a_version_1_store_migrate_it_once() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(FORMAT_MARKER), "1\n").unwrap();
        let id = legacy_thread(root.path());
        let path = root.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || FileStore::open(&path).map(|_| ()))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(marker_format(&path).unwrap(), Some(STORE_FORMAT));
        let entries =
            std::fs::read_to_string(path.join("threads").join(&id).join("entries.jsonl")).unwrap();
        assert_eq!(entries.lines().count(), 2);
        let session =
            std::fs::read_to_string(path.join("sessions").join(&id).join("entries.jsonl")).unwrap();
        assert_eq!(session.lines().count(), 4);
        assert!(session.contains(&serde_json::to_string(&ChatMessage::user("hi")).unwrap()));
    }

    #[test]
    fn tool_filename_encoding() {
        assert_eq!(encode_name("user__git_log", ".json"), "user__git_log.json");
        assert_eq!(encode_name("a/b:c", ".json"), "a%2Fb%3Ac.json");
        assert_eq!(encode_name("café", ".json"), "caf%C3%A9.json");
    }
}
