//! Automatic conversation-compaction trigger for orchestrator sessions.
//!
//! The provider owns the cross-turn conversation tail; the orchestrator owns
//! the durable summary in memory. Estimation is deliberately conservative and
//! byte-based because the provider does not receive token usage until after a
//! model call. `auto_threshold_percent == 0` disables the trigger.

use std::sync::Mutex;

use crate::model::ModelMessage;

use super::ConversationMessage;

/// Rough token estimate for a conversation tail. Uses four bytes per token,
/// which intentionally over-estimates for code-heavy sessions.
#[must_use]
pub(super) fn estimated_tokens(messages: &[ModelMessage]) -> usize {
    let bytes: usize = messages
        .iter()
        .map(|message| {
            message.text_content().len() + message.reasoning_summary.as_deref().map_or(0, str::len)
        })
        .sum();
    bytes.div_ceil(4)
}

/// Whether the estimated context use crosses the configured threshold.
///
/// The threshold leaves room for the summary the compaction call itself must
/// produce, so compacting never leaves the next request with too little output
/// headroom. `0` disables the trigger.
#[must_use]
pub(super) fn should_compact(
    estimated_tokens: usize,
    context_window_tokens: u64,
    threshold_percent: u8,
) -> bool {
    let Some(threshold) =
        crate::model::compaction_threshold_tokens(context_window_tokens, threshold_percent)
    else {
        return false;
    };
    estimated_tokens as u64 >= threshold
}

/// Keeps only the newest `retained` conversation messages.
pub(super) fn trim_conversation(conversation: &Mutex<Vec<ConversationMessage>>, retained: usize) {
    let retained = retained.max(1);
    let mut conversation = conversation.lock().expect("conversation poisoned");
    let excess = conversation.len().saturating_sub(retained);
    if excess > 0 {
        conversation.drain(..excess);
    }
}

/// Keeps only the newest `retained` normalized model messages, matching the
/// conversation tail trimmed after a successful compaction.
pub(super) fn trim_model_transcript(transcript: &Mutex<Vec<ModelMessage>>, retained: usize) {
    let retained = retained.max(1);
    let mut transcript = transcript.lock().expect("model transcript poisoned");
    let excess = transcript.len().saturating_sub(retained);
    if excess > 0 {
        transcript.drain(..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelRole;

    fn message(text: &str) -> ModelMessage {
        ModelMessage::text(ModelRole::User, text)
    }

    #[test]
    fn threshold_is_percentage_of_window_minus_summary_reserve() {
        // 80% of 100k is 80k, minus a 10k summary reserve.
        assert!(!should_compact(69_999, 100_000, 80));
        assert!(should_compact(70_000, 100_000, 80));
        assert!(!should_compact(100_000, 100_000, 0), "zero disables");
    }

    #[test]
    fn tiny_window_reserves_at_most_half_the_threshold() {
        // The floor (4k) would exceed an 8k window's 80% threshold, so the
        // reserve is clamped to half the threshold instead of disabling it.
        assert_eq!(crate::model::compaction_threshold_tokens(8_000, 80), Some(3_200));
    }

    #[test]
    fn estimate_uses_text_bytes() {
        assert_eq!(estimated_tokens(&[message("1234")]), 1);
        assert_eq!(estimated_tokens(&[message("12345")]), 2);
    }
}
