use async_trait::async_trait;
use dive_core::store::{
    Attachment, CancelOutcome, Entry, FenceError, Lease, NewSession, Page, Reaped, Renewal,
    RunEnded, RunMeta, SessionFilter, SessionMeta, SessionPage, Span, StartError, StartRun, Store,
    StoreError, ToolShape,
};
use serde_json::Value;
use std::sync::Arc;

pub struct ScopedStore {
    inner: Arc<dyn Store>,
    tenant: String,
}

impl ScopedStore {
    pub fn new(inner: Arc<dyn Store>, tenant: &str) -> Self {
        Self {
            inner,
            tenant: tenant.to_string(),
        }
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    async fn session(&self, id: &str) -> Result<Option<SessionMeta>, StoreError> {
        Ok(self
            .inner
            .get_session(id)
            .await?
            .filter(|meta| meta.tenant == self.tenant))
    }

    async fn owned(&self, id: &str) -> Result<SessionMeta, StoreError> {
        self.session(id)
            .await?
            .ok_or_else(|| StoreError(format!("no session {id}")))
    }

    async fn fenced(&self, lease: &Lease) -> Result<(), FenceError> {
        match self.session(&lease.session).await? {
            Some(_) => Ok(()),
            None => Err(FenceError::LeaseLost),
        }
    }

    async fn run(&self, id: &str) -> Result<Option<RunMeta>, StoreError> {
        Ok(self
            .inner
            .get_run(id)
            .await?
            .filter(|run| run.tenant == self.tenant))
    }
}

#[async_trait]
impl Store for ScopedStore {
    async fn create_session(&self, mut new: NewSession) -> Result<SessionMeta, StoreError> {
        new.identity.tenant = self.tenant.clone();
        self.inner.create_session(new).await
    }

    async fn get_session(&self, id: &str) -> Result<Option<SessionMeta>, StoreError> {
        self.session(id).await
    }

    async fn list_sessions(
        &self,
        filter: &SessionFilter,
        page: Page,
    ) -> Result<SessionPage, StoreError> {
        let scoped = SessionFilter {
            tenant: Some(self.tenant.clone()),
            ..filter.clone()
        };
        self.inner.list_sessions(&scoped, page).await
    }

    async fn delete_session(&self, id: &str) -> Result<bool, StoreError> {
        if self.session(id).await?.is_none() {
            return Ok(false);
        }
        self.inner.delete_session(id).await
    }

    async fn set_active_agent(&self, lease: &Lease, agent: &str) -> Result<(), FenceError> {
        self.fenced(lease).await?;
        self.inner.set_active_agent(lease, agent).await
    }

    async fn set_attachments(
        &self,
        id: &str,
        attachments: &[Attachment],
    ) -> Result<(), StoreError> {
        self.owned(id).await?;
        self.inner.set_attachments(id, attachments).await
    }

    async fn start_run(&self, mut start: StartRun) -> Result<Lease, StartError> {
        self.owned(&start.session).await?;
        start.started.identity.tenant = self.tenant.clone();
        match self.inner.start_run(start).await {
            Err(StartError::Duplicate { run, session }) => match self.session(&session).await? {
                Some(_) => Err(StartError::Duplicate { run, session }),
                None => Err(StartError::Store(StoreError(format!(
                    "run id {run} is already in use"
                )))),
            },
            other => other,
        }
    }

    async fn append(&self, lease: &Lease, entries: &[Entry]) -> Result<(), FenceError> {
        self.fenced(lease).await?;
        self.inner.append(lease, entries).await
    }

    async fn renew(&self, lease: &Lease) -> Result<Renewal, FenceError> {
        self.fenced(lease).await?;
        self.inner.renew(lease).await
    }

    async fn end_run(&self, lease: &Lease, ended: RunEnded) -> Result<(), FenceError> {
        self.fenced(lease).await?;
        self.inner.end_run(lease, ended).await
    }

    async fn request_cancel(&self, run: &str, by: &str) -> Result<CancelOutcome, StoreError> {
        if self.run(run).await?.is_none() {
            return Err(StoreError(format!("no run {run}")));
        }
        self.inner.request_cancel(run, by).await
    }

