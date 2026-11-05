use crate::session::{self as ops, Cancel};
use dive_core::store::{
    check_batch, Attachment, CancelOutcome, Entry, FenceError, Lease, NewSession, Reaped, Renewal,
    RunEnded, RunMeta, SessionMeta, Span, StartError, StartRun, StoreError,
};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const SESSIONS_DIR: &str = "sessions";

pub const RUNS_DIR: &str = "runs";

pub const SESSION_FILE: &str = "session.json";

pub const ENTRIES_FILE: &str = "entries.jsonl";

pub const SPANS_FILE: &str = "spans.jsonl";

const LOCK_FILE: &str = ".lock";

pub const SESSION_FILE_VERSION: u32 = 5;

#[derive(Serialize, Deserialize)]
pub struct SessionFile {
    pub version: u32,
    #[serde(flatten)]
    pub meta: SessionMeta,
}

#[derive(Serialize, Deserialize)]
pub struct RunFile {
    pub version: u32,
    #[serde(flatten)]
    pub run: RunMeta,
}

#[derive(Clone)]
pub struct Layout {
    pub sessions: PathBuf,
    pub runs: PathBuf,
}

fn failed(what: &str, path: &Path, e: impl std::fmt::Display) -> StoreError {
    StoreError(format!("{what} {}: {e}", path.display()))
}

pub fn encode_name(name: &str, extension: &str) -> String {
    let mut out = String::with_capacity(name.len() + extension.len());
    for byte in name.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out.push_str(extension);
    out
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let dir = path
        .parent()
        .ok_or_else(|| StoreError(format!("no parent dir for {}", path.display())))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| failed("creating temp file in", dir, e))?;
    tmp.write_all(bytes)
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|e| failed("writing", path, e))?;
    tmp.persist(path)
        .map_err(|e| failed("renaming into", path, e))?;
    Ok(())
}

pub fn create_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no parent directory"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    std::fs::hard_link(tmp.path(), path)
}

pub fn read_jsonl<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Option<Vec<T>>, StoreError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failed("reading", path, e)),
    };
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut items = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str::<T>(line) {
            Ok(item) => items.push(item),
            Err(e) if i == lines.len() - 1 => {
                tracing::warn!("dropping torn final line in {}: {e}", path.display());
            }
            Err(e) => {
                return Err(StoreError(format!(
                    "corrupt line {} in {}: {e}",
                    i + 1,
                    path.display()
                )))
            }
        }
    }
    Ok(Some(items))
}

pub fn json_lines<T: Serialize>(items: &[T], what: &str) -> Result<Vec<u8>, StoreError> {
    let mut buf = Vec::new();
    for item in items {
        serde_json::to_writer(&mut buf, item)
            .map_err(|e| StoreError(format!("serializing {what}: {e}")))?;
        buf.push(b'\n');
    }
    Ok(buf)
}

pub fn append_to(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| failed("opening", path, e))?
        .write_all(bytes)
        .map_err(|e| failed("appending to", path, e))
}

#[derive(Deserialize)]
struct SeqOnly {
    seq: u64,
}

pub fn last_seq(path: &Path) -> Result<u64, StoreError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(failed("opening", path, e)),
    };
    let len = file
        .metadata()
        .map_err(|e| failed("reading", path, e))?
        .len();
    let mut window: u64 = 64 * 1024;
    loop {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start))
            .map_err(|e| failed("reading", path, e))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| failed("reading", path, e))?;
        let text = String::from_utf8_lossy(&buf);
        let mut lines: Vec<&str> = text.lines().collect();
        if start > 0 && !lines.is_empty() {
            lines.remove(0);
        }
        let found = lines
            .iter()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .find_map(|line| serde_json::from_str::<SeqOnly>(line).ok());
        if let Some(found) = found {
            return Ok(found.seq);
        }
        if start == 0 {
            return Ok(0);
        }
        window = window.saturating_mul(4);
    }
}

pub struct SessionLock {
    _file: File,
}

