//! Shared test harness for the OpenRouter provider integration tests: a
//! scripted in-process OpenRouter HTTP responder, a server-over-memory-
//! transport spawner with a non-destructive frame pump, and response
//! helpers.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub use ee_acp_agent_server::{
    AcpAgentServer, AcpAgentServerConfig, AcpServerError, MemoryTransport, MemoryTransportHandle,
};
use ee_agent_protocol::{Error as RpcError, RawJsonRpcMessage, RequestId, Response};
use ee_openrouter_agent::provider::OpenRouterProvider;
use serde_json::{Value, json};

/// Scripted OpenRouter endpoint: each chat-completions request pops the
/// next canned response; request bodies are captured for assertions.
pub struct MockOpenRouter {
    listener: TcpListener,
    responses: Arc<Mutex<VecDeque<Value>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockOpenRouter {
    /// Starts the responder thread serving the given responses in order.
    pub fn start(responses: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock server binds");
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let responses = responses.clone();
            let bodies = bodies.clone();
            let stop = stop.clone();
            let listener = listener.try_clone().expect("mock listener clones");
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(_) => break,
                    };
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    handle_connection(&mut stream, &responses, &bodies);
                }
            })
        };
        Self { listener, responses, bodies, stop, thread: Some(thread) }
    }

    /// The chat-completions URL to point the provider at.
    pub fn api_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            self.listener.local_addr().expect("mock server address").port()
        )
    }

    /// Request bodies received so far, in order.
    pub fn request_bodies(&self) -> Vec<Value> {
        self.bodies.lock().expect("mock bodies poisoned").clone()
    }
}

impl Drop for MockOpenRouter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so the thread exits promptly.
        let _ = TcpStream::connect(self.listener.local_addr().expect("mock server address"));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Serves one HTTP/1.1 request: reads the request line, headers, and body,
/// records the body, then answers with the next scripted response and
/// closes the connection.
fn handle_connection(
    stream: &mut TcpStream,
    responses: &Mutex<VecDeque<Value>>,
    bodies: &Mutex<Vec<Value>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(stream.try_clone().expect("stream clones"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            return;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).unwrap_or_default();
    }
    if let Ok(value) = serde_json::from_slice::<Value>(&body) {
        bodies.lock().expect("mock bodies poisoned").push(value);
    }

    let response = responses.lock().expect("mock responses poisoned").pop_front();
    let (status, value) = match response {
        Some(value) => (200u16, value),
        None => (500u16, json!({ "error": { "message": "no scripted mock response left" } })),
    };
    let body_text = value.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        _ => "Mock",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body_text}",
        body_text.len()
    );
    let _ = stream.flush();
}

// ── Server harness ───────────────────────────────────────────────────────

/// Server-over-memory-transport harness with a non-destructive frame pump.
///
/// Outbound frames accumulate in a per-harness queue, so frames arriving in
/// separate batches (e.g. a tool update before a slow HTTP round finishes)
/// are never lost between `next_frames` calls.
pub struct Harness {
    handle: MemoryTransportHandle,
    pending: Arc<Mutex<VecDeque<RawJsonRpcMessage>>>,
}

impl Harness {
    /// Spawns a server over an in-memory transport and returns the harness
    /// (to inject frames and read responses) plus the task.
    pub async fn spawn(
        provider: OpenRouterProvider,
    ) -> (Self, tokio::task::JoinHandle<Result<(), AcpServerError>>) {
        let server = AcpAgentServer::new(provider, AcpAgentServerConfig::default());
        let (transport, handle) = MemoryTransport::new();
        let task = tokio::spawn(async move { server.run_with_transport(transport).await });
        (Self { handle, pending: Arc::new(Mutex::new(VecDeque::new())) }, task)
    }

    /// Queues one inbound frame for the server.
    pub fn send(&self, frame: RawJsonRpcMessage) -> bool {
        self.handle.send(frame)
    }

    /// Waits (without sleeping) for the next outbound frame.
    pub async fn next_frame(&self) -> RawJsonRpcMessage {
        self.next_frames(1).await.remove(0)
    }

    /// Waits for exactly `count` outbound frames, in order, keeping any
    /// overflow queued for the next call.
    pub async fn next_frames(&self, count: usize) -> Vec<RawJsonRpcMessage> {
        for _ in 0..5_000 {
            let frames = {
                let mut pending = self.pending.lock().expect("harness pending poisoned");
                if pending.len() < count {
                    pending.extend(self.handle.take_outbound());
                }
                if pending.len() >= count { pending.drain(..count).collect() } else { Vec::new() }
            };
            if !frames.is_empty() {
                return frames;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "not enough outbound frames within budget; wanted {count}, pending={:?}, fresh={:?}",
            self.pending.lock().expect("harness pending poisoned"),
            self.handle.outbound()
        );
    }

    /// Closes the transport and asserts the server shuts down cleanly.
    pub async fn shutdown(self, task: tokio::task::JoinHandle<Result<(), AcpServerError>>) {
        drop(self.handle);
        task.await.expect("server task joins").expect("server exits cleanly on EOF");
    }
}

// ── Frame builders / matchers ────────────────────────────────────────────

pub fn request(id: i64, method: &str, params: Value) -> RawJsonRpcMessage {
    RawJsonRpcMessage::request(method.to_string(), params, RequestId::Number(id))
        .expect("test request builds")
}

/// Answers a captured request frame with the given result/error.
pub fn respond_to(
    frame: &RawJsonRpcMessage,
    response: Result<Value, RpcError>,
) -> RawJsonRpcMessage {
    let RawJsonRpcMessage::Request(request) = frame else {
        panic!("expected a request frame, got {frame:?}");
    };
    RawJsonRpcMessage::response(request.id.clone(), response)
}

pub fn request_result(frame: RawJsonRpcMessage) -> Value {
    let Response::Result { result, .. } = unwrap_response(frame) else {
        panic!("expected a result response");
    };
    result
}

pub fn request_error(frame: RawJsonRpcMessage) -> RpcError {
    let Response::Error { error, .. } = unwrap_response(frame) else {
        panic!("expected an error response");
    };
    error
}

fn unwrap_response(frame: RawJsonRpcMessage) -> Response<Value> {
    let RawJsonRpcMessage::Response(response) = frame else {
        panic!("expected a response frame, got {frame:?}");
    };
    response
}

pub fn session_new_params(cwd: &str) -> Value {
    json!({
        "cwd": cwd,
        "additionalDirectories": [],
        "mcpServers": [],
    })
}

pub fn prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
    })
}

/// A chat-completions response requesting one `tool_read_file` call.
pub fn tool_call_response(tool_call_id: &str, path: &str) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": tool_call_id,
                    "type": "function",
                    "function": {
                        "name": "tool_read_file",
                        "arguments": json!({ "path": path }).to_string()
                    }
                }]
            }
        }]
    })
}

/// A chat-completions response with reasoning and a final answer.
pub fn reasoning_response(reasoning: &str, content: &str) -> Value {
    json!({
        "choices": [{
            "message": { "role": "assistant", "reasoning": reasoning, "content": content }
        }]
    })
}

/// A chat-completions response with a final answer only.
pub fn answer_response(content: &str) -> Value {
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": content }
        }]
    })
}

/// A chat-completions response with a final answer and token usage.
pub fn answer_response_with_usage(
    content: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Value {
    let mut value = answer_response(content);
    value["usage"] = json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    });
    value
}
