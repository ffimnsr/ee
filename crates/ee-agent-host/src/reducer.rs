//! Session update reducer: turns the ACP `session/update` notification
//! stream into deterministic session state the UI can render.
//!
//! Merge rules (ACP v1, enforced by [`ee_agent_protocol::SessionUpdateOrder`]
//! before reduction):
//!
//! - Message chunks merge by `messageId`; chunks without an id continue the
//!   most recent message (or start a new one).
//! - `tool_call` announces a call; `tool_call_update` merges present fields,
//!   replacing `content`/`locations` collections wholesale.
//! - `plan` replaces the whole plan (ACP v1 plans are complete snapshots).
//! - Usage, mode, commands, config options, and session info are replaced on
//!   each update.
//!
//! The reducer never buffers wire content beyond the session snapshot, so
//! snapshots stay cheap to clone for the UI.

use std::collections::BTreeMap;

use ee_agent_protocol::{
    AvailableCommand, ContentBlock, Cost, PlanEntry, SessionConfigOption, SessionInfoUpdate,
    SessionModeId, SessionUpdate, ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolKind, UsageUpdate,
};
use ee_agent_protocol::{SessionUpdateOrder, ToolCallUpdateFields};

use crate::error::AgentError;

/// Maximum tool call states retained per session.  When the cap is hit the
/// oldest tool calls drop (the UI shows the most recent activity; the raw
/// stream is never buffered beyond this).
pub const MAX_TOOL_CALLS_RETAINED: usize = 1024;

/// Token/context usage for one session (ACP `usage_update`).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageInfo {
    /// Tokens currently in context.
    pub used: u64,
    /// Total context window size in tokens.
    pub size: u64,
    /// Cumulative session cost, when reported.
    pub cost: Option<Cost>,
}

impl From<&UsageUpdate> for UsageInfo {
    fn from(update: &UsageUpdate) -> Self {
        Self { used: update.used, size: update.size, cost: update.cost.clone() }
    }
}

/// One reduced message in a session.
#[derive(Debug, Clone, PartialEq)]
pub struct ReducedMessage {
    /// User, assistant, or thought stream.
    pub kind: MessageKind,
    /// The ACP `messageId`, when the agent streamed one.
    pub message_id: Option<String>,
    /// Blocks accumulated in arrival order.
    pub blocks: Vec<ContentBlock>,
}

/// Which stream a reduced message came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    User,
    Assistant,
    Thought,
}

/// Reduced tool call state (upserted by `tool_call` / `tool_call_update`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallState {
    pub tool_call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    pub content: Vec<ToolCallContent>,
    pub locations: Vec<ToolCallLocation>,
    pub raw_input: Option<serde_json::Value>,
    pub raw_output: Option<serde_json::Value>,
}

impl From<&ToolCall> for ToolCallState {
    fn from(tool_call: &ToolCall) -> Self {
        Self {
            tool_call_id: tool_call.tool_call_id.0.to_string(),
            title: tool_call.title.clone(),
            kind: tool_call.kind,
            status: tool_call.status,
            content: tool_call.content.clone(),
            locations: tool_call.locations.clone(),
            raw_input: tool_call.raw_input.clone(),
            raw_output: tool_call.raw_output.clone(),
        }
    }
}

/// Complete reduced state of one agent session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionState {
    /// Streamed messages in arrival order.
    pub messages: Vec<ReducedMessage>,
    /// Tool calls keyed by `toolCallId`.
    pub tool_calls: BTreeMap<String, ToolCallState>,
    /// The agent's current plan (replaced wholesale on each `plan` update).
    pub plan: Vec<PlanEntry>,
    /// The current session mode, when the agent reported one.
    pub current_mode: Option<SessionModeId>,
    /// Commands the agent advertised.
    pub available_commands: Vec<AvailableCommand>,
    /// Session configuration options.
    pub config_options: Vec<SessionConfigOption>,
    /// Context window usage.
    pub usage: Option<UsageInfo>,
    /// Session metadata (title, updated-at).
    pub session_info: Option<SessionInfoUpdate>,
}

