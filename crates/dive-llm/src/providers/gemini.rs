use crate::image::{ImageData, ImageOptions, ImageProvider, ImageRequest, ImageResponse, Part};
use crate::types::Usage;
use crate::LlmError;
use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

const ASPECT_RATIOS: &[&str] = &[
    "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9",
];

const MAX_IMAGES: usize = 14;

pub struct GeminiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl GeminiProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            api_key,
        }
    }

    async fn post_once(&self, body: &WireRequest<'_>) -> Result<reqwest::Response, LlmError> {
        let response = self
            .client
            .post(format!("{}/v1beta/interactions", self.base_url))
            .header("x-goog-api-key", &self.api_key)
            .json(body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(crate::retry::api_error(response).await);
        }
        Ok(response)
    }
}

fn sizes(model: &str) -> Vec<&'static str> {
    match model {
        "gemini-2.5-flash-image" => Vec::new(),
        "gemini-3.1-flash-lite-image" => vec!["1K"],
        _ => vec!["1K", "2K", "4K"],
    }
}

#[async_trait]
impl ImageProvider for GeminiProvider {
    fn options(&self, model: &str) -> ImageOptions {
        ImageOptions {
            aspect_ratios: ASPECT_RATIOS.to_vec(),
            sizes: sizes(model),
            max_images: MAX_IMAGES,
        }
    }

    async fn generate(&self, req: ImageRequest) -> Result<ImageResponse, LlmError> {
        let body = WireRequest::from(&req);
        let response = crate::retry::with_retries(|| self.post_once(&body)).await?;
        let text = response.text().await?;
        let wire: WireInteraction = serde_json::from_str(&text)
            .map_err(|e| LlmError::Parse(format!("gemini interaction: {e}")))?;
        wire.into_response()
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    input: Vec<WireContent<'a>>,
    response_format: WireFormat<'a>,
    store: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContent<'a> {
    Text { text: &'a str },
    Image { mime_type: &'a str, data: String },
}

#[derive(Serialize)]
struct WireFormat<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    aspect_ratio: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_size: Option<&'a str>,
}

impl<'a> From<&'a ImageRequest> for WireRequest<'a> {
    fn from(req: &'a ImageRequest) -> Self {
        let input = req
            .parts
            .iter()
            .map(|part| match part {
                Part::Text(text) => WireContent::Text { text },
                Part::Image(image) => WireContent::Image {
                    mime_type: &image.mime,
                    data: base64::engine::general_purpose::STANDARD.encode(&image.bytes),
                },
            })
            .collect();
        WireRequest {
            model: &req.model,
            input,
            response_format: WireFormat {
                kind: "image",
                aspect_ratio: req.aspect_ratio.as_deref(),
                image_size: req.size.as_deref(),
            },
            store: false,
        }
    }
}

#[derive(Deserialize)]
struct WireInteraction {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    steps: Vec<WireStep>,
    #[serde(default)]
    usage: WireUsage,
    #[serde(default)]
    errors: Vec<Value>,
}

#[derive(Deserialize)]
struct WireStep {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Vec<WireBlock>,
}

#[derive(Deserialize)]
struct WireBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    total_input_tokens: u64,
    #[serde(default)]
    total_cached_tokens: u64,
    #[serde(default)]
    total_output_tokens: u64,
    #[serde(default)]
    total_thought_tokens: u64,
    #[serde(default)]
    output_tokens_by_modality: Vec<WireModality>,
}

#[derive(Deserialize)]
struct WireModality {
    modality: String,
    tokens: u64,
}

impl WireUsage {
    fn usage(&self) -> Usage {
        let image = self
            .output_tokens_by_modality
            .iter()
            .filter(|entry| entry.modality.eq_ignore_ascii_case("image"))
            .map(|entry| entry.tokens)
            .sum::<u64>()
            .min(self.total_output_tokens);
        Usage {
            input_tokens: self
                .total_input_tokens
                .saturating_sub(self.total_cached_tokens),
            cache_read_input_tokens: self.total_cached_tokens,
            output_tokens: self.total_output_tokens - image + self.total_thought_tokens,
            output_image_tokens: image,
            ..Usage::default()
        }
    }
}

