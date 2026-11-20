//! Host-side flow test for the DOM-free web model.
//!
//! Frames mirror the backend registry exactly, so this test pins the browser
//! contract without a browser.

use ee_web_frontend::model::{AppModel, ViewLine};
use ee_web_frontend::protocol;
use serde_json::{Value, json};

fn initialize_response(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": protocol::PROTOCOL_VERSION,
            "serverInfo": { "name": "ee-agent-remote", "version": "0.1" },
            "capabilities": { "steering": false, "queueing": false },
            "permissionMode": "ask"
        }
    })
}

fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

fn update(session: &str, cursor: u64, update: Value) -> Value {
    notification(
        protocol::SESSION_UPDATE,
        json!({ "sessionId": session, "cursor": cursor, "update": update }),
    )
}

#[test]
fn browser_flow_list_send_stream_approve_complete_and_reconnect() {
    let mut model = AppModel::new();
    model.on_open();
    let connect = model.drain_outbound();
    assert_eq!(connect[0]["method"], protocol::INITIALIZE);

    model.apply_frame(&initialize_response(connect[0]["id"].as_u64().expect("id")));
    let after_init = model.drain_outbound();
    assert_eq!(after_init[0]["method"], protocol::INITIALIZED);
    assert_eq!(after_init[1]["method"], protocol::SESSION_LIST);
    let list_id = after_init[1]["id"].as_u64().expect("list id");

    // A single session auto-selects and replays history from cursor 0.
    model.apply_frame(&json!({
        "jsonrpc": "2.0",
        "id": list_id,
        "result": {
            "sessions": [{ "sessionId": "s-1", "agentId": "a", "turnRunning": false }],
            "agents": []
        }
    }));
    let replay = model.drain_outbound();
    assert_eq!(replay[0]["method"], protocol::HISTORY_TAIL);
    assert_eq!(replay[0]["params"]["fromCursor"], 0);

    model.apply_frame(&json!({
        "jsonrpc": "2.0",
        "id": replay[0]["id"].as_u64().expect("history id"),
        "result": {
            "sessionId": "s-1",
            "cursor": 2,
            "notifications": [
                {
                    "cursor": 1,
                    "method": protocol::SESSION_UPDATE,
                    "params": {
                        "sessionId": "s-1",
                        "cursor": 1,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": "m-0",
                            "content": { "type": "text", "text": "previous" }
                        }
                    }
                },
                {
                    "cursor": 2,
                    "method": protocol::SESSION_UPDATE,
                    "params": {
                        "sessionId": "s-1",
                        "cursor": 2,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": "m-0",
                            "content": { "type": "text", "text": " answer" }
                        }
                    }
                }
            ],
            "gap": false
        }
    }));
    assert_eq!(model.cursor, 2);
    assert_eq!(model.view().lines, vec![ViewLine::Assistant(String::from("previous answer"))]);

    model.send_task("do work");
    let sent = model.drain_outbound();
    assert_eq!(sent[0]["method"], protocol::TURN_START);
    model.apply_frame(&json!({
        "jsonrpc": "2.0",
        "id": sent[0]["id"].as_u64().expect("turn id"),
        "result": { "turnId": 4, "sessionId": "s-1" }
    }));

    model.apply_frame(&update(
        "s-1",
        3,
        json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": "m-1",
            "content": { "type": "text", "text": "working" }
        }),
    ));
    model.apply_frame(&update(
        "s-1",
        4,
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-1",
            "title": "cargo test",
            "status": "in_progress"
        }),
    ));

    // Approval arrives as a server→client request; answering is a raw
    // JSON-RPC response with the same id.
    model.apply_frame(&json!({
        "jsonrpc": "2.0",
        "id": 900,
        "method": protocol::PERMISSION_REQUEST_APPROVAL,
        "params": {
            "requestId": 11,
            "sessionId": "s-1",
            "summary": "run cargo test",
            "toolCall": { "toolCallId": "call-1", "title": "cargo test" },
            "options": [
                { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
            ],
            "expiresInMs": 300000
        }
    }));
    assert_eq!(model.view().approvals.len(), 1);
    model.answer_approval(11, 0);
    let answer = model.drain_outbound();
    assert_eq!(answer[0]["id"], 900);
    assert_eq!(answer[0]["result"]["decision"], "allow");

    model.apply_frame(&notification(
        protocol::TURN_COMPLETED,
        json!({
            "sessionId": "s-1",
            "turnId": 4,
            "cursor": 5,
            "status": "completed",
            "stopReason": "end_turn"
        }),
    ));
    assert!(model.running_turn.is_none());
    let view = model.view();
    assert!(view.lines.iter().any(|line| matches!(line, ViewLine::Tool(title, status) if title == "cargo test" && status == "in_progress")));
    assert!(view.can_send);

    // Reconnect: transcript clears, cursor resets, replay restores it.
    model.on_close("network");
    model.on_open();
    assert!(model.transcript.is_empty());
    assert_eq!(model.cursor, 0);
    let reconnect = model.drain_outbound();
    assert_eq!(reconnect[0]["method"], protocol::INITIALIZE);
}
