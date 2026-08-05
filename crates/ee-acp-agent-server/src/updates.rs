//! Typed session-update emission for prompt turns.
//!
//! The server creates one [`UpdateSink`] per accepted prompt, bound to the
//! session id and the server's outbound writer path, and hands it to
//! [`AgentProvider::prompt`](crate::AgentProvider::prompt).  Every helper
//! builds an SDK [`SessionUpdate`] and queues a `session/update`
//! notification; the server runtime forwards it over the transport in FIFO
//! order.  Updates emitted before the prompt completes therefore always
//! arrive before the prompt response, and updates for sessions the store no
//! longer knows are dropped (never emitted).

use std::fmt;

use ee_agent_protocol::{
    AvailableCommand, AvailableCommandsUpdate, ContentBlock, ContentChunk, MessageId, Plan,
    PlanEntry, SessionId, SessionInfoUpdate, SessionUpdate, TextContent, ToolCall, ToolCallContent,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use tokio::sync::mpsc;

use crate::server::OutboundEvent;

/// Why an update could not be queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateSinkError {
    /// A required identifier (message id, tool-call id) was empty.
    EmptyId(&'static str),
    /// The sink's session is not registered with the server.
    UnknownSession(SessionId),
    /// The server's outbound path is gone (shutdown).
    Closed,
}

impl fmt::Display for UpdateSinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyId(what) => write!(f, "{what} must not be empty"),
            Self::UnknownSession(session_id) => write!(f, "unknown session: {session_id}"),
            Self::Closed => f.write_str("update sink is closed"),
        }
    }
}

impl std::error::Error for UpdateSinkError {}

/// Queues `session/update` notifications for one live session.
///
/// Clonable; providers may keep one per subtask.  All helpers validate that
/// required ids are non-empty before queueing.
#[derive(Clone)]
pub struct UpdateSink {
    session_id: SessionId,
    tx: mpsc::UnboundedSender<OutboundEvent>,
}

impl UpdateSink {
    /// Creates a sink bound to a session and an outbound event channel.
    ///
    /// The framework's server paths construct this internally; downstream
    /// crates (e.g. `ee-agent-orchestrator`) use it to build sinks over
    /// their own outbound channels.
    pub fn new(session_id: SessionId, tx: mpsc::UnboundedSender<OutboundEvent>) -> Self {
        Self { session_id, tx }
    }

    /// Test-only constructor: binds a sink to a session id and an outbound
    /// channel without a running server.  Downstream crates (e.g.
    /// `ee-agent-orchestrator`) use this to drive prompt turns over an
    /// in-memory channel in tests.
    #[cfg(feature = "test-utils")]
    pub fn new_for_test(session_id: SessionId, tx: mpsc::UnboundedSender<OutboundEvent>) -> Self {
        Self::new(session_id, tx)
    }

