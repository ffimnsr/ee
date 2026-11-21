//! Normalized model messages, the transcript builder, and the
//! provider-supplied model adapter trait.
//!
//! The orchestrator never sees provider-specific JSON: adapters consume a
//! normalized [`ModelRequest`] (transcript, tool schemas, budget snapshot,
//! current task) and return a normalized [`ModelResponse`] (text, reasoning,
//! tool intents, completion signal).  All message and response types are
//! serializable and deterministic for tests and future persistence.
//!
//! [`Transcript`] normalizes ACP prompt content into a deterministic
//! `ModelMessage` sequence and keeps the transcript inside the configured
//! memory budget by dropping the oldest messages first, recording truncation
//! metadata so later phases can surface it.
//!
//! Layout: `message` holds roles and content blocks, `request` and
//! `response` the two wire shapes, `transcript` the conversation builder, and
//! `error` the adapter failure type.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::watch;

pub use crate::compaction_checkpoint::{CompactionCheckpoint, CompactionOutcome};
pub use crate::context_budget::{
    COMPACTION_SUMMARY_RESERVE_DIVISOR, COMPACTION_SUMMARY_RESERVE_FLOOR, ModelLimits,
    OUTPUT_LIMIT_FALLBACK, OUTPUT_LIMIT_MAX, OUTPUT_LIMIT_MIN, PROMPT_BYTES_PER_TOKEN,
    PROMPT_ESTIMATE_MARGIN_PERCENT, compaction_threshold_tokens, estimate_json_prompt_tokens,
    estimate_prompt_tokens, fit_output_limit,
};
pub use crate::model_options::{ResponseFormat, ToolCallControl, ToolChoice, validate_tool_names};
pub use crate::opaque_state::OpaqueProviderState;

mod error;
mod message;
mod request;
mod response;
mod transcript;

#[cfg(test)]
mod tests;

pub use error::ModelError;
pub use message::{
    CACHE_BREAKPOINT_METADATA_KEY, CACHE_BREAKPOINT_METADATA_VALUE, MAX_METADATA_ENTRIES,
    ModelContent, ModelMessage, ModelRole, PROVIDER_EXECUTED_METADATA_PREFIX,
    PROVIDER_EXECUTED_RESULT_KEY, PROVIDER_EXECUTED_RESULT_PROVIDER_KEY, TOOL_NAME_METADATA_KEY,
    TOOL_PATH_METADATA_KEY, TOOL_RESULT_PRUNE_HEAD, TOOL_RESULT_PRUNE_MARKER,
    TOOL_RESULT_PRUNE_TAIL, TOOL_RESULT_PRUNE_THRESHOLD, TOOL_SIDE_EFFECT_METADATA_KEY,
};
pub(crate) use request::prompt_result_with_usage;
pub use request::{Continuation, ModelRequest, ModelUsage};
pub use response::ModelResponse;
pub(crate) use transcript::message_bytes;
pub use transcript::{Transcript, TranscriptTruncation};

/// Boxed future returned by [`ModelAdapter`] methods.
///
/// The box keeps the trait object-safe without depending on `async-trait`.
pub type ModelFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Provider-supplied model interface.
///
/// Implementations must be `Send + Sync + 'static` and cancellation-aware:
/// `cancel` flips when the turn is cancelled, and adapters should stop
/// promptly instead of returning late results.
pub trait ModelAdapter: Send + Sync + 'static {
    /// Completes one normalized request.
    fn complete(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>>;

    /// Completes one request while forwarding displayable deltas as they arrive.
    ///
    /// Adapters without native streaming keep the same client contract by
    /// emitting their completed reasoning and text as single chunks.
    fn complete_streaming(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
        events: crate::streaming::StreamSink,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let completion = self.complete(request, cancel);
        Box::pin(async move {
            let response = completion.await?;
            if let Some(reasoning) = response.reasoning.as_deref().filter(|text| !text.is_empty()) {
                events.reasoning(reasoning.to_string())?;
            }
            if !response.text.is_empty() {
                events.text(response.text.clone())?;
            }
            Ok(response)
        })
    }

    /// Runs one provider-native compaction request.
    ///
    /// This is the single provider-compaction entry point; there is no second
    /// compaction API on the orchestrator surface. The default fails closed: a
    /// route that does not implement provider compaction must say so instead of
    /// silently returning a local summary. Adapters that support native
    /// compaction return the provider replacement transcript plus the opaque
    /// checkpoint it carries.
    fn compact(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<CompactionOutcome, ModelError>> {
        let _ = cancel;
        let model = request.model_id.unwrap_or_else(|| String::from("unknown model"));
        Box::pin(async move { Err(ModelError::UnsupportedCompaction(model)) })
    }
}
