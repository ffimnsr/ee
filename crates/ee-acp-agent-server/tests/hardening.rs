//! Phase 5 hardening coverage: parse/protocol errors answered per JSON-RPC
//! (server keeps serving), provider output rejected when malformed (empty or
//! duplicate session ids, updates for removed sessions), and transport-close
//! teardown (active prompts cancelled, writer path closed, pending requests
//! failed).

mod common;

use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{AcpTransport, JsonRpcFrame};
use ee_agent_protocol::{ListSessionsResponse, RawJsonRpcMessage, RequestId, Response};
use serde_json::{Value, json};

use common::{
    AcpAgentServer, AcpServerError, FakeProvider, PromptBehavior, error_reason, prompt_params,
    request, request_error, request_result, session_new_params, spawn_server, wait_for_log,
};

/// A transport that plays back a scripted read sequence and captures every
/// written frame, so the server's read-error handling can be driven
/// deterministically (the in-memory transport only ever delivers valid
/// frames).
struct ScriptedTransport {
    reads: std::vec::IntoIter<Result<Option<JsonRpcFrame>, AcpServerError>>,
    outbound: Arc<Mutex<Vec<JsonRpcFrame>>>,
}

impl ScriptedTransport {
    fn new(
        reads: Vec<Result<Option<JsonRpcFrame>, AcpServerError>>,
    ) -> (Self, Arc<Mutex<Vec<JsonRpcFrame>>>) {
        let outbound = Arc::new(Mutex::new(Vec::new()));
        (Self { reads: reads.into_iter(), outbound: outbound.clone() }, outbound)
    }
}

impl AcpTransport for ScriptedTransport {
    async fn read_message(&mut self) -> Result<Option<JsonRpcFrame>, AcpServerError> {
        match self.reads.next() {
            Some(result) => result,
            None => Ok(None), // end of script: clean EOF
        }
    }

    async fn write_message(&mut self, frame: JsonRpcFrame) -> Result<(), AcpServerError> {
        self.outbound.lock().expect("scripted outbound lock poisoned").push(frame);
        Ok(())
    }
}

fn unwrap_error_response(frame: RawJsonRpcMessage) -> (RequestId, i32) {
    let RawJsonRpcMessage::Response(Response::Error { id, error }) = frame else {
        panic!("expected an error response frame, got {frame:?}");
    };
    (id, i32::from(error.code))
}

fn parse_error() -> AcpServerError {
    AcpServerError::JsonParse {
        raw: "{\"broken\"".to_string(),
        source: serde_json::from_str::<Value>("{\"broken\"").unwrap_err(),
    }
}

// ── Malformed-frame handling ─────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn parse_error_gets_32700_response_and_server_continues() {
    let (provider, _log) = FakeProvider::new(&[]);
    let server = AcpAgentServer::new(provider, Default::default());
    let (transport, outbound) = ScriptedTransport::new(vec![
        Err(parse_error()),
        Ok(Some(request(1, "initialize", json!({ "protocolVersion": 1 })))),
        Ok(None),
    ]);

    let result = server.run_with_transport(transport).await;
    result.expect("server exits cleanly after answering a parse error");

    let frames = outbound.lock().expect("outbound lock").clone();
    assert_eq!(frames.len(), 2, "parse error + initialize result");

    // First: a `-32700` parse-error response with a `null` id.
    let (id, code) = unwrap_error_response(frames[0].clone());
    assert_eq!(id, RequestId::Null, "parse errors are answered with a null id");
    assert_eq!(code, -32700);

    // Then: the server kept serving the rest of the stream.
    let result = request_result(frames[1].clone());
    assert_eq!(result["protocolVersion"], 1);
}

