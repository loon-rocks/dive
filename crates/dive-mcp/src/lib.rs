//! MCP client layer: server lifecycle, tool discovery, and adaptation of
//! MCP tools into the core `ToolRegistry`.

mod files;
mod manager;
mod stderr;

pub use manager::{McpManager, NAMESPACE_SEPARATOR};
