//! Pure UI state and render model.
//!
//! Everything here is plain Rust: no `web-sys`, no DOM, no timers. The WASM
//! layer only feeds frames in, drains outbound frames, and renders
//! [`AppModel::view`]. All backend policy (permissions, validation, cursors)
//! stays in the host; this model never invents a rule.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::protocol;

/// Maximum retained transcript items.
pub const MAX_TRANSCRIPT_ITEMS: usize = 500;
/// Maximum retained characters per message.
pub const MAX_TEXT_CHARS: usize = 16 * 1024;
/// Maximum lines rendered in one view.
pub const MAX_VIEW_LINES: usize = 200;

/// Message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Thought,
}

/// One transcript row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    Message { role: Role, message_id: Option<String>, text: String },
    Tool { id: String, title: String, status: String },
    Notice(String),
}

/// One answerable approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalCard {
    pub server_id: Option<u64>,
    pub request_id: u64,
    pub session_id: String,
    pub summary: String,
    pub options: Vec<ApprovalOption>,
}

/// One approval option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalOption {
    pub label: String,
    pub decision: &'static str,
}

/// Connection state shown in the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingRequest {
    Initialize,
    SessionList,
    SessionNew,
    HistoryTail,
    TurnStart,
    TurnInterrupt,
    PermissionsRespond,
}

/// The whole client state.
#[derive(Debug, Clone)]
pub struct AppModel {
    pub connection: ConnectionState,
    pub sessions: Vec<protocol::SessionEntry>,
    pub selected: Option<String>,
    pub transcript: Vec<TranscriptItem>,
    pub approvals: Vec<ApprovalCard>,
    pub plan: Vec<String>,
    pub running_turn: Option<u64>,
    pub notice: Option<String>,
    pub permission_mode: Option<String>,
    pub cursor: u64,
    ever_connected: bool,
    last_message: Option<(Role, Option<String>)>,
    outbound: Vec<Value>,
    next_id: u64,
    pending: BTreeMap<u64, PendingRequest>,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            connection: ConnectionState::Disconnected,
            sessions: Vec::new(),
            selected: None,
            transcript: Vec::new(),
            approvals: Vec::new(),
            plan: Vec::new(),
            running_turn: None,
            notice: None,
            permission_mode: None,
            cursor: 0,
            ever_connected: false,
            last_message: None,
            outbound: Vec::new(),
            next_id: 1,
            pending: BTreeMap::new(),
        }
    }
}

