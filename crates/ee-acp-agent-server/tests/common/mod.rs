//! Shared test harness for the integration test binaries: a recording fake
//! provider with per-session prompt behaviors, a server-over-memory-
//! transport spawner, and frame/response helpers.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub use ee_acp_agent_server::{
    AcpAgentServer, AcpAgentServerConfig, AcpServerError, AgentProvider, ClientBridge,
    LoadSessionContext, MemoryTransport, MemoryTransportHandle, NewSessionContext, PromptContext,
    PromptResult, ProviderError, ProviderFuture, SessionInit, UpdateSink,
};
use ee_agent_protocol::{
    AgentCapabilities, AvailableCommand, ContentBlock, ContentChunk, CreateTerminalRequest,
    Error as RpcError, Implementation, MessageId, RawJsonRpcMessage, RawJsonRpcParams,
    ReadTextFileRequest, RequestId, Response, SessionCapabilities, SessionCloseCapabilities,
    SessionId, SessionListCapabilities, SessionUpdate, StopReason, TextContent,
};
use serde_json::{Value, json};
use tokio::sync::watch;

/// How the fake provider behaves for one session's prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptBehavior {
    /// Returns immediately with `EndTurn`.
    Return,
    /// Returns a provider backend failure.
    Fail,
    /// Emits one agent message chunk, then returns `EndTurn`.
    EmitMessageThenReturn,
    /// Blocks until the cancellation signal flips, then returns
    /// `ProviderError::Cancellation`.
    AwaitCancelThenCancelled,
    /// Blocks until the cancellation signal flips, then tries to emit one
    /// update and records whether the sink still works
    /// (`prompt:<id>:emit-after-cancel:emitted` or `err:<error>`) before
    /// returning `ProviderError::Cancellation`.  Used to prove updates for
    /// removed sessions are dropped and that the writer path closes after
    /// reader shutdown.
    AwaitCancelThenTryEmitThenCancelled,
    /// Calls `client.read_text_file` with the given path; records the
    /// outcome as `client:read_text_file:ok:<content>` or
    /// `client:read_text_file:err:<error>`, and fails the prompt with the
    /// bridge error when the call fails.
    ReadTextFile { path: String },
    /// Calls `client.read_text_file` with the given path; records the
    /// outcome, and always returns `EndTurn` (used to prove invalid input
    /// never reaches the transport).
    ReadTextFileAndContinue { path: String },
    /// Calls `client.create_terminal` with a relative `cwd`; records the
    /// outcome, and always returns `EndTurn`.
    CreateTerminalRelativeCwd,
}

/// Ordered call record for the fake provider.
#[derive(Clone)]
pub struct CallLog(Arc<Mutex<Vec<String>>>);

impl CallLog {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    pub fn record(&self, entry: impl Into<String>) {
        self.0.lock().expect("call log poisoned").push(entry.into());
    }

    pub fn calls(&self) -> Vec<String> {
        self.0.lock().expect("call log poisoned").clone()
    }

    pub fn has_call(&self, prefix: &str) -> bool {
        self.calls().iter().any(|call| call.starts_with(prefix))
    }
}

/// Deterministic fake provider: each `session/new` takes the next id from a
/// per-test queue, `session/load` echoes the requested id, and prompts
/// follow the per-session [`PromptBehavior`] map.
pub struct FakeProvider {
    pub log: CallLog,
    ids: Arc<Mutex<VecDeque<String>>>,
    /// Commands advertised in every `SessionInit` (empty by default).
    commands: Arc<Mutex<Vec<AvailableCommand>>>,
    /// `(role, text)` conversation messages replayed by `session/load`
    /// through the replay sink (empty by default).
    replay: Arc<Mutex<Vec<(String, String)>>>,
    /// When set, `session/load`/`session/resume` fail with this message
    /// (simulates "no persisted state").
    load_error: Arc<Mutex<Option<String>>>,
    pub behaviors: Arc<Mutex<HashMap<String, PromptBehavior>>>,
}

impl FakeProvider {
    pub fn new(ids: &[&str]) -> (Self, CallLog) {
        let log = CallLog::new();
        let provider = Self {
            log: log.clone(),
            ids: Arc::new(Mutex::new(ids.iter().map(|id| id.to_string()).collect())),
            commands: Arc::new(Mutex::new(Vec::new())),
            replay: Arc::new(Mutex::new(Vec::new())),
            load_error: Arc::new(Mutex::new(None)),
            behaviors: Arc::new(Mutex::new(HashMap::new())),
        };
        (provider, log)
    }

    /// Advertises the given commands in every session init (tests prove the
    /// framework forwards them as `available_commands_update`).
    pub fn with_commands(self, commands: Vec<AvailableCommand>) -> Self {
        *self.commands.lock().expect("fake provider commands poisoned") = commands;
        self
    }