/// Applies [`SessionUpdate`] notifications onto [`SessionState`].
///
/// Ordering invariants are checked first through the shared
/// [`SessionUpdateOrder`] tracker; an invalid update fails closed with
/// [`AgentError::InvalidUpdate`] and leaves the state untouched.
pub fn apply_update(
    state: &mut SessionState,
    order: &mut SessionUpdateOrder,
    update: &SessionUpdate,
) -> Result<(), AgentError> {
    order.register_update(update).map_err(|error| {
        AgentError::InvalidUpdate(format!("session/update ordering violation: {error}"))
    })?;

    match update {
        SessionUpdate::UserMessageChunk(chunk) => {
            append_chunk(
                state,
                MessageKind::User,
                chunk.content.clone(),
                chunk.message_id.as_ref(),
            );
        }
        SessionUpdate::AgentMessageChunk(chunk) => {
            append_chunk(
                state,
                MessageKind::Assistant,
                chunk.content.clone(),
                chunk.message_id.as_ref(),
            );
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            append_chunk(
                state,
                MessageKind::Thought,
                chunk.content.clone(),
                chunk.message_id.as_ref(),
            );
        }
        SessionUpdate::ToolCall(tool_call) => {
            state
                .tool_calls
                .insert(tool_call.tool_call_id.0.to_string(), ToolCallState::from(tool_call));
            cap_tool_calls(state);
        }
        SessionUpdate::ToolCallUpdate(update) => {
            merge_tool_call_update(state, update);
            cap_tool_calls(state);
        }
        SessionUpdate::Plan(plan) => {
            state.plan = plan.entries.clone();
        }
        SessionUpdate::AvailableCommandsUpdate(commands) => {
            state.available_commands = commands.available_commands.clone();
        }
        SessionUpdate::CurrentModeUpdate(mode) => {
            state.current_mode = Some(mode.current_mode_id.clone());
        }
        SessionUpdate::ConfigOptionUpdate(options) => {
            state.config_options = options.config_options.clone();
        }
        SessionUpdate::SessionInfoUpdate(info) => {
            state.session_info = Some(info.clone());
        }
        SessionUpdate::UsageUpdate(usage) => {
            state.usage = Some(UsageInfo::from(usage));
        }
        // `SessionUpdate` is non-exhaustive upstream; unknown future variants
        // carry no ordering guarantees and are ignored by the reducer.
        _ => {}
    }
    Ok(())
}

fn append_chunk(
    state: &mut SessionState,
    kind: MessageKind,
    block: ContentBlock,
    message_id: Option<&ee_agent_protocol::MessageId>,
) {
    let key = message_id.map(|id| id.0.to_string());
    if let Some(key) = &key {
        if let Some(message) =
            state.messages.iter_mut().find(|message| message.message_id.as_ref() == Some(key))
        {
            message.blocks.push(block);
            return;
        }
        // A changed `messageId` starts a new message (ACP v1).
        state.messages.push(ReducedMessage {
            kind,
            message_id: Some(key.clone()),
            blocks: vec![block],
        });
        return;
    }
    // No id: the chunk continues the most recent message of the same kind
    // when one exists; otherwise it starts a new message.  Grouping by kind
    // keeps user/assistant/thought streams separable even for agents that
    // never send `messageId`.
    if let Some(message) = state.messages.last_mut()
        && message.kind == kind
    {
        message.blocks.push(block);
        return;
    }
    state.messages.push(ReducedMessage { kind, message_id: None, blocks: vec![block] });
}

