//! Wire contract used by the web client.
//!
//! The backend registry is the source of truth; this module mirrors only the
//! envelopes and fields the UI needs. ACP `session/update` payloads stay as
//! `serde_json::Value` and are rendered defensively, so an unknown future
//! update variant cannot break the client.

use serde_json::{Value, json};

/// Remote protocol version this client speaks.
pub const PROTOCOL_VERSION: &str = "1.0";
/// `initialize` request.
pub const INITIALIZE: &str = "initialize";
/// `initialized` notification.
pub const INITIALIZED: &str = "initialized";
/// `session/list` request.
pub const SESSION_LIST: &str = "session/list";
/// `session/new` request.
pub const SESSION_NEW: &str = "session/new";
/// `turn/start` request.
pub const TURN_START: &str = "turn/start";
/// `turn/interrupt` request.
pub const TURN_INTERRUPT: &str = "turn/interrupt";
/// `permissions/respond` request.
pub const PERMISSIONS_RESPOND: &str = "permissions/respond";
/// `history/tail` request.
pub const HISTORY_TAIL: &str = "history/tail";
/// ACP update passthrough notification.
pub const SESSION_UPDATE: &str = "session/update";
/// Turn terminal notification.
pub const TURN_COMPLETED: &str = "turn/completed";
/// Connection/session status notification.
pub const STATUS_CHANGED: &str = "status/changed";
/// Server→client approval request.
pub const PERMISSION_REQUEST_APPROVAL: &str = "permission/requestApproval";

/// Builds a JSON-RPC request frame.
#[must_use]
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Builds a JSON-RPC notification frame.
#[must_use]
pub fn notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

/// Builds the response to a server→client approval request.
#[must_use]
pub fn approval_response(server_id: u64, decision: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": server_id,
        "result": { "decision": decision },
    })
}

/// One session row from `session/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub session_id: String,
    pub agent_id: String,
    pub title: Option<String>,
    pub turn_running: bool,
}

/// Parses `session/list` into UI rows.
#[must_use]
pub fn parse_sessions(result: &Value) -> Vec<SessionEntry> {
    result["sessions"]
        .as_array()
        .map(|sessions| {
            sessions
                .iter()
                .filter_map(|session| {
                    Some(SessionEntry {
                        session_id: session["sessionId"].as_str()?.to_string(),
                        agent_id: session["agentId"].as_str().unwrap_or_default().to_string(),
                        title: session["title"].as_str().map(str::to_string),
                        turn_running: session["turnRunning"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Text content of one ACP content block, when it is text.
#[must_use]
pub fn content_text(content: &Value) -> Option<&str> {
    content["text"].as_str()
}

/// Rendered label for one approval option.
#[must_use]
pub fn option_label(option: &Value) -> String {
    option["name"].as_str().or_else(|| option["optionId"].as_str()).unwrap_or("option").to_string()
}

/// Maps an approval option kind to the wire decision value.
#[must_use]
pub fn decision_for_option(option: &Value) -> &'static str {
    match option["kind"].as_str().unwrap_or_default() {
        "allow_always" => "allow_always",
        "allow_once" => "allow",
        _ => "deny",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_and_methods_are_preserved() {
        let frame = request(7, SESSION_NEW, json!({}));
        assert_eq!(frame["id"], 7);
        assert_eq!(frame["method"], SESSION_NEW);
    }

    #[test]
    fn session_rows_parse_missing_fields_defensively() {
        let sessions = parse_sessions(&json!({
            "sessions": [
                { "sessionId": "s-1", "agentId": "a" },
                { "agentId": "missing-id" }
            ]
        }));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s-1");
        assert!(!sessions[0].turn_running);
    }

    #[test]
    fn option_kinds_map_to_decisions() {
        assert_eq!(decision_for_option(&json!({ "kind": "allow_once" })), "allow");
        assert_eq!(decision_for_option(&json!({ "kind": "allow_always" })), "allow_always");
        assert_eq!(decision_for_option(&json!({ "kind": "reject_once" })), "deny");
    }
}
