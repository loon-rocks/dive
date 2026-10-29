mod chat;

pub const TOOL_FORMAT: &str = include_str!("tool_format.md");

pub const AGENT_FORMAT: &str = include_str!("agent_format.md");

pub const DESIGN_RULES: &str = include_str!("design_rules.md").trim_ascii_end();

pub const FILE_HANDLING: &str = include_str!("file_handling.md").trim_ascii_end();

pub const FILES_RULE: &str = include_str!("files_rule.md").trim_ascii_end();

pub use chat::{chat_system_prompt, user_context_section, DEFAULT_CHAT_PROMPT};