    /// Makes `session/load` replay the given `(role, text)` conversation
    /// messages through the replay sink before responding (tests prove the
    /// ACP v1 replay-before-response ordering).
    pub fn with_replay(self, messages: Vec<(&str, &str)>) -> Self {
        *self.replay.lock().expect("fake provider replay poisoned") =
            messages.into_iter().map(|(role, text)| (role.to_string(), text.to_string())).collect();
        self
    }

    /// Makes `session/load`/`session/resume` fail with `message` (simulates
    /// a provider with no persisted state to restore).
    pub fn with_load_error(self, message: &str) -> Self {
        *self.load_error.lock().expect("fake provider load error poisoned") =
            Some(message.to_string());
        self
    }

    pub fn next_id(&self) -> String {
        self.ids
            .lock()
            .expect("fake provider ids poisoned")
            .pop_front()
            .expect("fake provider id queue exhausted")
    }

    pub fn set_prompt_behavior(&self, session_id: &str, behavior: PromptBehavior) {
        self.behaviors
            .lock()
            .expect("fake provider behaviors poisoned")
            .insert(session_id.to_string(), behavior);
    }
}

impl AgentProvider for FakeProvider {
    fn info(&self) -> Implementation {
        Implementation::new("fake-provider", "0.0.1").title("Fake Provider")
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default().load_session(true).session_capabilities(
            SessionCapabilities::new()
                .list(SessionListCapabilities::new())
                .close(SessionCloseCapabilities::new()),
        )
    }

    fn new_session(
        &self,
        ctx: NewSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let log = self.log.clone();
        let id = self.next_id();
        let cwd = ctx.cwd.clone();
        let commands = self.commands.lock().expect("fake provider commands poisoned").clone();
        Box::pin(async move {
            log.record(format!("new_session:{}", cwd.display()));
            Ok(SessionInit::new(SessionId::new(id)).title("Test Session").commands(commands))
        })
    }

    fn load_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let log = self.log.clone();
        let session_id = ctx.session_id.clone();
        let commands = self.commands.lock().expect("fake provider commands poisoned").clone();
        let replay = self.replay.lock().expect("fake provider replay poisoned").clone();
        let load_error = self.load_error.lock().expect("fake provider load error poisoned").clone();
        let sink = ctx.replay_sink;
        Box::pin(async move {
            log.record(format!("load_session:{session_id}"));
            if let Some(message) = load_error {
                return Err(ProviderError::BackendFailure(message));
            }
            if let Some(sink) = sink {
                for (index, (role, text)) in replay.iter().enumerate() {
                    match role.as_str() {
                        "user" => {
                            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(
                                text.clone(),
                            )))
                            .message_id(MessageId::new(format!("replay-u-{}", index + 1)));
                            sink.raw_update(SessionUpdate::UserMessageChunk(chunk)).map_err(
                                |error| ProviderError::BackendFailure(error.to_string()),
                            )?;
                        }
                        _ => {
                            sink.agent_message_chunk(format!("replay-a-{}", index + 1), text)
                                .map_err(|error| {
                                    ProviderError::BackendFailure(error.to_string())
                                })?;
                        }
                    }
                }
            }
            Ok(SessionInit::new(session_id).commands(commands))
        })
    }

    fn prompt(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        mut cancel: watch::Receiver<bool>,
    ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
        let log = self.log.clone();
        let session_id = ctx.session_id.to_string();
        let behavior = self
            .behaviors
            .lock()
            .expect("fake provider behaviors poisoned")
            .get(&session_id)
            .cloned()
            .unwrap_or(PromptBehavior::Return);
        Box::pin(async move {
            log.record(format!("prompt:{session_id}:started"));
            match behavior {
                PromptBehavior::Return => Ok(PromptResult::new(StopReason::EndTurn)),
                PromptBehavior::Fail => {
                    Err(ProviderError::BackendFailure("fake provider boom".into()))
                }
                PromptBehavior::EmitMessageThenReturn => {
                    sink.agent_message_chunk("m-1", "hello from provider").expect("sink emits");
                    Ok(PromptResult::new(StopReason::EndTurn))
                }
                PromptBehavior::AwaitCancelThenCancelled => {
                    let _ = cancel.changed().await;
                    log.record(format!("prompt:{session_id}:cancelled"));
                    Err(ProviderError::Cancellation)
                }
                PromptBehavior::AwaitCancelThenTryEmitThenCancelled => {
                    let _ = cancel.changed().await;
                    let outcome = match sink.agent_message_chunk("m-after", "post-close") {
                        Ok(()) => "emitted".to_string(),
                        Err(error) => format!("err:{error}"),
                    };
                    log.record(format!("prompt:{session_id}:emit-after-cancel:{outcome}"));
                    Err(ProviderError::Cancellation)
                }
                PromptBehavior::ReadTextFile { path } => {
                    let request = ReadTextFileRequest::new(session_id.clone(), path);
                    match client.read_text_file(request).await {
                        Ok(response) => {
                            log.record(format!("client:read_text_file:ok:{}", response.content));
                            Ok(PromptResult::new(StopReason::EndTurn))
                        }
                        Err(error) => {
                            log.record(format!("client:read_text_file:err:{error}"));
                            Err(error)
                        }
                    }
                }
                PromptBehavior::ReadTextFileAndContinue { path } => {
                    let request = ReadTextFileRequest::new(session_id.clone(), path);
                    let outcome = match client.read_text_file(request).await {
                        Ok(response) => format!("ok:{}", response.content),
                        Err(error) => format!("err:{error}"),
                    };
                    log.record(format!("client:read_text_file:{outcome}"));
                    Ok(PromptResult::new(StopReason::EndTurn))
                }
                PromptBehavior::CreateTerminalRelativeCwd => {
                    let request = CreateTerminalRequest::new(session_id.clone(), "echo hi")
                        .cwd(std::path::PathBuf::from("relative/dir"));
                    let outcome = match client.create_terminal(request).await {
                        Ok(_) => "ok".to_string(),
                        Err(error) => format!("err:{error}"),
                    };
                    log.record(format!("client:create_terminal:{outcome}"));
                    Ok(PromptResult::new(StopReason::EndTurn))
                }
            }
        })
    }

    fn cancel_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        let log = self.log.clone();
        Box::pin(async move {
            log.record(format!("cancel_session:{session_id}"));
            Ok(())
        })
    }

    fn close_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        let log = self.log.clone();
        Box::pin(async move {
            log.record(format!("close_session:{session_id}"));
            Ok(())
        })
    }
}

