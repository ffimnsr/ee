//! Opaque provider-side compaction checkpoints.
//!
//! A provider that implements native compaction (`POST /responses/compact` on
//! the OpenAI Responses protocol) returns an encrypted item that replaces the
//! conversation prefix. The payload is only meaningful to the provider that
//! produced it and must be replayed byte-for-byte, so it is wrapped here and
//! never parsed, logged, or rendered as text.
//!
//! The checkpoint travels as an ordinary transcript content part
//! ([`crate::ModelContent::CompactionCheckpoint`]) and is re-emitted by the
//! codec that understands it. Every other codec ignores it, so a checkpoint can
//! never leak into a text prompt or another provider's request.

use serde::{Deserialize, Serialize};

use crate::model::ModelMessage;
use crate::usage::ModelUsage;

/// Largest accepted serialized checkpoint payload, in bytes.
///
/// A checkpoint stands in for a conversation prefix, so it is larger than the
/// reasoning-state bound ([`crate::OpaqueProviderState::MAX_BYTES`]). Values
/// above this bound are dropped rather than replayed truncated, because a
/// truncated checkpoint is invalid to the provider.
pub const MAX_CHECKPOINT_BYTES: usize = 8 * 1024 * 1024;

/// Encrypted conversation checkpoint produced by provider-side compaction.
///
/// `Debug` prints only the provider and id so a transcript, metric, or error
/// dump can never disclose the encrypted conversation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionCheckpoint {
    /// Provider label that produced the checkpoint (diagnostic only).
    pub provider: String,
    /// Provider item id, when one was reported.
    pub id: Option<String>,
    /// Provider-encrypted payload, replayed verbatim.
    encrypted: String,
}

impl CompactionCheckpoint {
    /// Wraps a checkpoint when it fits [`MAX_CHECKPOINT_BYTES`].
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        id: Option<String>,
        encrypted: impl Into<String>,
    ) -> Option<Self> {
        let encrypted = encrypted.into();
        (!encrypted.is_empty() && encrypted.len() <= MAX_CHECKPOINT_BYTES).then_some(Self {
            provider: provider.into(),
            id,
            encrypted,
        })
    }

    /// Borrows the encrypted payload for verbatim replay.
    #[must_use]
    pub fn encrypted(&self) -> &str {
        &self.encrypted
    }

    /// Serialized byte size of the encrypted payload.
    #[must_use]
    pub fn len(&self) -> usize {
        self.encrypted.len()
    }

    /// Whether the payload is empty (never true for a constructed checkpoint).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.encrypted.is_empty()
    }
}

impl std::fmt::Debug for CompactionCheckpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompactionCheckpoint")
            .field("provider", &self.provider)
            .field("id", &self.id)
            .field("encrypted", &"[opaque]")
            .finish()
    }
}

/// Result of one provider-side compaction call.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionOutcome {
    /// Replacement transcript the provider returned: summary text, reasoning
    /// items, and the opaque checkpoint, in provider order.
    pub replacement: Vec<ModelMessage>,
    /// The encrypted checkpoint when the provider returned one.
    pub checkpoint: Option<CompactionCheckpoint>,
    /// Reported token usage for the compaction request.
    pub usage: ModelUsage,
}

impl CompactionOutcome {
    /// Creates an outcome from its parts.
    #[must_use]
    pub fn new(
        replacement: Vec<ModelMessage>,
        checkpoint: Option<CompactionCheckpoint>,
        usage: ModelUsage,
    ) -> Self {
        Self { replacement, checkpoint, usage }
    }

    /// Concatenated text of the replacement, used as the continuation summary.
    #[must_use]
    pub fn summary_text(&self) -> String {
        self.replacement
            .iter()
            .filter(|message| message.role != crate::ModelRole::System)
            .map(ModelMessage::text_content)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_and_empty_checkpoints_are_dropped() {
        assert!(CompactionCheckpoint::new("openai", None, "").is_none());
        let oversized = "x".repeat(MAX_CHECKPOINT_BYTES + 1);
        assert!(CompactionCheckpoint::new("openai", None, oversized).is_none());
    }

    #[test]
    fn debug_never_prints_the_encrypted_payload() {
        let checkpoint =
            CompactionCheckpoint::new("openai", Some(String::from("cmp-1")), "c2VjcmV0")
                .expect("checkpoint");
        let rendered = format!("{checkpoint:?}");
        assert!(rendered.contains("[opaque]"), "{rendered}");
        assert!(!rendered.contains("c2VjcmV0"), "{rendered}");
    }

    #[test]
    fn checkpoint_serializes_without_losing_the_payload() {
        let checkpoint =
            CompactionCheckpoint::new("openai", Some(String::from("cmp-1")), "payload")
                .expect("checkpoint");
        let encoded = serde_json::to_string(&checkpoint).expect("serializes");
        let restored: CompactionCheckpoint = serde_json::from_str(&encoded).expect("parses");
        assert_eq!(restored, checkpoint);
        assert_eq!(restored.encrypted(), "payload");
    }
}
