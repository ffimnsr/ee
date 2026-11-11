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
#[must_use]
pub(super) fn should_compact(
    estimated_tokens: usize,
    context_window_tokens: u64,
    threshold_percent: u8,
) -> bool {
    if threshold_percent == 0 || context_window_tokens == 0 {
        return false;
    }
    let window = context_window_tokens as u128;
    let threshold = (window * u128::from(threshold_percent)).div_ceil(100);
    estimated_tokens as u128 >= threshold
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelRole;

    fn message(text: &str) -> ModelMessage {
        ModelMessage::text(ModelRole::User, text)
    }

    #[test]
    fn threshold_is_percentage_of_window() {
        assert!(should_compact(80_000, 100_000, 80));
        assert!(!should_compact(79_999, 100_000, 80));
        assert!(!should_compact(100_000, 100_000, 0), "zero disables");
    }

    #[test]
    fn estimate_uses_text_bytes() {
        assert_eq!(estimated_tokens(&[message("1234")]), 1);
        assert_eq!(estimated_tokens(&[message("12345")]), 2);
    }
}
