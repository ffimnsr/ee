//! In-process fake ACP agent harness for tests (feature `test-utils`).
//!
//! No external binaries are spawned: the fake is a line-level actor over an
//! in-process duplex transport, giving tests deterministic control over
//! initialize/session flows, agent-to-client requests, malformed JSON, and
//! process-exit behavior (EOF).
//!
//! # Script model
//!
//! [`FakeStep::WaitFor`] consumes the next host request/notification whose
//! method matches (remembering the request id); [`FakeStep::Respond`] /
//! [`FakeStep::RespondError`] answer the remembered request; [`FakeStep::Emit`]
//! / [`FakeStep::EmitRaw`] push lines to the host; [`FakeStep::Close`] closes
//! the agent side (EOF).  Every line the host sends is recorded by the
//! transport sink and inspectable via [`FakeAgent::log`].

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc;
use futures::{StreamExt, sink, stream};
use serde_json::{Value, json};

/// The line transport the fake agent hands to the host connection.
///
/// A concrete, nameable type so test harnesses can store transport
/// factories (see [`crate::manager::FakeTransportFactory`]) without
/// resorting to generics.
/// Maximum bytes accepted on one ACP JSON-RPC line from an agent (Phase 7
/// resource limit).  Oversized lines fail the connection as a transport
/// error instead of being parsed.
pub const MAX_ACP_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

pub type FakeAgentTransport = ee_agent_protocol::Lines<
    Pin<Box<dyn futures::Sink<String, Error = io::Error> + Send>>,
    Pin<Box<dyn futures::Stream<Item = io::Result<String>> + Send>>,
>;

/// One scripted step the fake agent performs.
#[derive(Debug, Clone)]
pub enum FakeStep {
    /// Wait for the host to send a request/notification with `method` and
    /// remember the request id for a later [`FakeStep::Respond`].  Host
    /// responses and malformed lines are skipped; requests with other
    /// methods are answered with method-not-found so the host never hangs.
    WaitFor { method: String },
    /// Wait until the host sends a response (no `method` key) with `id`;
    /// other lines are skipped.
    WaitForResponse { id: i64 },
    /// Pause the script for `millis` (lets host-side tasks settle).
    Delay { millis: u64 },
    /// Respond to the remembered request with a JSON-RPC result.
    Respond { result: Value },
    /// Respond to the remembered request with a JSON-RPC error.
    RespondError { code: i64, message: String },
    /// Emit a JSON value as a line to the host (notification or
    /// agent-to-client request).
    Emit(Value),
    /// Emit a raw string line (may be malformed JSON).
    EmitRaw(String),
    /// Close the agent side of the transport (EOF, like a process exit).
    Close,
}

/// A script of [`FakeStep`]s executed in order.
#[derive(Debug, Clone, Default)]
pub struct FakeAgentScript {
    steps: Vec<FakeStep>,
}

impl FakeAgentScript {
    /// Creates an empty script.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends [`FakeStep::WaitFor`].
    #[must_use]
    pub fn wait_for(self, method: impl Into<String>) -> Self {
        self.step(FakeStep::WaitFor { method: method.into() })
    }

    /// Appends [`FakeStep::WaitForResponse`].
    #[must_use]
    pub fn wait_for_response(self, id: i64) -> Self {
        self.step(FakeStep::WaitForResponse { id })
    }

    /// Appends [`FakeStep::Delay`].
    #[must_use]
    pub fn delay(self, millis: u64) -> Self {
        self.step(FakeStep::Delay { millis })
    }

    /// Appends [`FakeStep::Respond`].
    #[must_use]
    pub fn respond(self, result: Value) -> Self {
        self.step(FakeStep::Respond { result })
    }

    /// Appends [`FakeStep::RespondError`].
    #[must_use]
    pub fn respond_error(self, code: i64, message: impl Into<String>) -> Self {
        self.step(FakeStep::RespondError { code, message: message.into() })
    }

