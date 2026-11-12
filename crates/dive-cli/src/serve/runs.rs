use super::auth::{Caller, Claims};
use super::error::{ErrorBody, ServeError};
use super::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use dive_core::store::{RunMeta, RunStatus};
use dive_runtime::ops::run::{self, Outcome, RunView};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct RunBody {
    pub id: String,
    pub session_id: String,
    #[schema(example = "triage")]
    pub plan: Option<String>,
    #[schema(value_type = String, example = "completed")]
    pub status: RunStatus,
    #[schema(value_type = Option<String>, example = "ok")]
    pub outcome: Option<Outcome>,
    #[schema(value_type = Option<Object>)]
    pub result: Option<Value>,
    pub error: Option<String>,
    pub first_seq: u64,
    pub last_seq: Option<u64>,
    pub cancel_requested: bool,
    #[schema(value_type = Option<Object>)]
    pub usage: Option<Value>,
    #[schema(example = "2026-11-11T17:02:11Z")]
    pub started_at: String,
    pub ended_at: Option<String>,
}

impl From<RunView> for RunBody {
    fn from(view: RunView) -> Self {
        Self {
            id: view.id,
            session_id: view.session_id,
            plan: view.plan,
            status: view.status,
            outcome: view.outcome,
            result: view.result,
            error: view.error,
            first_seq: view.first_seq,
            last_seq: view.last_seq,
            cancel_requested: view.cancel_requested,
            usage: view.usage,
            started_at: view.started_at,
            ended_at: view.ended_at,
        }
    }
}

pub async fn owned_run(
    state: &AppState,
    claims: &Claims,
    run: &str,
) -> Result<RunMeta, ServeError> {
    state
        .store
        .get_run(run)
        .await?
        .filter(|meta| meta.tenant == claims.tenant)
        .ok_or_else(|| ServeError::run_not_found(run))
}

async fn body(state: &AppState, meta: &RunMeta) -> Result<Json<RunBody>, ServeError> {
    Ok(Json(RunBody::from(
        run::view(state.store.as_ref(), meta).await?,
    )))
}

#[utoipa::path(
    get,
    path = "/v1/runs/{run}",
    tag = "runs",
    params(("run" = String, Path, description = "The run's id")),
    security(("run_token" = ["runs:read"])),
    responses(
        (status = 200, description = "The run", body = RunBody),
        (status = 401, description = "invalid_token", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
        (status = 404, description = "not_found", body = ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    Path(run): Path<String>,
    caller: Caller,
) -> Result<Json<RunBody>, ServeError> {
    caller.claims.require("runs:read")?;
    let meta = owned_run(&state, &caller.claims, &run).await?;
    body(&state, &meta).await
}

#[utoipa::path(
    post,
    path = "/v1/runs/{run}/cancel",
    tag = "runs",
    params(("run" = String, Path, description = "The run's id")),
    security(("run_token" = ["runs:cancel"])),
    responses(
        (status = 202, description = "The cancel was recorded; the run as it stands", body = RunBody),
        (status = 401, description = "invalid_token", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
        (status = 404, description = "not_found", body = ErrorBody),
    )
)]
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    Path(run): Path<String>,
    caller: Caller,
) -> Result<(StatusCode, Json<RunBody>), ServeError> {
    caller.claims.require("runs:cancel")?;
    let meta = owned_run(&state, &caller.claims, &run).await?;
    if meta.status != RunStatus::Running {
        return Ok((StatusCode::ACCEPTED, body(&state, &meta).await?));
    }
    state.store.request_cancel(&run, &caller.claims.sub).await?;
    state.cancel_here(&run, &caller.claims.sub);
    let meta = owned_run(&state, &caller.claims, &run).await?;
    Ok((StatusCode::ACCEPTED, body(&state, &meta).await?))
}
