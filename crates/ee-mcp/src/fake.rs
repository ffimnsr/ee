//! In-process fake MCP server harness for deterministic client tests.
//!
//! No external binaries are spawned: the fake is a line-level actor over an
//! in-memory duplex transport, giving tests control over `server/discover`
//! results, primitive responses, MRTR `input_required` rounds, list-changed
//! notifications, and connection closure.
//!
//! # Script model
//!
//! [`FakeMcpStep::Respond`] answers every incoming request with `method`
//! using the request's own id; [`FakeMcpStep::RespondOnce`] does the same
//! only for the first matching request.  [`FakeMcpStep::Emit`] pushes a
//! notification line to the client.  Every line the client sends is recorded
//! and inspectable via [`FakeMcpServer::log`].

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::service::RoleClient;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

/// A factory that builds an in-memory client transport for one server
/// connection (each connect/reconnect gets a fresh fake).
pub trait FakeMcpTransportFactory: Send + Sync + 'static {
    /// Builds a new fake client transport.
    fn build(&self) -> DuplexStream;
}

/// The client transport handed to the manager for tests.
pub type FakeMcpTransport = DuplexStream;

/// One scripted step the fake server performs.
#[derive(Debug, Clone)]
pub enum FakeMcpStep {
    /// Answer every request with `method` using the request's own id.
    Respond {
        /// Request method to match.
        method: String,
        /// Result payload (the `result` field of the response).
        result: Value,
    },
    /// Answer the first request with `method` only.
    RespondOnce {
        /// Request method to match.
        method: String,
        /// Result payload (the `result` field of the response).
        result: Value,
    },
    /// Answer every request with `method` with a JSON-RPC error.
    RespondError {
        /// Request method to match.
        method: String,
        /// Error code.
        code: i64,
        /// Error message.
        message: String,
    },
    /// Emit a line to the client (notification or server-to-client request).
    Emit(Value),
    /// Pause the script.
    Delay {
        /// Milliseconds to pause.
        millis: u64,
    },
    /// Close the server side of the transport (EOF).
    Close,
}

/// A script of [`FakeMcpStep`]s executed in order.
#[derive(Debug, Clone, Default)]
pub struct FakeMcpScript {
    steps: Vec<FakeMcpStep>,
}

impl FakeMcpScript {
    /// Creates an empty script.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends [`FakeMcpStep::Respond`].
    #[must_use]
    pub fn respond(self, method: impl Into<String>, result: Value) -> Self {
        self.step(FakeMcpStep::Respond { method: method.into(), result })
    }

    /// Appends [`FakeMcpStep::RespondOnce`].
    #[must_use]
    pub fn respond_once(self, method: impl Into<String>, result: Value) -> Self {
        self.step(FakeMcpStep::RespondOnce { method: method.into(), result })
    }

    /// Appends [`FakeMcpStep::RespondError`].
    #[must_use]
    pub fn respond_error(
        self,
        method: impl Into<String>,
        code: i64,
        message: impl Into<String>,
    ) -> Self {
        self.step(FakeMcpStep::RespondError {
            method: method.into(),
            code,
            message: message.into(),
        })
    }

    /// Appends [`FakeMcpStep::Emit`].
    #[must_use]
    pub fn emit(self, value: Value) -> Self {
        self.step(FakeMcpStep::Emit(value))
    }

    /// Appends [`FakeMcpStep::Delay`].
    #[must_use]
    pub fn delay(self, millis: u64) -> Self {
        self.step(FakeMcpStep::Delay { millis })
    }

    /// Appends [`FakeMcpStep::Close`].
    #[must_use]
    pub fn close(self) -> Self {
        self.step(FakeMcpStep::Close)
    }

    fn step(mut self, step: FakeMcpStep) -> Self {
        self.steps.push(step);
        self
    }
}

/// Convenience builders for common server behaviors.
impl FakeMcpScript {
    /// Auto-answers `server/discover` (both handshake and explicit calls)
    /// with a 2026-07-28 result.
    #[must_use]
    pub fn discover_2026_07_28(self, capabilities: Value) -> Self {
        self.respond(
            "server/discover",
            json!({
                "resultType": "complete",
                "supportedVersions": ["2026-07-28"],
                "capabilities": capabilities,
                "ttlMs": 0,
                "cacheScope": "private",
            }),
        )
    }

    /// The most common opening: discover + empty tool list.
    #[must_use]
    pub fn standard(self) -> Self {
        self.discover_2026_07_28(json!({ "tools": {} })).respond(
            "tools/list",
            json!({ "tools": [], "resultType": "complete", "ttlMs": 0, "cacheScope": "private" }),
        )
    }
}

/// The test-side handle of the fake server.
#[derive(Debug, Clone)]
pub struct FakeMcpServer {
    log: Arc<Mutex<Vec<String>>>,
    handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl FakeMcpServer {
    /// Spawns the fake server and returns its handle plus the client
    /// transport to hand to the manager.
    #[must_use]
    pub fn spawn(script: FakeMcpScript) -> (Self, FakeMcpTransport) {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let handle = tokio::spawn(driver(script, server_side, log.clone()));
        (FakeMcpServer { log, handle: Arc::new(Mutex::new(Some(handle))) }, client_side)
    }

    /// Every line the client sent, in order.
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.log.lock().expect("fake log poisoned").clone()
    }

