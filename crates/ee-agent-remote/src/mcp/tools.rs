//! Tool definitions and dispatch for the MCP server.
//!
//! Drivers get deterministic, poll-based control: what the agent is doing,
//! start new work, read history. Every tool validates its arguments, returns
//! bounded results, and never widens the host's permission mode.
//!
//! Tool-level failures (unknown session, busy turn, disabled history) come
//! back as `isError` results carrying a typed `code`, so drivers never have to
//! parse prose.

use rmcp::model::{CallToolRequestParams, CallToolResponse, ErrorData, JsonObject, Tool};
use serde_json::{Value, json};

use crate::dispatch::CallerIdentity;
use crate::error::RemoteError;
use crate::history::MAX_HISTORY_ENTRIES;
use crate::host::RemoteHost;
use crate::protocol::Method;

use super::args::{
    complete_json, optional_string, optional_string_array, optional_u64, require_arguments,
    require_no_arguments, require_string, tool_error,
};

/// Maximum updates returned by one poll regardless of the requested limit.
const MAX_POLL_UPDATES: usize = 200;
/// Maximum active tools reported by one poll.
const MAX_ACTIVE_TOOLS: usize = 8;
/// Maximum plan entries reported by one poll.
const MAX_PLAN_ENTRIES: usize = 20;

/// Stable names of every tool advertised by this server, in discovery order.
///
/// Keep this list aligned with [`tool_list`]; the unit test below fails when
/// they diverge, and the documentation drift test reads this list.
pub(crate) const TOOL_NAMES: &[&str] = &[
    "ee_status",
    "ee_sessions_list",
    "ee_session_new",
    "ee_task_send",
    "ee_updates_poll",
    "ee_turn_interrupt",
    "ee_permissions_pending",
    "ee_permission_respond",
    "ee_session_history",
];

/// The tool list advertised by this server.
pub(crate) fn tool_list() -> Vec<Tool> {
    vec![
        Tool::new(
            "ee_status",
            "Return the headless host status: configured agents, connection state, loaded sessions, permission mode, and audit health. Takes no arguments.",
            schema(json!({ "type": "object", "properties": {} })),
        ),
        Tool::new(
            "ee_sessions_list",
            "List loaded sessions and configured agents. Takes no arguments.",
            schema(json!({ "type": "object", "properties": {} })),
        ),
        Tool::new(
            "ee_session_new",
            "Start a new agent session. Returns the ACP session id used by every other tool.",
            schema(json!({
                "type": "object",
                "properties": {
                    "agentId": { "type": "string" },
                    "cwd": { "type": "string" },
                    "additionalDirectories": { "type": "array", "items": { "type": "string" } },
                    "mode": { "type": "string" }
                }
            })),
        ),
        Tool::new(
            "ee_task_send",
            "Send a task to a session. Returns immediately with the turn id; poll ee_updates_poll for progress and completion. A busy session returns code already_active.",
            schema(json!({
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "text": { "type": "string" }
                },
                "required": ["sessionId", "text"]
            })),
        ),
        Tool::new(
            "ee_updates_poll",
            "Return buffered updates after a cursor plus turn state, active tools, plan state, and a bounded assistant text tail. Pass the returned cursor back to poll again; gap=true means re-sync from the snapshot.",
            schema(json!({
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "cursor": { "type": "integer" },
                    "limit": { "type": "integer" }
                },
                "required": ["sessionId"]
            })),
        ),
        Tool::new(
            "ee_turn_interrupt",
            "Cancel the running turn. A second interrupt (or an interrupt with no running turn) returns interrupted=false.",
            schema(json!({
                "type": "object",
                "properties": { "sessionId": { "type": "string" } },
                "required": ["sessionId"]
            })),
        ),
        Tool::new(
            "ee_permissions_pending",
            "List pending approvals. Takes no arguments. In rules mode entries are not answerable and never appear here.",
            schema(json!({ "type": "object", "properties": {} })),
        ),
        Tool::new(
            "ee_permission_respond",
            "Answer a pending approval. decision is allow, allow_always, or deny. A late answer returns code already_resolved; rules and deny modes reject client answers.",
            schema(json!({
                "type": "object",
                "properties": {
                    "requestId": { "type": "integer" },
                    "decision": { "type": "string", "enum": ["allow", "allow_always", "deny"] },
                    "optionId": { "type": "string" }
                },
                "required": ["requestId", "decision"]
            })),
        ),
        Tool::new(
            "ee_session_history",
            "Tail the durable turn history for a session. Requires a configured session log directory; otherwise returns code history_disabled.",
            schema(json!({
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["sessionId"]
            })),
        ),
    ]
}