// ── Server harness ───────────────────────────────────────────────────────

/// Server-over-memory-transport harness with a non-destructive frame pump.
///
/// Outbound frames accumulate in a per-harness queue, so frames arriving in
/// separate batches (e.g. an update queued before a slow prompt round
/// finishes) are never lost between [`Harness::next_frames`] calls.
pub struct Harness {
    handle: MemoryTransportHandle,
    pending: Arc<Mutex<VecDeque<RawJsonRpcMessage>>>,
}

impl Harness {
    /// Wraps a raw memory-transport handle.
    pub fn new(handle: MemoryTransportHandle) -> Self {
        Self { handle, pending: Arc::new(Mutex::new(VecDeque::new())) }
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

    /// Snapshot of frames not yet consumed by the harness.
    pub fn outbound(&self) -> Vec<RawJsonRpcMessage> {
        self.handle.outbound()
    }

    /// Injects EOF: the server shuts down cleanly once queued messages are
    /// drained.
    pub fn close(self) {
        drop(self.handle);
    }

    /// Closes the transport and asserts the server shuts down cleanly.
    pub async fn shutdown(self, task: tokio::task::JoinHandle<Result<(), AcpServerError>>) {
        drop(self.handle);
        task.await.expect("server task joins").expect("server exits cleanly on EOF");
    }
}

/// Spawns a server over an in-memory transport and returns the harness (to
/// inject frames and read responses) plus the task.
pub async fn spawn_server(
    provider: FakeProvider,
) -> (Harness, tokio::task::JoinHandle<Result<(), AcpServerError>>) {
    spawn_server_with_config(provider, Default::default()).await
}

/// Spawns a server with a custom config (e.g. short bridge timeouts).
pub async fn spawn_server_with_config(
    provider: FakeProvider,
    config: AcpAgentServerConfig,
) -> (Harness, tokio::task::JoinHandle<Result<(), AcpServerError>>) {
    let server = AcpAgentServer::new(provider, config);
    let (transport, handle) = MemoryTransport::new();
    let task = tokio::spawn(async move { server.run_with_transport(transport).await });
    (Harness::new(handle), task)
}

/// Waits until a predicate over the provider call log holds, then returns
/// the log.
pub async fn wait_for_log(log: &CallLog, predicate: impl Fn(&[String]) -> bool) -> Vec<String> {
    for _ in 0..5_000 {
        let calls = log.calls();
        if predicate(&calls) {
            return calls;
        }
        tokio::task::yield_now().await;
    }
    panic!("call log predicate not satisfied; calls={:?}", log.calls());
}

pub fn request(id: i64, method: &str, params: Value) -> RawJsonRpcMessage {
    RawJsonRpcMessage::request(method.to_string(), params, RequestId::Number(id))
        .expect("test request builds")
}

pub fn notification(method: &str, params: Value) -> RawJsonRpcMessage {
    RawJsonRpcMessage::notification(method.to_string(), params).expect("test notification builds")
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

/// Extracts the `reason` detail attached to validation errors.
pub fn error_reason(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn session_new_params(cwd: &str) -> Value {
    json!({
        "cwd": cwd,
        "additionalDirectories": [],
        "mcpServers": [],
    })
}

pub fn prompt_params(session_id: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": "hello" }],
    })
}

/// Converts raw JSON-RPC params into a plain JSON value (mirrors the
/// server's own conversion; `RawJsonRpcParams` has no `Serialize`).
pub fn raw_params_to_value(params: Option<RawJsonRpcParams>) -> Value {
    match params {
        None => Value::Null,
        Some(RawJsonRpcParams::Object(map)) => Value::Object(map),
        Some(RawJsonRpcParams::Array(array)) => Value::Array(array),
    }
}
