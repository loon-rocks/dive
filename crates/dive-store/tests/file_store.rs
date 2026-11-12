use dive_core::run::Identity;
use dive_core::store::{
    EntryBody, NewEntry, NewSession, Page, RunEnded, RunStarted, SessionFilter, SessionKind,
    StartRun, Store,
};
use dive_llm::types::ChatMessage;
use dive_store::{FileStore, STORE_FORMAT};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};

fn ada() -> Identity {
    Identity {
        user: "ada".into(),
        tenant: "local".into(),
        deployment: "cli".into(),
        environment: "local".into(),
    }
}

async fn session_with(store: &FileStore, texts: &[&str]) -> String {
    let id = store
        .create_session(NewSession::chat("t", "chat", &ada()))
        .await
        .unwrap()
        .id;
    let mut lease = store
        .start_run(StartRun {
            run_id: uuid::Uuid::new_v4().to_string(),
            session: id.clone(),
            kind: SessionKind::Chat,
            holder: "test".into(),
            token_expires_at: None,
            started: RunStarted {
                input: json!(null),
                identity: ada(),
                trace_id: None,
                plan: None,
                synthetic: false,
            },
        })
        .await
        .unwrap();
    let entries: Vec<_> = texts
        .iter()
        .map(|text| {
            let entry = NewEntry::message("user", ChatMessage::user(*text)).into_entry(
                lease.next_seq,
                &lease.run,
                1,
            );
            lease.next_seq += 1;
            entry
        })
        .collect();
    store.append(&lease, &entries).await.unwrap();
    store
        .end_run(&lease, RunEnded::completed(None))
        .await
        .unwrap();
    id
}

fn messages(entries: &[dive_core::store::Entry]) -> usize {
    entries
        .iter()
        .filter(|entry| matches!(entry.body, EntryBody::Message { .. }))
        .count()
}

#[tokio::test]
async fn persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let store = FileStore::open(dir.path()).unwrap();
        store
            .record_tool_shape("t__x", &json!({"type": "object"}), &json!({}))
            .await
            .unwrap();
        session_with(&store, &["hello"]).await
    };
    let store = FileStore::open(dir.path()).unwrap();
    assert_eq!(messages(&store.load_entries(&id, None).await.unwrap()), 1);
    let meta = store.get_session(&id).await.unwrap().unwrap();
    assert_eq!(meta.message_count, 1);
    assert_eq!(store.tool_shapes().await.unwrap()[0].tool, "t__x");
}

fn entries_file(root: &Path, id: &str) -> PathBuf {
    root.join("sessions").join(id).join("entries.jsonl")
}

#[tokio::test]
async fn torn_final_line_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    let id = session_with(&store, &["one", "two"]).await;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(entries_file(dir.path(), &id))
        .unwrap();
    file.write_all(b"{\"seq\":99,\"run_id\":\"r\",\"kind\":\"mess")
        .unwrap();
    drop(file);
    assert_eq!(messages(&store.load_entries(&id, None).await.unwrap()), 2);
}