    /// Appends [`FakeStep::Emit`].
    #[must_use]
    pub fn emit(self, value: Value) -> Self {
        self.step(FakeStep::Emit(value))
    }

    /// Appends [`FakeStep::EmitRaw`].
    #[must_use]
    pub fn emit_raw(self, line: impl Into<String>) -> Self {
        self.step(FakeStep::EmitRaw(line.into()))
    }

    /// Appends [`FakeStep::Close`].
    #[must_use]
    pub fn close(self) -> Self {
        self.step(FakeStep::Close)
    }

    fn step(mut self, step: FakeStep) -> Self {
        self.steps.push(step);
        self
    }
}

/// The test-side handle of the fake agent.
#[derive(Debug, Clone)]
pub struct FakeAgent {
    log: Arc<Mutex<Vec<String>>>,
    handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl FakeAgent {
    /// Spawns the fake agent and returns its handle plus the line transport
    /// to hand to the host connection.
    ///
    /// The transport sink records every line the host sends; inspect with
    /// [`Self::log`].
    #[must_use]
    pub fn spawn(script: FakeAgentScript) -> (Self, FakeAgentTransport) {
        let (to_host_tx, to_host_rx) = mpsc::unbounded::<Result<String, io::Error>>();
        let (from_host_tx, from_host_rx) = mpsc::unbounded::<String>();
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let log_for_sink = log.clone();

        // Host → agent: record every line and forward to the driver.
        let outgoing_sink =
            sink::unfold((from_host_tx, log_for_sink), |(tx, log), line: String| async move {
                log.lock().expect("fake log poisoned").push(line.clone());
                tx.unbounded_send(line).map_err(|error| io::Error::other(error.to_string()))?;
                Ok::<_, io::Error>((tx, log))
            });
        // Agent → host: scripted lines, capped at [`MAX_ACP_MESSAGE_BYTES`]
        // per line (Phase 7 resource limit).  An oversized frame yields one
        // error item and then ends the stream, so the SDK transport surfaces
        // a connection failure instead of parsing the frame.
        let incoming_stream = stream::unfold((to_host_rx, false), |(mut rx, ended)| async move {
            if ended {
                return None;
            }
            let line = rx.next().await?;
            match line {
                Ok(line) if line.len() > MAX_ACP_MESSAGE_BYTES => Some((
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("agent message exceeds the {MAX_ACP_MESSAGE_BYTES}-byte cap"),
                    )),
                    (rx, true),
                )),
                other => Some((other, (rx, false))),
            }
        });

        let transport = FakeAgentTransport::new(Box::pin(outgoing_sink), Box::pin(incoming_stream));
        let handle = tokio::spawn(driver(script, from_host_rx, to_host_tx, log.clone()));
        (FakeAgent { log, handle: Arc::new(Mutex::new(Some(handle))) }, transport)
    }

    /// Every line the host sent to the fake, in order.
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.log.lock().expect("fake log poisoned").clone()
    }

    /// Lines the host sent, parsed as JSON values that carry `method`.
    #[must_use]
    pub fn requests_by_method(&self, method: &str) -> Vec<Value> {
        self.log()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value.get("method").and_then(Value::as_str) == Some(method))
            .collect()
    }

    /// Whether any recorded host line contains `needle`.
    #[must_use]
    pub fn log_contains(&self, needle: &str) -> bool {
        self.log().iter().any(|line| line.contains(needle))
    }

    /// The last response the host sent for the request with `id`, parsed as
    /// JSON (agent-to-client requests carry fixed ids in the wire helpers).
    #[must_use]
    pub fn response_with_id(&self, id: i64) -> Option<Value> {
        self.log()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value.get("id") == Some(&json!(id)) && value.get("method").is_none())
    }

    /// Joins the driver task, failing the test when it panics or when the
    /// script runner reported a mismatch.
    pub async fn join(self, timeout: Duration) {
        let handle = self.handle.lock().expect("fake handle poisoned").take();
        let Some(handle) = handle else {
            return;
        };
        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("fake agent driver panicked: {error}"),
            Err(_) => panic!("fake agent driver did not finish within {timeout:?}"),
        }
    }
}

