use crate::cancel::CancelHandle;
use crate::writer::RunWriter;
use dive_core::pipeline::doc::PlanDoc;
use dive_core::pipeline::{ExitStatus, PipelineError, PipelineOutcome};
use dive_core::run::Run;
use dive_core::store::{
    NewSession, RunEnded, RunStarted, RunStatus, SessionKind, StartError, StartRun,
};
use dive_core::usage::UsageReport;
use dive_core::Store;
use serde_json::Value;
use std::sync::Arc;

pub async fn begin(
    store: &Arc<dyn Store>,
    run: &Run,
    doc: &PlanDoc,
    input: &Value,
    project: Option<&str>,
    cancel: Option<CancelHandle>,
) -> Result<RunWriter, StartError> {
    let session = store
        .create_session(
            NewSession::plan(&doc.name, &doc.identifier, &run.identity).in_project(project),
        )
        .await?;
    let start = StartRun {
        run_id: run.id.to_string(),
        session: session.id,
        kind: SessionKind::Plan,
        holder: crate::writer::holder().to_string(),
        token_expires_at: None,
        started: RunStarted {
            input: input.clone(),
            identity: run.identity.clone(),
            trace_id: run.trace.as_ref().map(|trace| trace.trace_id.clone()),
            plan: serde_json::to_value(doc).ok(),
            synthetic: false,
        },
    };
    RunWriter::start(Arc::clone(store), start, cancel).await
}

pub fn ending(result: &Result<PipelineOutcome, PipelineError>, usage: &UsageReport) -> RunEnded {
    let ended = match result {
        Ok(outcome) => match &outcome.exit {
            Some(exit) if exit.status == ExitStatus::Error => RunEnded {
                outcome: Some(crate::telemetry::plan_output(outcome)),
                ..RunEnded::failed(&exit.message)
            },
            _ => RunEnded::completed(Some(crate::telemetry::plan_output(outcome))),
        },
        Err(PipelineError::Aborted { .. }) => RunEnded::new(RunStatus::Cancelled),
        Err(error) => RunEnded::failed(&error.to_string()),
    };
    ended.with_usage(serde_json::to_value(usage).ok())
}

pub async fn end(
    writer: Option<RunWriter>,
    result: &Result<PipelineOutcome, PipelineError>,
    usage: &UsageReport,
) {
    let Some(writer) = writer else {
        return;
    };
    if let Err(error) = writer.finish(ending(result, usage)).await {
        tracing::warn!(error = %error, "couldn't close the plan run");
    }
}

pub async fn start_or_warn(
    store: &Arc<dyn Store>,
    run: &Run,
    doc: &PlanDoc,
    input: &Value,
    project: Option<&str>,
    cancel: Option<CancelHandle>,
) -> Option<RunWriter> {
    match begin(store, run, doc, input, project, cancel).await {
        Ok(writer) => Some(writer),
        Err(error) => {
            tracing::warn!(error = %error, "the plan run won't be recorded");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dive_core::store::{EntryBody, Page, SessionFilter};

    #[tokio::test]
    async fn a_plan_run_is_recorded_as_a_plan_session_with_its_snapshot_and_status() {
        let store: Arc<dyn Store> = Arc::new(dive_store::MemoryStore::new());
        let run = Run::new(
            crate::ops::session::tests::identity(),
            Arc::new(dive_core::NullSink),
        );
        let doc: PlanDoc =
            serde_yaml::from_str("identifier: digest\nname: Digest\ndescription: d\nsteps: []\n")
                .unwrap();
        let writer = begin(
            &store,
            &run,
            &doc,
            &serde_json::json!({"day": 1}),
            None,
            None,
        )
        .await
        .unwrap();
        let session = writer.session().to_string();
        let failed: Result<PipelineOutcome, PipelineError> =
            Err(PipelineError::OutputRender("boom".into()));
        end(Some(writer), &failed, &UsageReport::default()).await;
        let listed = store
            .list_sessions(
                &SessionFilter {
                    kind: Some(SessionKind::Plan),
                    ..SessionFilter::default()
                },
                Page::ALL,
            )
            .await
            .unwrap();
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.sessions[0].plan.as_deref(), Some("digest"));
        let entries = store.load_entries(&session, None).await.unwrap();
        let EntryBody::RunStarted(started) = &entries[0].body else {
            panic!("{entries:?}");
        };
        assert_eq!(started.plan.as_ref().unwrap()["identifier"], "digest");
        assert_eq!(started.input["day"], 1);
        let EntryBody::RunEnded(ended) = &entries[1].body else {
            panic!("{entries:?}");
        };
        assert_eq!(ended.status, RunStatus::Failed);
        assert_eq!(
            store
                .get_run(run.id.as_str())
                .await
                .unwrap()
                .unwrap()
                .status,
            RunStatus::Failed
        );
    }
}
