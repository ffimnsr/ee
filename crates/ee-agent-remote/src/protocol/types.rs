//! Typed params, results, and payloads for every remote registry entry.
//!
//! Wire naming is camelCase, matching the ACP surface drivers already know.
//! Types are the single source of truth for fixtures and handlers.

use ee_agent_protocol::{
    PermissionOption, SessionConfigOption, SessionModeState, SessionUpdate, StopReason,
    ToolCallUpdate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Client identity supplied on `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// Driver or UI name.
    pub name: String,
    /// Optional version string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// `initialize` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Remote protocol version (`"1.0"`).
    pub protocol_version: String,
    /// Client identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,
    /// Stable connection identity used in audit records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// Server identity returned by `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
}

/// What this remote server can do. `steering` and `queueing` are always false:
/// ACP has no steer and the prompt inbox is a separate plan item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Mid-turn steering is not supported.
    pub steering: bool,
    /// Follow-up queueing is not supported.
    pub queueing: bool,
    /// Client-visible approval requests are supported.
    pub permission_approvals: bool,
    /// Cursor replay through `history/tail` is supported.
    pub history: bool,
}

/// `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// Negotiated protocol version.
    pub protocol_version: String,
    /// Server identity.
    pub server_info: ServerInfo,
    /// Advertised capabilities.
    pub capabilities: ServerCapabilities,
    /// Approval posture in force (`ask`, `rules`, or `deny`).
    pub permission_mode: String,
}

/// One configured agent summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    /// Configured agent id.
    pub agent_id: String,
    /// Whether a connection is already live.
    pub connected: bool,
    /// Whether this is the resolved default agent.
    pub default: bool,
}

/// `session/list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListParams {
    /// Restrict to one agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// One loaded session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    /// ACP session id.
    pub session_id: String,
    /// Owning agent id.
    pub agent_id: String,
    /// Session working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Host-visible session title, when the agent reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether a turn is currently running.
    pub turn_running: bool,
}

/// `session/list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResult {
    /// Loaded sessions, sorted by session id.
    pub sessions: Vec<SessionSummary>,
    /// Configured agents.
    pub agents: Vec<AgentSummary>,
}

/// `session/new` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewParams {
    /// Agent id; defaults to the resolved default agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Workspace root; defaults to the host process working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra session directories.
    #[serde(default)]
    pub additional_directories: Vec<String>,
    /// Initial mode id, when the agent advertises modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// `session/new` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewResult {
    /// ACP session id.
    pub session_id: String,
    /// Owning agent id.
    pub agent_id: String,
    /// Advertised mode state, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModeState>,
    /// Advertised config options.
    #[serde(default)]
    pub config_options: Vec<SessionConfigOption>,
}

/// `session/resume` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResumeParams {
    /// Session id to resume.
    pub session_id: String,
    /// Agent id; defaults to the resolved default agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Workspace root; defaults to the host process working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra session directories.
    #[serde(default)]
    pub additional_directories: Vec<String>,
}

/// `session/close` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCloseParams {
    /// Session id to close.
    pub session_id: String,
}

/// `session/close` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCloseResult {
    /// Whether a loaded session was closed.
    pub closed: bool,
}

/// `session/set_mode` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetModeParams {
    /// Target session.
    pub session_id: String,
    /// Mode id advertised by the agent.
    pub mode_id: String,
}

/// `session/set_mode` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetModeResult {
    /// Whether the mode was applied.
    pub applied: bool,
}

/// `session/set-model` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetModelParams {
    /// Target session.
    pub session_id: String,
    /// Model id advertised by the agent.
    pub model_id: String,
}

/// `session/set-model` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetModelResult {
    /// Whether the model was applied.
    pub applied: bool,
}

/// `turn/start` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    /// Target session.
    pub session_id: String,
    /// Prompt text.
    pub text: String,
}

/// `turn/start` result. The call returns immediately; completion is a
/// notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResult {
    /// Remote turn id used in `turn/completed`.
    pub turn_id: u64,
    /// Target session.
    pub session_id: String,
}

/// `turn/interrupt` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    /// Target session.
    pub session_id: String,
}

/// `turn/interrupt` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptResult {
    /// Always true; a second interrupt is a typed no-op (`interrupted: false`).
    pub interrupted: bool,
}

/// `permissions/pending` params (empty object).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsPendingParams {}

/// One pending approval, bounded and redacted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    /// Monotonic remote approval id.
    pub request_id: u64,
    /// Session the approval belongs to.
    pub session_id: String,
    /// Bounded human summary.
    pub summary: String,
    /// Tool call requiring approval (redacted).
    pub tool_call: ToolCallUpdate,
    /// Options the driver may select.
    pub options: Vec<PermissionOption>,
    /// Milliseconds until expiry; expiry always denies.
    pub expires_in_ms: u64,
}

