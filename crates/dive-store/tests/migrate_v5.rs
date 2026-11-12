use dive_core::run::Identity;
use dive_core::store::{Entry, EntryBody, RunStatus, SessionFilter, SessionKind, Store};
use dive_llm::types::ChatMessage;
use dive_store::{marker_format, FileStore, STORE_FORMAT};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn fixture_dir(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(version)
}

fn ada() -> Identity {
    Identity {
        user: "ada".into(),
        tenant: "acme".into(),
        deployment: "cli".into(),
        environment: "local".into(),
    }
}

fn data_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                pending.push(path);
            } else if name != ".lock" && name != ".migrate.lock" {
                files.push(path.strip_prefix(root).unwrap().to_path_buf());
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

fn contents(path: &Path) -> Vec<Value> {
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

fn outside_threads(files: Vec<PathBuf>) -> Vec<PathBuf> {
    files
        .into_iter()
        .filter(|file| !file.starts_with("threads"))
        .collect()
}

#[test]
fn every_frozen_version_4_thread_migrates_to_the_frozen_version_5_layout() {
    let dir = copy_fixture("v4");
    FileStore::open_as(dir.path(), &ada()).unwrap();
    let expected = fixture_dir("v5");
    assert_eq!(
        outside_threads(data_files(dir.path())),
        data_files(&expected)
    );
    for file in data_files(&expected) {
        assert_eq!(
            contents(&dir.path().join(&file)),
            contents(&expected.join(&file)),
            "{}",
            file.display()
        );
    }
    assert_eq!(marker_format(dir.path()).unwrap(), Some(STORE_FORMAT));
    assert!(!dir.path().join("sessions.v5-tmp").exists());
}

#[test]
fn migrating_leaves_the_version_4_threads_untouched() {
    let dir = copy_fixture("v4");
    FileStore::open_as(dir.path(), &ada()).unwrap();
    let v4 = fixture_dir("v4");
    for file in data_files(&v4)
        .into_iter()
        .filter(|file| file.starts_with("threads"))
    {
        assert_eq!(
            std::fs::read(dir.path().join(&file)).unwrap(),
            std::fs::read(v4.join(&file)).unwrap(),
            "{}",
            file.display()
        );
    }
}

#[tokio::test]
async fn a_frozen_version_5_data_directory_opens_as_is() {
    let dir = copy_fixture("v5");
    let store = FileStore::open_as(dir.path(), &ada()).unwrap();
    let listed = store
        .list_sessions(&SessionFilter::default(), dive_core::store::Page::ALL)
        .await
        .unwrap();
    assert_eq!(listed.sessions.len(), 6);
    assert!(listed
        .sessions
        .iter()
        .all(|meta| meta.kind == SessionKind::Chat && meta.tenant == "local"));
    let expected = fixture_dir("v5");
    assert_eq!(data_files(dir.path()), data_files(&expected));
    for file in data_files(&expected) {
        assert_eq!(
            std::fs::read(dir.path().join(&file)).unwrap(),
            std::fs::read(expected.join(&file)).unwrap(),
            "{}",
            file.display()
        );
    }
}

fn kinds(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            let kind = serde_json::to_value(entry).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_string();
            if entry.path.is_empty() {
                kind
            } else {
                format!("{}/{kind}", entry.path)
            }
        })
        .collect()
}

