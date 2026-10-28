//! MCP client layer: server lifecycle, tool discovery, and adaptation of
//! MCP tools into the core `ToolRegistry`.

mod files;
mod manager;
pub mod oauth;
mod stderr;

pub use manager::{McpManager, NAMESPACE_SEPARATOR};
pub use oauth::{NeedsApp, SignIn, SignInScope, SignIns, APP_CALLBACK};
