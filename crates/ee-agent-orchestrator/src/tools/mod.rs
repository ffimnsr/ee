//! Tool types, the tool registry, the execution pipeline, and the
//! server-side tool trait.
//!
//! The model adapter returns [`ToolIntent`] values; the [`ToolExecutor`]
//! resolves them against the [`ToolRegistry`], validates argument shape,
//! gates them through the policy engine, executes them (with timeouts and
//! cancellation), emits the tool-call update lifecycle, and returns
//! normalized [`ToolResult`] values to the transcript.  Editor/client
//! operations are performed through the framework's `ClientBridge`;
//! server-side tools implement [`ServerTool`].
//!
//! Layout: [`definition`] holds the wire types, [`registry`] the server-side
//! tools and their lookup, and [`executor`] the end-to-end execution pipeline.

mod definition;
mod executor;
mod registry;
mod session_tools;
mod worktree_tools;

#[cfg(test)]
mod tests;

pub use definition::{
    SideEffectClass, TerminalWaitLimits, ToolCallContext, ToolDefinition, ToolErrorKind,
    ToolExecutionLogEntry, ToolFormat, ToolFuture, ToolIntent, ToolResult,
};
pub use executor::ToolExecutor;
pub(crate) use executor::emit_tool_outcome;
pub use registry::{ServerTool, ToolRegistry};
pub(crate) use registry::{canonical_tool_definitions, read_only_definitions};
pub(crate) use session_tools::{SessionToolContext, session_tools};
pub(crate) use worktree_tools::worktree_tools;
