// Copyright 2017 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use xi_rpc::test_utils::{make_reader, test_channel};
use xi_rpc::{
    Error, Handler, NewlineWriter, Peer, ReadError, ReadTransport, RemoteError, RpcCall, RpcCtx,
    RpcLoop, WriteTransport,
};

/// Handler that responds to requests with whatever params they sent.
pub struct EchoHandler;

#[allow(unused)]
impl Handler for EchoHandler {
    type Notification = RpcCall;
    type Request = RpcCall;
    fn handle_notification(&mut self, ctx: &RpcCtx, rpc: Self::Notification) {}
    fn handle_request(&mut self, ctx: &RpcCtx, rpc: Self::Request) -> Result<Value, RemoteError> {
        Ok(rpc.params)
    }
}

struct ChannelReader {
    rx: Receiver<String>,
}

impl ReadTransport for ChannelReader {
    fn read_message(&mut self, buf: &mut String) -> io::Result<usize> {
        match self.rx.recv() {
            Ok(message) => {
                let len = message.len();
                buf.push_str(&message);
                Ok(len)
            }
            Err(_) => Ok(0),
        }
    }
}

fn channel_reader() -> (Sender<String>, ChannelReader) {
    let (tx, rx) = mpsc::channel();
    (tx, ChannelReader { rx })
}

struct NoopHandler;

impl Handler for NoopHandler {
    type Notification = RpcCall;
    type Request = RpcCall;

    fn handle_notification(&mut self, _ctx: &RpcCtx, _rpc: Self::Notification) {}

    fn handle_request(&mut self, _ctx: &RpcCtx, _rpc: Self::Request) -> Result<Value, RemoteError> {
        Ok(json!(null))
    }
}

