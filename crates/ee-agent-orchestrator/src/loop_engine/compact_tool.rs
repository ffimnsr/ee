//! Model-callable session compaction.
//!
//! `/compact` is a client slash command, so the model never sees it. This
//! module adds a matching `compact` tool that the loop advertises when the
//! runtime is reachable through a shared handle; the tool runs the same
//! compaction pipeline and stores the continuation summary in session memory.

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use crate::model::ModelMessage;
use crate::tools::{SideEffectClass, ToolDefinition};

/// Tool name advertised to the model for self-initiated compaction.
pub(crate) const COMPACT_TOOL_NAME: &str = "compact";

/// Bounded result returned to the model after a compaction run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactToolOutcome {
    /// Human-readable status text for the tool result.
    pub report: String,
}

/// Boxed compaction future produced by the runtime-owned hook.
pub(crate) type CompactToolFuture =
    Pin<Box<dyn Future<Output = Result<CompactToolOutcome, String>> + Send>>;

type CompactToolFn = dyn Fn(Vec<ModelMessage>, Option<String>) -> CompactToolFuture + Send + Sync;

/// Runtime-owned hook that executes one compaction request.
#[derive(Clone)]
pub(crate) struct CompactToolHook {
    run: Arc<CompactToolFn>,
}

impl fmt::Debug for CompactToolHook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("CompactToolHook").finish_non_exhaustive()
    }
}

impl CompactToolHook {
    /// Creates a hook from a closure that receives the transcript snapshot and
    /// optional instructions.
    pub(crate) fn new<F>(run: F) -> Self
    where
        F: Fn(Vec<ModelMessage>, Option<String>) -> CompactToolFuture + Send + Sync + 'static,
    {
        Self { run: Arc::new(run) }
    }

    /// Runs the compaction hook.
    pub(crate) async fn invoke(
        &self,
        history: Vec<ModelMessage>,
        instructions: Option<String>,
    ) -> Result<CompactToolOutcome, String> {
        (self.run)(history, instructions).await
    }
}

/// Definition of the model-callable `compact` tool.
pub(crate) fn compact_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        COMPACT_TOOL_NAME,
        "Compacts the session into a continuation summary stored in session memory. \
         Use when the user asks to compact, or when the conversation is growing too long. \
         Optional instructions say what the summary must preserve.",
    )
    .input_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "instructions": { "type": "string" }
        }
    }))
    // Compaction mutates session memory, not the workspace; keep it out of the
    // write class so read-only modes can still compact.
    .side_effect_class(SideEffectClass::Read)
}