/// Validates and dispatches one tool call.
pub(crate) async fn dispatch(
    host: &RemoteHost,
    request: &CallToolRequestParams,
) -> Result<CallToolResponse, ErrorData> {
    match request.name.as_ref() {
        "ee_status" => {
            require_no_arguments(request)?;
            Ok(complete_json(status(host)))
        }
        "ee_sessions_list" => {
            require_no_arguments(request)?;
            Ok(dispatch_result(host, Method::SessionList, json!({})).await)
        }
        "ee_session_new" => {
            let arguments = require_arguments(request)?;
            let mut params = JsonObject::new();
            if let Some(agent_id) = optional_string(arguments, "agentId")? {
                params.insert("agentId".into(), json!(agent_id));
            }
            if let Some(cwd) = optional_string(arguments, "cwd")? {
                params.insert("cwd".into(), json!(cwd));
            }
            if let Some(mode) = optional_string(arguments, "mode")? {
                params.insert("mode".into(), json!(mode));
            }
            let directories = optional_string_array(arguments, "additionalDirectories")?;
            if !directories.is_empty() {
                params.insert("additionalDirectories".into(), json!(directories));
            }
            Ok(dispatch_result(host, Method::SessionNew, Value::Object(params)).await)
        }
        "ee_task_send" => {
            let arguments = require_arguments(request)?;
            let session_id = require_string(arguments, "sessionId")?;
            let text = require_string(arguments, "text")?;
            Ok(dispatch_result(
                host,
                Method::TurnStart,
                json!({ "sessionId": session_id, "text": text }),
            )
            .await)
        }
        "ee_updates_poll" => {
            let arguments = require_arguments(request)?;
            let session_id = require_string(arguments, "sessionId")?;
            let cursor = optional_u64(arguments, "cursor")?.unwrap_or(0);
            let limit = optional_u64(arguments, "limit")?
                .map(|limit| limit as usize)
                .unwrap_or(host.config().history_page_size)
                .clamp(1, MAX_POLL_UPDATES)
                .min(host.config().history_page_size);
            poll(host, session_id, cursor, limit)
        }
        "ee_turn_interrupt" => {
            let arguments = require_arguments(request)?;
            let session_id = require_string(arguments, "sessionId")?;
            Ok(dispatch_result(host, Method::TurnInterrupt, json!({ "sessionId": session_id }))
                .await)
        }
        "ee_permissions_pending" => {
            require_no_arguments(request)?;
            Ok(dispatch_result(host, Method::PermissionsPending, json!({})).await)
        }
        "ee_permission_respond" => {
            let arguments = require_arguments(request)?;
            let request_id = optional_u64(arguments, "requestId")?.ok_or_else(|| {
                ErrorData::invalid_params("missing integer argument \"requestId\"", None)
            })?;
            let decision = require_string(arguments, "decision")?;
            if !matches!(decision, "allow" | "allow_always" | "deny") {
                return Err(ErrorData::invalid_params(
                    "decision must be allow, allow_always, or deny",
                    None,
                ));
            }
            let mut params = JsonObject::new();
            params.insert("requestId".into(), json!(request_id));
            params.insert("decision".into(), json!(decision));
            if let Some(option_id) = optional_string(arguments, "optionId")? {
                params.insert("optionId".into(), json!(option_id));
            }
            Ok(dispatch_result(host, Method::PermissionsRespond, Value::Object(params)).await)
        }
        "ee_session_history" => {
            let arguments = require_arguments(request)?;
            let session_id = require_string(arguments, "sessionId")?;
            let limit = optional_u64(arguments, "limit")?
                .map(|limit| limit as usize)
                .unwrap_or(100)
                .clamp(1, MAX_HISTORY_ENTRIES);
            history(host, session_id, limit)
        }
        other => Ok(tool_error(format!("unknown tool {other:?}"))),
    }
}

async fn dispatch_result(host: &RemoteHost, method: Method, params: Value) -> CallToolResponse {
    match host.dispatch_as(&CallerIdentity::new("mcp"), method, Some(params)).await {
        Ok(result) => complete_json(result),
        Err(error) => remote_error(host, error),
    }
}

