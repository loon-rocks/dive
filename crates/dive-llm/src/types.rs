//! Provider-neutral chat types.

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One message in a conversation, shaped to map onto both the Anthropic
/// content-block model and the OpenAI tool-call model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatMessage {
    User {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        /// Provider reasoning blocks, kept verbatim so they can be echoed
        /// back unchanged on the next turn — the documented multi-turn
        /// pattern, and what lets a tool-calling loop keep its own reasoning
        /// across rounds instead of re-deriving it. Opaque on purpose: the
        /// API rejects blocks whose content has been modified, so Dive
        /// stores them exactly as received and never inspects them.
        ///
        /// Empty for providers with no such concept, and `serde(default)`
        /// so threads written before this field stay readable.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        thinking: Vec<Value>,
    },
    /// The result of executing one tool call, fed back to the model.
    ToolResult {
        tool_call_id: String,
        /// JSON result (objects preferred; plain text wrapped as a string).
        content: Value,
        #[serde(default)]
        is_error: bool,
    },
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage::User {
            content: content.into(),
            images: Vec::new(),
        }
    }
}

pub fn user_text(content: &str, images: &[String]) -> String {
    let unnamed: Vec<&str> = images
        .iter()
        .map(String::as_str)
        .filter(|image| !content.contains(image))
        .collect();
    if unnamed.is_empty() {
        return content.to_string();
    }
    let listed = unnamed.join(", ");
    if content.is_empty() {
        format!("Attached images: {listed}")
    } else {
        format!("{content}\n\nAttached images: {listed}")
    }
}

pub fn images_in<'a>(
    text: &str,
    images: &'a std::collections::BTreeMap<String, crate::image::ImageData>,
) -> Vec<&'a crate::image::ImageData> {
    images
        .iter()
        .filter(|(reference, _)| text.contains(reference.as_str()))
        .map(|(_, image)| image)
        .collect()
}

pub fn base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A tool made visible to the model via its native tool-use API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool input.
    pub input_schema: Value,
    #[serde(default)]
    pub deferred: bool,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Structured-output request: forces the model to produce JSON matching
/// `schema`. Providers enforce this natively where possible.
#[derive(Debug, Clone)]
pub struct ResponseSchema {
    pub name: String,
    pub schema: Value,
}

/// Whether the provider should place prompt-cache breakpoints.
///
/// Defaults to [`CacheHint::Auto`] because the economics are lopsided: a
/// missed hit re-bills the whole prefix at full rate, while a wasted write
/// costs 1.25x on one call's input, once — and a prefix under the provider's
/// minimum simply isn't cached at all, at no charge. Opting in per call site
/// would mean every future site is uncached until someone remembers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheHint {
    /// Cache the stable prefix, and the conversation tail on multi-turn
    /// requests. What that means concretely is the provider's business.
    #[default]
    Auto,
    /// Send no breakpoints. For measuring what caching is worth, and for a
    /// prefix known to differ on every call.
    Off,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// When set, the provider forces schema-conforming JSON output and the
    /// response carries it in `ChatResponse::structured`.
    pub response_schema: Option<ResponseSchema>,
    /// Prompt-caching policy. Honoured by providers with an explicit cache
    /// API (Anthropic); providers that cache automatically ignore it.
    pub cache: CacheHint,
    pub tool_references: std::collections::BTreeMap<String, Vec<String>>,
    pub images: std::collections::BTreeMap<String, crate::image::ImageData>,
    pub withheld_images: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    #[default]
    Other,
}

/// Token counts for one model call.
///
/// The two cache fields are how prompt caching is priced *and* how it is
/// verified: a cache that never reads back reports no error, it just leaves
/// `cache_read_input_tokens` at zero forever. Providers that don't report a
/// field leave it zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input tokens, billed at the model's full input rate. Does
    /// **not** include the two cache figures below — the prompt's total size
    /// is the sum of all three.
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub output_image_tokens: u64,
    /// Tokens written to the cache this call (billed at a premium — 1.25x
    /// input on Anthropic's 5-minute TTL).
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Tokens served from the cache this call (billed at ~0.1x input).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// Accumulate another call's counts into this one.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.output_image_tokens += other.output_image_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }

    /// Every input token the prompt carried, however it was billed.
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }

    pub fn total_output_tokens(&self) -> u64 {
        self.output_tokens + self.output_image_tokens
    }
}

impl From<&Usage> for dive_config::TokenCounts {
    fn from(usage: &Usage) -> Self {
        Self {
            input: usage.input_tokens,
            output: usage.output_tokens,
            output_image: usage.output_image_tokens,
            cache_write: usage.cache_creation_input_tokens,
            cache_read: usage.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Reasoning blocks as the provider sent them. Echo back verbatim in the
    /// next turn's assistant message; never edit or reorder them.
    #[serde(default)]
    pub thinking: Vec<Value>,
    /// Present when the request carried a `response_schema`.
    pub structured: Option<Value>,
    pub stop_reason: StopReason,
    #[serde(default)]
    pub usage: Usage,
}

/// Streaming events. Providers assemble tool-call JSON internally and deliver
/// complete calls in the final `Completed` response; only text streams.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    /// The model started emitting a tool call (name known, arguments pending).
    ToolCallStarted {
        name: String,
    },
    Completed(ChatResponse),
}

pub type EventStream = BoxStream<'static, Result<StreamEvent, crate::LlmError>>;