    /// Requests (lines with a `method` and `id`) by method name.
    #[must_use]
    pub fn requests_by_method(&self, method: &str) -> Vec<Value> {
        self.log()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| {
                value.get("method").and_then(Value::as_str) == Some(method)
                    && value.get("id").is_some()
            })
            .collect()
    }

    /// Whether any recorded line contains `needle`.
    #[must_use]
    pub fn log_contains(&self, needle: &str) -> bool {
        self.log().iter().any(|line| line.contains(needle))
    }

    /// Joins the driver task, failing the test on panic or incomplete
    /// scripts within `timeout`.
    pub async fn join(self, timeout: Duration) {
        let handle = self.handle.lock().expect("fake handle poisoned").take();
        let Some(handle) = handle else {
            return;
        };
        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("fake mcp server driver panicked: {error}"),
            Err(_) => panic!("fake mcp server script did not finish within {timeout:?}"),
        }
    }
}

/// Runs the script against the client's line stream.
///
/// `Respond`/`RespondError` steps become always-responders (they answer
/// every matching request, so `server/discover` works for both the handshake
/// and explicit re-discovery); `RespondOnce`, `Emit`, `Delay`, and `Close`
/// execute strictly in order.
async fn driver(script: FakeMcpScript, stream: DuplexStream, log: Arc<Mutex<Vec<String>>>) {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    // Always-responders: method -> result or error.
    let mut always: BTreeMap<String, Result<Value, (i64, String)>> = BTreeMap::new();
    let mut steps: VecDeque<FakeMcpStep> = VecDeque::new();
    for step in script.steps {
        match step {
            FakeMcpStep::Respond { method, result } => {
                always.insert(method, Ok(result));
            }
            FakeMcpStep::RespondError { method, code, message } => {
                always.insert(method, Err((code, message)));
            }
            other => steps.push_back(other),
        }
    }
    let mut line = String::new();

    loop {
        // 1. Process steps that need no new line.
        while let Some(step) = steps.front() {
            match step {
                FakeMcpStep::Delay { .. } => {
                    let FakeMcpStep::Delay { millis } = steps.pop_front().expect("delay") else {
                        unreachable!()
                    };
                    tokio::time::sleep(Duration::from_millis(millis)).await;
                }
                FakeMcpStep::Close => {
                    steps.pop_front();
                    return;
                }
                FakeMcpStep::Emit(_) => {
                    let FakeMcpStep::Emit(value) = steps.pop_front().expect("emit") else {
                        unreachable!()
                    };
                    let _ = write_half.write_all(value.to_string().as_bytes()).await;
                    let _ = write_half.write_all(b"\n").await;
                    let _ = write_half.flush().await;
                }
                _ => break,
            }
        }

        // 2. Answer the current line: always-responders first, then the next
        // matching RespondOnce in the queue.
        if !line.is_empty() {
            let parsed = serde_json::from_str::<Value>(&line).ok();
            let method = parsed.as_ref().and_then(|v| v.get("method")).and_then(Value::as_str);
            let id = parsed.as_ref().and_then(|v| v.get("id")).cloned();
            let consumed = if let (Some(method), Some(id)) = (method, &id) {
                let always_step = always.get(method).cloned();
                let once_index = steps.iter().position(|step| {
                    matches!(step, FakeMcpStep::RespondOnce { method: m, .. } if m == method)
                });
                let once_step = once_index.map(|index| steps.remove(index).expect("positioned"));
                let step = match (always_step, once_step) {
                    // The ordered queue wins: RespondOnce steps model a
                    // specific request sequence (e.g. MRTR retries).
                    (_, Some(FakeMcpStep::RespondOnce { result, method, .. })) => {
                        Some((Ok(result), method == "subscriptions/listen"))
                    }
                    (Some(always), None) => Some((always, method == "subscriptions/listen")),
                    _ => None,
                };
                if let Some((response, is_listen)) = step {
                    if is_listen {
                        // SEP subscriptions: the server acknowledges with a
                        // notification carrying the request id; a normal
                        // result is a protocol error.
                        let accepted = parsed
                            .as_ref()
                            .and_then(|v| v.pointer("/params/notifications"))
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        let ack = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/subscriptions/acknowledged",
                            "params": {
                                "_meta": { "io.modelcontextprotocol/subscriptionId": id },
                                "notifications": accepted,
                            },
                        });
                        let _ = write_half.write_all(ack.to_string().as_bytes()).await;
                        let _ = write_half.write_all(b"\n").await;
                        let _ = write_half.flush().await;
                        true
                    } else {
                        let response = match response {
                            Ok(result) => {
                                json!({ "jsonrpc": "2.0", "id": id, "result": result })
                            }
                            Err((code, message)) => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": code, "message": message }
                            }),
                        };
                        let _ = write_half.write_all(response.to_string().as_bytes()).await;
                        let _ = write_half.write_all(b"\n").await;
                        let _ = write_half.flush().await;
                        true
                    }
                } else {
                    false
                }
            } else {
                false
            };
            if consumed {
                line.clear();
                continue;
            }
        }

        // 3. Wait for the next line from the client (EOF ends the script).
        line.clear();
        let read = reader.read_line(&mut line).await.unwrap_or(0);
        if read == 0 {
            return;
        }
        let trimmed = line.trim_end().to_string();
        if !trimmed.is_empty() {
            log.lock().expect("fake log poisoned").push(trimmed.clone());
        }
        // Keep `line` (with the trailing newline stripped for parsing) so the
        // next iteration can match it against Respond steps.
        line = trimmed;
    }
}

/// In-memory transport note (the combined `AsyncRead + AsyncWrite` impl
/// makes a bare `DuplexStream` a valid rmcp transport).
#[allow(dead_code)]
fn _adapter_note() {
    fn assert_transport<T: rmcp::transport::IntoTransport<RoleClient, std::io::Error, ()>>() {}
    let _ = "DuplexStream implements AsyncRead + AsyncWrite, so the combined";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_builders_produce_expected_values() {
        let script =
            FakeMcpScript::new().standard().respond_once("tools/list", json!({ "tools": [] }));
        assert!(script.steps.len() >= 3);
    }
}
