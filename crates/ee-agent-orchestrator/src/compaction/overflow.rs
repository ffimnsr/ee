//! In-turn compaction for a provider context-overflow retry.
//!
//! A provider can reject a prompt as too large mid-turn. The reference agent
//! compacts once and retries once; a second overflow ends the turn. Compaction
//! here is deterministic and synchronous — the loop cannot make a second model
//! call to summarize a prompt the model just refused.
//!
//! Stages, in order: lossless pruning of superseded read-only results, offload
//! of stale binary multimodal payloads, then dropping the oldest half of the
//! removable messages. Every stage either preserves the message count or drops
//! a prefix, so `Transcript::log_ids` stays aligned and the durable session log
//! keeps the bytes the projection replaced.

use crate::model::{Transcript, message_bytes};

use super::pipeline::{offload_stale_multimodal, prune_redundant_readonly_tool_outputs};

/// What one overflow compaction changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverflowCompaction {
    /// Bytes removed from retained messages by pruning and offload.
    pub removed_bytes: usize,
    /// Binary multimodal blocks replaced with text placeholders.
    pub offloaded_multimodal: usize,
    /// Oldest messages dropped.
    pub dropped_messages: usize,
    /// Messages retained after compaction.
    pub retained_messages: usize,
}

impl OverflowCompaction {
    /// Whether the compaction actually shrank the request.
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.removed_bytes > 0 || self.dropped_messages > 0
    }
}

/// Compacts `transcript` in place for one overflow retry.
///
/// Returns the accounting when the transcript shrank, and `None` when nothing
/// could be removed — the caller must then end the turn instead of retrying the
/// same prompt again.
pub fn compact_transcript_for_overflow(
    transcript: &mut Transcript,
    keep_recent_multimodal_turns: usize,
) -> Option<OverflowCompaction> {
    let before_bytes: usize = transcript.messages.iter().map(message_bytes).sum();
    let pruned = prune_redundant_readonly_tool_outputs(std::mem::take(&mut transcript.messages));
    let (offloaded, offloaded_multimodal) =
        offload_stale_multimodal(pruned, keep_recent_multimodal_turns);
    transcript.messages = offloaded;
    let retained_after_rewrite = transcript.messages.len();
    let after_rewrite_bytes: usize = transcript.messages.iter().map(message_bytes).sum();
    // `shrink_oldest` drops a prefix and keeps the system prompt and the newest
    // message, so the surviving tail can never start with an orphan tool result.
    let dropped_messages = if transcript.shrink_oldest() {
        retained_after_rewrite.saturating_sub(transcript.messages.len())
    } else {
        0
    };
    let report = OverflowCompaction {
        removed_bytes: before_bytes.saturating_sub(after_rewrite_bytes),
        offloaded_multimodal,
        dropped_messages,
        retained_messages: transcript.messages.len(),
    };
    report.changed().then_some(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelMessage, ModelRole};
    use crate::tools::ToolResult;

    fn user(text: &str) -> ModelMessage {
        ModelMessage::text(ModelRole::User, text)
    }

    #[test]
    fn drops_oldest_half_and_keeps_the_newest_message() {
        let mut transcript = Transcript::new();
        transcript.messages = (0..10).map(|index| user(&format!("m{index}"))).collect();
        let report = compact_transcript_for_overflow(&mut transcript, 2).expect("compacts");
        assert_eq!(report.dropped_messages, 5);
        assert_eq!(transcript.messages.len(), 5);
        assert_eq!(transcript.messages.last().expect("newest").text_content(), "m9");
    }

    #[test]
    fn nothing_removable_reports_none() {
        let mut transcript = Transcript::new();
        transcript.messages = vec![user("only")];
        assert!(compact_transcript_for_overflow(&mut transcript, 2).is_none());
    }

    #[test]
    fn superseded_read_only_results_are_pruned_before_dropping() {
        use crate::model::{TOOL_PATH_METADATA_KEY, TOOL_SIDE_EFFECT_METADATA_KEY};
        let mut transcript = Transcript::new();
        let mut first = ModelMessage::tool_result("tc-1", ToolResult::success("a".repeat(4_000)));
        first.metadata.insert(TOOL_PATH_METADATA_KEY.into(), "src/lib.rs".into());
        first.metadata.insert(TOOL_SIDE_EFFECT_METADATA_KEY.into(), "read".into());
        let mut second = ModelMessage::tool_result("tc-2", ToolResult::success("short tail"));
        second.metadata.insert(TOOL_PATH_METADATA_KEY.into(), "src/lib.rs".into());
        second.metadata.insert(TOOL_SIDE_EFFECT_METADATA_KEY.into(), "read".into());
        transcript.messages = vec![user("read it"), first, second];
        let report = compact_transcript_for_overflow(&mut transcript, 2).expect("compacts");
        assert!(report.removed_bytes > 3_000, "pruned the superseded read");
    }

    #[test]
    fn system_prefix_survives_compaction() {
        let mut transcript = Transcript::new();
        let mut messages = vec![ModelMessage::text(ModelRole::System, "policy")];
        messages.extend((0..6).map(|index| user(&format!("m{index}"))));
        transcript.messages = messages;
        compact_transcript_for_overflow(&mut transcript, 2).expect("compacts");
        assert_eq!(transcript.messages[0].role, ModelRole::System);
        assert_eq!(transcript.messages[0].text_content(), "policy");
    }
}