impl AppModel {
    /// Fresh model.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames the transport must send.
    pub fn drain_outbound(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.outbound)
    }

    /// Socket opened: start or restart the handshake.
    pub fn on_open(&mut self) {
        self.connection = ConnectionState::Connecting;
        self.notice = None;
        self.approvals.clear();
        if self.ever_connected {
            // Re-sync from the durable notification ring; the server replays
            // everything after cursor 0 so no delta is duplicated or lost.
            self.transcript.clear();
            self.plan.clear();
            self.cursor = 0;
            self.last_message = None;
        }
        self.ever_connected = true;
        self.send_request(
            PendingRequest::Initialize,
            protocol::INITIALIZE,
            json!({
                "protocolVersion": protocol::PROTOCOL_VERSION,
                "clientInfo": { "name": "ee-web", "version": env!("CARGO_PKG_VERSION") },
                "clientId": "ee-web"
            }),
        );
    }

    /// Socket closed.
    pub fn on_close(&mut self, reason: impl Into<String>) {
        self.connection = ConnectionState::Disconnected;
        self.notice = Some(format!("disconnected: {}", reason.into()));
        self.running_turn = None;
    }

    /// Applies one decoded server frame.
    pub fn apply_frame(&mut self, frame: &Value) {
        if let Some(method) = frame["method"].as_str() {
            if let Some(id) = frame["id"].as_u64() {
                self.apply_server_request(id, method, &frame["params"]);
            } else {
                self.apply_notification(method, &frame["params"]);
            }
            return;
        }
        let Some(id) = frame["id"].as_u64() else {
            return;
        };
        let Some(kind) = self.pending.remove(&id) else {
            return;
        };
        if let Some(error) = frame.get("error") {
            self.apply_error(kind, error);
            return;
        }
        self.apply_response(kind, &frame["result"]);
    }

    fn send_request(&mut self, kind: PendingRequest, method: &str, params: Value) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, kind);
        self.outbound.push(protocol::request(id, method, params));
    }

    fn send_notification(&mut self, method: &str, params: Value) {
        self.outbound.push(protocol::notification(method, params));
    }

    fn apply_server_request(&mut self, server_id: u64, method: &str, params: &Value) {
        if method != protocol::PERMISSION_REQUEST_APPROVAL {
            return;
        }
        let request_id = params["requestId"].as_u64().unwrap_or_default();
        if self.approvals.iter().any(|card| card.request_id == request_id) {
            return;
        }
        let options = params["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .map(|option| ApprovalOption {
                        label: protocol::option_label(option),
                        decision: protocol::decision_for_option(option),
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.approvals.push(ApprovalCard {
            server_id: Some(server_id),
            request_id,
            session_id: params["sessionId"].as_str().unwrap_or_default().to_string(),
            summary: params["summary"].as_str().unwrap_or("approval required").to_string(),
            options,
        });
    }

    fn apply_notification(&mut self, method: &str, params: &Value) {
        match method {
            protocol::SESSION_UPDATE => {
                let session_id = params["sessionId"].as_str().unwrap_or_default();
                self.note_cursor(session_id, params);
                if self.selected.as_deref() == Some(session_id) {
                    self.apply_update(&params["update"]);
                }
            }
            protocol::TURN_COMPLETED => {
                let session_id = params["sessionId"].as_str().unwrap_or_default();
                self.note_cursor(session_id, params);
                if self.selected.as_deref() == Some(session_id) {
                    self.running_turn = None;
                    self.approvals.clear();
                    let status = params["status"].as_str().unwrap_or("completed");
                    self.notice = Some(match status {
                        "completed" => String::from("turn completed"),
                        "cancelled" => String::from("turn cancelled"),
                        "failed" => format!(
                            "turn failed: {}",
                            params["error"]["message"].as_str().unwrap_or("unknown error")
                        ),
                        other => format!("turn {other}"),
                    });
                }
            }
            protocol::STATUS_CHANGED => {
                let session_id = params["sessionId"].as_str().unwrap_or_default();
                self.note_cursor(session_id, params);
                if let Some(detail) = params["detail"].as_str() {
                    self.notice = Some(detail.to_string());
                }
            }
            _ => {}
        }
    }

    fn note_cursor(&mut self, session_id: &str, params: &Value) {
        if self.selected.as_deref() != Some(session_id) && self.selected.is_some() {
            return;
        }
        if let Some(cursor) = params["cursor"].as_u64() {
            self.cursor = self.cursor.max(cursor);
        }
    }

    fn apply_update(&mut self, update: &Value) {
        match update["sessionUpdate"].as_str().unwrap_or_default() {
            "user_message_chunk" => {
                self.push_chunk(Role::User, update, protocol::content_text(&update["content"]));
            }
            "agent_message_chunk" => {
                self.push_chunk(
                    Role::Assistant,
                    update,
                    protocol::content_text(&update["content"]),
                );
            }
            "agent_thought_chunk" => {
                self.push_chunk(Role::Thought, update, protocol::content_text(&update["content"]));
            }
            "tool_call" | "tool_call_update" => {
                let id = update["toolCallId"].as_str().unwrap_or_default().to_string();
                let title = update["title"]
                    .as_str()
                    .or_else(|| update["name"].as_str())
                    .unwrap_or("tool")
                    .to_string();
                let status = update["status"]
                    .as_str()
                    .or_else(|| update["kind"].as_str())
                    .unwrap_or("pending")
                    .to_string();
                self.upsert_tool(id, title, status);
            }
            "plan" => {
                self.plan = update["entries"]
                    .as_array()
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(|entry| entry["content"].as_str())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "current_mode_update" => {
                if let Some(mode) = update["currentModeId"].as_str() {
                    self.notice = Some(format!("mode: {mode}"));
                }
            }
            // Usage, config-option, command-list, and session-info updates are
            // metadata; the v1 web UI does not render them.
            _ => {}
        }
        self.trim_transcript();
    }

    fn push_chunk(&mut self, role: Role, update: &Value, text: Option<&str>) {
        let Some(text) = text else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let message_id = update["messageId"].as_str().map(str::to_string);
        let merge = self
            .last_message
            .as_ref()
            .is_some_and(|(last_role, last_id)| *last_role == role && last_id == &message_id)
            && matches!(
                self.transcript.last(),
                Some(TranscriptItem::Message { role: current, .. }) if *current == role
            );
        if merge
            && let Some(TranscriptItem::Message { text: existing, .. }) = self.transcript.last_mut()
        {
            append_capped(existing, text);
            return;
        }
        self.transcript.push(TranscriptItem::Message {
            role,
            message_id: message_id.clone(),
            text: text.to_string(),
        });
        self.last_message = Some((role, message_id));
    }

    fn upsert_tool(&mut self, id: String, title: String, status: String) {
        for item in self.transcript.iter_mut().rev() {
            if let TranscriptItem::Tool {
                id: existing,
                title: existing_title,
                status: existing_status,
            } = item
                && *existing == id
            {
                *existing_title = title;
                *existing_status = status;
                return;
            }
        }
        self.transcript.push(TranscriptItem::Tool { id, title, status });
    }

    fn trim_transcript(&mut self) {
        if self.transcript.len() > MAX_TRANSCRIPT_ITEMS {
            let excess = self.transcript.len() - MAX_TRANSCRIPT_ITEMS;
            self.transcript.drain(..excess);
        }
    }

    fn apply_response(&mut self, kind: PendingRequest, result: &Value) {
        match kind {
            PendingRequest::Initialize => {
                self.connection = ConnectionState::Connected;
                self.permission_mode = result["permissionMode"].as_str().map(str::to_string);
                self.send_notification(protocol::INITIALIZED, json!({}));
                self.send_request(PendingRequest::SessionList, protocol::SESSION_LIST, json!({}));
            }
            PendingRequest::SessionList => {
                self.sessions = protocol::parse_sessions(result);
                if let Some(selected) = self.selected.clone() {
                    if self.sessions.iter().any(|session| session.session_id == selected) {
                        self.request_history(&selected);
                    } else {
                        self.selected = None;
                    }
                } else if self.sessions.len() == 1 {
                    let session_id = self.sessions[0].session_id.clone();
                    self.select_session(&session_id);
                }
            }
            PendingRequest::SessionNew => {
                if let Some(session_id) = result["sessionId"].as_str() {
                    self.selected = Some(session_id.to_string());
                    self.transcript.clear();
                    self.plan.clear();
                    self.cursor = 0;
                    self.last_message = None;
                    self.send_request(
                        PendingRequest::SessionList,
                        protocol::SESSION_LIST,
                        json!({}),
                    );
                }
            }
            PendingRequest::HistoryTail => {
                if result["gap"].as_bool() == Some(true) {
                    self.notice =
                        Some(String::from("history gap: replay started from the retained window"));
                }
                if let Some(notifications) = result["notifications"].as_array() {
                    for notification in notifications {
                        let cursor = notification["cursor"].as_u64().unwrap_or_default();
                        if cursor > self.cursor {
                            self.cursor = cursor;
                        }
                        if let Some(method) = notification["method"].as_str() {
                            self.apply_notification(method, &notification["params"]);
                        }
                    }
                }
                if let Some(cursor) = result["cursor"].as_u64() {
                    self.cursor = self.cursor.max(cursor);
                }
            }
            PendingRequest::TurnStart => {
                self.running_turn = result["turnId"].as_u64();
                if self.running_turn.is_some() {
                    self.notice = Some(String::from("turn started"));
                }
            }
            PendingRequest::TurnInterrupt => {
                self.notice = Some(String::from("interrupt requested"));
            }
            PendingRequest::PermissionsRespond => {
                self.notice = Some(String::from("approval recorded"));
            }
        }
    }

    fn apply_error(&mut self, kind: PendingRequest, error: &Value) {
        let code = error["code"].as_i64().unwrap_or_default();
        let message = error["message"].as_str().unwrap_or("request failed");
        if kind == PendingRequest::Initialize {
            self.connection = ConnectionState::Failed(format!("{code}: {message}"));
        }
        self.notice = Some(format!("{message} ({code})"));
    }

    /// Requests a new session on the server's default agent/root.
    pub fn new_session(&mut self) {
        self.send_request(PendingRequest::SessionNew, protocol::SESSION_NEW, json!({}));
    }

    /// Selects a session and replays its buffered history.
    pub fn select_session(&mut self, session_id: &str) {
        self.selected = Some(session_id.to_string());
        self.transcript.clear();
        self.plan.clear();
        self.last_message = None;
        self.cursor = 0;
        self.running_turn = None;
        self.request_history(session_id);
    }

    fn request_history(&mut self, session_id: &str) {
        self.send_request(
            PendingRequest::HistoryTail,
            protocol::HISTORY_TAIL,
            json!({ "sessionId": session_id, "fromCursor": 0 }),
        );
    }

    /// Sends a task to the selected session.
    pub fn send_task(&mut self, text: &str) {
        let Some(session_id) = self.selected.clone() else {
            self.notice = Some(String::from("select or create a session first"));
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        if self.running_turn.is_some() {
            self.notice = Some(String::from("a turn is already running"));
            return;
        }
        self.transcript.push(TranscriptItem::Message {
            role: Role::User,
            message_id: None,
            text: text.to_string(),
        });
        self.last_message = None;
        self.send_request(
            PendingRequest::TurnStart,
            protocol::TURN_START,
            json!({ "sessionId": session_id, "text": text }),
        );
    }

    /// Interrupts the running turn.
    pub fn interrupt(&mut self) {
        let Some(session_id) = self.selected.clone() else {
            return;
        };
        self.send_request(
            PendingRequest::TurnInterrupt,
            protocol::TURN_INTERRUPT,
            json!({ "sessionId": session_id }),
        );
    }

    /// Answers an approval by option index.
    pub fn answer_approval(&mut self, request_id: u64, option_index: usize) {
        let Some(position) = self.approvals.iter().position(|card| card.request_id == request_id)
        else {
            return;
        };
        let card = self.approvals.remove(position);
        let Some(option) = card.options.get(option_index) else {
            return;
        };
        match card.server_id {
            Some(server_id) => {
                self.outbound.push(protocol::approval_response(server_id, option.decision))
            }
            None => self.send_request(
                PendingRequest::PermissionsRespond,
                protocol::PERMISSIONS_RESPOND,
                json!({
                    "requestId": request_id,
                    "decision": option.decision,
                }),
            ),
        }
    }

    /// The render model for the DOM layer.
    #[must_use]
    pub fn view(&self) -> ViewState {
        let connection = match &self.connection {
            ConnectionState::Disconnected => String::from("disconnected"),
            ConnectionState::Connecting => String::from("connecting"),
            ConnectionState::Connected => match &self.permission_mode {
                Some(mode) => format!("connected · {mode}"),
                None => String::from("connected"),
            },
            ConnectionState::Failed(reason) => format!("failed: {reason}"),
        };
        let sessions = self
            .sessions
            .iter()
            .map(|session| SessionView {
                id: session.session_id.clone(),
                label: session.title.clone().unwrap_or_else(|| session.session_id.clone()),
                active: self.selected.as_deref() == Some(session.session_id.as_str()),
                running: session.turn_running
                    || self.running_turn.is_some()
                        && self.selected.as_deref() == Some(session.session_id.as_str()),
            })
            .collect();
        let lines: Vec<ViewLine> = self
            .transcript
            .iter()
            .rev()
            .take(MAX_VIEW_LINES)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|item| match item {
                TranscriptItem::Message { role, text, .. } => match role {
                    Role::User => ViewLine::User(text.clone()),
                    Role::Assistant => ViewLine::Assistant(text.clone()),
                    Role::Thought => ViewLine::Thought(text.clone()),
                },
                TranscriptItem::Tool { title, status, .. } => {
                    ViewLine::Tool(title.clone(), status.clone())
                }
                TranscriptItem::Notice(text) => ViewLine::Notice(text.clone()),
            })
            .collect();
        let approvals = self
            .approvals
            .iter()
            .map(|card| ApprovalView {
                request_id: card.request_id,
                summary: card.summary.clone(),
                options: card
                    .options
                    .iter()
                    .enumerate()
                    .map(|(index, option)| ApprovalOptionView {
                        index,
                        label: option.label.clone(),
                    })
                    .collect(),
            })
            .collect();
        ViewState {
            connection,
            sessions,
            lines,
            plan: self.plan.clone(),
            approvals,
            can_send: self.connection == ConnectionState::Connected
                && self.selected.is_some()
                && self.running_turn.is_none(),
            notice: self.notice.clone(),
        }
    }
}

/// Immutable view state consumed by the DOM layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewState {
    pub connection: String,
    pub sessions: Vec<SessionView>,
    pub lines: Vec<ViewLine>,
    pub plan: Vec<String>,
    pub approvals: Vec<ApprovalView>,
    pub can_send: bool,
    pub notice: Option<String>,
}