#[tokio::test]
async fn mid_file_corruption_errors() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    let id = session_with(&store, &["one"]).await;
    let path = entries_file(dir.path(), &id);
    let mut raw = std::fs::read_to_string(&path).unwrap();
    raw.insert_str(0, "not json\n");
    std::fs::write(&path, raw).unwrap();
    let err = store.load_entries(&id, None).await.unwrap_err();
    assert!(err.to_string().contains("corrupt line"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn separate_instances_write_their_own_sessions_without_corrupting_each_other() {
    const WRITERS: usize = 8;
    let dir = tempfile::tempdir().unwrap();
    let mut handles = Vec::new();
    for _ in 0..WRITERS {
        let root = dir.path().to_path_buf();
        handles.push(tokio::spawn(async move {
            let store = FileStore::open(&root).unwrap();
            for _ in 0..5 {
                session_with(&store, &["a", "b"]).await;
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    let store = FileStore::open(dir.path()).unwrap();
    let listed = store
        .list_sessions(&SessionFilter::default(), Page::ALL)
        .await
        .unwrap();
    assert_eq!(listed.sessions.len(), WRITERS * 5);
    for meta in listed.sessions {
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.active_run, None);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_shape_writes_never_corrupt() {
    const WRITERS: usize = 8;
    let dir = tempfile::tempdir().unwrap();
    let mut handles = Vec::new();
    for w in 0..WRITERS {
        let root = dir.path().to_path_buf();
        handles.push(tokio::spawn(async move {
            let store = FileStore::open(&root).unwrap();
            for _ in 0..10 {
                store
                    .record_tool_shape("hot__tool", &json!({"type": "object"}), &json!({"w": w}))
                    .await
                    .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    let shapes = FileStore::open(dir.path())
        .unwrap()
        .tool_shapes()
        .await
        .unwrap();
    assert_eq!(shapes.len(), 1);
    assert_eq!(shapes[0].tool, "hot__tool");
    assert!(shapes[0].seen_count >= 1);
    assert!(shapes[0].seen_count <= (WRITERS * 10) as i64);
}

#[tokio::test]
async fn a_listing_skips_incomplete_session_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    session_with(&store, &[]).await;
    std::fs::create_dir_all(dir.path().join("sessions").join("halfmade")).unwrap();
    let listed = store
        .list_sessions(&SessionFilter::default(), Page::ALL)
        .await
        .unwrap();
    assert_eq!(listed.sessions.len(), 1);
}

#[tokio::test]
async fn deleting_a_session_removes_its_runs() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap();
    let id = session_with(&store, &["x"]).await;
    let runs = || std::fs::read_dir(dir.path().join("runs")).unwrap().count();
    assert_eq!(runs(), 1);
    assert!(store.delete_session(&id).await.unwrap());
    assert_eq!(runs(), 0);
}

fn fixture_dir(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(version)
}

fn data_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if relative.starts_with("sessions") || relative.starts_with("runs") {
                continue;
            }
            if path.is_dir() {
                pending.push(path);
            } else if ![".lock", ".migrate.lock"]
                .contains(&path.file_name().unwrap().to_str().unwrap())
            {
                files.push(relative);
            }
        }
    }
    files.sort();
    files
}

fn copy_fixture(version: &str) -> tempfile::TempDir {
    let from = fixture_dir(version);
    let dir = tempfile::tempdir().unwrap();
    for file in data_files(&from) {
        let to = dir.path().join(&file);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(from.join(&file), to).unwrap();
    }
    dir
}

fn contents(path: &Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path).unwrap();
    if path.extension().is_some_and(|ext| ext == "jsonl") {
        return raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    }
    vec![serde_json::from_str(&raw).unwrap_or_else(|_| json!(raw.trim()))]
}

fn assert_matches_fixture_except_marker(root: &Path, version: &str) {
    let expected = fixture_dir(version);
    assert_eq!(data_files(root), data_files(&expected));
    for file in data_files(&expected) {
        if file == Path::new("FORMAT") {
            continue;
        }
        assert_eq!(
            contents(&root.join(&file)),
            contents(&expected.join(&file)),
            "{}",
            file.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.join("FORMAT")).unwrap(),
        format!("{STORE_FORMAT}\n")
    );
}

#[test]
fn a_version_1_data_directory_migrates_to_the_frozen_version_2_threads_and_is_marked_current() {
    let dir = copy_fixture("v1");
    FileStore::open(dir.path()).unwrap();
    assert_matches_fixture_except_marker(dir.path(), "v2");
}

#[tokio::test]
async fn a_frozen_version_2_data_directory_keeps_its_threads_and_opens_as_sessions() {
    let dir = copy_fixture("v2");
    let store = FileStore::open(dir.path()).unwrap();
    assert_matches_fixture_except_marker(dir.path(), "v2");
    let entries_file = "threads/a1b2c3d4e5f6/entries.jsonl";
    assert_eq!(
        std::fs::read(dir.path().join(entries_file)).unwrap(),
        std::fs::read(fixture_dir("v2").join(entries_file)).unwrap()
    );
    let session = store.get_session("a1b2c3d4e5f6").await.unwrap().unwrap();
    assert_eq!((session.agent.as_str(), session.message_count), ("chat", 4));
    let entries = store.load_entries("a1b2c3d4e5f6", None).await.unwrap();
    let authors: Vec<&str> = entries
        .iter()
        .filter(|entry| matches!(entry.body, EntryBody::Message { .. }))
        .map(|entry| entry.author.as_str())
        .collect();
    assert_eq!(authors, ["user", "chat", "chat", "chat"]);
    assert_eq!(store.tool_shapes().await.unwrap().len(), 1);
}

#[test]
fn a_version_3_data_directory_moves_its_files_into_the_frozen_version_4_layout() {
    let dir = copy_fixture("v3");
    FileStore::open(dir.path()).unwrap();
    assert_matches_fixture_except_marker(dir.path(), "v4");
}

#[tokio::test]
async fn a_frozen_version_4_data_directory_keeps_its_threads_as_they_were() {
    let dir = copy_fixture("v4");
    let store = FileStore::open(dir.path()).unwrap();
    let expected = fixture_dir("v4");
    assert_eq!(data_files(dir.path()), data_files(&expected));
    for file in data_files(&expected) {
        if file == Path::new("FORMAT") {
            continue;
        }
        assert_eq!(
            std::fs::read(dir.path().join(&file)).unwrap(),
            std::fs::read(expected.join(&file)).unwrap(),
            "{}",
            file.display()
        );
    }
    let site = store.get_session("517e00000001").await.unwrap().unwrap();
    assert_eq!(site.project.as_deref(), Some("/work/site"));
    let draft = store.get_session("c0ffee000001").await.unwrap().unwrap();
    assert_eq!(
        draft.attachments,
        [dive_core::store::Attachment::new("network").with("state", "open")]
    );
    let entries = store.load_entries("a1b2c3d4e5f6", None).await.unwrap();
    let ms: Vec<Option<u64>> = entries
        .iter()
        .filter(|entry| matches!(entry.body, EntryBody::Message { .. }))
        .map(|entry| entry.ms)
        .collect();
    assert_eq!(ms, [None, Some(1840), Some(5230), Some(910)]);
    assert!(entries
        .iter()
        .all(|entry| !serde_json::to_string(entry).unwrap().contains("\"calls\"")));
}