#[tokio::test]
async fn a_session_that_starts_with_the_assistant_gets_a_synthetic_run_at_seq_1() {
    let dir = copy_fixture("v4");
    let store = FileStore::open_as(dir.path(), &ada()).unwrap();
    let entries = store.load_entries("be9100000001", None).await.unwrap();
    assert_eq!(entries[0].seq, 1);
    let EntryBody::RunStarted(started) = &entries[0].body else {
        panic!("{:?}", entries[0]);
    };
    assert!(started.synthetic);
    assert_eq!(started.input, Value::Null);
    assert_eq!(started.identity.user, "ada");
    assert_eq!(started.identity.tenant, "local");
    assert_eq!(
        kinds(&entries),
        [
            "run_started",
            "message",
            "message",
            "run_ended",
            "run_started",
            "message",
            "message",
            "run_ended"
        ]
    );
    let seqs: Vec<u64> = entries.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, (1..=8).collect::<Vec<u64>>());
    let second = store.get_run("be9100000001-r2").await.unwrap().unwrap();
    assert_eq!(
        (second.first_seq, second.last_seq, second.status),
        (5, Some(8), RunStatus::Completed)
    );
    let EntryBody::RunStarted(started) = &entries[4].body else {
        panic!("{:?}", entries[4]);
    };
    assert_eq!(started.input, json!("a word counter"));
    let meta = store.get_session("be9100000001").await.unwrap().unwrap();
    assert_eq!(
        (meta.agent.as_str(), meta.active_agent.as_str()),
        ("network_drafter", "network_drafter")
    );
    assert_eq!(meta.owner, "ada");
    assert_eq!(meta.attachments.len(), 1);
}

#[tokio::test]
async fn a_subagent_run_becomes_an_agent_pair_with_its_messages_between() {
    let dir = copy_fixture("v4");
    let store = FileStore::open_as(dir.path(), &ada()).unwrap();
    let entries = store.load_entries("5ab000000001", None).await.unwrap();
    assert_eq!(
        kinds(&entries),
        [
            "run_started",
            "message",
            "handoff",
            "message",
            "agent_started",
            "agent:plan_refiner/message",
            "agent:plan_refiner/message",
            "agent_finished",
            "message",
            "message",
            "run_ended",
            "run_started",
            "message",
            "run_ended"
        ]
    );
    let started = &entries[4];
    assert_eq!(started.span_id.as_deref(), Some("53995c3f42cd8ad8"));
    for entry in &entries[5..8] {
        assert_eq!(entry.parent, Some(started.seq));
    }
    assert!(matches!(
        &entries[5].body,
        EntryBody::Message {
            message: ChatMessage::User { .. }
        }
    ));
    let EntryBody::AgentFinished { output, final_, .. } = &entries[7].body else {
        panic!("{:?}", entries[7]);
    };
    assert_eq!(output, &json!({"changes": ["add a changelog step"]}));
    assert!(*final_);
    assert_eq!(entries[3].span_id.as_deref(), Some("00f067aa0ba902b7"));
    assert_eq!(store.load_spans("5ab000000001").await.unwrap().len(), 2);
    let meta = store.get_session("5ab000000001").await.unwrap().unwrap();
    assert_eq!(
        (meta.agent.as_str(), meta.active_agent.as_str()),
        ("front_desk", "plan_author")
    );
}

#[test]
fn concurrent_opens_of_a_version_4_store_migrate_it_once() {
    let dir = copy_fixture("v4");
    let root = dir.path().to_path_buf();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || FileStore::open_as(&root, &ada()).map(|_| ()))
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    assert_eq!(marker_format(&root).unwrap(), Some(STORE_FORMAT));
    let expected = fixture_dir("v5");
    assert_eq!(outside_threads(data_files(&root)), data_files(&expected));
    let raw = std::fs::read_to_string(root.join("sessions/a1b2c3d4e5f6/entries.jsonl")).unwrap();
    assert_eq!(raw.lines().count(), 6);
}

#[test]
fn every_older_frozen_store_migrates_to_version_5() {
    for version in ["v1", "v2", "v3"] {
        let dir = copy_fixture(version);
        FileStore::open_as(dir.path(), &ada()).unwrap();
        assert_eq!(
            marker_format(dir.path()).unwrap(),
            Some(STORE_FORMAT),
            "{version}"
        );
        assert!(
            dir.path()
                .join("sessions/a1b2c3d4e5f6/session.json")
                .is_file(),
            "{version}"
        );
    }
}
