use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::{json, Map, Value};
use utoipa::ToSchema;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    InvalidToken(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("run {run} already started")]
    Duplicate { run: String, session: String },
    #[error("this token's jti was already used")]
    JtiTaken,
    #[error("{message}")]
    InvalidInput {
        message: String,
        problems: Vec<String>,
    },
    #[error("{0}")]
    PlanUnavailable(String),
    #[error("{0}")]
    AtCapacity(String),
    #[error("this server is shutting down")]
    ShuttingDown,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub const RETRY_AFTER_SECS: u64 = 5;

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorDetail {
    #[schema(example = "not_found")]
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problems: Option<Vec<String>>,
}

impl ServeError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::InvalidToken(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Duplicate { .. } | Self::JtiTaken => StatusCode::CONFLICT,
            Self::InvalidInput { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::PlanUnavailable(_) | Self::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            Self::AtCapacity(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::InvalidToken(_) => "invalid_token",
            Self::Forbidden(_) => "forbidden",
            Self::NotFound(_) => "not_found",
            Self::Duplicate { .. } | Self::JtiTaken => "idempotency_violation",
            Self::InvalidInput { .. } => "invalid_input",
            Self::PlanUnavailable(_) => "plan_unavailable",
            Self::AtCapacity(_) => "at_capacity",
            Self::ShuttingDown => "shutting_down",
            Self::Internal(_) => "internal",
        }
    }

    pub fn body(&self) -> Value {
        let mut error = Map::new();
        error.insert("code".into(), json!(self.code()));
        let message = match self {
            Self::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        };
        error.insert("message".into(), json!(message));
        match self {
            Self::Duplicate { run, session } => {
                error.insert("run_id".into(), json!(run));
                error.insert("session_id".into(), json!(session));
            }
            Self::InvalidInput { problems, .. } => {
                error.insert("problems".into(), json!(problems));
            }
            _ => {}
        }
        json!({ "error": error })
    }

    pub fn run_not_found(run: &str) -> Self {
        Self::NotFound(format!("no run {run}"))
    }
}

impl IntoResponse for ServeError {
    fn into_response(self) -> Response {
        if let Self::Internal(error) = &self {
            tracing::error!(error = %format!("{error:#}"), "request failed");
        }
        let mut response = (self.status(), Json(self.body())).into_response();
        match self {
            Self::AtCapacity(_) => {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(RETRY_AFTER_SECS));
            }
            Self::InvalidToken(_) => {
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            }
            _ => {}
        }
        response
    }
}

impl From<dive_core::store::StoreError> for ServeError {
    fn from(error: dive_core::store::StoreError) -> Self {
        Self::Internal(anyhow::Error::new(error))
    }
}