/// `permissions/pending` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsPendingResult {
    /// Pending approvals, oldest first.
    pub approvals: Vec<PendingApproval>,
}

/// Driver-visible decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Allow this one operation.
    Allow,
    /// Allow this operation and remember it for matching future operations.
    AllowAlways,
    /// Deny the operation.
    Deny,
}

/// `permissions/respond` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsRespondParams {
    /// Approval id from `permissions/pending` or `permission/requestApproval`.
    pub request_id: u64,
    /// Decision to apply.
    pub decision: PermissionDecision,
    /// Optional exact option id offered by the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
}

/// `permissions/respond` result. A second answer is a typed
/// `already_resolved` error, not a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsRespondResult {
    /// Always true on success.
    pub resolved: bool,
}

/// `history/tail` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryTailParams {
    /// Target session.
    pub session_id: String,
    /// Last cursor the client processed; omit to replay from the oldest
    /// retained entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_cursor: Option<u64>,
    /// Maximum notifications to return (bounded by the host page size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// One buffered notification with its cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BufferedNotification {
    /// Cursor assigned when the notification was first recorded.
    pub cursor: u64,
    /// Notification method name.
    pub method: String,
    /// Notification params.
    pub params: Value,
}

/// `history/tail` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryTailResult {
    /// Target session.
    pub session_id: String,
    /// Highest cursor retained after this page.
    pub cursor: u64,
    /// Notifications after `fromCursor`, oldest first.
    pub notifications: Vec<BufferedNotification>,
    /// True when the requested range fell out of the ring buffer; clients must
    /// re-sync from the snapshot.
    pub gap: bool,
}

/// `session/update` notification params (ACP passthrough).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateNotification {
    /// Session the update belongs to.
    pub session_id: String,
    /// Buffer cursor.
    pub cursor: u64,
    /// Raw ACP update.
    pub update: SessionUpdate,
}

/// Terminal outcome of one remote turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// The agent ended the turn.
    Completed,
    /// The turn was cancelled or interrupted.
    Cancelled,
    /// The turn failed.
    Failed,
    /// The turn paused on a recoverable interruption.
    PausedRecoverable,
}

/// Bounded failure description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFailure {
    /// Stable error class.
    pub class: String,
    /// Redacted message.
    pub message: String,
}

/// `turn/completed` notification params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    /// Session the turn belonged to.
    pub session_id: String,
    /// Remote turn id from `turn/start`.
    pub turn_id: u64,
    /// Buffer cursor.
    pub cursor: u64,
    /// Terminal status.
    pub status: TurnStatus,
    /// ACP stop reason, when the agent completed normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// Bounded failure, when the turn failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RemoteFailure>,
}

/// Connection/session status value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    /// Connection is starting.
    Starting,
    /// Connection completed the ACP handshake.
    Ready,
    /// Connection failed.
    Failed,
    /// Connection closed.
    Closed,
}

/// `status/changed` notification params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusChangedNotification {
    /// Buffer cursor.
    pub cursor: u64,
    /// Agent id, when connection-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Session id, when session-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// New state.
    pub state: StatusState,
    /// Bounded detail (redacted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `permission/requestApproval` server→client request params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionApprovalRequest {
    /// Approval id.
    pub request_id: u64,
    /// Session the approval belongs to.
    pub session_id: String,
    /// Bounded human summary.
    pub summary: String,
    /// Tool call requiring approval (redacted).
    pub tool_call: ToolCallUpdate,
    /// Options the client may select.
    pub options: Vec<PermissionOption>,
    /// Milliseconds until expiry; expiry always denies.
    pub expires_in_ms: u64,
}

/// Result a client sends in response to `permission/requestApproval`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionApprovalResponse {
    /// Decision to apply.
    pub decision: PermissionDecision,
    /// Optional exact option id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_start_params_require_text() {
        assert!(
            serde_json::from_value::<TurnStartParams>(serde_json::json!({
                "sessionId": "s-1",
                "text": "hello"
            }))
            .is_ok()
        );
        assert!(
            serde_json::from_value::<TurnStartParams>(serde_json::json!({
                "sessionId": "s-1"
            }))
            .is_err()
        );
    }

    #[test]
    fn permission_decision_rejects_unknown_values() {
        assert!(serde_json::from_value::<PermissionDecision>(serde_json::json!("allow")).is_ok());
        assert!(
            serde_json::from_value::<PermissionDecision>(serde_json::json!("allow_all")).is_err()
        );
    }

    #[test]
    fn status_states_are_snake_case() {
        assert_eq!(
            serde_json::to_value(StatusState::Ready).expect("serialize"),
            serde_json::json!("ready")
        );
    }
}
