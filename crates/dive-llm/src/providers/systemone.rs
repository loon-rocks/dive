use super::typesafe_images::{prepare, Prepared, MAX_REQUEST_BYTES};
use crate::decision::{
    Answer, DecisionProvider, DecisionRequest, DecisionResponse, LikelihoodCriteria, Question,
};
use crate::types::Usage;
use crate::LlmError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";

pub const IMAGES_HEADER: &str = "TypeSafe-Danger-Zone-Images";

pub struct SystemOneProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl SystemOneProvider {
    pub fn new(base_url: Option<String>, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            api_key: api_key.filter(|key| !key.is_empty()),
        }
    }

    async fn post_once(&self, body: &WireRequest<'_>) -> Result<reqwest::Response, LlmError> {
        let mut request = self
            .client
            .post(format!("{}/v1/systemone", self.base_url))
            .json(body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(crate::retry::api_error(response).await);
        }
        Ok(response)
    }

    async fn post_with_images(
        &self,
        body: &[u8],
        images: &[(String, Prepared)],
    ) -> Result<reqwest::Response, LlmError> {
        let mut form = reqwest::multipart::Form::new().part(
            "request",
            reqwest::multipart::Part::bytes(body.to_vec())
                .mime_str("application/json")
                .map_err(LlmError::Http)?,
        );
        for (name, image) in images {
            form = form.part(
                name.clone(),
                reqwest::multipart::Part::bytes(image.bytes.clone())
                    .file_name(format!("{name}.{}", image.extension))
                    .mime_str(image.mime)
                    .map_err(LlmError::Http)?,
            );
        }
        let mut request = self
            .client
            .post(format!("{}/v1/systemone", self.base_url))
            .header(IMAGES_HEADER, "true")
            .multipart(form);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(crate::retry::api_error(response).await);
        }
        Ok(response)
    }

    async fn send(&self, req: &DecisionRequest) -> Result<reqwest::Response, LlmError> {
        let mut body = WireRequest::from(req);
        let text = req.text();
        let mut prepared: Vec<(String, Prepared)> = Vec::new();
        for (reference, image) in &req.images {
            if !text.contains(reference.as_str()) {
                continue;
            }
            match prepare(reference, image) {
                Ok(image) => prepared.push((reference.clone(), image)),
                Err(error) => {
                    tracing::warn!(file = %reference, %error, "image left out of a decision")
                }
            }
        }
        if prepared.is_empty() {
            return crate::retry::with_retries(|| self.post_once(&body)).await;
        }
        let state_bytes = serde_json::to_vec(&body)
            .map_err(|e| LlmError::Parse(e.to_string()))?
            .len();
        let mut total = state_bytes
            + prepared
                .iter()
                .map(|(_, image)| image.bytes.len())
                .sum::<usize>();
        while total > MAX_REQUEST_BYTES && !prepared.is_empty() {
            let largest = prepared
                .iter()
                .enumerate()
                .max_by_key(|(_, (_, image))| image.bytes.len())
                .map(|(index, _)| index)
                .unwrap_or_default();
            let (reference, image) = prepared.remove(largest);
            total -= image.bytes.len();
            tracing::warn!(
                file = %reference,
                bytes = image.bytes.len(),
                "image left out of a decision, which would be over TypeSafe's {MAX_REQUEST_BYTES} bytes"
            );
        }
        if prepared.is_empty() {
            return crate::retry::with_retries(|| self.post_once(&body)).await;
        }
        let mut images: Vec<(String, Prepared)> = Vec::new();
        for (reference, image) in prepared {
            let name = format!("a{}", images.len());
            body.assets.insert(
                reference,
                WireAsset {
                    kind: "image",
                    part: name.clone(),
                },
            );
            images.push((name, image));
        }
        let json = serde_json::to_vec(&body).map_err(|e| LlmError::Parse(e.to_string()))?;
        crate::retry::with_retries(|| self.post_with_images(&json, &images)).await
    }
}

