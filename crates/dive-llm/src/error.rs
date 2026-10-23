use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned {status}: {body}")]
    Api {
        status: u16,
        body: String,
        /// Parsed Retry-After header, when the provider sent one.
        retry_after: Option<u64>,
    },
    #[error("failed to parse provider response: {0}")]
    Parse(String),
    #[error("model output did not match the requested schema: {0}")]
    SchemaMismatch(String),
    #[error("the model hit its output limit{} before finishing the structured result; ask for less output or split the work across steps", limit_phrase(*.limit))]
    OutputLimit { limit: Option<u64> },
    #[error("provider '{0}' is not configured")]
    UnknownProvider(String),
    /// Configured, but cannot be used as configured — most commonly an
    /// unset `${VAR}` behind its api_key. The reason names the variable and
    /// the config path, because this message is often all a caller (or an
    /// agent driving `dive mcp serve`) gets to diagnose with.
    #[error("provider '{provider}' is configured but not usable: {reason}")]
    ProviderUnavailable { provider: String, reason: String },
    #[error("no model configured for role '{0}' and no default set")]
    NoModelForRole(String),
    #[error("no model named '{name}' is configured; available names: {available}")]
    UnknownModelName { name: String, available: String },
    #[error("{0}")]
    Unsupported(String),
    #[error("model role '{role}' is a {actual} model, but this call needs a {expected} model")]
    WrongModelKind {
        role: String,
        expected: dive_config::ModelKind,
        actual: dive_config::ModelKind,
    },
    #[error("{0}")]
    Config(String),
    #[error("no decision model is configured: add [models.decider] on a provider with type = \"systemone\", or name a decision role with `model:`")]
    NoDecider,
    #[error("no image model is configured: add [models.image] on a provider with type = \"gemini\", or name an image role with `model:`")]
    NoImageModel,
    #[error("state for model role '{role}' ({model}) is about {estimate} tokens, over its context_window of {limit}; trim it in an earlier step")]
    ContextWindowExceeded {
        role: String,
        model: String,
        limit: u32,
        estimate: u64,
    },
}

fn limit_phrase(limit: Option<u64>) -> String {
    limit.map(|n| format!(" of {n} tokens")).unwrap_or_default()
}

impl LlmError {
    pub fn output_limit(requested: Option<u32>, usage: &crate::types::Usage) -> Self {
        let limit = requested
            .map(u64::from)
            .or((usage.output_tokens > 0).then_some(usage.output_tokens));
        LlmError::OutputLimit { limit }
    }
}
