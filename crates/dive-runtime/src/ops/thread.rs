use anyhow::{bail, Result};
use dive_core::files::{refs_in_text, BlobRef, BlobStore};
use dive_core::{Store, ThreadEntry, ThreadMeta};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub id: String,
    pub title: String,
    pub message_count: i64,
    pub updated_at: i64,
    pub owner: String,
    pub active: String,
    pub project: Option<String>,
}

impl From<&ThreadMeta> for ThreadSummary {
    fn from(meta: &ThreadMeta) -> Self {
        Self {
            id: meta.id.clone(),
            title: meta.title.clone(),
            message_count: meta.message_count,
            updated_at: meta.updated_at,
            owner: meta.owner.clone(),
            active: meta.active.clone(),
            project: meta.project.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadList {
    pub threads: Vec<ThreadSummary>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadDetail {
    #[serde(flatten)]
    pub summary: ThreadSummary,
    pub entries: Vec<ThreadEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDeleted {
    pub deleted: String,
    pub files_removed: usize,
}

pub async fn list(store: &dyn Store) -> Result<ThreadList> {
    listed(store, |_| true).await
}

pub async fn list_in(store: &dyn Store, project: Option<&str>) -> Result<ThreadList> {
    listed(store, |meta| meta.in_project(project)).await
}

async fn listed(store: &dyn Store, keep: impl Fn(&ThreadMeta) -> bool) -> Result<ThreadList> {
    let threads: Vec<ThreadSummary> = store
        .list_threads()
        .await?
        .iter()
        .filter(|meta| keep(meta))
        .map(ThreadSummary::from)
        .collect();
    Ok(ThreadList {
        count: threads.len(),
        threads,
    })
}

pub async fn show(store: &dyn Store, id: &str) -> Result<ThreadDetail> {
    let Some(meta) = store.get_thread(id).await? else {
        bail!("no thread with id {id}");
    };
    Ok(ThreadDetail {
        summary: ThreadSummary::from(&meta),
        entries: store.load_entries(id).await?,
    })
}

pub async fn delete(store: &dyn Store, files: &dyn BlobStore, id: &str) -> Result<ThreadDeleted> {
    let meta = store.get_thread(id).await?;
    let held = match &meta {
        Some(meta) => thread_text(store, meta).await?,
        None => String::new(),
    };
    if !store.delete_thread(id).await? {
        bail!("no thread with id {id}");
    }
    let files_removed = if files.shared() {
        0
    } else {
        remove_unused(store, files, refs_in_text(&held)).await
    };
    Ok(ThreadDeleted {
        deleted: id.to_string(),
        files_removed,
    })
}

async fn thread_text(store: &dyn Store, meta: &ThreadMeta) -> Result<String> {
    let entries = store.load_entries(&meta.id).await?;
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
    let threads = match store.list_threads().await {
        Ok(threads) => threads,
        Err(e) => {
            tracing::warn!(error = %e, "kept the deleted thread's files: couldn't list threads");
            return 0;
        }
    };
    for meta in &threads {
        match thread_text(store, meta).await {
            Ok(text) => candidates.retain(|blob| !text.contains(blob.hash())),
            Err(e) => {
                tracing::warn!(error = %e, thread = %meta.id, "kept the deleted thread's files: couldn't read a thread");
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
            Err(e) => tracing::warn!(error = %e, "couldn't remove a deleted thread's file"),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_store::MemoryStore;
    use serde_json::json;

    #[tokio::test]
    async fn threads_list_show_and_delete_with_the_cli_json_shapes() {
        let store = MemoryStore::new();
        assert_eq!(list(&store).await.unwrap().count, 0);
        let meta = store
            .create_thread("Release notes", "chat", None)
            .await
            .unwrap();

        let listed = serde_json::to_value(list(&store).await.unwrap()).unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["threads"][0]["id"], json!(meta.id));
        assert_eq!(listed["threads"][0]["messageCount"], 0);
        assert!(listed["threads"][0].get("updatedAt").is_some());

        let shown = serde_json::to_value(show(&store, &meta.id).await.unwrap()).unwrap();
        assert_eq!(shown["title"], "Release notes");
        assert_eq!(shown["entries"], json!([]));

        let files = dive_store::ObjectFiles::memory().unwrap();
        assert_eq!(
            delete(&store, &files, &meta.id).await.unwrap().deleted,
            meta.id
        );
        let missing = delete(&store, &files, &meta.id)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(missing, format!("no thread with id {}", meta.id));
        assert!(show(&store, &meta.id).await.is_err());
    }

    #[tokio::test]
    async fn deleting_a_thread_removes_the_files_only_it_refers_to() {
        use dive_core::{EntryBody, NewEntry};
        use dive_llm::types::ChatMessage;
        let store = MemoryStore::new();
        let files = dive_store::ObjectFiles::memory().unwrap();
        let only = files.put(b"only".to_vec(), "image/png").await.unwrap().blob;
        let both = files
            .put(b"both".to_vec(), "image/jpeg")
            .await
            .unwrap()
            .blob;
        let say = |text: String| {
            NewEntry::new(
                "chat",
                EntryBody::Message {
                    message: ChatMessage::User { content: text },
                },
            )
        };
        let gone = store.create_thread("gone", "chat", None).await.unwrap();
        store
            .append_entries(
                &gone.id,
                &[say(format!("Here: ![drawn]({only}) and {both}."))],
            )
            .await
            .unwrap();
        let kept = store.create_thread("kept", "chat", None).await.unwrap();
        store
            .append_entries(&kept.id, &[say(format!("Same one: {both}"))])
            .await
            .unwrap();

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
    async fn a_project_listing_holds_only_that_projects_threads_and_global_only_global_ones() {
        let store = MemoryStore::new();
        let global = store.create_thread("global", "chat", None).await.unwrap();
        let site = store
            .create_thread("site", "chat", Some("/work/site"))
            .await
            .unwrap();
        let other = store
            .create_thread("other", "chat", Some("/work/other"))
            .await
            .unwrap();
        let ids = |list: ThreadList| -> Vec<String> {
            list.threads.into_iter().map(|thread| thread.id).collect()
        };

        assert_eq!(
            ids(list_in(&store, Some("/work/site")).await.unwrap()),
            [site.id.as_str()]
        );
        assert_eq!(
            ids(list_in(&store, None).await.unwrap()),
            [global.id.as_str()]
        );
        let mut all = ids(list(&store).await.unwrap());
        all.sort();
        let mut expected = vec![global.id, site.id, other.id];
        expected.sort();
        assert_eq!(all, expected);

        let listed =
            serde_json::to_value(list_in(&store, Some("/work/site")).await.unwrap()).unwrap();
        assert_eq!(listed["threads"][0]["project"], "/work/site");
        let listed = serde_json::to_value(list_in(&store, None).await.unwrap()).unwrap();
        assert_eq!(listed["threads"][0]["project"], json!(null));
    }
}