#[async_trait]
impl DecisionProvider for SystemOneProvider {
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, LlmError> {
        let response = self.send(&req).await?;
        let text = response.text().await?;
        let wire: WireResponse = serde_json::from_str(&text)
            .map_err(|e| LlmError::Parse(format!("systemone response: {e}: {text}")))?;
        Ok(wire.into())
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    state: Value,
    questions: BTreeMap<&'a str, WireQuestion<'a>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    assets: BTreeMap<String, WireAsset>,
}

#[derive(Serialize)]
struct WireAsset {
    #[serde(rename = "type")]
    kind: &'static str,
    part: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireQuestion<'a> {
    Noul {
        instructions: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<&'a LikelihoodCriteria>,
    },
    Choice {
        instructions: &'a str,
        criteria: &'a BTreeMap<String, String>,
    },
    Score {
        instructions: &'a str,
        criteria: &'a [String],
    },
}

impl<'a> From<&'a DecisionRequest> for WireRequest<'a> {
    fn from(req: &'a DecisionRequest) -> Self {
        let questions = req
            .questions
            .iter()
            .map(|(name, question)| {
                let wire = match question {
                    Question::Likelihood {
                        instructions,
                        criteria,
                    } => WireQuestion::Noul {
                        instructions,
                        criteria: criteria.as_ref(),
                    },
                    Question::Choice {
                        instructions,
                        criteria,
                    } => WireQuestion::Choice {
                        instructions,
                        criteria,
                    },
                    Question::Score {
                        instructions,
                        criteria,
                    } => WireQuestion::Score {
                        instructions,
                        criteria,
                    },
                };
                (name.as_str(), wire)
            })
            .collect();
        let state = match &req.state {
            Value::Null => Value::String(String::new()),
            state => state.clone(),
        };
        WireRequest {
            model: &req.model,
            state,
            questions,
            assets: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize)]
struct WireResponse {
    model: String,
    answers: BTreeMap<String, WireAnswer>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
    },
}

