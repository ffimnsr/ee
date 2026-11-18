//! Output-token and compaction budgets.
//!
//! Limits come from the provider catalog. The helpers here fit an output bound
//! to the room a prompt leaves in the context window, estimate a prompt size
//! from serialized bytes when the provider has not reported one, and derive the
//! compaction trigger with room reserved for the summary the compaction call
//! itself produces.

use serde::{Deserialize, Serialize};

use crate::model::ModelMessage;
use crate::tools::ToolDefinition;

/// Output-token bound used when a model's catalog limit is unknown.
pub const OUTPUT_LIMIT_FALLBACK: u64 = 32_000;
/// Largest output-token bound ee requests, however large the catalog limit is.
pub const OUTPUT_LIMIT_MAX: u64 = 256_000;
/// Smallest useful output-token bound used when a context window has room.
pub const OUTPUT_LIMIT_MIN: u64 = 1_024;
/// Prompt bytes per estimated token (documented 4:1 heuristic).
pub const PROMPT_BYTES_PER_TOKEN: u64 = 4;
/// Margin applied to the estimated prompt before fitting output tokens.
pub const PROMPT_ESTIMATE_MARGIN_PERCENT: u64 = 16;
/// Fraction of the context window reserved for the compaction summary output.
pub const COMPACTION_SUMMARY_RESERVE_DIVISOR: u64 = 10;
/// Smallest summary reserve, so a small window still has room to summarize.
pub const COMPACTION_SUMMARY_RESERVE_FLOOR: u64 = 4_096;

/// Input-token threshold at which a session compacts before the next model call.
///
/// The threshold is `percent` of the context window minus room for the summary
/// the compaction call itself must produce, clamped to at least one token.
/// `percent == 0` (or an unknown window) disables automatic compaction.
#[must_use]
pub fn compaction_threshold_tokens(context_window: u64, percent: u8) -> Option<u64> {
    if percent == 0 || context_window == 0 {
        return None;
    }
    let threshold = context_window.saturating_mul(u64::from(percent)).div_ceil(100);
    let reserve = (context_window / COMPACTION_SUMMARY_RESERVE_DIVISOR)
        .max(COMPACTION_SUMMARY_RESERVE_FLOOR)
        .min(threshold / 2);
    Some(threshold.saturating_sub(reserve).max(1))
}

/// Catalog-declared context and output limits for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLimits {
    /// Total context window in tokens.
    pub context: u64,
    /// Maximum output tokens the model documents.
    pub output: u64,
}

impl ModelLimits {
    /// Builds limits from a context and output bound.
    #[must_use]
    pub const fn new(context: u64, output: u64) -> Self {
        Self { context, output }
    }
}

/// Fits an output-token bound to the room a prompt leaves in the context window.
///
/// Returns `None` when the model carries no catalog limit, so a caller can omit
/// the field instead of guessing. The requested bound is the catalog output
/// limit capped at [`OUTPUT_LIMIT_MAX`]; when the prompt size is known the
/// result also leaves [`PROMPT_ESTIMATE_MARGIN_PERCENT`] of headroom. A window
/// with no room yields [`OUTPUT_LIMIT_MIN`] rather than zero, matching the
/// reference agent: every provider rejects a tiny bound, and the prompt-size
/// estimate is approximate anyway.
#[must_use]
pub fn fit_output_limit(limits: Option<ModelLimits>, prompt_tokens: Option<u64>) -> Option<u64> {
    let limits = limits?;
    let requested = limits.output.clamp(1, OUTPUT_LIMIT_MAX);
    let Some(prompt) = prompt_tokens else {
        return Some(requested);
    };
    if limits.context == 0 {
        return Some(requested);
    }
    let margin = prompt.saturating_mul(PROMPT_ESTIMATE_MARGIN_PERCENT).saturating_div(100);
    let headroom = limits.context.saturating_div(100);
    let room =
        limits.context.saturating_sub(headroom).saturating_sub(prompt).saturating_sub(margin);
    Some(room.min(requested).max(OUTPUT_LIMIT_MIN))
}

/// Estimates prompt tokens from the normalized transcript, tools, and system
/// prompt using the documented 4-bytes-per-token heuristic.
#[must_use]
pub fn estimate_prompt_tokens(
    transcript: &[ModelMessage],
    tools: &[ToolDefinition],
    system_prompt: &str,
) -> u64 {
    let messages = serde_json::to_vec(transcript).map_or(0, |bytes| bytes.len() as u64);
    let tools = serde_json::to_vec(tools).map_or(0, |bytes| bytes.len() as u64);
    let system = system_prompt.len() as u64;
    messages
        .saturating_add(tools)
        .saturating_add(system)
        .saturating_div(PROMPT_BYTES_PER_TOKEN)
        .max(1)
}

/// Estimates prompt tokens from wire-level message and tool arrays.
#[must_use]
pub fn estimate_json_prompt_tokens(messages: &serde_json::Value, tools: &serde_json::Value) -> u64 {
    let messages = serde_json::to_vec(messages).map_or(0, |bytes| bytes.len() as u64);
    let tools = serde_json::to_vec(tools).map_or(0, |bytes| bytes.len() as u64);
    messages.saturating_add(tools).saturating_div(PROMPT_BYTES_PER_TOKEN).max(1)
}
