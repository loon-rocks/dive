use anyhow::{bail, Result};
use graph_core::{Store, ThreadEntry, ThreadMeta};
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
pub struct ThreadDeleted {
    pub deleted: String,
}

pub async fn list(store: &dyn Store) -> Result<ThreadList> {
    let threads: Vec<ThreadSummary> = store
        .list_threads()
        .await?
        .iter()
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

pub async fn delete(store: &dyn Store, id: &str) -> Result<ThreadDeleted> {
    if !store.delete_thread(id).await? {
        bail!("no thread with id {id}");
    }
    Ok(ThreadDeleted {
        deleted: id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use graph_store::MemoryStore;
    use serde_json::json;

    #[tokio::test]
    async fn threads_list_show_and_delete_with_the_cli_json_shapes() {
        let store = MemoryStore::new();
        assert_eq!(list(&store).await.unwrap().count, 0);
        let meta = store.create_thread("Release notes", "chat").await.unwrap();

        let listed = serde_json::to_value(list(&store).await.unwrap()).unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["threads"][0]["id"], json!(meta.id));
        assert_eq!(listed["threads"][0]["messageCount"], 0);
        assert!(listed["threads"][0].get("updatedAt").is_some());

        let shown = serde_json::to_value(show(&store, &meta.id).await.unwrap()).unwrap();
        assert_eq!(shown["title"], "Release notes");
        assert_eq!(shown["entries"], json!([]));

        assert_eq!(delete(&store, &meta.id).await.unwrap().deleted, meta.id);
        let missing = delete(&store, &meta.id).await.unwrap_err().to_string();
        assert_eq!(missing, format!("no thread with id {}", meta.id));
        assert!(show(&store, &meta.id).await.is_err());
    }
}
