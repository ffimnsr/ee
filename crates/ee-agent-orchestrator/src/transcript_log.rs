//! Transcript integration with the append-only session log.
//!
//! Kept out of `model.rs` so the normalized data types stay stable while the
//! durable-log seam grows. Every method appends through the attached
//! [`SessionLog`] and fails closed before a model call.

use std::sync::Arc;

use crate::error::OrchestratorError;
use crate::model::{ModelMessage, ModelRequest, ModelRole, Transcript};
use crate::session_log::{SessionLog, projection_hash, tool_call_id as message_tool_call_id};
use crate::trust::TrustLevel;

impl Transcript {
    /// Attaches a durable session log. Subsequent pushes append to it.
    pub fn attach_session_log(&mut self, log: Arc<SessionLog>) {
        self.log = Some(log);
    }

    /// The attached durable session log, when any.
    #[must_use]
    pub fn session_log(&self) -> Option<&Arc<SessionLog>> {
        self.log.as_ref()
    }

    /// Replaces the message view and per-message log ids together.
    pub(crate) fn replace_messages_with_ids(
        &mut self,
        messages: Vec<ModelMessage>,
        mut ids: Vec<Option<String>>,
    ) {
        ids.resize(messages.len(), None);
        ids.truncate(messages.len());
        self.messages = messages;
        self.log_ids = ids;
    }

    /// Appends every message that is not in the log yet. Fails closed before a
    /// model call when the log cannot accept a record.
    pub(crate) fn ensure_logged(&mut self) -> Result<(), OrchestratorError> {
        let Some(log) = self.log.clone() else {
            self.log_ids.resize(self.messages.len(), None);
            return Ok(());
        };
        self.log_ids.resize(self.messages.len(), None);
        for index in 0..self.messages.len() {
            if self.log_ids[index].is_some() {
                continue;
            }
            let id = log.record_message(
                &self.messages[index],
                message_tool_call_id(&self.messages[index]),
            )?;
            self.log_ids[index] = Some(id);
        }
        Ok(())
    }

    /// Records the effective model request (post-prune, post-injection) in the
    /// durable log. Must run before `model.complete`.
    pub(crate) fn record_model_request(
        &mut self,
        request: &ModelRequest,
    ) -> Result<(), OrchestratorError> {
        let Some(log) = self.log.clone() else { return Ok(()) };
        self.ensure_logged()?;
        let mut ids = Vec::with_capacity(request.transcript.len());
        for (index, message) in request.transcript.iter().enumerate() {
            if let Some(Some(id)) = self.log_ids.get(index) {
                ids.push(id.clone());
            } else {
                ids.push(log.record_message(message, message_tool_call_id(message))?);
            }
        }
        log.record_model_request(
            ids,
            &request.tools,
            projection_hash(&request.transcript),
            &request.budget,
            request.model_id.as_deref(),
        )?;
        Ok(())
    }

    pub(crate) fn record_message_at(
        &self,
        index: usize,
        tool_call_id: Option<&str>,
    ) -> Option<String> {
        let log = self.log.as_ref()?;
        let message = self.messages.get(index)?;
        log.record_message(message, tool_call_id).ok()
    }

    /// Appends a host-owned repair instruction as system policy.
    pub(crate) fn push_system_notice(&mut self, text: impl Into<String>) {
        self.messages.push(
            ModelMessage::text(ModelRole::System, text)
                .with_trust(TrustLevel::SystemPolicy)
                .with_metadata([("ee_prompt".to_string(), "stop_gate_repair".to_string())]),
        );
        let id = self.record_message_at(self.messages.len() - 1, None);
        self.log_ids.push(id);
    }

    /// Appends a host-assembled message that is logged lazily before the next
    /// model request.
    pub(crate) fn push_unlogged(&mut self, message: ModelMessage) {
        self.sync_log_len();
        self.messages.push(message);
        self.log_ids.push(None);
    }

    /// Inserts a host-assembled message at `index`; logged lazily.
    pub(crate) fn insert_unlogged(&mut self, index: usize, message: ModelMessage) {
        self.sync_log_len();
        let index = index.min(self.messages.len());
        self.messages.insert(index, message);
        self.log_ids.insert(index, None);
    }

    fn sync_log_len(&mut self) {
        self.log_ids.resize(self.messages.len(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelRole;
    use crate::session_log::SessionLogRecord;

    fn message(text: &str) -> ModelMessage {
        ModelMessage::text(ModelRole::User, text)
    }

    #[test]
    fn pushes_append_to_the_log_and_keep_ids_aligned() {
        let log = Arc::new(SessionLog::in_memory("s-1"));
        let mut transcript = Transcript::new();
        transcript.attach_session_log(log.clone());
        transcript.push_subagent_summary("first");
        transcript.push_tool_result("tc-1", crate::tools::ToolResult::success("out"));
        assert_eq!(transcript.log_ids, vec![Some("msg-1".into()), Some("msg-2".into())]);
        assert_eq!(log.replay_messages().expect("replays").len(), 2);
    }

    #[test]
    fn truncation_is_a_view_operation_only() {
        let log = Arc::new(SessionLog::in_memory("s-1"));
        let mut transcript = Transcript::new();
        transcript.attach_session_log(log.clone());
        for index in 0..5 {
            transcript.push_subagent_summary(format!("message {index} with padding"));
        }
        transcript.enforce_memory_limit(1);
        assert_eq!(transcript.messages().len(), 1, "request view keeps the newest message");
        assert_eq!(transcript.log_ids.len(), 1);
        assert_eq!(log.replay_messages().expect("replays").len(), 5);
        assert!(
            log.records()
                .expect("records")
                .iter()
                .any(|record| matches!(record, SessionLogRecord::Truncation(_)))
        );
    }

    #[test]
    fn replaced_messages_keep_replayed_ids_without_duplicates() {
        let log = Arc::new(SessionLog::in_memory("s-1"));
        let replayed = message("history");
        let id = log.record_message(&replayed, None).expect("records");
        let mut transcript = Transcript::new();
        transcript.attach_session_log(log.clone());
        transcript.replace_messages_with_ids(vec![replayed], vec![Some(id)]);
        transcript.ensure_logged().expect("already logged");
        assert_eq!(log.replay_messages().expect("replays").len(), 1);
    }

    #[test]
    fn unlogged_messages_are_backfilled_before_a_request() {
        let log = Arc::new(SessionLog::in_memory("s-1"));
        let mut transcript = Transcript::new();
        transcript.attach_session_log(log.clone());
        transcript.push_unlogged(message("host assembled"));
        assert!(transcript.log_ids[0].is_none());
        transcript.ensure_logged().expect("backfills");
        assert!(transcript.log_ids[0].is_some());
        assert_eq!(log.replay_messages().expect("replays").len(), 1);
    }
}
