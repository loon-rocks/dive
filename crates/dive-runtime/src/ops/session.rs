use anyhow::{bail, Result};
use dive_core::files::{refs_in_text, BlobRef, BlobStore};
use dive_core::store::{Entry, Page, RunMeta, SessionFilter, SessionKind, SessionMeta};
use dive_core::Store;
use serde::Serialize;

pub const DEFAULT_PAGE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub kind: SessionKind,
    pub title: String,
    pub message_count: u64,
    pub updated_at: i64,
    pub owner: String,
    pub agent: String,
    pub active: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_run: Option<String>,
}

impl From<&SessionMeta> for SessionSummary {
    fn from(meta: &SessionMeta) -> Self {
        Self {
            id: meta.id.clone(),
            kind: meta.kind,
            title: meta.title.clone(),
            message_count: meta.message_count,
            updated_at: meta.updated_at,
            owner: meta.owner.clone(),
            agent: meta.agent.clone(),
            active: meta.active_agent.clone(),
            plan: meta.plan.clone(),
            project: meta.project.clone(),
            active_run: meta.active_run.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
    pub count: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub summary: SessionSummary,
    pub entries: Vec<Entry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<RunMeta>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<dive_core::trace::Span>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeleted {
    pub deleted: String,
    pub files_removed: usize,
}

pub fn in_project(project: Option<&str>) -> SessionFilter {
    SessionFilter {
        project: Some(project.map(str::to_string)),
        ..SessionFilter::default()
    }
}

pub async fn list(store: &dyn Store, filter: &SessionFilter, page: Page) -> Result<SessionList> {
    let listed = store.list_sessions(filter, page).await?;
    let sessions: Vec<SessionSummary> = listed.sessions.iter().map(SessionSummary::from).collect();
    Ok(SessionList {
        count: sessions.len(),
        sessions,
        more: listed.more,
    })
}

pub async fn list_in(store: &dyn Store, project: Option<&str>) -> Result<SessionList> {
    list(store, &in_project(project), Page::first(DEFAULT_PAGE)).await
}

pub async fn show(store: &dyn Store, id: &str) -> Result<SessionDetail> {
    let Some(meta) = store.get_session(id).await? else {
        bail!("no session with id {id}");
    };
    let entries = store.load_entries(id, None).await?;
    let mut run_ids: Vec<&str> = entries.iter().map(|entry| entry.run_id.as_str()).collect();
    run_ids.dedup();
    let mut runs = Vec::new();
    for run in run_ids {
        if let Some(meta) = store.get_run(run).await? {
            runs.push(meta);
        }
    }
    Ok(SessionDetail {
        summary: SessionSummary::from(&meta),
        runs,
        spans: store.load_spans(id).await?,
        entries,
    })
}

pub async fn delete(store: &dyn Store, files: &dyn BlobStore, id: &str) -> Result<SessionDeleted> {
    let meta = store.get_session(id).await?;
    let held = match &meta {
        Some(meta) => session_text(store, meta).await?,
        None => String::new(),
    };
    if !store.delete_session(id).await? {
        bail!("no session with id {id}");
    }
    let files_removed = if files.shared() {
        0
    } else {
        remove_unused(store, files, refs_in_text(&held)).await
    };
    Ok(SessionDeleted {
        deleted: id.to_string(),
        files_removed,
    })
}

async fn session_text(store: &dyn Store, meta: &SessionMeta) -> Result<String> {
    let entries = store.load_entries(&meta.id, None).await?;
    Ok(format!(
        "{}\n{}",
        serde_json::to_string(meta)?,
        serde_json::to_string(&entries)?
    ))
}

async fn remove_unused(
    store: &dyn Store,
    files: &dyn BlobStore,
    mut candidates: Vec<BlobRef>,
) -> usize {
    if candidates.is_empty() {
        return 0;
    }
    let sessions = match store
        .list_sessions(&SessionFilter::default(), Page::ALL)
        .await
    {
        Ok(listed) => listed.sessions,
        Err(e) => {
            tracing::warn!(error = %e, "kept the deleted session's files: couldn't list sessions");
            return 0;
        }
    };
    for meta in &sessions {
        match session_text(store, meta).await {
            Ok(text) => candidates.retain(|blob| !text.contains(blob.hash())),
            Err(e) => {
                tracing::warn!(error = %e, session = %meta.id, "kept the deleted session's files: couldn't read a session");
                return 0;
            }
        }
        if candidates.is_empty() {
            return 0;
        }
    }
    let mut removed = 0;
    for blob in candidates {
        match files.delete(&blob).await {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(error = %e, "couldn't remove a deleted session's file"),
        }
    }
    removed
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dive_core::run::Identity;
    use dive_core::store::{NewEntry, NewSession, RunEnded, RunStarted, StartRun};
    use dive_llm::types::ChatMessage;
    use dive_store::MemoryStore;
    use serde_json::json;

    pub(crate) fn identity() -> Identity {
        Identity {
            user: "ada".into(),
            tenant: "local".into(),
            deployment: "cli".into(),
            environment: "local".into(),
        }
    }

    pub(crate) async fn chat_session(
        store: &dyn Store,
        title: &str,
        project: Option<&str>,
        entries: Vec<NewEntry>,
    ) -> SessionMeta {
        session_of(store, title, "chat", project, entries).await
    }

    pub(crate) async fn session_of(
        store: &dyn Store,
        title: &str,
        agent: &str,
        project: Option<&str>,
        entries: Vec<NewEntry>,
    ) -> SessionMeta {
        let meta = store
            .create_session(NewSession::chat(title, agent, &identity()).in_project(project))
            .await
            .unwrap();
        if entries.is_empty() {
            return meta;
        }
        let mut lease = store
            .start_run(StartRun {
                run_id: uuid::Uuid::new_v4().to_string(),
                session: meta.id.clone(),
                kind: SessionKind::Chat,
                holder: "test".into(),
                token_expires_at: None,
                started: RunStarted {
                    input: json!(null),
                    identity: identity(),
                    trace_id: None,
                    plan: None,
                    synthetic: false,
                },
            })
            .await
            .unwrap();
        let numbered: Vec<Entry> = entries
            .into_iter()
            .map(|entry| {
                let entry = entry.into_entry(lease.next_seq, &lease.run, 0);
                lease.next_seq += 1;
                entry
            })
            .collect();
        store.append(&lease, &numbered).await.unwrap();
        store
            .end_run(&lease, RunEnded::completed(None))
            .await
            .unwrap();
        store.get_session(&meta.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn sessions_list_show_and_delete_with_the_cli_json_shapes() {
        let store = MemoryStore::new();
        assert_eq!(
            list(&store, &SessionFilter::default(), Page::ALL)
                .await
                .unwrap()
                .count,
            0
        );
        let meta = chat_session(
            &store,
            "Release notes",
            None,
            vec![NewEntry::message("user", ChatMessage::user("hi"))],
        )
        .await;

        let listed = serde_json::to_value(
            list(&store, &SessionFilter::default(), Page::ALL)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["sessions"][0]["id"], json!(meta.id));
        assert_eq!(listed["sessions"][0]["kind"], "chat");
        assert_eq!(listed["sessions"][0]["messageCount"], 1);
        assert_eq!(listed["sessions"][0]["owner"], "ada");
        assert_eq!(listed["sessions"][0]["agent"], "chat");
        assert!(listed["sessions"][0].get("updatedAt").is_some());

        let shown = serde_json::to_value(show(&store, &meta.id).await.unwrap()).unwrap();
        assert_eq!(shown["title"], "Release notes");
        assert_eq!(shown["entries"].as_array().unwrap().len(), 3);
        assert_eq!(shown["runs"][0]["status"], "completed");

        let files = dive_store::ObjectFiles::memory().unwrap();
        assert_eq!(
            delete(&store, &files, &meta.id).await.unwrap().deleted,
            meta.id
        );
        let missing = delete(&store, &files, &meta.id)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(missing, format!("no session with id {}", meta.id));
        assert!(show(&store, &meta.id).await.is_err());
    }

    #[tokio::test]
    async fn deleting_a_session_removes_the_files_only_it_refers_to() {
        let store = MemoryStore::new();
        let files = dive_store::ObjectFiles::memory().unwrap();
        let only = files.put(b"only".to_vec(), "image/png").await.unwrap().blob;
        let both = files
            .put(b"both".to_vec(), "image/jpeg")
            .await
            .unwrap()
            .blob;
        let say = |text: String| NewEntry::message("chat", ChatMessage::user(text));
        let gone = chat_session(
            &store,
            "gone",
            None,
            vec![say(format!("Here: ![drawn]({only}) and {both}."))],
        )
        .await;
        chat_session(&store, "kept", None, vec![say(format!("Same one: {both}"))]).await;

        let deleted = delete(&store, &files, &gone.id).await.unwrap();
        assert_eq!(deleted.files_removed, 1);
        assert!(files.get(&only).await.is_err());
        assert_eq!(files.get(&both).await.unwrap(), b"both");
        assert_eq!(
            serde_json::to_value(&deleted).unwrap(),
            json!({"deleted": gone.id, "filesRemoved": 1})
        );
    }

    #[tokio::test]
    async fn a_project_listing_holds_only_that_projects_sessions_and_global_only_global_ones() {
        let store = MemoryStore::new();
        let global = chat_session(&store, "global", None, Vec::new()).await;
        let site = chat_session(&store, "site", Some("/work/site"), Vec::new()).await;
        let other = chat_session(&store, "other", Some("/work/other"), Vec::new()).await;
        let ids = |list: SessionList| -> Vec<String> {
            list.sessions
                .into_iter()
                .map(|session| session.id)
                .collect()
        };

        assert_eq!(
            ids(list_in(&store, Some("/work/site")).await.unwrap()),
            [site.id.as_str()]
        );
        assert_eq!(
            ids(list_in(&store, None).await.unwrap()),
            [global.id.as_str()]
        );
        let mut all = ids(list(&store, &SessionFilter::default(), Page::ALL)
            .await
            .unwrap());
        all.sort();
        let mut expected = vec![global.id, site.id, other.id];
        expected.sort();
        assert_eq!(all, expected);

        let listed =
            serde_json::to_value(list_in(&store, Some("/work/site")).await.unwrap()).unwrap();
        assert_eq!(listed["sessions"][0]["project"], "/work/site");
        let listed = serde_json::to_value(list_in(&store, None).await.unwrap()).unwrap();
        assert_eq!(listed["sessions"][0]["project"], json!(null));
    }
}
