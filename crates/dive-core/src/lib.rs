//! Core runtime: the ReAct agent loop and the plan-based execution pipeline.
//!
//! Defines the `ToolRegistry` and `Store` traits implemented by dive-mcp
//! and dive-store respectively.

pub mod agent;
pub mod files;
pub mod format;
pub mod list_tools;
pub mod packs;
pub mod pipeline;
pub mod prompts;
pub mod schemas;
pub mod secrets;
pub mod shapes;
pub mod store;
pub mod template;
pub mod toolbox;
pub mod tools;
pub mod trace;
pub mod usage;
pub mod user_tools;
#[cfg(test)]
mod user_tools_tests;

pub use agent::{Agent, AgentError, EventSink, NullSink, RunStart, TeeSink, TurnOutcome};
pub use store::{
    conversation, message_entries, Attachment, EntryBody, EntryUsage, NewEntry, Stamp, Store,
    StoreError, ThreadEntry, ThreadMeta, ToolShape, USER_AUTHOR,
};
pub use tools::{
    AllowlistRegistry, CompositeRegistry, ExcludingRegistry, ServerIdentity, ToolDef, ToolError,
    ToolOutcome, ToolRegistry, ToolServer,
};
pub use usage::{compact_tokens, CallSite, ModelUsage, StepUsage, UsageLedger, UsageReport};