fn open_lock(dir: &Path) -> Result<File, StoreError> {
    let path = dir.join(LOCK_FILE);
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| failed("opening", &path, e))
}

pub fn lock(dir: &Path) -> Result<SessionLock, StoreError> {
    let file = open_lock(dir)?;
    file.lock_exclusive()
        .map_err(|e| failed("locking", &dir.join(LOCK_FILE), e))?;
    Ok(SessionLock { _file: file })
}

pub fn try_lock(dir: &Path) -> Result<Option<SessionLock>, StoreError> {
    let file = open_lock(dir)?;
    match file.try_lock_exclusive() {
        Ok(true) => Ok(Some(SessionLock { _file: file })),
        Ok(false) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(failed("locking", &dir.join(LOCK_FILE), e)),
    }
}

pub fn read_session(dir: &Path) -> Result<Option<SessionMeta>, StoreError> {
    let path = dir.join(SESSION_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failed("reading", &path, e)),
    };
    let file: SessionFile = serde_json::from_str(&raw).map_err(|e| failed("corrupt", &path, e))?;
    Ok(Some(file.meta))
}

pub fn write_session(dir: &Path, meta: &SessionMeta) -> Result<(), StoreError> {
    let file = SessionFile {
        version: SESSION_FILE_VERSION,
        meta: meta.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|e| StoreError(format!("serializing session: {e}")))?;
    write_atomic(&dir.join(SESSION_FILE), &bytes)
}

pub fn run_bytes(run: &RunMeta) -> Result<Vec<u8>, StoreError> {
    let file = RunFile {
        version: SESSION_FILE_VERSION,
        run: run.clone(),
    };
    serde_json::to_vec_pretty(&file).map_err(|e| StoreError(format!("serializing run: {e}")))
}

impl Layout {
    pub fn new(root: &Path) -> Self {
        Self {
            sessions: root.join(SESSIONS_DIR),
            runs: root.join(RUNS_DIR),
        }
    }