    /// The session this sink emits updates for.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Emits one chunk of the agent's response message.
    pub fn agent_message_chunk(
        &self,
        message_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<(), UpdateSinkError> {
        let message_id = check_id(message_id.into(), "message_id")?;
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
            .message_id(MessageId::new(message_id));
        self.emit(SessionUpdate::AgentMessageChunk(chunk))
    }

    /// Emits one chunk of the agent's internal reasoning.
    pub fn agent_thought_chunk(
        &self,
        message_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<(), UpdateSinkError> {
        let message_id = check_id(message_id.into(), "message_id")?;
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
            .message_id(MessageId::new(message_id));
        self.emit(SessionUpdate::AgentThoughtChunk(chunk))
    }

    /// Announces a new tool call awaiting execution.
    pub fn tool_call_pending(
        &self,
        tool_call_id: impl Into<String>,
        title: impl Into<String>,
        kind: ToolKind,
    ) -> Result<(), UpdateSinkError> {
        let tool_call_id = check_id(tool_call_id.into(), "tool_call_id")?;
        let call = ToolCall::new(ToolCallId::new(tool_call_id), title)
            .kind(kind)
            .status(ToolCallStatus::Pending);
        self.emit(SessionUpdate::ToolCall(call))
    }

    /// Marks a tool call as running, carrying its title and content.
    pub fn tool_call_in_progress(
        &self,
        tool_call_id: impl Into<String>,
        title: impl Into<String>,
        content: Vec<ToolCallContent>,
    ) -> Result<(), UpdateSinkError> {
        let tool_call_id = check_id(tool_call_id.into(), "tool_call_id")?;
        let update = ToolCallUpdate::new(
            ToolCallId::new(tool_call_id),
            ToolCallUpdateFields::new()
                .title(title.into())
                .status(ToolCallStatus::InProgress)
                .content(content),
        );
        self.emit(SessionUpdate::ToolCallUpdate(update))
    }

    /// Marks a tool call as completed, carrying its title and result
    /// content.
    pub fn tool_call_completed(
        &self,
        tool_call_id: impl Into<String>,
        title: impl Into<String>,
        content: Vec<ToolCallContent>,
    ) -> Result<(), UpdateSinkError> {
        let tool_call_id = check_id(tool_call_id.into(), "tool_call_id")?;
        let update = ToolCallUpdate::new(
            ToolCallId::new(tool_call_id),
            ToolCallUpdateFields::new()
                .title(title.into())
                .status(ToolCallStatus::Completed)
                .content(content),
        );
        self.emit(SessionUpdate::ToolCallUpdate(update))
    }

    /// Marks a tool call as failed.  The SDK exposes no error field on the
    /// failed status, so the error text is carried as tool-call content.
    pub fn tool_call_failed(
        &self,
        tool_call_id: impl Into<String>,
        title: impl Into<String>,
        error: impl Into<String>,
    ) -> Result<(), UpdateSinkError> {
        let tool_call_id = check_id(tool_call_id.into(), "tool_call_id")?;
        let content = vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(error)))];
        let update = ToolCallUpdate::new(
            ToolCallId::new(tool_call_id),
            ToolCallUpdateFields::new()
                .title(title.into())
                .status(ToolCallStatus::Failed)
                .content(content),
        );
        self.emit(SessionUpdate::ToolCallUpdate(update))
    }

    /// Replaces the session plan with a complete new entry list.
    pub fn plan_replace(&self, entries: Vec<PlanEntry>) -> Result<(), UpdateSinkError> {
        self.emit(SessionUpdate::Plan(Plan::new(entries)))
    }

    /// Replaces the session's available commands.
    pub fn available_commands_replace(
        &self,
        commands: Vec<AvailableCommand>,
    ) -> Result<(), UpdateSinkError> {
        self.emit(SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(commands)))
    }

    /// Emits a full session-info update (title, timestamps, metadata).
    pub fn session_info_update(&self, info: SessionInfoUpdate) -> Result<(), UpdateSinkError> {
        self.emit(SessionUpdate::SessionInfoUpdate(info))
    }

    /// Escape hatch: queues any SDK [`SessionUpdate`] value as-is.
    pub fn raw_update(&self, update: SessionUpdate) -> Result<(), UpdateSinkError> {
        self.emit(update)
    }

    fn emit(&self, update: SessionUpdate) -> Result<(), UpdateSinkError> {
        self.tx
            .send(OutboundEvent::Update {
                session_id: self.session_id.clone(),
                update: Box::new(update),
            })
            .map_err(|_| UpdateSinkError::Closed)
    }
}