impl From<WireResponse> for DecisionResponse {
    fn from(wire: WireResponse) -> Self {
        let answers = wire
            .answers
            .into_iter()
            .map(|(name, answer)| {
                let answer = match answer {
                    WireAnswer::Noul { noul } => Answer::Likelihood { probability: noul },
                    WireAnswer::Choice {
                        choice,
                        confidence,
                        probabilities,
                    } => Answer::Choice {
                        choice,
                        confidence,
                        probabilities,
                    },
                    WireAnswer::Score {
                        score,
                        confidence,
                        legend,
                        probabilities,
                    } => Answer::Score {
                        score,
                        confidence,
                        legend,
                        probabilities,
                    },
                };
                (name, answer)
            })
            .collect();
        DecisionResponse {
            model: wire.model,
            answers,
            usage: Usage {
                input_tokens: wire.usage.input_tokens,
                output_tokens: wire.usage.output_tokens,
                output_image_tokens: 0,
                ..Usage::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::stub::stub;
    use serde_json::json;

    fn request() -> DecisionRequest {
        DecisionRequest {
            model: "jev-latest".into(),
            state: Value::Null,
            questions: BTreeMap::from([
                (
                    "urgent".to_string(),
                    Question::Likelihood {
                        instructions: "Is it urgent?".into(),
                        criteria: Some(LikelihoodCriteria {
                            yes: "time-sensitive".into(),
                            no: "not urgent".into(),
                        }),
                    },
                ),
                (
                    "team".to_string(),
                    Question::Choice {
                        instructions: "Which team?".into(),
                        criteria: BTreeMap::from([
                            ("billing".to_string(), "Payments".to_string()),
                            ("technical".to_string(), "Bugs".to_string()),
                        ]),
                    },
                ),
            ]),
            images: Default::default(),
        }
    }

    fn answered() -> String {
        json!({
            "model": "jev-1.13.0",
            "answers": {
                "urgent": {"type": "noul", "noul": 0.95},
                "team": {
                    "type": "choice",
                    "choice": "billing",
                    "confidence": 0.96,
                    "probabilities": {"billing": 0.98, "technical": 0.02}
                }
            },
            "usage": {"input_tokens": 389, "output_tokens": 61}
        })
        .to_string()
    }

    #[tokio::test]
    async fn requests_speak_the_wire_format_and_answers_come_back_typed() {
        let (base, seen) = stub(vec![(200, answered())]).await;
        let provider = SystemOneProvider::new(Some(base), Some("secret".into()));
        let response = provider.decide(request()).await.unwrap();

        let seen = seen.lock().unwrap()[0].clone();
        assert_eq!(seen.path, "/v1/systemone");
        assert_eq!(seen.authorization.as_deref(), Some("Bearer secret"));
        assert_eq!(
            seen.body,
            json!({
                "model": "jev-latest",
                "state": "",
                "questions": {
                    "team": {
                        "type": "choice",
                        "instructions": "Which team?",
                        "criteria": {"billing": "Payments", "technical": "Bugs"}
                    },
                    "urgent": {
                        "type": "noul",
                        "instructions": "Is it urgent?",
                        "criteria": {"true": "time-sensitive", "false": "not urgent"}
                    }
                }
            })
        );

        assert_eq!(response.model, "jev-1.13.0");
        assert_eq!(
            response.answers["urgent"],
            Answer::Likelihood { probability: 0.95 }
        );
        match &response.answers["team"] {
            Answer::Choice {
                choice, confidence, ..
            } => {
                assert_eq!(choice, "billing");
                assert_eq!(*confidence, 0.96);
            }
            other => panic!("expected a choice, got {other:?}"),
        }
        assert_eq!(response.usage.input_tokens, 389);
        assert_eq!(response.usage.output_tokens, 61);
    }

    const SHOT: &str =
        "blob:sha256-9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08.png";

    fn with_shot(mut req: DecisionRequest, mentioned: bool) -> DecisionRequest {
        if mentioned {
            req.state = json!({"page": SHOT});
        }
        req.images.insert(
            SHOT.to_string(),
            crate::ImageData {
                mime: "image/png".into(),
                bytes: super::super::typesafe_images::tests::encoded(
                    600,
                    400,
                    image::ImageFormat::Png,
                ),
            },
        );
        req
    }

    #[tokio::test]
    async fn images_go_as_multipart_assets_with_the_images_header() {
        let (base, seen) = stub(vec![(200, answered())]).await;
        let provider = SystemOneProvider::new(Some(base), Some("secret".into()));
        provider.decide(with_shot(request(), true)).await.unwrap();
        let seen = seen.lock().unwrap()[0].clone();
        let header = |name: &str| {
            seen.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            header("typesafe-danger-zone-images").as_deref(),
            Some("true")
        );
        assert!(header("content-type")
            .unwrap()
            .starts_with("multipart/form-data"));
        let raw = String::from_utf8_lossy(&seen.raw);
        let request_part = raw
            .split("name=\"request\"")
            .nth(1)
            .and_then(|rest| rest.split("\r\n\r\n").nth(1))
            .and_then(|rest| rest.split("\r\n--").next())
            .unwrap();
        let body: Value = serde_json::from_str(request_part).unwrap();
        assert_eq!(body["state"], json!({"page": SHOT}));
        assert_eq!(
            body["assets"],
            json!({SHOT: {"type": "image", "part": "a0"}})
        );
        assert!(raw.contains("name=\"a0\"; filename=\"a0.png\""), "{raw}");
        assert!(raw.contains("Content-Type: image/png"), "{raw}");
    }

    #[tokio::test]
    async fn an_image_the_request_never_mentions_is_not_sent() {
        let (base, seen) = stub(vec![(200, answered())]).await;
        let provider = SystemOneProvider::new(Some(base), None);
        provider.decide(with_shot(request(), false)).await.unwrap();
        let seen = seen.lock().unwrap()[0].clone();
        assert!(!seen
            .headers
            .iter()
            .any(|(key, _)| key == "typesafe-danger-zone-images"));
        assert!(seen.body.get("assets").is_none());
    }

    #[tokio::test]
    async fn an_image_that_cant_be_read_is_left_out_and_the_decision_still_goes() {
        let (base, seen) = stub(vec![(200, answered())]).await;
        let provider = SystemOneProvider::new(Some(base), None);
        let mut req = with_shot(request(), true);
        req.images.get_mut(SHOT).unwrap().bytes = b"not an image".to_vec();
        provider.decide(req).await.unwrap();
        let seen = seen.lock().unwrap()[0].clone();
        assert!(seen.body.get("assets").is_none());
        assert_eq!(seen.body["state"], json!({"page": SHOT}));
    }

    #[tokio::test]
    async fn graph_never_shows_the_wire_name_for_likelihood() {
        let (base, _) = stub(vec![(200, answered())]).await;
        let provider = SystemOneProvider::new(Some(base), None);
        let response = provider.decide(request()).await.unwrap();
        let shown = serde_json::to_string(&response).unwrap();
        assert!(!shown.contains("noul"), "{shown}");
        assert!(shown.contains("\"likelihood\""), "{shown}");
    }

    #[tokio::test]
    async fn no_api_key_sends_no_authorization_header() {
        let (base, seen) = stub(vec![(200, answered())]).await;
        let provider = SystemOneProvider::new(Some(format!("{base}/")), Some(String::new()));
        provider.decide(request()).await.unwrap();
        assert_eq!(seen.lock().unwrap()[0].authorization, None);
    }

    #[tokio::test]
    async fn structured_state_is_sent_as_is() {
        let (base, seen) = stub(vec![(200, answered())]).await;
        let provider = SystemOneProvider::new(Some(base), None);
        let mut req = request();
        req.state = json!({"diff": "+ fn a() {}"});
        provider.decide(req).await.unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].body["state"],
            json!({"diff": "+ fn a() {}"})
        );
    }

    #[tokio::test]
    async fn auth_and_validation_failures_are_not_retried() {
        for status in [401u16, 422] {
            let (base, seen) = stub(vec![
                (status, r#"{"detail":"no"}"#.to_string()),
                (200, answered()),
            ])
            .await;
            let provider = SystemOneProvider::new(Some(base), None);
            let error = provider.decide(request()).await.unwrap_err();
            assert!(
                matches!(error, LlmError::Api { status: s, .. } if s == status),
                "{error}"
            );
            assert_eq!(seen.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn overload_is_retried() {
        let (base, seen) = stub(vec![
            (529, r#"{"detail":"overloaded"}"#.to_string()),
            (200, answered()),
        ])
        .await;
        let provider = SystemOneProvider::new(Some(base), None);
        provider.decide(request()).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_unknown_answer_type_is_a_parse_error() {
        let body = json!({
            "model": "jev-1.13.0",
            "answers": {"urgent": {"type": "mystery", "value": 1}},
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string();
        let (base, _) = stub(vec![(200, body)]).await;
        let provider = SystemOneProvider::new(Some(base), None);
        let error = provider.decide(request()).await.unwrap_err();
        assert!(matches!(error, LlmError::Parse(_)), "{error}");
    }

    #[tokio::test]
    #[ignore = "calls the live TypeSafe API; needs TYPESAFE_API_KEY"]
    async fn systemone_live() {
        let key = std::env::var("TYPESAFE_API_KEY").expect("TYPESAFE_API_KEY");
        let provider = SystemOneProvider::new(None, Some(key));
        let mut req = request();
        req.state = json!("Help! My payouts have been failing for 3 days.");
        let response = provider.decide(req).await.unwrap();
        assert!(response.model.starts_with("jev-"), "{}", response.model);
        match &response.answers["urgent"] {
            Answer::Likelihood { probability } => assert!(*probability > 0.5),
            other => panic!("expected a likelihood, got {other:?}"),
        }
        match &response.answers["team"] {
            Answer::Choice { choice, .. } => assert_eq!(choice, "billing"),
            other => panic!("expected a choice, got {other:?}"),
        }
        assert!(response.usage.input_tokens > 0);
    }
}