    pub fn create_dirs(&self) -> Result<(), StoreError> {
        for dir in [&self.sessions, &self.runs] {
            std::fs::create_dir_all(dir).map_err(|e| failed("creating", dir, e))?;
        }
        Ok(())
    }

    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions.join(encode_name(id, ""))
    }

    fn run_path(&self, id: &str) -> PathBuf {
        self.runs.join(encode_name(id, ".json"))
    }

    pub fn read_run(&self, id: &str) -> Result<Option<RunMeta>, StoreError> {
        let path = self.run_path(id);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(failed("reading", &path, e)),
        };
        let file: RunFile = serde_json::from_str(&raw).map_err(|e| failed("corrupt", &path, e))?;
        Ok(Some(file.run))
    }

    fn write_run(&self, run: &RunMeta) -> Result<(), StoreError> {
        write_atomic(&self.run_path(&run.id), &run_bytes(run)?)
    }

    fn locked(&self, id: &str) -> Result<(SessionLock, PathBuf, SessionMeta), StoreError> {
        let dir = self.session_dir(id);
        let missing = || StoreError(format!("no session {id}"));
        read_session(&dir)?.ok_or_else(missing)?;
        let guard = lock(&dir)?;
        let meta = read_session(&dir)?.ok_or_else(missing)?;
        Ok((guard, dir, meta))
    }

    fn fenced(
        &self,
        lease: &Lease,
    ) -> Result<(SessionLock, PathBuf, SessionMeta, RunMeta), FenceError> {
        let dir = self.session_dir(&lease.session);
        if read_session(&dir)?.is_none() {
            return Err(FenceError::LeaseLost);
        }
        let guard = lock(&dir)?;
        let meta = read_session(&dir)?.ok_or(FenceError::LeaseLost)?;
        ops::fence(&meta, lease)?;
        let run = self.read_run(&lease.run)?.ok_or(FenceError::LeaseLost)?;
        Ok((guard, dir, meta, run))
    }

    pub fn create_session(&self, new: NewSession, now: i64) -> Result<SessionMeta, StoreError> {
        let meta = new.meta(ops::new_session_id(), now);
        let dir = self.session_dir(&meta.id);
        std::fs::create_dir_all(&dir).map_err(|e| failed("creating", &dir, e))?;
        write_session(&dir, &meta)?;
        Ok(meta)
    }

    pub fn scan(&self) -> Result<Vec<SessionMeta>, StoreError> {
        let entries = match std::fs::read_dir(&self.sessions) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(failed("reading", &self.sessions, e)),
        };
        let mut sessions = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            match read_session(&path) {
                Ok(Some(meta)) => sessions.push(meta),
                Ok(None) => {}
                Err(e) => tracing::warn!("skipping session dir {}: {e}", path.display()),
            }
        }
        Ok(sessions)
    }

    pub fn delete_session(&self, id: &str) -> Result<bool, StoreError> {
        let dir = self.session_dir(id);
        let mut runs: Vec<String> = read_jsonl::<Entry>(&dir.join(ENTRIES_FILE))
            .unwrap_or_default()
            .unwrap_or_default()
            .into_iter()
            .map(|entry| entry.run_id)
            .collect();
        runs.dedup();
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(failed("deleting", &dir, e)),
        }
        for run in runs {
            let path = self.run_path(&run);
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("couldn't remove {}: {e}", path.display());
                }
            }
        }
        Ok(true)
    }

    pub fn set_agent(&self, lease: &Lease, agent: &str, now: i64) -> Result<(), FenceError> {
        let (_guard, dir, mut meta, _) = self.fenced(lease)?;
        meta.active_agent = agent.to_string();
        meta.updated_at = meta.updated_at.max(now);
        Ok(write_session(&dir, &meta)?)
    }

    pub fn set_attachments(
        &self,
        id: &str,
        attachments: &[Attachment],
        now: i64,
    ) -> Result<(), StoreError> {
        let (_guard, dir, mut meta) = self.locked(id)?;
        meta.attachments = attachments.to_vec();
        meta.updated_at = meta.updated_at.max(now);
        write_session(&dir, &meta)
    }

    pub fn start_run(&self, start: &StartRun, now: i64) -> Result<Lease, StartError> {
        let duplicate = |run: RunMeta| StartError::Duplicate {
            run: run.id,
            session: run.session,
        };
        if let Some(existing) = self.read_run(&start.run_id)? {
            return Err(duplicate(existing));
        }
        let (_guard, dir, mut meta) = self.locked(&start.session)?;
        let entries = dir.join(ENTRIES_FILE);
        let started = ops::start(&mut meta, last_seq(&entries)?, start, now)?;
        let path = self.run_path(&start.run_id);
        match create_new(&path, &run_bytes(&started.run)?) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return match self.read_run(&start.run_id)? {
                    Some(existing) => Err(duplicate(existing)),
                    None => Err(StartError::Store(failed("creating", &path, e))),
                };
            }
            Err(e) => return Err(StartError::Store(failed("creating", &path, e))),
        }
        append_to(
            &entries,
            &json_lines(std::slice::from_ref(&started.entry), "entry")?,
        )?;
        write_session(&dir, &meta)?;
        Ok(started.lease)
    }

    pub fn append(&self, lease: &Lease, entries: &[Entry]) -> Result<(), FenceError> {
        let (_guard, dir, _, _) = self.fenced(lease)?;
        let path = dir.join(ENTRIES_FILE);
        check_batch(lease, last_seq(&path)?, entries)?;
        Ok(append_to(&path, &json_lines(entries, "entry")?)?)
    }

    pub fn renew(&self, lease: &Lease, now: i64) -> Result<Renewal, FenceError> {
        let (_guard, dir, mut meta, run) = self.fenced(lease)?;
        let renewal = ops::renew(&mut meta, &run, lease, now)?;
        write_session(&dir, &meta)?;
        Ok(renewal)
    }

    fn messages(dir: &Path, run: &str) -> Result<(u64, u64), StoreError> {
        let entries = read_jsonl::<Entry>(&dir.join(ENTRIES_FILE))?.unwrap_or_default();
        let last = entries.last().map_or(0, |entry| entry.seq);
        Ok((ops::messages_of(&entries, run), last))
    }

    pub fn end_run(&self, lease: &Lease, ended: RunEnded, now: i64) -> Result<(), FenceError> {
        let (_guard, dir, mut meta, mut run) = self.fenced(lease)?;
        let (messages, last) = Self::messages(&dir, &run.id)?;
        let entry = ops::finish(&mut meta, &mut run, last, messages, ended, now);
        append_to(
            &dir.join(ENTRIES_FILE),
            &json_lines(std::slice::from_ref(&entry), "entry")?,
        )?;
        self.write_run(&run)?;
        write_session(&dir, &meta)?;
        Ok(())
    }

    pub fn request_cancel(
        &self,
        run_id: &str,
        by: &str,
        now: i64,
    ) -> Result<CancelOutcome, StoreError> {
        let run = self
            .read_run(run_id)?
            .ok_or_else(|| StoreError(format!("no run {run_id}")))?;
        let (_guard, dir, mut meta) = self.locked(&run.session)?;
        let mut run = self
            .read_run(run_id)?
            .ok_or_else(|| StoreError(format!("no run {run_id}")))?;
        let (messages, last) = Self::messages(&dir, run_id)?;
        let cancel = ops::cancel(&mut meta, &mut run, last, messages, by, now);
        let outcome = cancel.outcome();
        match cancel {
            Cancel::NotRunning => return Ok(outcome),
            Cancel::Flag => {}
            Cancel::End(entries) => {
                append_to(&dir.join(ENTRIES_FILE), &json_lines(&entries, "entry")?)?;
                write_session(&dir, &meta)?;
            }
        }
        self.write_run(&run)?;
        Ok(outcome)
    }

    pub fn reap_expired(&self, limit: usize, now: i64) -> Result<Vec<Reaped>, StoreError> {
        let mut reaped = Vec::new();
        for candidate in self.scan()? {
            if reaped.len() >= limit {
                break;
            }
            if !candidate.lease_expired(now) {
                continue;
            }
            let dir = self.session_dir(&candidate.id);
            let Some(_guard) = try_lock(&dir)? else {
                continue;
            };
            let Some(mut meta) = read_session(&dir)? else {
                continue;
            };
            let Some(mut run) = meta
                .active_run
                .as_deref()
                .map(|id| self.read_run(id))
                .transpose()?
                .flatten()
            else {
                continue;
            };
            let (messages, last) = Self::messages(&dir, &run.id)?;
            let Some((entry, done)) = ops::reap(&mut meta, &mut run, last, messages, now) else {
                continue;
            };
            append_to(
                &dir.join(ENTRIES_FILE),
                &json_lines(std::slice::from_ref(&entry), "entry")?,
            )?;
            self.write_run(&run)?;
            write_session(&dir, &meta)?;
            reaped.push(done);
        }
        Ok(reaped)
    }

    pub fn entries(&self, session: &str, after: Option<u64>) -> Result<Vec<Entry>, StoreError> {
        let after = after.unwrap_or(0);
        let path = self.session_dir(session).join(ENTRIES_FILE);
        Ok(read_jsonl::<Entry>(&path)?
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.seq > after)
            .collect())
    }

    pub fn append_spans(&self, session: &str, spans: &[Span]) -> Result<(), StoreError> {
        if spans.is_empty() {
            return Ok(());
        }
        let (_guard, dir, _) = self.locked(session)?;
        append_to(&dir.join(SPANS_FILE), &json_lines(spans, "trace span")?)
    }

    pub fn spans(&self, session: &str) -> Result<Vec<Span>, StoreError> {
        let path = self.session_dir(session).join(SPANS_FILE);
        Ok(read_jsonl::<Span>(&path)?.unwrap_or_default())
    }
}