fn poll(
    host: &RemoteHost,
    session_id: &str,
    cursor: u64,
    limit: usize,
) -> Result<CallToolResponse, ErrorData> {
    let Ok(session) = host.session(session_id) else {
        return Ok(remote_error(host, RemoteError::not_found(format!("session {session_id:?}"))));
    };
    let tail = host.bus().tail(session_id, cursor, limit);
    let snapshot = session.thread.snapshot();
    let updates: Vec<Value> = tail
        .notifications
        .into_iter()
        .map(|notification| {
            json!({
                "cursor": notification.cursor,
                "method": notification.method,
                "params": host.redactor().value(&notification.params),
            })
        })
        .collect();
    let mut active_tools = Vec::new();
    for tool_call in snapshot.tool_calls.values() {
        let status = format!("{:?}", tool_call.status).to_ascii_lowercase();
        if !matches!(status.as_str(), "pending" | "in_progress") {
            continue;
        }
        active_tools.push(json!({
            "toolCallId": tool_call.tool_call_id,
            "title": host.redactor().text(&tool_call.title),
            "kind": format!("{:?}", tool_call.kind).to_ascii_lowercase(),
            "status": status,
        }));
        if active_tools.len() >= MAX_ACTIVE_TOOLS {
            break;
        }
    }
    let plan: Vec<Value> = snapshot
        .plan
        .iter()
        .take(MAX_PLAN_ENTRIES)
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    Ok(complete_json(json!({
        "sessionId": session_id,
        "cursor": tail.cursor,
        "gap": tail.gap,
        "updates": updates,
        "turnRunning": session.turn_running(),
        "activeTurnId": session.active_turn(),
        "activeTools": active_tools,
        "plan": plan,
        "textTail": crate::history::assistant_text_tail(&snapshot, host.redactor()),
    })))
}

fn history(
    host: &RemoteHost,
    session_id: &str,
    limit: usize,
) -> Result<CallToolResponse, ErrorData> {
    if !host.history().enabled() {
        return Ok(tool_error(
            json!({
                "code": "history_disabled",
                "message": "durable session history is disabled; start serve/mcp with --session-log-dir",
            })
            .to_string(),
        ));
    }
    if host.session(session_id).is_err() {
        return Ok(remote_error(host, RemoteError::not_found(format!("session {session_id:?}"))));
    }
    match host.history().tail(session_id, limit) {
        Ok(entries) => Ok(complete_json(json!({
            "sessionId": session_id,
            "entries": entries,
        }))),
        Err(error) => Ok(tool_error(
            json!({
                "code": "history_unavailable",
                "message": host.redactor().text(&error.to_string()),
            })
            .to_string(),
        )),
    }
}

/// Typed tool-level timeout result.
pub(crate) fn timeout_response(method: &str, timeout: std::time::Duration) -> CallToolResponse {
    tool_error(
        json!({
            "code": "timeout",
            "message": format!("tool {method:?} exceeded {} ms", timeout.as_millis()),
        })
        .to_string(),
    )
}

fn remote_error(host: &RemoteHost, error: RemoteError) -> CallToolResponse {
    let payload = json!({
        "code": error.code().as_str(),
        "message": host.redactor().text(error.message()),
        "data": error.data().map(|data| host.redactor().value(data)),
    });
    tool_error(payload.to_string())
}

fn status(host: &RemoteHost) -> Value {
    let default_agent =
        host.manager().resolve_default_agent(host.config().default_agent.as_deref());
    let agents: Vec<Value> = host
        .manager()
        .agent_ids()
        .into_iter()
        .map(|agent_id| {
            json!({
                "agentId": agent_id,
                "connected": host.manager().connection_state(&agent_id).is_some(),
                "default": default_agent.as_deref() == Some(agent_id.as_str()),
            })
        })
        .collect();
    json!({
        "server": {
            "name": super::MCP_SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "protocolVersion": crate::REMOTE_PROTOCOL_VERSION,
        },
        "agents": agents,
        "sessions": host.session_count(),
        "permissionMode": host.config().permission_mode.as_str(),
        "audit": {
            "enabled": host.approvals().audit().path().is_some(),
            "failures": host.approvals().audit().failure_count(),
        },
        "history": {
            "enabled": host.history().enabled(),
        },
        "capabilities": {
            "steering": false,
            "queueing": false,
            "permissionApprovals": true,
            "history": true,
        },
    })
}

fn schema(value: Value) -> JsonObject {
    value.as_object().expect("tool schema must be an object").clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_tool_names_match_stable_list() {
        let advertised: Vec<String> =
            tool_list().iter().map(|tool| tool.name.to_string()).collect();
        assert_eq!(advertised, TOOL_NAMES);
    }
}