    async fn reap_expired(&self, limit: usize) -> Result<Vec<Reaped>, StoreError> {
        let mut visible = Vec::new();
        for reaped in self.inner.reap_expired(limit).await? {
            if self.session(&reaped.session).await?.is_some() {
                visible.push(reaped);
            }
        }
        Ok(visible)
    }

    async fn get_run(&self, id: &str) -> Result<Option<RunMeta>, StoreError> {
        self.run(id).await
    }

    async fn load_entries(
        &self,
        session: &str,
        after: Option<u64>,
    ) -> Result<Vec<Entry>, StoreError> {
        match self.session(session).await? {
            Some(_) => self.inner.load_entries(session, after).await,
            None => Ok(Vec::new()),
        }
    }

    async fn append_spans(&self, session: &str, spans: &[Span]) -> Result<(), StoreError> {
        self.owned(session).await?;
        self.inner.append_spans(session, spans).await
    }

    async fn load_spans(&self, session: &str) -> Result<Vec<Span>, StoreError> {
        match self.session(session).await? {
            Some(_) => self.inner.load_spans(session).await,
            None => Ok(Vec::new()),
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_core::run::Identity;
    use dive_core::store::{RunStarted, SessionKind};
    use dive_store::MemoryStore;
    use serde_json::json;

    fn who(tenant: &str) -> Identity {
        Identity {
            user: "ada".into(),
            tenant: tenant.into(),
            deployment: "serve".into(),
            environment: "production".into(),
        }
    }

    fn starting(run: &str, session: &str) -> StartRun {
        StartRun {
            run_id: run.into(),
            session: session.into(),
            kind: SessionKind::Chat,
            holder: "pod".into(),
            token_expires_at: None,
            started: RunStarted {
                input: json!("hi"),
                identity: who("anything"),
                trace_id: None,
                plan: None,
                synthetic: false,
            },
        }
    }

    #[tokio::test]
    async fn a_scoped_store_sees_only_its_own_tenant_s_sessions_and_runs() {
        let shared: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let acme = ScopedStore::new(shared.clone(), "acme");
        let globex = ScopedStore::new(shared.clone(), "globex");

        let mine = acme
            .create_session(NewSession::chat("mine", "chat", &who("globex")))
            .await
            .unwrap();
        assert_eq!(mine.tenant, "acme");
        let theirs = globex
            .create_session(NewSession::chat("theirs", "chat", &who("globex")))
            .await
            .unwrap();

        let listed = acme
            .list_sessions(&SessionFilter::default(), Page::ALL)
            .await
            .unwrap();
        assert_eq!(listed.sessions, std::slice::from_ref(&mine));
        assert!(acme.get_session(&theirs.id).await.unwrap().is_none());
        assert!(acme
            .load_entries(&theirs.id, None)
            .await
            .unwrap()
            .is_empty());
        assert!(acme.set_attachments(&theirs.id, &[]).await.is_err());
        assert!(!acme.delete_session(&theirs.id).await.unwrap());
        assert!(shared.get_session(&theirs.id).await.unwrap().is_some());

        assert!(matches!(
            acme.start_run(starting("r-theirs", &theirs.id)).await,
            Err(StartError::Store(_))
        ));
        let lease = globex
            .start_run(starting("r-theirs", &theirs.id))
            .await
            .unwrap();
        assert!(acme.get_run("r-theirs").await.unwrap().is_none());
        assert!(acme.request_cancel("r-theirs", "ada").await.is_err());
        assert!(matches!(
            acme.renew(&lease).await,
            Err(FenceError::LeaseLost)
        ));
        assert!(matches!(
            acme.start_run(starting("r-theirs", &mine.id)).await,
            Err(StartError::Store(_))
        ));

        let own = acme.start_run(starting("r-mine", &mine.id)).await.unwrap();
        assert_eq!(
            acme.get_run("r-mine").await.unwrap().unwrap().tenant,
            "acme"
        );
        acme.end_run(&own, RunEnded::completed(None)).await.unwrap();
        assert!(matches!(
            acme.start_run(starting("r-mine", &mine.id)).await,
            Err(StartError::Duplicate { .. })
        ));
    }
}
