//! Session-update ordering helpers for ACP v1.
//!
//! ACP v1 streams prompt turns through `session/update` notifications.
//! This tracker validates the ordering contract:
//!
//! - Message chunks (`user_message_chunk`, `agent_message_chunk`,
//!   `agent_thought_chunk`) may carry an opaque `messageId`; a chunk with a
//!   new id starts a new message, and a chunk with a known id extends it.
//!   Chunks without an id are valid and belong to the most recent message.
//! - `tool_call` announces a new `toolCallId`.
//! - `tool_call_update` referencing an unknown `toolCallId` is only valid
//!   when it carries enough fields to construct the tool call (ACP v1 marks
//!   this upsert valid); otherwise it fails closed with `invalid params`.
//!
//! SDK gap: the official SDK documents ordering guarantees in its `concepts`
//! module but ships no order-tracking type, so this tracker is ee-owned
//! (see `ordering` tests for the covered contract).
//!
//! The tracker only inspects wire values; it never buffers content.

use std::collections::BTreeSet;

use agent_client_protocol::schema::v1::{ContentChunk, SessionUpdate, ToolCall, ToolCallUpdate};

use crate::Error;

/// Tracks known message and tool-call ids for one session's update stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionUpdateOrder {
    message_ids: BTreeSet<String>,
    tool_call_ids: BTreeSet<String>,
}

impl SessionUpdateOrder {
    /// Creates an empty tracker for a new session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears all tracked ids (e.g. after `session/load` replays or session
    /// resets).
    pub fn reset(&mut self) {
        self.message_ids.clear();
        self.tool_call_ids.clear();
    }

    /// Returns whether `id` has been seen on a message chunk.
    #[must_use]
    pub fn message_known(&self, id: &str) -> bool {
        self.message_ids.contains(id)
    }

    /// Returns whether `id` has been seen on a `tool_call` update.
    #[must_use]
    pub fn tool_call_known(&self, id: &str) -> bool {
        self.tool_call_ids.contains(id)
    }

    /// Returns all known message ids, in insertion order.
    pub fn known_message_ids(&self) -> impl Iterator<Item = &str> {
        self.message_ids.iter().map(String::as_str)
    }

    /// Returns all known tool call ids, in insertion order.
    pub fn known_tool_call_ids(&self) -> impl Iterator<Item = &str> {
        self.tool_call_ids.iter().map(String::as_str)
    }

    /// Registers an update, rejecting chunks that reference unknown ids
    /// unless ACP v1 marks them valid.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] with code [`ErrorCode::InvalidParams`] when a
    /// `tool_call_update` references an unknown `toolCallId` and lacks the
    /// fields needed to construct the tool call.
    pub fn register_update(&mut self, update: &SessionUpdate) -> std::result::Result<(), Error> {
        match update {
            SessionUpdate::UserMessageChunk(chunk)
            | SessionUpdate::AgentMessageChunk(chunk)
            | SessionUpdate::AgentThoughtChunk(chunk) => {
                self.register_message_chunk(chunk);
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.register_tool_call(tool_call);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.register_tool_call_update(update)?;
            }
            SessionUpdate::Plan(_)
            | SessionUpdate::AvailableCommandsUpdate(_)
            | SessionUpdate::CurrentModeUpdate(_)
            | SessionUpdate::ConfigOptionUpdate(_)
            | SessionUpdate::SessionInfoUpdate(_)
            | SessionUpdate::UsageUpdate(_) => {}
            // `SessionUpdate` is non-exhaustive upstream; unknown future
            // variants are accepted without ordering guarantees.
            _ => {}
        }
        Ok(())
    }

    fn register_message_chunk(&mut self, chunk: &ContentChunk) {
        if let Some(message_id) = &chunk.message_id {
            self.message_ids.insert(message_id.0.to_string());
        }
    }

    fn register_tool_call(&mut self, tool_call: &ToolCall) {
        self.tool_call_ids.insert(tool_call.tool_call_id.0.to_string());
    }

    fn register_tool_call_update(
        &mut self,
        update: &ToolCallUpdate,
    ) -> std::result::Result<(), Error> {
        let id = update.tool_call_id.0.to_string();
        if self.tool_call_ids.contains(&id) {
            return Ok(());
        }
        // ACP v1 allows an update to construct a tool call it references, but
        // only when the update carries the required fields (title).
        let constructible = update.fields.title.is_some();
        if !constructible {
            return Err(Error::invalid_params().data(serde_json::json!({
                "toolCallUpdate": id,
                "reason": "references unknown toolCallId and lacks the title required to construct the tool call",
            })));
        }
        self.tool_call_ids.insert(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;
    use agent_client_protocol::schema::v1::{
        ContentBlock, TextContent, ToolCallId, ToolCallUpdateFields,
    };

    fn chunk(message_id: Option<&str>) -> ContentChunk {
        let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("hi")));
        if let Some(id) = message_id {
            chunk = chunk.message_id(id);
        }
        chunk
    }

    #[test]
    fn message_chunks_register_and_extend_messages() {
        let mut order = SessionUpdateOrder::new();
        assert!(!order.message_known("msg_1"));

        order.register_update(&SessionUpdate::AgentMessageChunk(chunk(Some("msg_1")))).unwrap();
        assert!(order.message_known("msg_1"));

        // Same id extends the message; a new id starts a new message.
        order.register_update(&SessionUpdate::AgentMessageChunk(chunk(Some("msg_1")))).unwrap();
        order.register_update(&SessionUpdate::AgentThoughtChunk(chunk(Some("msg_2")))).unwrap();
        assert!(order.message_known("msg_2"));

        // Chunks without an id are always valid.
        order.register_update(&SessionUpdate::UserMessageChunk(chunk(None))).unwrap();
    }

    #[test]
    fn tool_call_update_unknown_id_requires_constructible_fields() {
        let mut order = SessionUpdateOrder::new();

        // Unknown id without title fails closed.
        let update =
            ToolCallUpdate::new(ToolCallId::new("call_1"), ToolCallUpdateFields::default());
        let err = order.register_update(&SessionUpdate::ToolCallUpdate(update)).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);

        // Unknown id with title is valid (ACP v1 upsert) and gets registered.
        let update = ToolCallUpdate::new(
            ToolCallId::new("call_1"),
            ToolCallUpdateFields::new().title("Reading config"),
        );
        order.register_update(&SessionUpdate::ToolCallUpdate(update)).unwrap();
        assert!(order.tool_call_known("call_1"));
    }

    #[test]
    fn tool_call_announcement_registers_id() {
        let mut order = SessionUpdateOrder::new();
        let tool_call = ToolCall::new(ToolCallId::new("call_9"), "Running tests");
        order.register_update(&SessionUpdate::ToolCall(tool_call)).unwrap();
        assert!(order.tool_call_known("call_9"));

        // Subsequent updates for the announced id pass without fields.
        let update =
            ToolCallUpdate::new(ToolCallId::new("call_9"), ToolCallUpdateFields::default());
        order.register_update(&SessionUpdate::ToolCallUpdate(update)).unwrap();
    }

    #[test]
    fn reset_clears_all_ids() {
        let mut order = SessionUpdateOrder::new();
        order.register_update(&SessionUpdate::AgentMessageChunk(chunk(Some("m")))).unwrap();
        order
            .register_update(&SessionUpdate::ToolCall(ToolCall::new(ToolCallId::new("t"), "T")))
            .unwrap();
        order.reset();
        assert!(!order.message_known("m"));
        assert!(!order.tool_call_known("t"));
    }
}