/// Runs the script against the host line stream.
async fn driver(
    script: FakeAgentScript,
    mut from_host: mpsc::UnboundedReceiver<String>,
    to_host: mpsc::UnboundedSender<Result<String, io::Error>>,
    log: Arc<Mutex<Vec<String>>>,
) {
    let mut steps: VecDeque<FakeStep> = script.steps.into_iter().collect();
    let mut to_host = Some(to_host);
    let mut remembered: Option<RequestView> = None;
    let mut line: Option<String> = None;

    loop {
        // 1. Try to advance WaitFor steps with the current line.
        if let Some(current) = &line
            && let Some(FakeStep::WaitFor { method }) = steps.front()
        {
            match parse_request(current) {
                Some(request) if &request.method == method => {
                    remembered = Some(request);
                    steps.pop_front();
                    line = None;
                    continue;
                }
                Some(request) => {
                    log.lock().expect("fake log poisoned").push(format!(
                        "FAKE: unexpected request {:?} while waiting for {:?}",
                        request.method, method
                    ));
                    emit_response(
                        &to_host,
                        request.id.as_ref(),
                        None,
                        Some(json!({ "code": -32601, "message": "method not found" })),
                    );
                    line = None;
                    continue;
                }
                None => {
                    // Response or malformed line: skip it.
                    line = None;
                    continue;
                }
            }
        }

        // 1b. WaitForResponse consumes the host response with the given id.
        if let Some(current) = &line
            && let Some(FakeStep::WaitForResponse { id }) = steps.front()
        {
            let is_match = serde_json::from_str::<Value>(current).is_ok_and(|value| {
                value.get("method").is_none() && value.get("id") == Some(&json!(id))
            });
            if is_match {
                steps.pop_front();
            }
            line = None;
            continue;
        }

        // 2. Execute steps that need no host line.
        enum StepKind {
            Emit,
            EmitRaw,
            Respond,
            RespondError,
            Close,
            WaitFor,
            WaitForResponse,
            Delay,
            Done,
        }
        let kind = match steps.front() {
            Some(FakeStep::Emit(_)) => StepKind::Emit,
            Some(FakeStep::EmitRaw(_)) => StepKind::EmitRaw,
            Some(FakeStep::Respond { .. }) => StepKind::Respond,
            Some(FakeStep::RespondError { .. }) => StepKind::RespondError,
            Some(FakeStep::Close) => StepKind::Close,
            Some(FakeStep::WaitFor { .. }) => StepKind::WaitFor,
            Some(FakeStep::WaitForResponse { .. }) => StepKind::WaitForResponse,
            Some(FakeStep::Delay { .. }) => StepKind::Delay,
            None => StepKind::Done,
        };
        match kind {
            StepKind::Emit => {
                let Some(FakeStep::Emit(value)) = steps.pop_front() else {
                    unreachable!("kind matched above")
                };
                emit_value(&to_host, &value);
            }
            StepKind::EmitRaw => {
                let Some(FakeStep::EmitRaw(raw)) = steps.pop_front() else {
                    unreachable!("kind matched above")
                };
                if let Some(tx) = &to_host {
                    let _ = tx.unbounded_send(Ok(raw));
                }
            }
            StepKind::Respond => {
                let Some(FakeStep::Respond { result }) = steps.pop_front() else {
                    unreachable!("kind matched above")
                };
                let id = remembered.as_ref().and_then(|request| request.id.clone());
                emit_response(&to_host, id.as_ref(), Some(result), None);
            }
            StepKind::RespondError => {
                let Some(FakeStep::RespondError { code, message }) = steps.pop_front() else {
                    unreachable!("kind matched above")
                };
                let id = remembered.as_ref().and_then(|request| request.id.clone());
                emit_response(
                    &to_host,
                    id.as_ref(),
                    None,
                    Some(json!({ "code": code, "message": message })),
                );
            }
            StepKind::Close => {
                steps.pop_front();
                to_host = None;
            }
            StepKind::WaitFor => {
                // Need a new host line.
                line = from_host.next().await;
                if line.is_none() {
                    break;
                }
            }
            StepKind::WaitForResponse => {
                // Need a new host line (responses are consumed in phase 1b).
                line = from_host.next().await;
                if line.is_none() {
                    break;
                }
            }
            StepKind::Delay => {
                let Some(FakeStep::Delay { millis }) = steps.pop_front() else {
                    unreachable!("kind matched above")
                };
                tokio::time::sleep(Duration::from_millis(millis)).await;
            }
            StepKind::Done => {
                // Script finished: drain remaining host lines until the
                // host tears the transport down.
                while from_host.next().await.is_some() {}
                break;
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RequestView {
    method: String,
    id: Option<Value>,
}

fn parse_request(line: &str) -> Option<RequestView> {
    let value: Value = serde_json::from_str(line).ok()?;
    let method = value.get("method")?.as_str()?.to_string();
    let id = value.get("id").cloned();
    Some(RequestView { method, id })
}

fn emit_value(to_host: &Option<mpsc::UnboundedSender<Result<String, io::Error>>>, value: &Value) {
    if let Some(tx) = to_host {
        let _ = tx.unbounded_send(Ok(value.to_string()));
    }
}

fn emit_response(
    to_host: &Option<mpsc::UnboundedSender<Result<String, io::Error>>>,
    id: Option<&Value>,
    result: Option<Value>,
    error: Option<Value>,
) {
    let mut response = json!({ "jsonrpc": "2.0", "id": id.cloned().unwrap_or(Value::Null) });
    if let Some(result) = result {
        response["result"] = result;
    }
    if let Some(error) = error {
        response["error"] = error;
    }
    emit_value(to_host, &response);
}

/// Convenience builders for common ACP v1 wire values used by tests.
pub mod wire {
    use serde_json::{Value, json};

    /// A `session/update` notification line for `session_id`.
    #[must_use]
    pub fn session_update(session_id: &str, update: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": session_id, "update": update }
        })
    }

    /// An agent message chunk update.
    #[must_use]
    pub fn agent_message_chunk(message_id: &str, text: &str) -> Value {
        json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": message_id,
            "content": { "type": "text", "text": text }
        })
    }

    /// A user message chunk update.
    #[must_use]
    pub fn user_message_chunk(text: &str) -> Value {
        json!({
            "sessionUpdate": "user_message_chunk",
            "content": { "type": "text", "text": text }
        })
    }

    /// A `session/request_permission` request line.
    #[must_use]
    pub fn request_permission(
        session_id: &str,
        tool_call_id: &str,
        title: &str,
        options: Value,
    ) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {
                    "toolCallId": tool_call_id,
                    "title": title
                },
                "options": options
            }
        })
    }

    /// A `fs/read_text_file` request line.
    #[must_use]
    pub fn read_text_file(session_id: &str, path: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 101,
            "method": "fs/read_text_file",
            "params": { "sessionId": session_id, "path": path }
        })
    }

    /// A `terminal/create` request line.
    #[must_use]
    pub fn terminal_create(session_id: &str, command: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 102,
            "method": "terminal/create",
            "params": { "sessionId": session_id, "command": command }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_helpers_produce_valid_json() {
        let value = wire::session_update("s1", wire::agent_message_chunk("m1", "hi"));
        let reparsed: Value = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(reparsed["method"], "session/update");
        assert_eq!(reparsed["params"]["update"]["sessionUpdate"], "agent_message_chunk");
    }

    #[test]
    fn parse_request_ignores_responses_and_malformed_lines() {
        assert!(parse_request("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}").is_none());
        assert!(parse_request("not json").is_none());
        let request =
            parse_request("{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"initialize\"}").unwrap();
        assert_eq!(request.method, "initialize");
        assert_eq!(request.id, Some(json!(3)));
    }
}
