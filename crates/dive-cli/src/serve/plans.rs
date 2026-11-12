use super::auth::Caller;
use super::error::{ErrorBody, ServeError};
use super::{AppState, DeltaSink};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use dive_core::run::RunToken;
use dive_runtime::ops::run::{self, PlanInfo, RunControl, StartFailure, StartPlan};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use utoipa::ToSchema;

pub const MAX_JTI: usize = 128;

pub fn valid_jti(jti: &str) -> bool {
    !jti.is_empty()
        && jti.len() <= MAX_JTI
        && jti
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PlanBody {
    #[schema(example = "triage")]
    pub identifier: String,
    pub name: String,
    pub description: String,
    #[schema(value_type = Option<Object>)]
    pub input_schema: Option<Value>,
    #[schema(value_type = Option<Object>)]
    pub result_schema: Option<Value>,
}

impl From<PlanInfo> for PlanBody {
    fn from(plan: PlanInfo) -> Self {
        Self {
            identifier: plan.identifier,
            name: plan.name,
            description: plan.description,
            input_schema: plan.input_schema,
            result_schema: plan.result_schema,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PlanList {
    pub plans: Vec<PlanBody>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct StartBody {
    #[schema(value_type = Object, example = json!({"issue": "ENG-142"}))]
    pub input: Option<Value>,
    #[schema(example = "Triage ENG-142")]
    pub title: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StartedBody {
    pub run_id: String,
    pub session_id: String,
    #[schema(example = "running")]
    pub status: String,
    pub entries_url: String,
}

#[utoipa::path(
    get,
    path = "/v1/plans",
    tag = "plans",
    security(("run_token" = ["plans:read"])),
    responses(
        (status = 200, description = "The deployed plans", body = PlanList),
        (status = 401, description = "invalid_token", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> Result<Json<PlanList>, ServeError> {
    caller.claims.require("plans:read")?;
    Ok(Json(PlanList {
        plans: run::plans(&state.runtime)
            .into_iter()
            .map(PlanBody::from)
            .collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/v1/plans/{plan}",
    tag = "plans",
    params(("plan" = String, Path, description = "The plan's identifier")),
    security(("run_token" = ["plans:read"])),
    responses(
        (status = 200, description = "The plan", body = PlanBody),
        (status = 401, description = "invalid_token", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
        (status = 404, description = "not_found", body = ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    Path(plan): Path<String>,
    caller: Caller,
) -> Result<Json<PlanBody>, ServeError> {
    caller.claims.require("plans:read")?;
    run::plan(&state.runtime, &plan)
        .map(|plan| Json(PlanBody::from(plan)))
        .ok_or_else(|| ServeError::NotFound(format!("no plan named '{plan}'")))
}

#[utoipa::path(
    post,
    path = "/v1/plans/{plan}/runs",
    tag = "runs",
    params(("plan" = String, Path, description = "The plan's identifier")),
    request_body = StartBody,
    security(("run_token" = [])),
    responses(
        (status = 202, description = "The run started", body = StartedBody,
            headers(("Location" = String, description = "The run's URL"))),
        (status = 400, description = "bad_request", body = ErrorBody),
        (status = 401, description = "invalid_token", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
        (status = 404, description = "not_found", body = ErrorBody),
        (status = 409, description = "idempotency_violation", body = ErrorBody),
        (status = 422, description = "invalid_input", body = ErrorBody),
        (status = 429, description = "at_capacity", body = ErrorBody),
        (status = 503, description = "shutting_down or plan_unavailable", body = ErrorBody),
    )
)]
pub async fn start(
    State(state): State<Arc<AppState>>,
    Path(plan): Path<String>,
    caller: Caller,
    body: Result<Json<StartBody>, JsonRejection>,
) -> Result<Response, ServeError> {
    if state.draining() {
        return Err(ServeError::ShuttingDown);
    }
    let claims = &caller.claims;
    match claims.plan.as_deref() {
        Some(claimed) if claimed == plan => {}
        Some(claimed) => {
            return Err(ServeError::Forbidden(format!(
                "the token starts plan '{claimed}', not '{plan}'"
            )))
        }
        None => {
            return Err(ServeError::Forbidden(
                "the token has no plan claim, so it can't start a run".into(),
            ))
        }
    }
    let run_id = claims
        .jti
        .clone()
        .ok_or_else(|| ServeError::InvalidToken("a start token needs a jti".into()))?;
    if !valid_jti(&run_id) {
        return Err(ServeError::InvalidToken(format!(
            "a jti is 1 to {MAX_JTI} letters, digits, `-`, `_`, `.` or `~`"
        )));
    }
    let Json(body) = body.map_err(|rejection| ServeError::BadRequest(rejection.body_text()))?;
    let input = body
        .input
        .ok_or_else(|| ServeError::BadRequest("the body needs an `input`".into()))?;
    let slot = state.limits.acquire(&claims.tenant)?;
    let identity =
        dive_runtime::identity::serve(&state.runtime.config.identity, &claims.sub, &claims.tenant)
            .map_err(|error| ServeError::Internal(error.into()))?;
    let request = StartPlan {
        plan,
        input,
        title: body.title,
        run_id,
        identity,
        token: Some(RunToken::new(caller.token)),
    };
    let starting = Arc::clone(&state);
    let started = tokio::spawn(async move { launch(starting, request, slot).await })
        .await
        .map_err(|error| ServeError::Internal(anyhow::anyhow!("the start task failed: {error}")))?;
    let (run_id, session_id) = started.map_err(|failure| match failure {
        StartFailure::NoPlan(plan) => ServeError::NotFound(format!("no plan named '{plan}'")),
        StartFailure::Unrunnable { .. } => ServeError::PlanUnavailable(failure.to_string()),
        StartFailure::InvalidInput { ref problems, .. } => ServeError::InvalidInput {
            message: failure.to_string(),
            problems: problems.clone(),
        },
        StartFailure::Duplicate { run, session } => ServeError::Duplicate { run, session },
        StartFailure::JtiTaken => ServeError::JtiTaken,
        StartFailure::Internal(error) => ServeError::Internal(error),
    })?;
    let started = StartedRun { run_id, session_id };
    let location = format!("/v1/runs/{}", started.run_id);
    let body = StartedBody {
        entries_url: format!("{location}/entries"),
        run_id: started.run_id,
        session_id: started.session_id,
        status: "running".into(),
    };
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, location)],
        Json(body),
    )
        .into_response())
}

struct StartedRun {
    run_id: String,
    session_id: String,
}

struct Forget {
    state: Arc<AppState>,
    run: String,
}

impl Drop for Forget {
    fn drop(&mut self) {
        self.state.forget(&self.run);
    }
}

async fn launch(
    state: Arc<AppState>,
    request: StartPlan,
    slot: super::limits::Slot,
) -> Result<(String, String), StartFailure> {
    let control = RunControl::new();
    let deltas = super::delta_channel();
    let started = run::start_plan(
        &state.runtime,
        &state.store,
        request,
        control.clone(),
        Arc::new(DeltaSink::new(deltas.clone())),
    )
    .await?;
    state.track(&started.run_id, control, deltas);
    let forget = Forget {
        state: Arc::clone(&state),
        run: started.run_id.clone(),
    };
    let execution = started.execution;
    state.spawn(async move {
        let status = execution.await;
        tracing::info!(run = %forget.run, status = status.as_str(), "run ended");
        drop(forget);
        drop(slot);
    });
    Ok((started.run_id, started.session_id))
}
