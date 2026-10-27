mod anthropic;
mod gemini;
mod image_fit;
mod openai_compat;
#[cfg(test)]
pub(crate) mod stub;
mod systemone;

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use openai_compat::OpenAiCompatProvider;
pub use systemone::SystemOneProvider;

pub const fn gemini_base_url() -> &'static str {
    gemini::DEFAULT_BASE_URL
}