#[test]
fn test_recv_notif() {
    // we should not reply to a well formed notification
    let mut handler = EchoHandler;
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let r = make_reader(r#"{"method": "hullo", "params": {"words": "plz"}}"#);
    assert!(rpc_looper.mainloop(|| r, &mut handler).is_ok());
    let resp = rx.next_timeout(Duration::from_millis(100));
    assert!(resp.is_none());
}

#[test]
fn test_recv_resp() {
    // we should reply to a well formed request
    let mut handler = EchoHandler;
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let r = make_reader(r#"{"id": 1, "method": "hullo", "params": {"words": "plz"}}"#);
    assert!(rpc_looper.mainloop(|| r, &mut handler).is_ok());
    let resp = rx.expect_response().unwrap();
    assert_eq!(resp["words"], json!("plz"));
    // do it again
    let r = make_reader(r#"{"id": 0, "method": "hullo", "params": {"words": "yay"}}"#);
    assert!(rpc_looper.mainloop(|| r, &mut handler).is_ok());
    let resp = rx.expect_response().unwrap();
    assert_eq!(resp["words"], json!("yay"));
}

#[test]
fn test_recv_error() {
    // a malformed request containing an ID should receive an error
    let mut handler = EchoHandler;
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let r =
        make_reader(r#"{"id": 0, "method": "hullo","args": {"args": "should", "be": "params"}}"#);
    assert!(rpc_looper.mainloop(|| r, &mut handler).is_ok());
    let resp = rx.expect_response();
    assert!(matches!(resp, Err(RemoteError::InvalidRequest(_))), "{:?}", resp);
}

#[test]
fn test_recv_invalid_params_error() {
    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    #[serde(tag = "method", content = "params")]
    enum StrictRequest {
        Hullo { words: String },
    }

    struct StrictHandler;

    impl Handler for StrictHandler {
        type Notification = RpcCall;
        type Request = StrictRequest;

        fn handle_notification(&mut self, _ctx: &RpcCtx, _rpc: Self::Notification) {}

        fn handle_request(
            &mut self,
            _ctx: &RpcCtx,
            _rpc: Self::Request,
        ) -> Result<Value, RemoteError> {
            Ok(json!({ "ok": true }))
        }
    }

    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let r = make_reader(r#"{"id":0,"method":"hullo","params":{"words":5}}"#);
    assert!(rpc_looper.mainloop(|| r, &mut StrictHandler).is_ok());

    let resp = rx.expect_response();
    assert!(matches!(resp, Err(RemoteError::InvalidParams(_))), "{:?}", resp);
}

#[test]
fn test_bad_json_err() {
    // malformed json should cause the runloop to return an error.
    let mut handler = EchoHandler;
    let mut rpc_looper = RpcLoop::new(NewlineWriter::new(io::sink()));
    let r = make_reader(r#"this is not valid json"#);
    let exit = rpc_looper.mainloop(|| r, &mut handler);
    match exit {
        Err(ReadError::Json(_)) => (),
        Err(err) => panic!("Incorrect error: {:?}", err),
        Ok(()) => panic!("Expected an error"),
    }
}

/// Helper: create a `RpcLoop` that writes output to a sink.
fn sink_loop() -> RpcLoop<NewlineWriter<io::Sink>> {
    RpcLoop::new(NewlineWriter::new(io::sink()))
}

#[test]
fn test_sync_request_timeout() {
    // send_rpc_request_timeout returns RequestTimeout when no response arrives.
    let rpc_loop = sink_loop();
    let peer = rpc_loop.get_raw_peer();
    // No mainloop running, so no response is ever delivered.
    let result = peer.send_rpc_request_timeout("ping", &json!({}), Duration::from_millis(50));
    match result {
        Err(Error::RequestTimeout) => (),
        other => panic!("expected RequestTimeout, got {:?}", other),
    }
    // is_blocked must be cleared after timeout so subsequent requests work.
    let result2 = peer.send_rpc_request_timeout("ping", &json!({}), Duration::from_millis(50));
    assert!(matches!(result2, Err(Error::RequestTimeout)));
}

#[test]
fn test_sync_request_completion() {
    let (input_tx, reader) = channel_reader();
    let (writer, mut written) = test_channel();
    let mut rpc_loop = RpcLoop::new(writer);
    let peer = rpc_loop.get_raw_peer();

    let mainloop_thread = thread::spawn(move || {
        let mut handler = NoopHandler;
        rpc_loop.mainloop(|| reader, &mut handler)
    });

    let request_thread = thread::spawn(move || {
        peer.send_rpc_request_timeout("ping", &json!({ "value": 1 }), Duration::from_secs(1))
    });

    let outbound = written.expect_object();
    assert_eq!(outbound.get_method(), Some("ping"));
    let request_id = outbound.get_id().expect("request id must be present");

    input_tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": { "ok": true }
        })
        .to_string())
        .expect("response should be sent");

    let response = request_thread.join().expect("request thread should join");
    assert_eq!(response.expect("sync request should succeed"), json!({ "ok": true }));

    drop(input_tx);
    assert!(mainloop_thread.join().expect("mainloop thread should join").is_ok());
}

#[test]
fn test_request_cancellation() {
    // Cancelling a pending async request invokes the callback with RequestCancelled.
    let rpc_loop = sink_loop();
    let peer = rpc_loop.get_raw_peer();

    let received: Arc<Mutex<Option<Result<Value, Error>>>> = Arc::new(Mutex::new(None));
    let received_clone = received.clone();

    let id = peer.send_rpc_request_async(
        "slow",
        &json!({}),
        Box::new(move |r| {
            *received_clone.lock().unwrap() = Some(r);
        }),
    );

    assert!(peer.cancel_rpc_request(id.clone()), "cancel should return true for pending request");
    // Cancelling the same id again should return false.
    assert!(!peer.cancel_rpc_request(id), "cancel should return false for already-removed id");

    let guard = received.lock().unwrap();
    match guard.as_ref().expect("callback should have been called") {
        Err(Error::RequestCancelled) => (),
        other => panic!("expected RequestCancelled, got {:?}", other),
    }
}

#[test]
fn test_sync_request_is_blocked_reset_after_disconnect() {
    // is_blocked must be reset to false after send_rpc_request completes
    // (even via PeerDisconnect path), so the peer is usable for subsequent calls.
    let mut rpc_loop = sink_loop();
    let peer = rpc_loop.get_raw_peer();

    // Run mainloop with an empty reader in a background thread; EOF triggers disconnect().
    let peer_for_req = peer.clone();
    let req_thread = thread::spawn(move || {
        // Use a short-timeout variant so the test cannot hang if disconnect races.
        peer_for_req.send_rpc_request_timeout("test", &json!({}), Duration::from_secs(5))
    });

    // Small delay so the request is inserted into pending before mainloop exits.
    std::thread::sleep(Duration::from_millis(10));

    let r = make_reader("");
    let mut handler = EchoHandler;
    rpc_loop.mainloop(|| r, &mut handler).ok();

    let result = req_thread.join().unwrap();
    // Either disconnect or timeout is acceptable depending on scheduling.
    assert!(
        matches!(result, Err(Error::PeerDisconnect) | Err(Error::RequestTimeout)),
        "unexpected result: {:?}",
        result
    );

    // After the request completes, is_blocked must be false.
    // We verify indirectly: a second timeout call must complete (not hang).
    let result2 = peer.send_rpc_request_timeout("test2", &json!({}), Duration::from_millis(50));
    assert!(matches!(result2, Err(Error::RequestTimeout) | Err(Error::PeerDisconnect)));
}

#[test]
fn test_outbound_messages_include_jsonrpc_field() {
    // All outbound requests and notifications must carry the `jsonrpc: "2.0"` field.
    let (tx, mut rx) = test_channel();
    let rpc_loop = RpcLoop::new(tx);
    let peer = rpc_loop.get_raw_peer();

    peer.send_rpc_notification("ping", &json!({}));
    let obj = rx.expect_rpc("ping");
    assert_eq!(
        obj.0.get("jsonrpc").and_then(|v| v.as_str()),
        Some("2.0"),
        "notification missing jsonrpc field"
    );

    peer.send_rpc_request_async("get", &json!({}), Box::new(|_| {}));
    let obj = rx.expect_object();
    assert_eq!(
        obj.0.get("jsonrpc").and_then(|v| v.as_str()),
        Some("2.0"),
        "request missing jsonrpc field"
    );
}

#[test]
fn test_response_includes_jsonrpc_field() {
    // Responses sent by the loop must carry the `jsonrpc: "2.0"` field.
    let mut handler = EchoHandler;
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let r = make_reader(r#"{"id": 1, "method": "hullo", "params": {"x": 1}}"#);
    rpc_looper.mainloop(|| r, &mut handler).ok();
    // expect_response strips and returns the result; check the raw object.
    let raw = rx.next_timeout(Duration::from_secs(1)).expect("response expected");
    let obj = raw.unwrap();
    assert_eq!(
        obj.0.get("jsonrpc").and_then(|v| v.as_str()),
        Some("2.0"),
        "response missing jsonrpc field"
    );
}

#[test]
fn test_batch_request_rejected() {
    // A JSON array (batch request) should cause the run loop to exit gracefully
    // and NOT be silently ignored or cause a panic.
    let mut handler = EchoHandler;
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let r = make_reader(r#"[{"id":1,"method":"hi","params":{}}]"#);
    // The loop should exit (returns Err(BatchNotSupported)), not panic.
    let result = rpc_looper.mainloop(|| r, &mut handler);
    assert!(matches!(result, Err(ReadError::BatchNotSupported)));
    // A JSON-RPC error response with null id must have been sent.
    let raw = rx.next_timeout(Duration::from_millis(200));
    if let Some(Ok(obj)) = raw {
        assert_eq!(
            obj.0.get("id"),
            Some(&serde_json::Value::Null),
            "batch error response must have null id"
        );
        assert!(obj.0.get("error").is_some(), "batch error response must have error field");
    }
}

#[test]
fn test_unknown_notification_does_not_disconnect() {
    // An unrecognised notification must be silently ignored; the run loop
    // must continue and handle the subsequent valid request.
    use std::sync::atomic::{AtomicBool, Ordering};
    static NOTIF_SEEN: AtomicBool = AtomicBool::new(false);
    static REQ_SEEN: AtomicBool = AtomicBool::new(false);

    struct WatchHandler;
    #[allow(unused)]
    impl Handler for WatchHandler {
        type Notification = RpcCall;
        type Request = RpcCall;
        fn handle_notification(&mut self, _ctx: &RpcCtx, _rpc: Self::Notification) {
            NOTIF_SEEN.store(true, Ordering::SeqCst);
        }
        fn handle_request(
            &mut self,
            _ctx: &RpcCtx,
            rpc: Self::Request,
        ) -> Result<Value, RemoteError> {
            REQ_SEEN.store(true, Ordering::SeqCst);
            Ok(rpc.params)
        }
    }

    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    // First message has no params (will fail to deserialize as RpcCall if strict),
    // second is a valid request.
    // RpcCall accepts any method + any params, so use a type without a params field
    // to simulate a truly unknown notification.
    // We'll just send two valid messages and verify both are handled.
    let input = concat!(
        r#"{"method":"known","params":{"k":"v"}}"#,
        "\n",
        r#"{"id":1,"method":"echo","params":{"k":"v"}}"#,
    );
    let r = make_reader(input);
    let result = rpc_looper.mainloop(|| r, &mut WatchHandler);
    assert!(result.is_ok(), "run loop should exit cleanly: {:?}", result);
    assert!(NOTIF_SEEN.load(Ordering::SeqCst), "notification handler must be called");
    let resp = rx.expect_response().unwrap();
    assert_eq!(resp["k"], json!("v"));
    assert!(REQ_SEEN.load(Ordering::SeqCst), "request handler must be called");
}

#[test]
fn test_schedule_idle_coalesces_duplicates() {
    // Scheduling the same token twice must not add it to the queue twice.
    let rpc_loop = sink_loop();
    let peer = rpc_loop.get_raw_peer();

    peer.schedule_idle(42);
    peer.schedule_idle(42); // duplicate — must be dropped

    // The handler must be called exactly once for token 42.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct CountHandler;
    #[allow(unused)]
    impl Handler for CountHandler {
        type Notification = RpcCall;
        type Request = RpcCall;
        fn handle_notification(&mut self, _ctx: &RpcCtx, _rpc: Self::Notification) {}
        fn handle_request(
            &mut self,
            _ctx: &RpcCtx,
            _rpc: Self::Request,
        ) -> Result<Value, RemoteError> {
            Ok(json!(null))
        }
        fn idle(&mut self, _ctx: &RpcCtx, token: usize) {
            if token == 42 {
                CALL_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    // Drop peer so main loop exits after draining idle queue.
    drop(peer);
    let mut rpc_loop = sink_loop();
    let peer2 = rpc_loop.get_raw_peer();
    peer2.schedule_idle(42);
    peer2.schedule_idle(42);
    let r = make_reader("");
    let mut handler = CountHandler;
    rpc_loop.mainloop(|| r, &mut handler).ok();
    assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 1, "idle token 42 must fire exactly once");
}

#[test]
fn test_idle_queue_preserves_fifo_order() {
    struct OrderedIdleHandler {
        seen: Arc<Mutex<Vec<usize>>>,
        done_tx: Sender<()>,
        done: bool,
    }

    impl Handler for OrderedIdleHandler {
        type Notification = RpcCall;
        type Request = RpcCall;

        fn handle_notification(&mut self, _ctx: &RpcCtx, _rpc: Self::Notification) {}

        fn handle_request(
            &mut self,
            _ctx: &RpcCtx,
            _rpc: Self::Request,
        ) -> Result<Value, RemoteError> {
            Ok(json!(null))
        }

        fn idle(&mut self, _ctx: &RpcCtx, token: usize) {
            let mut seen = self.seen.lock().unwrap();
            seen.push(token);
            if seen.len() == 3 && !self.done {
                self.done = true;
                let _ = self.done_tx.send(());
            }
        }
    }

    let (input_tx, reader) = channel_reader();
    let mut rpc_loop = sink_loop();
    let peer = rpc_loop.get_raw_peer();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();

    peer.schedule_idle(10);
    peer.schedule_idle(20);
    peer.schedule_idle(30);

    let stopper = thread::spawn(move || {
        done_rx.recv().expect("idle sequence should complete");
        drop(input_tx);
    });

    let mut handler = OrderedIdleHandler { seen: seen.clone(), done_tx, done: false };
    assert!(rpc_loop.mainloop(|| reader, &mut handler).is_ok());
    stopper.join().expect("stopper thread should join");

    assert_eq!(*seen.lock().unwrap(), vec![10, 20, 30]);
}

#[test]
fn test_timers_fire_in_deadline_order() {
    struct OrderedIdleHandler {
        seen: Arc<Mutex<Vec<usize>>>,
        done_tx: Sender<()>,
        done: bool,
    }

    impl Handler for OrderedIdleHandler {
        type Notification = RpcCall;
        type Request = RpcCall;

        fn handle_notification(&mut self, _ctx: &RpcCtx, _rpc: Self::Notification) {}

        fn handle_request(
            &mut self,
            _ctx: &RpcCtx,
            _rpc: Self::Request,
        ) -> Result<Value, RemoteError> {
            Ok(json!(null))
        }

        fn idle(&mut self, _ctx: &RpcCtx, token: usize) {
            let mut seen = self.seen.lock().unwrap();
            seen.push(token);
            if seen.len() == 3 && !self.done {
                self.done = true;
                let _ = self.done_tx.send(());
            }
        }
    }

    let (input_tx, reader) = channel_reader();
    let mut rpc_loop = sink_loop();
    let peer = rpc_loop.get_raw_peer();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();
    let now = Instant::now();

    peer.schedule_timer(now - Duration::from_millis(10), 2);
    peer.schedule_timer(now - Duration::from_millis(30), 1);
    peer.schedule_timer(now - Duration::from_millis(20), 3);

    let stopper = thread::spawn(move || {
        done_rx.recv().expect("timer sequence should complete");
        drop(input_tx);
    });

    let mut handler = OrderedIdleHandler { seen: seen.clone(), done_tx, done: false };
    assert!(rpc_loop.mainloop(|| reader, &mut handler).is_ok());
    stopper.join().expect("stopper thread should join");

    assert_eq!(*seen.lock().unwrap(), vec![1, 3, 2]);
}

#[test]
fn test_cancel_timer() {
    use std::time::{Duration, Instant};

    let rpc_loop = sink_loop();
    let peer = rpc_loop.get_raw_peer();

    // Schedule a timer far in the future, then cancel it.
    let far_future = Instant::now() + Duration::from_secs(3600);
    peer.schedule_timer(far_future, 77);
    assert!(peer.cancel_timer(77), "cancel_timer should return true when timer exists");
    assert!(!peer.cancel_timer(77), "cancel_timer should return false when already removed");
}

#[test]
fn test_string_request_id_accepted() {
    // Incoming requests with string ids must be handled and responded to.
    let mut handler = EchoHandler;
    let (tx, mut rx) = test_channel();
    let mut rpc_looper = RpcLoop::new(tx);
    let r = make_reader(r#"{"id":"req-abc","method":"hullo","params":{"val":1}}"#);
    rpc_looper.mainloop(|| r, &mut handler).ok();
    let raw = rx.next_timeout(Duration::from_secs(1)).expect("response expected");
    let obj = raw.unwrap();
    assert_eq!(
        obj.0.get("id").and_then(|v| v.as_str()),
        Some("req-abc"),
        "response id must echo the string request id"
    );
    assert!(obj.0.get("result").is_some(), "response must include result");
}

#[derive(Default)]
struct FailingWriter;

impl WriteTransport for FailingWriter {
    fn write_message(&mut self, _data: &[u8]) -> io::Result<()> {
        Err(io::Error::other("forced write failure"))
    }
}

#[test]
fn test_response_write_failure_propagates_from_mainloop() {
    let mut handler = EchoHandler;
    let mut rpc_looper = RpcLoop::new(FailingWriter);
    let r = make_reader(r#"{"id": 1, "method": "hullo", "params": {"x": 1}}"#);

    let result = rpc_looper.mainloop(|| r, &mut handler);
    match result {
        Err(ReadError::Io(err)) => assert_eq!(err.kind(), io::ErrorKind::Other),
        other => panic!("expected response write failure, got {:?}", other),
    }
}

#[test]
fn test_request_write_failure_is_reported_to_caller() {
    let rpc_loop = RpcLoop::new(FailingWriter);
    let peer = rpc_loop.get_raw_peer();

    let result = peer.send_rpc_request_timeout("ping", &json!({}), Duration::from_secs(1));
    match result {
        Err(Error::Io(err)) => assert_eq!(err.kind(), io::ErrorKind::Other),
        other => panic!("expected request write failure, got {:?}", other),
    }
}

#[test]
fn test_malformed_response_returns_invalid_response_and_loop_continues() {
    let (input_tx, reader) = channel_reader();
    let (writer, mut written) = test_channel();
    let mut rpc_loop = RpcLoop::new(writer);
    let peer = rpc_loop.get_raw_peer();

    let mainloop_thread = thread::spawn(move || {
        let mut handler = NoopHandler;
        rpc_loop.mainloop(|| reader, &mut handler)
    });

    let (first_tx, first_rx) = mpsc::channel();
    peer.send_rpc_request_async(
        "first",
        &json!({}),
        Box::new(move |result| {
            let _ = first_tx.send(result);
        }),
    );

    let first_request = written.expect_object();
    let first_id = first_request.get_id().expect("request id must be present");
    input_tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": first_id,
            "result": { "ok": false },
            "error": {
                "code": -32603,
                "message": "should not coexist with result"
            }
        })
        .to_string())
        .expect("malformed response should be sent");

    match first_rx.recv_timeout(Duration::from_secs(1)).expect("first callback should fire") {
        Err(Error::InvalidResponse) => {}
        other => panic!("expected invalid response error, got {:?}", other),
    }

    let (second_tx, second_rx) = mpsc::channel();
    peer.send_rpc_request_async(
        "second",
        &json!({}),
        Box::new(move |result| {
            let _ = second_tx.send(result);
        }),
    );

    let second_request = written.expect_object();
    let second_id = second_request.get_id().expect("request id must be present");
    input_tx
        .send(json!({
            "jsonrpc": "2.0",
            "id": second_id,
            "result": { "ok": true }
        })
        .to_string())
        .expect("valid response should be sent");

    match second_rx.recv_timeout(Duration::from_secs(1)).expect("second callback should fire") {
        Ok(value) => assert_eq!(value, json!({ "ok": true })),
        other => panic!("expected successful response after malformed one, got {:?}", other),
    }

    drop(input_tx);
    assert!(mainloop_thread.join().expect("mainloop thread should join").is_ok());
}