/// Session row for the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: String,
    pub label: String,
    pub active: bool,
    pub running: bool,
}

/// One rendered transcript line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewLine {
    User(String),
    Assistant(String),
    Thought(String),
    Tool(String, String),
    Notice(String),
}

/// Approval card for the DOM layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalView {
    pub request_id: u64,
    pub summary: String,
    pub options: Vec<ApprovalOptionView>,
}

/// Approval button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalOptionView {
    pub index: usize,
    pub label: String,
}

fn append_capped(existing: &mut String, addition: &str) {
    existing.push_str(addition);
    let length = existing.chars().count();
    if length <= MAX_TEXT_CHARS {
        return;
    }
    let excess = length - MAX_TEXT_CHARS;
    let cut = existing.char_indices().nth(excess).map_or(0, |(index, _)| index);
    existing.drain(..cut);
    existing.insert(0, '…');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_response() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": protocol::PROTOCOL_VERSION,
                "serverInfo": { "name": "ee-agent-remote", "version": "0.1" },
                "capabilities": { "steering": false, "queueing": false },
                "permissionMode": "ask"
            }
        })
    }

    #[test]
    fn handshake_sequence_is_ordered() {
        let mut model = AppModel::new();
        model.on_open();
        let outbound = model.drain_outbound();
        assert_eq!(outbound[0]["method"], protocol::INITIALIZE);

        model.apply_frame(&initialize_response());
        let outbound = model.drain_outbound();
        assert_eq!(outbound[0]["method"], protocol::INITIALIZED);
        assert_eq!(outbound[1]["method"], protocol::SESSION_LIST);
        assert_eq!(model.connection, ConnectionState::Connected);
    }

    #[test]
    fn streaming_updates_render_in_order() {
        let mut model = AppModel::new();
        model.selected = Some(String::from("s-1"));
        model.apply_frame(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s-1",
                "cursor": 1,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "m-1",
                    "content": { "type": "text", "text": "hello" }
                }
            }
        }));
        model.apply_frame(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s-1",
                "cursor": 2,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "m-1",
                    "content": { "type": "text", "text": " world" }
                }
            }
        }));
        let view = model.view();
        assert_eq!(view.lines, vec![ViewLine::Assistant(String::from("hello world"))]);
        assert_eq!(model.cursor, 2);
    }

    #[test]
    fn notifications_for_other_sessions_do_not_leak() {
        let mut model = AppModel::new();
        model.selected = Some(String::from("s-1"));
        model.apply_frame(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s-2",
                "cursor": 9,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "other" }
                }
            }
        }));
        assert!(model.view().lines.is_empty());
    }

    #[test]
    fn approvals_are_answerable_and_removed() {
        let mut model = AppModel::new();
        model.selected = Some(String::from("s-1"));
        model.apply_frame(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "permission/requestApproval",
            "params": {
                "requestId": 7,
                "sessionId": "s-1",
                "summary": "write file",
                "toolCall": { "toolCallId": "c-1", "title": "write" },
                "options": [
                    { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
                ],
                "expiresInMs": 1000
            }
        }));
        assert_eq!(model.view().approvals.len(), 1);
        model.answer_approval(7, 0);
        let outbound = model.drain_outbound();
        assert_eq!(outbound[0]["id"], 3);
        assert_eq!(outbound[0]["result"]["decision"], "allow");
        assert!(model.view().approvals.is_empty());
    }

    #[test]
    fn reconnect_clears_and_replays() {
        let mut model = AppModel::new();
        model.on_open();
        model.apply_frame(&initialize_response());
        let _ = model.drain_outbound();
        model.apply_frame(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s-1",
                "cursor": 5,
                "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "old" } }
            }
        }));
        model.selected = Some(String::from("s-1"));
        model.cursor = 5;
        model.on_close("network");
        model.on_open();
        assert!(model.transcript.is_empty());
        assert_eq!(model.cursor, 0);
        let _ = model.drain_outbound();
        // History replay restores the transcript from cursor 0.
        model.apply_frame(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s-1",
                "cursor": 6,
                "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "new" } }
            }
        }));
        assert_eq!(model.view().lines, vec![ViewLine::Assistant(String::from("new"))]);
    }

    #[test]
    fn transcript_and_view_are_capped() {
        let mut model = AppModel::new();
        model.selected = Some(String::from("s-1"));
        for index in 0..(MAX_VIEW_LINES + 50) {
            model.transcript.push(TranscriptItem::Notice(format!("line {index}")));
        }
        let view = model.view();
        assert_eq!(view.lines.len(), MAX_VIEW_LINES);
        assert_eq!(
            view.lines.last(),
            Some(&ViewLine::Notice(format!("line {}", MAX_VIEW_LINES + 49)))
        );
    }

    #[test]
    fn long_chunks_are_truncated_without_splitting_characters() {
        let mut existing = String::from("é");
        append_capped(&mut existing, &"x".repeat(MAX_TEXT_CHARS * 2));
        assert!(existing.chars().count() <= MAX_TEXT_CHARS + 1);
    }

    #[test]
    fn errors_are_surfaced_not_swallowed() {
        let mut model = AppModel::new();
        model.on_open();
        model.apply_frame(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32001, "message": "outbound queue is full" }
        }));
        assert!(matches!(model.connection, ConnectionState::Failed(_)));
        assert!(model.view().connection.contains("-32001"));
    }
}
