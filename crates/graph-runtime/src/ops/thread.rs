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
pub struct ThreadDeleted {
    pub deleted: String,
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

        assert_eq!(delete(&store, &meta.id).await.unwrap().deleted, meta.id);
        let missing = delete(&store, &meta.id).await.unwrap_err().to_string();
        assert_eq!(missing, format!("no thread with id {}", meta.id));
        assert!(show(&store, &meta.id).await.is_err());
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