#[tokio::test(flavor = "current_thread")]
async fn protocol_error_gets_32600_response_and_server_continues() {
    let (provider, _log) = FakeProvider::new(&[]);
    let server = AcpAgentServer::new(provider, Default::default());
    // An oversized frame surfaces as a protocol error at the transport; the
    // server must answer it with `-32600` (invalid request) and keep going.
    let oversized =
        AcpServerError::Protocol("frame of 99999 bytes exceeds the 1024 byte cap".to_string());
    let (transport, outbound) = ScriptedTransport::new(vec![
        Err(oversized),
        Ok(Some(request(1, "session/list", json!({})))),
        Ok(None),
    ]);

    let result = server.run_with_transport(transport).await;
    result.expect("server exits cleanly after answering a protocol error");

    let frames = outbound.lock().expect("outbound lock").clone();
    assert_eq!(frames.len(), 2, "invalid-request response + session/list result");

    let (id, code) = unwrap_error_response(frames[0].clone());
    assert_eq!(id, RequestId::Null, "invalid requests are answered with a null id");
    assert_eq!(code, -32600);

    let result = request_result(frames[1].clone());
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert!(response.sessions.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn io_error_stops_the_server_without_a_response() {
    let (provider, _log) = FakeProvider::new(&[]);
    let server = AcpAgentServer::new(provider, Default::default());
    let (transport, outbound) =
        ScriptedTransport::new(vec![Err(AcpServerError::Io(std::io::Error::other("pipe closed")))]);

    let result = server.run_with_transport(transport).await;
    assert!(matches!(result, Err(AcpServerError::Io(_))), "I/O errors propagate");
    assert!(outbound.lock().expect("outbound lock").is_empty(), "no response for I/O errors");
}

// ── Provider result hardening ────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn provider_empty_session_id_is_rejected() {
    let (provider, log) = FakeProvider::new(&[""]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32603, "provider output is an internal error");
    assert!(error_reason(&error).contains("provider returned an invalid session id"));
    assert!(log.has_call("new_session:/work"), "provider was called and returned bad output");

    // Nothing was registered.
    handle.send(request(2, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert!(response.sessions.is_empty(), "empty-id session must not be registered");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn provider_duplicate_session_id_is_rejected() {
    let (provider, log) = FakeProvider::new(&["dup", "dup"]);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);

    handle.send(request(2, "session/new", session_new_params("/other")));
    let error = request_error(handle.next_frame().await);
    assert_eq!(i32::from(error.code), -32603);
    assert!(error_reason(&error).contains("duplicate session id"));
    assert_eq!(log.calls().iter().filter(|c| c.starts_with("new_session")).count(), 2);

    // Only the first session is registered.
    handle.send(request(3, "session/list", json!({})));
    let result = request_result(handle.next_frame().await);
    let response: ListSessionsResponse =
        serde_json::from_value(result).expect("parses as ListSessionsResponse");
    assert_eq!(response.sessions.len(), 1);
    assert_eq!(response.sessions[0].session_id.to_string(), "dup");

    handle.shutdown(task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn update_for_removed_session_is_dropped() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitCancelThenTryEmitThenCancelled);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);

    // Prompt starts and blocks on its cancellation signal.
    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "prompt:session-a:started")).await;

    // Closing the session cancels the prompt; the prompt then tries to emit
    // an update for the session that is being removed.
    handle.send(request(3, "session/close", json!({ "sessionId": "session-a" })));
    let frames = handle.next_frames(2).await;
    let close_result = request_result(frames[0].clone());
    assert_eq!(close_result, json!({}));
    let prompt_result = request_result(frames[1].clone());
    assert_eq!(prompt_result["stopReason"], "cancelled");

    // The prompt's post-cancel emit succeeded (the writer path was still
    // open during session/close) but the framework dropped it.
    wait_for_log(&log, |calls| {
        calls.iter().any(|c| c == "prompt:session-a:emit-after-cancel:emitted")
    })
    .await;
    assert!(
        handle.outbound().is_empty(),
        "no session/update notification may be written for a removed session"
    );

    handle.shutdown(task).await;
}

// ── Transport-close teardown ─────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn eof_cancels_active_prompt_and_closes_writer_path() {
    let (provider, log) = FakeProvider::new(&["session-a"]);
    provider.set_prompt_behavior("session-a", PromptBehavior::AwaitCancelThenTryEmitThenCancelled);
    let (handle, task) = spawn_server(provider).await;

    handle.send(request(1, "session/new", session_new_params("/work")));
    let _ = request_result(handle.next_frame().await);
    handle.send(request(2, "session/prompt", prompt_params("session-a")));
    wait_for_log(&log, |calls| calls.iter().any(|c| c == "prompt:session-a:started")).await;

    // The client never answers; the transport closes instead.  The active
    // prompt must observe cancellation, and its post-shutdown update must
    // fail because the writer path is closed after reader shutdown.
    handle.close();
    wait_for_log(&log, |calls| {
        calls.iter().any(|c| c == "prompt:session-a:emit-after-cancel:err:update sink is closed")
    })
    .await;
    task.await.expect("server task joins").expect("clean EOF shutdown");
}

// ── Harness frame-pump hardening ─────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn harness_accumulates_frames_across_poll_batches() {
    let (transport, handle) = common::MemoryTransport::new();
    let harness = common::Harness::new(handle);
    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel();

    let frame =
        |id: i64| RawJsonRpcMessage::response(RequestId::Number(id), Ok(json!({ "n": id })));
    let wire = |frame: &RawJsonRpcMessage| serde_json::to_string(frame).expect("frame serializes");

    // A writer task emits frame A, then waits for a test signal before
    // emitting B and C with a forced yield between them, so the frames
    // arrive in three separate batches.
    let writer = tokio::spawn(async move {
        let mut transport = transport;
        transport.write_message(frame(1)).await.expect("writes A");
        let _ = signal_rx.await;
        transport.write_message(frame(2)).await.expect("writes B");
        tokio::task::yield_now().await;
        transport.write_message(frame(3)).await.expect("writes C");
    });

    // First batch consumed alone.
    let first = harness.next_frame().await;
    assert_eq!(wire(&first), wire(&frame(1)));

    // Batches B and C arrive after the signal; the pump must accumulate
    // them across partial polls instead of discarding them.
    signal_tx.send(()).expect("signals writer");
    let frames = harness.next_frames(2).await;
    assert_eq!(frames.len(), 2);
    assert_eq!(wire(&frames[0]), wire(&frame(2)));
    assert_eq!(wire(&frames[1]), wire(&frame(3)));
    assert!(harness.outbound().is_empty(), "no leftover frames");

    writer.await.expect("writer task joins");
}
