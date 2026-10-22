use crate::LlmError;
use graph_config::ProviderKind;
use serde_json::Value;
use std::time::Duration;

pub const ANTHROPIC_URL: &str = "https://api.anthropic.com";
pub const OPENAI_URL: &str = "https://api.openai.com/v1";
pub const SYSTEMONE_URL: &str = "https://api.typesafe.ai";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const TIMEOUT: Duration = Duration::from_secs(15);

pub fn lists_models(kind: ProviderKind) -> bool {
    matches!(
        kind,
        ProviderKind::Anthropic
            | ProviderKind::Openai
            | ProviderKind::OpenaiCompat
            | ProviderKind::Systemone
    )
}

pub fn models_url(kind: ProviderKind, base_url: Option<&str>) -> Option<String> {
    let base = |default: &str| {
        base_url
            .unwrap_or(default)
            .trim_end_matches('/')
            .to_string()
    };
    match kind {
        ProviderKind::Anthropic => Some(format!("{}/v1/models?limit=1000", base(ANTHROPIC_URL))),
        ProviderKind::Openai => Some(format!("{}/models", base(OPENAI_URL))),
        ProviderKind::OpenaiCompat => base_url.map(|_| format!("{}/models", base(""))),
        ProviderKind::Systemone => Some(format!("{}/v1/models", base(SYSTEMONE_URL))),
        ProviderKind::Bedrock => None,
    }
}

pub fn model_ids(body: &str) -> Result<Vec<String>, LlmError> {
    let value: Value =
        serde_json::from_str(body).map_err(|error| LlmError::Parse(error.to_string()))?;
    let (items, field) = match (value.get("data"), value.get("models")) {
        (Some(Value::Array(items)), _) => (items, "id"),
        (_, Some(Value::Array(items))) => (items, "name"),
        _ => {
            return Err(LlmError::Parse(
                "the model list has no data or models array".to_string(),
            ));
        }
    };
    Ok(items
        .iter()
        .filter_map(|item| item.get(field).and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

pub async fn list_models(
    kind: ProviderKind,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<Vec<String>, LlmError> {
    let Some(url) = models_url(kind, base_url) else {
        return Err(LlmError::Parse(match kind {
            ProviderKind::OpenaiCompat => "openai_compat needs a base_url".to_string(),
            _ => "this provider can't list its models".to_string(),
        }));
    };
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(LlmError::Http)?;
    let mut request = client.get(url);
    match kind {
        ProviderKind::Anthropic => {
            request = request
                .header("x-api-key", api_key.unwrap_or_default())
                .header("anthropic-version", ANTHROPIC_VERSION);
        }
        _ => {
            if let Some(key) = api_key.filter(|key| !key.is_empty()) {
                request = request.bearer_auth(key);
            }
        }
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(LlmError::Api {
            status: status.as_u16(),
            body,
            retry_after: None,
        });
    }
    let mut ids = model_ids(&body)?;
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_lists_come_from_the_data_array() {
        let body = r#"{"data":[{"id":"claude-sonnet-5-5","type":"model"},{"id":"claude-haiku-4-5"}],"has_more":false}"#;
        assert_eq!(
            model_ids(body).unwrap(),
            ["claude-sonnet-5-5", "claude-haiku-4-5"]
        );
        assert!(model_ids(r#"{"items":[]}"#).is_err());
    }

    #[test]
    fn each_provider_kind_has_its_own_models_endpoint() {
        assert_eq!(
            models_url(ProviderKind::Anthropic, None).unwrap(),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
        assert_eq!(
            models_url(ProviderKind::Openai, None).unwrap(),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_url(
                ProviderKind::OpenaiCompat,
                Some("http://localhost:11434/v1/")
            )
            .unwrap(),
            "http://localhost:11434/v1/models"
        );
        assert_eq!(models_url(ProviderKind::OpenaiCompat, None), None);
        assert_eq!(
            models_url(ProviderKind::Systemone, None).unwrap(),
            "https://api.typesafe.ai/v1/models"
        );
        assert!(lists_models(ProviderKind::Systemone));
        assert_eq!(models_url(ProviderKind::Bedrock, None), None);
    }

    #[test]
    fn systemone_model_lists_come_from_the_models_array() {
        let body = r#"{"models":[{"name":"jev-latest","description":"General-purpose system one model.","release_date":"2026-09-15"}]}"#;
        assert_eq!(model_ids(body).unwrap(), ["jev-latest"]);
    }
}