fn check_id(id: String, what: &'static str) -> Result<String, UpdateSinkError> {
    if id.is_empty() { Err(UpdateSinkError::EmptyId(what)) } else { Ok(id) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::{PlanEntryPriority, PlanEntryStatus, ToolCallContent};

    fn sink_and_rx() -> (UpdateSink, mpsc::UnboundedReceiver<OutboundEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (UpdateSink::new(SessionId::new("session-a"), tx), rx)
    }

    fn take_update(rx: &mut mpsc::UnboundedReceiver<OutboundEvent>) -> (SessionId, SessionUpdate) {
        let event = rx.blocking_recv().expect("update queued");
        match event {
            OutboundEvent::Update { session_id, update } => (session_id, *update),
            other => panic!("expected update event, got {other:?}"),
        }
    }

    #[test]
    fn agent_message_chunk_emits_update_for_bound_session() {
        let (sink, mut rx) = sink_and_rx();
        sink.agent_message_chunk("m-1", "hello").expect("emits");

        let (session_id, update) = take_update(&mut rx);
        assert_eq!(session_id, SessionId::new("session-a"));
        let SessionUpdate::AgentMessageChunk(chunk) = update else {
            panic!("expected agent message chunk");
        };
        assert_eq!(chunk.message_id, Some(MessageId::new("m-1")));
        let ContentBlock::Text(text) = chunk.content else {
            panic!("expected text content");
        };
        assert_eq!(text.text, "hello");
    }

    #[test]
    fn agent_thought_chunk_emits_expected_update_name() {
        let (sink, mut rx) = sink_and_rx();
        sink.agent_thought_chunk("m-2", "reasoning").expect("emits");

        let (_, update) = take_update(&mut rx);
        assert!(
            matches!(update, SessionUpdate::AgentThoughtChunk(_)),
            "expected agent thought chunk, got {update:?}"
        );
    }

    #[test]
    fn tool_call_completed_includes_title_status_and_content() {
        let (sink, mut rx) = sink_and_rx();
        let content = vec![ToolCallContent::from(ContentBlock::Text(TextContent::new("done")))];
        sink.tool_call_completed("tc-1", "Run tests", content).expect("emits");

        let (_, update) = take_update(&mut rx);
        let SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected tool call update");
        };
        assert_eq!(update.tool_call_id, ToolCallId::new("tc-1"));
        assert_eq!(update.fields.title.as_deref(), Some("Run tests"));
        assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
        assert_eq!(update.fields.content.expect("content").len(), 1);
    }

    #[test]
    fn tool_call_failed_carries_error_as_content() {
        let (sink, mut rx) = sink_and_rx();
        sink.tool_call_failed("tc-2", "Run tests", "tests exploded").expect("emits");

        let (_, update) = take_update(&mut rx);
        let SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected tool call update");
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
        let ToolCallContent::Content(content) = &update.fields.content.expect("content")[0] else {
            panic!("expected text content");
        };
        let ContentBlock::Text(text) = &content.content else {
            panic!("expected text block");
        };
        assert_eq!(text.text, "tests exploded");
    }

    #[test]
    fn plan_replace_emits_complete_replacement() {
        let (sink, mut rx) = sink_and_rx();
        let entries = vec![
            PlanEntry::new("step one", PlanEntryPriority::High, PlanEntryStatus::Pending),
            PlanEntry::new("step two", PlanEntryPriority::Low, PlanEntryStatus::InProgress),
        ];
        sink.plan_replace(entries.clone()).expect("emits");

        let (_, update) = take_update(&mut rx);
        let SessionUpdate::Plan(plan) = update else {
            panic!("expected plan update");
        };
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].content, "step one");
        assert_eq!(plan.entries[1].content, "step two");
    }

    #[test]
    fn raw_update_escape_hatch_passes_value_through() {
        let (sink, mut rx) = sink_and_rx();
        let update = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![]));
        sink.raw_update(update.clone()).expect("emits");
        assert_eq!(take_update(&mut rx).1, update);
    }

    #[test]
    fn empty_ids_are_rejected() {
        let (sink, _rx) = sink_and_rx();
        assert_eq!(sink.agent_message_chunk("", "hi"), Err(UpdateSinkError::EmptyId("message_id")));
        assert_eq!(
            sink.agent_thought_chunk("", "thought"),
            Err(UpdateSinkError::EmptyId("message_id"))
        );
        assert_eq!(
            sink.tool_call_pending("", "title", ToolKind::Read),
            Err(UpdateSinkError::EmptyId("tool_call_id"))
        );
        assert_eq!(
            sink.tool_call_completed("", "title", vec![]),
            Err(UpdateSinkError::EmptyId("tool_call_id"))
        );
    }

    #[test]
    fn send_after_outbound_closed_fails() {
        let (sink, rx) = sink_and_rx();
        drop(rx);
        assert_eq!(sink.agent_message_chunk("m-1", "hi"), Err(UpdateSinkError::Closed));
    }
}
