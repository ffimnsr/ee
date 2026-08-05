//! Conformance coverage: protocol fixture files under `tests/fixtures` must
//! parse as SDK-backed wire frames and round-trip into typed SDK request and
//! response structs.  The fixtures mirror real ACP v1 traffic captured at
//! the wire boundary; these tests pin the wire shape the framework accepts.
//!
//! Fixtures are exact JSON lines (like the transport's newline-delimited
//! framing), so each one must also survive `RawJsonRpcMessage` serialization
//! unchanged — a round-trip failure means the frame would not be re-emitted
//! byte-identically by the writer path.

mod common;

use std::path::PathBuf;

use ee_agent_protocol::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    ProtocolVersion, RawJsonRpcMessage, ReadTextFileResponse, Response, SessionId,
};
use serde_json::Value;

fn fixture_value(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("failed to read fixture {name}: {error}");
    });
    serde_json::from_str(&raw).unwrap_or_else(|error| {
        panic!("fixture {name} is not valid JSON: {error}");
    })
}

/// Asserts the fixture parses as a raw wire frame and serializes back to the
/// exact same JSON value (field order is irrelevant to `serde_json::Value`
/// equality).
fn assert_frame_roundtrip(name: &str) -> RawJsonRpcMessage {
    let value = fixture_value(name);
    let frame: RawJsonRpcMessage = serde_json::from_value(value.clone()).unwrap_or_else(|error| {
        panic!("fixture {name} does not parse as a JSON-RPC frame: {error}");
    });
    let reserialized = serde_json::to_value(&frame).expect("frame serializes back to JSON");
    assert_eq!(reserialized, value, "fixture {name} must round-trip unchanged");
    frame
}

// ── Request fixtures round-trip into typed SDK requests ──────────────────

#[test]
fn conformance_initialize_request_roundtrips() {
    let frame = assert_frame_roundtrip("initialize_request.json");
    let RawJsonRpcMessage::Request(request) = frame else {
        panic!("initialize fixture must be a request frame");
    };
    assert_eq!(request.method.as_ref(), "initialize");
    assert!(matches!(request.id, ee_agent_protocol::RequestId::Number(1)));

    let params = common::raw_params_to_value(request.params);
    let typed: InitializeRequest = serde_json::from_value(params).expect("typed InitializeRequest");
    assert_eq!(typed.protocol_version, ProtocolVersion::V1);
}

#[test]
fn conformance_session_new_request_roundtrips() {
    let frame = assert_frame_roundtrip("session_new_request.json");
    let RawJsonRpcMessage::Request(request) = frame else {
        panic!("session/new fixture must be a request frame");
    };
    assert_eq!(request.method.as_ref(), "session/new");

    let params = common::raw_params_to_value(request.params);
    let typed: NewSessionRequest = serde_json::from_value(params).expect("typed NewSessionRequest");
    assert_eq!(typed.cwd, PathBuf::from("/workspace/project"));
    assert_eq!(typed.additional_directories, vec![PathBuf::from("/workspace/shared")]);
    assert!(typed.mcp_servers.is_empty());
}

#[test]
fn conformance_session_prompt_request_roundtrips() {
    let frame = assert_frame_roundtrip("session_prompt_request.json");
    let RawJsonRpcMessage::Request(request) = frame else {
        panic!("session/prompt fixture must be a request frame");
    };
    assert_eq!(request.method.as_ref(), "session/prompt");

    let params = common::raw_params_to_value(request.params);
    let typed: PromptRequest = serde_json::from_value(params).expect("typed PromptRequest");
    assert_eq!(typed.session_id, SessionId::new("session-1"));
    assert_eq!(typed.prompt.len(), 1);
    let ContentBlock::Text(text) = &typed.prompt[0] else {
        panic!("fixture prompt must be a text block");
    };
    assert_eq!(text.text, "hello");
}

#[test]
fn conformance_session_cancel_request_roundtrips() {
    let frame = assert_frame_roundtrip("session_cancel_request.json");
    let RawJsonRpcMessage::Request(request) = frame else {
        panic!("session/cancel fixture must be a request frame");
    };
    assert_eq!(request.method.as_ref(), "session/cancel");

    // The request form and the notification form share the
    // CancelNotification params shape.
    let params = common::raw_params_to_value(request.params);
    let typed: CancelNotification =
        serde_json::from_value(params).expect("typed CancelNotification");
    assert_eq!(typed.session_id, SessionId::new("session-1"));
}

// ── Response fixtures round-trip into typed SDK responses ────────────────

#[test]
fn conformance_fs_read_response_roundtrips() {
    let frame = assert_frame_roundtrip("fs_read_response.json");
    let RawJsonRpcMessage::Response(response) = frame else {
        panic!("fs read fixture must be a response frame");
    };
    let Response::Result { result, .. } = response else {
        panic!("fs read fixture must be a result response");
    };
    let typed: ReadTextFileResponse = serde_json::from_value(result).expect("typed response");
    assert_eq!(typed.content, "fixture file contents");
}

// ── Fixture contents are exact JSON lines ────────────────────────────────

#[test]
fn conformance_fixtures_are_single_json_objects() {
    for name in [
        "initialize_request.json",
        "session_new_request.json",
        "session_prompt_request.json",
        "session_cancel_request.json",
        "fs_read_response.json",
    ] {
        let raw = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name),
        )
        .unwrap_or_else(|error| panic!("failed to read fixture {name}: {error}"));
        let value: Value = serde_json::from_str(&raw)
            .unwrap_or_else(|error| panic!("fixture {name} is not valid JSON: {error}"));
        assert!(value.is_object(), "fixture {name} must be one JSON object");
    }
}