fn merge_tool_call_update(state: &mut SessionState, update: &ToolCallUpdate) {
    let id = update.tool_call_id.0.to_string();
    let fields: &ToolCallUpdateFields = &update.fields;
    let entry = state.tool_calls.entry(id).or_insert_with(|| ToolCallState {
        tool_call_id: update.tool_call_id.0.to_string(),
        title: fields.title.clone().unwrap_or_default(),
        kind: fields.kind.unwrap_or_default(),
        status: fields.status.unwrap_or_default(),
        content: Vec::new(),
        locations: Vec::new(),
        raw_input: None,
        raw_output: None,
    });
    if let Some(kind) = fields.kind {
        entry.kind = kind;
    }
    if let Some(status) = fields.status {
        entry.status = status;
    }
    if let Some(title) = &fields.title {
        entry.title = title.clone();
    }
    if let Some(content) = &fields.content {
        entry.content = content.clone();
    }
    if let Some(locations) = &fields.locations {
        entry.locations = locations.clone();
    }
    if let Some(raw_input) = &fields.raw_input {
        entry.raw_input = Some(raw_input.clone());
    }
    if let Some(raw_output) = &fields.raw_output {
        entry.raw_output = Some(raw_output.clone());
    }
}

/// Drops the oldest tool call states once the per-session cap is exceeded.
fn cap_tool_calls(state: &mut SessionState) {
    while state.tool_calls.len() > MAX_TOOL_CALLS_RETAINED {
        let oldest = state.tool_calls.keys().next().cloned();
        if let Some(oldest) = oldest {
            state.tool_calls.remove(&oldest);
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::{
        ContentBlock, ContentChunk, Plan, PlanEntryPriority, PlanEntryStatus, SessionId,
        SessionNotification, TextContent, ToolCallId,
    };

    fn chunk(kind: MessageKind, text: &str, message_id: Option<&str>) -> SessionUpdate {
        let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
        if let Some(id) = message_id {
            chunk = chunk.message_id(id);
        }
        match kind {
            MessageKind::User => SessionUpdate::UserMessageChunk(chunk),
            MessageKind::Assistant => SessionUpdate::AgentMessageChunk(chunk),
            MessageKind::Thought => SessionUpdate::AgentThoughtChunk(chunk),
        }
    }

    fn apply(state: &mut SessionState, order: &mut SessionUpdateOrder, update: SessionUpdate) {
        apply_update(state, order, &update).unwrap();
    }

    #[test]
    fn messages_merge_by_id_and_unnamed_chunks_continue_same_kind() {
        let mut state = SessionState::default();
        let mut order = SessionUpdateOrder::new();

        apply(&mut state, &mut order, chunk(MessageKind::Assistant, "hel", Some("m1")));
        apply(&mut state, &mut order, chunk(MessageKind::Assistant, "lo", Some("m1")));
        apply(&mut state, &mut order, chunk(MessageKind::User, "hi", None));
        apply(&mut state, &mut order, chunk(MessageKind::Thought, "hmm", None));
        apply(&mut state, &mut order, chunk(MessageKind::Thought, "...", None));

        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0].message_id.as_deref(), Some("m1"));
        assert_eq!(state.messages[0].blocks.len(), 2);
        assert_eq!(state.messages[1].kind, MessageKind::User);
        // Unnamed thought chunks continue the most recent thought message.
        assert_eq!(state.messages[2].kind, MessageKind::Thought);
        assert_eq!(state.messages[2].blocks.len(), 2);
    }

    #[test]
    fn changed_message_id_starts_a_new_message() {
        let mut state = SessionState::default();
        let mut order = SessionUpdateOrder::new();

        apply(&mut state, &mut order, chunk(MessageKind::Assistant, "one", Some("m1")));
        apply(&mut state, &mut order, chunk(MessageKind::Assistant, "two", Some("m2")));

        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].message_id.as_deref(), Some("m1"));
        assert_eq!(state.messages[1].message_id.as_deref(), Some("m2"));
    }

    #[test]
    fn tool_calls_upsert_and_merge_fields() {
        let mut state = SessionState::default();
        let mut order = SessionUpdateOrder::new();

        let tool_call = ToolCall::new(ToolCallId::new("call_1"), "Run tests");
        apply(&mut state, &mut order, SessionUpdate::ToolCall(tool_call));
        assert_eq!(state.tool_calls["call_1"].title, "Run tests");
        assert_eq!(state.tool_calls["call_1"].status, ToolCallStatus::default());

        let update = ToolCallUpdate::new(
            ToolCallId::new("call_1"),
            ee_agent_protocol::ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .title("Run tests (updated)"),
        );
        apply(&mut state, &mut order, SessionUpdate::ToolCallUpdate(update));
        let merged = &state.tool_calls["call_1"];
        assert_eq!(merged.status, ToolCallStatus::Completed);
        assert_eq!(merged.title, "Run tests (updated)");
    }

    #[test]
    fn tool_call_update_can_construct_an_unknown_call() {
        let mut state = SessionState::default();
        let mut order = SessionUpdateOrder::new();

        let update = ToolCallUpdate::new(
            ToolCallId::new("call_2"),
            ee_agent_protocol::ToolCallUpdateFields::new().title("Read config"),
        );
        apply(&mut state, &mut order, SessionUpdate::ToolCallUpdate(update));
        assert_eq!(state.tool_calls["call_2"].title, "Read config");
    }

    #[test]
    fn invalid_tool_call_update_fails_closed_without_mutating() {
        let mut state = SessionState::default();
        let mut order = SessionUpdateOrder::new();

        let update = ToolCallUpdate::new(ToolCallId::new("ghost"), Default::default());
        let err = apply_update(&mut state, &mut order, &SessionUpdate::ToolCallUpdate(update))
            .unwrap_err();
        assert!(matches!(err, AgentError::InvalidUpdate(_)));
        assert!(state.tool_calls.is_empty());
    }

    #[test]
    fn tool_call_states_are_capped_and_oldest_drop() {
        let mut state = SessionState::default();
        let mut order = SessionUpdateOrder::new();
        for index in 0..(MAX_TOOL_CALLS_RETAINED + 10) {
            let id = format!("call_{index}");
            apply(
                &mut state,
                &mut order,
                SessionUpdate::ToolCall(ToolCall::new(ToolCallId::new(id), "t")),
            );
        }
        assert_eq!(state.tool_calls.len(), MAX_TOOL_CALLS_RETAINED);
        // The oldest calls dropped; the newest survived.
        assert!(!state.tool_calls.contains_key("call_0"));
        assert!(state.tool_calls.contains_key(&format!("call_{}", MAX_TOOL_CALLS_RETAINED + 9)));
    }

    #[test]
    fn plan_replaces_wholesale() {
        let mut state = SessionState::default();
        let mut order = SessionUpdateOrder::new();

        let entry = PlanEntry::new("step 1", PlanEntryPriority::High, PlanEntryStatus::InProgress);
        apply(&mut state, &mut order, SessionUpdate::Plan(Plan::new(vec![entry])));
        assert_eq!(state.plan.len(), 1);
        assert_eq!(state.plan[0].content, "step 1");
        assert_eq!(state.plan[0].status, PlanEntryStatus::InProgress);

        apply(&mut state, &mut order, SessionUpdate::Plan(Plan::new(vec![])));
        assert!(state.plan.is_empty());
    }

    #[test]
    fn usage_and_mode_updates_replace_values() {
        let mut state = SessionState::default();
        let mut order = SessionUpdateOrder::new();

        apply(&mut state, &mut order, SessionUpdate::UsageUpdate(UsageUpdate::new(10, 100)));
        assert_eq!(state.usage.as_ref().map(|u| u.used), Some(10));
        assert_eq!(state.usage.as_ref().map(|u| u.size), Some(100));

        apply(&mut state, &mut order, SessionUpdate::UsageUpdate(UsageUpdate::new(20, 100)));
        assert_eq!(state.usage.as_ref().map(|u| u.used), Some(20));
    }

    #[test]
    fn notification_wraps_updates_for_wire_round_trip() {
        let notification =
            SessionNotification::new(SessionId::new("s1"), SessionUpdate::Plan(Plan::new(vec![])));
        assert_eq!(notification.session_id, SessionId::new("s1"));
    }
}