impl WireInteraction {
    fn into_response(self) -> Result<ImageResponse, LlmError> {
        if let Some(status) = self.status.as_deref() {
            if status != "completed" {
                let errors = serde_json::to_string(&self.errors).unwrap_or_default();
                return Err(LlmError::Parse(format!(
                    "gemini interaction ended {status}: {errors}"
                )));
            }
        }
        let usage = self.usage.usage();
        let mut parts = Vec::new();
        for block in self
            .steps
            .into_iter()
            .filter(|step| step.kind == "model_output")
            .flat_map(|step| step.content)
        {
            match block.kind.as_str() {
                "text" => parts.extend(block.text.map(Part::Text)),
                "image" => {
                    let Some(data) = block.data else {
                        continue;
                    };
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data.as_bytes())
                        .map_err(|e| LlmError::Parse(format!("gemini image data: {e}")))?;
                    parts.push(Part::Image(ImageData {
                        mime: block.mime_type.unwrap_or_else(|| "image/png".to_string()),
                        bytes,
                    }));
                }
                _ => {}
            }
        }
        let response = ImageResponse { parts, usage };
        if response.images().next().is_none() {
            let said = response
                .text()
                .map(|text| format!(": {text}"))
                .unwrap_or_default();
            return Err(LlmError::Unsupported(format!(
                "the model returned no image{said}"
            )));
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::stub::stub;
    use serde_json::json;

    fn png() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 1, 2, 3]
    }

    fn encoded(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn generated() -> String {
        json!({
            "id": "v1_x",
            "status": "completed",
            "steps": [
                {"type": "thought", "summary": []},
                {"type": "model_output", "content": [
                    {"type": "text", "text": "Here is your fox."},
                    {"type": "image", "mime_type": "image/png", "data": encoded(&png())}
                ]}
            ],
            "usage": {
                "total_input_tokens": 300,
                "total_cached_tokens": 100,
                "total_output_tokens": 1300,
                "total_thought_tokens": 40,
                "output_tokens_by_modality": [
                    {"modality": "text", "tokens": 10},
                    {"modality": "image", "tokens": 1290}
                ]
            }
        })
        .to_string()
    }

    fn request() -> ImageRequest {
        ImageRequest {
            model: "gemini-nano-banana-2.1".into(),
            parts: vec![
                Part::Text("a fox in the snow".into()),
                Part::Image(ImageData {
                    mime: "image/jpeg".into(),
                    bytes: vec![9, 9],
                }),
            ],
            aspect_ratio: Some("16:9".into()),
            size: Some("2K".into()),
        }
    }

    #[tokio::test]
    async fn generates_through_the_interactions_api() {
        let (base, seen) = stub(vec![(200, generated())]).await;
        let provider = GeminiProvider::new("key-1".into(), Some(base));
        let response = provider.generate(request()).await.unwrap();

        let seen = seen.lock().unwrap()[0].clone();
        assert_eq!(seen.path, "/v1beta/interactions");
        assert_eq!(seen.header("x-goog-api-key"), Some("key-1"));
        assert_eq!(
            seen.body,
            json!({
                "model": "gemini-nano-banana-2.1",
                "input": [
                    {"type": "text", "text": "a fox in the snow"},
                    {"type": "image", "mime_type": "image/jpeg", "data": encoded(&[9, 9])}
                ],
                "response_format": {"type": "image", "aspect_ratio": "16:9", "image_size": "2K"},
                "store": false
            })
        );
        assert_eq!(
            response.images().cloned().collect::<Vec<_>>(),
            vec![ImageData {
                mime: "image/png".into(),
                bytes: png()
            }]
        );
        assert_eq!(response.text().as_deref(), Some("Here is your fox."));
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 200,
                cache_read_input_tokens: 100,
                output_tokens: 50,
                output_image_tokens: 1290,
                ..Usage::default()
            }
        );
    }

    #[tokio::test]
    async fn a_reply_without_an_image_is_an_error_carrying_what_the_model_said() {
        let body = json!({
            "status": "completed",
            "steps": [{"type": "model_output", "content": [
                {"type": "text", "text": "I can't make that image."}
            ]}]
        })
        .to_string();
        let (base, _) = stub(vec![(200, body)]).await;
        let provider = GeminiProvider::new("k".into(), Some(base));
        let err = provider.generate(request()).await.unwrap_err().to_string();
        assert_eq!(err, "the model returned no image: I can't make that image.");
    }

    #[tokio::test]
    async fn a_failed_interaction_reports_its_errors() {
        let body = json!({"status": "failed", "errors": [{"message": "blocked"}]}).to_string();
        let (base, _) = stub(vec![(200, body)]).await;
        let provider = GeminiProvider::new("k".into(), Some(base));
        let err = provider.generate(request()).await.unwrap_err().to_string();
        assert!(
            err.contains("ended failed") && err.contains("blocked"),
            "{err}"
        );
    }

    #[test]
    fn older_models_take_no_size_and_lite_only_1k() {
        let provider = GeminiProvider::new("k".into(), None);
        assert!(provider.options("gemini-2.5-flash-image").sizes.is_empty());
        assert_eq!(
            provider.options("gemini-3.1-flash-lite-image").sizes,
            vec!["1K"]
        );
        assert_eq!(
            provider.options("gemini-3-pro-image").sizes,
            vec!["1K", "2K", "4K"]
        );
    }
}
