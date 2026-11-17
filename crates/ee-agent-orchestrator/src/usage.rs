//! Normalized token usage reported by a model adapter.
//!
//! `None` fields mean the adapter did not report them — treated as unknown (not
//! zero) by the budget tracker when a token cap is configured. The reference
//! agent reports a richer `Usage` (reasoning tokens, effective context size,
//! provider-reported cost); [`ModelUsage`] carries the subset the orchestrator
//! and ACP boundary can act on without changing the wire schema.

use serde::{Deserialize, Serialize};

/// Reported token usage for one model completion.
///
/// `None` fields mean the adapter did not report usage — treated as unknown
/// (not zero) by the budget tracker when a token cap is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelUsage {
    /// Input tokens consumed; `None` when the provider did not report them.
    pub input_tokens: Option<usize>,
    /// Output tokens produced; `None` when the provider did not report them.
    pub output_tokens: Option<usize>,
    /// Input tokens served from the provider's prompt cache (cache read).
    pub cached_input_tokens: Option<usize>,
    /// Input tokens written to the provider's prompt cache (cache write).
    pub cache_creation_input_tokens: Option<usize>,
    /// Hidden reasoning tokens; a subset of `output_tokens` when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<usize>,
    /// Effective context size of the final message iteration, when reported.
    ///
    /// Unlike token counters this is not summed across steps: the next request
    /// is sized by the latest context, not by every step's context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<usize>,
    /// Per-turn cost in integer micro-dollars; `None` when neither the provider
    /// nor the catalog priced the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
}

impl ModelUsage {
    /// Creates usage with no reported tokens.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the reported input-token count.
    #[must_use]
    pub fn with_input_tokens(mut self, tokens: usize) -> Self {
        self.input_tokens = Some(tokens);
        self
    }

    /// Sets the reported output-token count.
    #[must_use]
    pub fn with_output_tokens(mut self, tokens: usize) -> Self {
        self.output_tokens = Some(tokens);
        self
    }

    /// Sets the reported prompt-cache read token count.
    #[must_use]
    pub fn with_cached_input_tokens(mut self, tokens: usize) -> Self {
        self.cached_input_tokens = Some(tokens);
        self
    }

    /// Sets the reported prompt-cache write token count.
    #[must_use]
    pub fn with_cache_creation_input_tokens(mut self, tokens: usize) -> Self {
        self.cache_creation_input_tokens = Some(tokens);
        self
    }

    /// Sets the reported hidden-reasoning token count.
    #[must_use]
    pub fn with_reasoning_tokens(mut self, tokens: usize) -> Self {
        self.reasoning_tokens = Some(tokens);
        self
    }

    /// Sets the reported effective context size.
    #[must_use]
    pub fn with_context_tokens(mut self, tokens: usize) -> Self {
        self.context_tokens = Some(tokens);
        self
    }

    /// Sets the per-turn cost in integer micro-dollars.
    #[must_use]
    pub fn with_cost_micros(mut self, cost_micros: u64) -> Self {
        self.cost_micros = Some(cost_micros);
        self
    }

    /// Adds another round's usage, skipping unknown fields instead of counting
    /// them as zero.
    ///
    /// Counters saturate at `usize::MAX` with a warning instead of wrapping.
    /// `context_tokens` keeps the latest report: context is a snapshot, not a
    /// per-step total.
    #[must_use]
    pub fn accumulate(&self, other: &Self) -> Self {
        Self {
            input_tokens: add_tokens("input_tokens", self.input_tokens, other.input_tokens),
            output_tokens: add_tokens("output_tokens", self.output_tokens, other.output_tokens),
            cached_input_tokens: add_tokens(
                "cached_input_tokens",
                self.cached_input_tokens,
                other.cached_input_tokens,
            ),
            cache_creation_input_tokens: add_tokens(
                "cache_creation_input_tokens",
                self.cache_creation_input_tokens,
                other.cache_creation_input_tokens,
            ),
            reasoning_tokens: add_tokens(
                "reasoning_tokens",
                self.reasoning_tokens,
                other.reasoning_tokens,
            ),
            context_tokens: other.context_tokens.or(self.context_tokens),
            cost_micros: add_cost(self.cost_micros, other.cost_micros),
        }
    }

    /// Maps to the ACP per-turn usage when both input and output tokens are
    /// known; unknown stays unknown (never counted as zero).
    #[must_use]
    pub(crate) fn acp_usage(&self) -> Option<ee_agent_protocol::Usage> {
        let input_tokens = self.input_tokens?;
        let output_tokens = self.output_tokens?;
        let usage = ee_agent_protocol::Usage::new(
            input_tokens.saturating_add(output_tokens) as u64,
            input_tokens as u64,
            output_tokens as u64,
        );
        Some(usage)
            .map(|usage| match self.reasoning_tokens {
                Some(tokens) => usage.thought_tokens(tokens as u64),
                None => usage,
            })
            .map(|usage| match self.cached_input_tokens {
                Some(tokens) => usage.cached_read_tokens(tokens as u64),
                None => usage,
            })
            .map(|usage| match self.cache_creation_input_tokens {
                Some(tokens) => usage.cached_write_tokens(tokens as u64),
                None => usage,
            })
    }
}

/// Adds two optional token counters; unknown stays unknown, overflow saturates
/// with a warning.
fn add_tokens(field: &'static str, left: Option<usize>, right: Option<usize>) -> Option<usize> {
    match (left, right) {
        (Some(left), Some(right)) => match left.checked_add(right) {
            Some(total) => Some(total),
            None => {
                tracing::warn!(field, "token usage overflowed usize; saturating");
                Some(left.saturating_add(right))
            }
        },
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn add_cost(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => match left.checked_add(right) {
            Some(total) => Some(total),
            None => {
                tracing::warn!("cost_micros overflowed u64; saturating");
                Some(left.saturating_add(right))
            }
        },
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_stay_unknown() {
        let total = ModelUsage::new().with_input_tokens(10).accumulate(&ModelUsage::new());
        assert_eq!(total.input_tokens, Some(10));
        assert_eq!(total.output_tokens, None);
        assert_eq!(total.reasoning_tokens, None);
    }

    #[test]
    fn counters_accumulate_and_context_keeps_the_latest() {
        let first = ModelUsage::new()
            .with_input_tokens(100)
            .with_reasoning_tokens(5)
            .with_context_tokens(900);
        let second = ModelUsage::new().with_input_tokens(20).with_context_tokens(1_200);
        let total = first.accumulate(&second);
        assert_eq!(total.input_tokens, Some(120));
        assert_eq!(total.reasoning_tokens, Some(5));
        assert_eq!(total.context_tokens, Some(1_200), "context is a snapshot, not a sum");
    }

    #[test]
    fn accumulation_saturates_instead_of_wrapping() {
        let high = ModelUsage::new().with_input_tokens(usize::MAX - 1);
        let total = high.accumulate(&ModelUsage::new().with_input_tokens(usize::MAX));
        assert_eq!(total.input_tokens, Some(usize::MAX));
    }

    #[test]
    fn reported_reasoning_tokens_reach_the_acp_usage() {
        let usage = ModelUsage::new()
            .with_input_tokens(10)
            .with_output_tokens(20)
            .with_reasoning_tokens(7)
            .acp_usage()
            .expect("usage maps");
        assert_eq!(usage.thought_tokens, Some(7));
    }
}
