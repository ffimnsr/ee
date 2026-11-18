//! End-to-end ACP flows for the OpenCode agent over the framework's memory
//! transport.
//!
//! These tests drive the production path — routed config, [`OpenCodeModelAdapter`]
//! dispatcher, orchestrator provider, `AcpAgentServer` — with a scripted dialect
//! codec in the codec's place. Nothing contacts the network, and every session
//! writes its durable state into a per-test temporary directory.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ee_agent_acp_server::{
    AcpAgentServer, AcpAgentServerConfig, JsonRpcFrame, MemoryTransport, MemoryTransportHandle,
};
use ee_agent_opencode::adapter::test_support::{ScriptedAnswer, ScriptedCodec, test_config};
use ee_agent_opencode::config::Config;
use ee_agent_opencode::critic::opencode_multi_model_provider_with_codecs;
use ee_agent_opencode::routes::{
    OpenCodeDialect, OpenCodeRoute, OpenCodeSurface, catalog, resolve_route,
};
use ee_agent_orchestrator::{CritiqueReport, CritiqueTarget, ModelRole, OrchestratorProvider};
use ee_agent_protocol::{Error as RpcError, RawJsonRpcMessage, RequestId, Response};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Bounded wait for the scripted codec to record `count` requests.
async fn wait_for_requests(codec: &ScriptedCodec, count: usize) {
    for _ in 0..5_000 {
        if codec.requests().len() >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for {count} recorded codec request(s)");
}

/// Per-test owned state: a durable session-state directory plus the workspace
/// the ACP session opens, both inside one temporary tree.
struct TestDirs {
    root: TempDir,
}

impl TestDirs {
    fn new() -> Self {
        let dirs = Self { root: TempDir::new().expect("temp dir") };
        fs::create_dir_all(dirs.workspace()).expect("workspace created");
        fs::create_dir_all(dirs.checkpoints()).expect("checkpoint directory created");
        dirs
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("agent-sessions")
    }

    fn checkpoints(&self) -> PathBuf {
        self.root.path().join("checkpoints")
    }

    fn workspace(&self) -> PathBuf {
        self.root.path().join("workspace")
    }
}

/// Minimal ACP client over the framework's memory transport.
struct Harness {
    handle: MemoryTransportHandle,
    pending: Arc<Mutex<VecDeque<JsonRpcFrame>>>,
    workspace: PathBuf,
}

impl Harness {
    fn send(&self, frame: RawJsonRpcMessage) -> bool {
        self.handle.send(JsonRpcFrame::Single(frame))
    }

    async fn next_frame(&self) -> RawJsonRpcMessage {
        self.next_frames(1).await.remove(0)
    }

    async fn next_frames(&self, count: usize) -> Vec<RawJsonRpcMessage> {
        for _ in 0..5_000 {
            let frames = {
                let mut pending = self.pending.lock().expect("harness pending poisoned");
                if pending.len() < count {
                    pending.extend(self.handle.take_outbound());
                }
                // Drain only a full batch: returning a partial one makes callers
                // that index `frames[count - 1]` panic when the responder is slow.
                // Partial frames stay queued for the next attempt.
                if pending.len() < count {
                    Vec::new()
                } else {
                    let mut collected = Vec::with_capacity(count);
                    while collected.len() < count {
                        match pending.pop_front() {
                            Some(JsonRpcFrame::Single(message)) => collected.push(message),
                            Some(batch) => {
                                panic!(
                                    "a batch response is queued; use a batch-aware harness instead: {batch:?}"
                                )
                            }
                            None => break,
                        }
                    }
                    collected
                }
            };
            if !frames.is_empty() {
                return frames;
            }
            tokio::task::yield_now().await;
        }
        panic!("timed out waiting for {count} outbound frames");
    }

    /// Collects every notification until the response to `id` arrives.
    async fn collect_until_response_frame(&self, id: i64) -> (Vec<Value>, Response<Value>) {
        let mut updates = Vec::new();
        for _ in 0..500 {
            match self.next_frame().await {
                RawJsonRpcMessage::Notification(notification) => {
                    updates.push(raw_params_to_value(notification.params.clone()));
                }
                RawJsonRpcMessage::Response(response) => {
                    let response_id = match &response {
                        Response::Result { id, .. } | Response::Error { id, .. } => id.clone(),
                    };
                    assert_eq!(response_id, RequestId::Number(id), "response for request {id}");
                    return (updates, response);
                }
                other => panic!("unexpected frame while waiting for request {id}: {other:?}"),
            }
        }
        panic!("timed out waiting for the response to request {id}");
    }

    /// Collects every notification until the successful response to `id`.
    async fn collect_until_response(&self, id: i64) -> (Vec<Value>, Value) {
        let (updates, response) = self.collect_until_response_frame(id).await;
        match response {
            Response::Result { result, .. } => (updates, result),
            Response::Error { error, .. } => panic!("request {id} failed: {}", error.message),
        }
    }

    /// Collects every notification until the failed response to `id`.
    async fn collect_until_error(&self, id: i64) -> (Vec<Value>, RpcError) {
        let (updates, response) = self.collect_until_response_frame(id).await;
        match response {
            Response::Error { error, .. } => (updates, error),
            Response::Result { result, .. } => {
                panic!("request {id} unexpectedly succeeded: {result}")
            }
        }
    }

    /// Collects every notification until the server asks the client for `method`.
    async fn collect_until_client_request(&self, method: &str) -> (Vec<Value>, RawJsonRpcMessage) {
        let mut updates = Vec::new();
        for _ in 0..500 {
            let frame = self.next_frame().await;
            match &frame {
                RawJsonRpcMessage::Notification(notification) => {
                    updates.push(raw_params_to_value(notification.params.clone()));
                }
                RawJsonRpcMessage::Request(request) => {
                    assert_eq!(request.method.as_ref(), method, "server asked for {method}");
                    return (updates, frame);
                }
                other => panic!("unexpected frame while waiting for {method}: {other:?}"),
            }
        }
        panic!("timed out waiting for the {method} bridge request");
    }

    async fn shutdown(
        self,
        task: tokio::task::JoinHandle<Result<(), ee_agent_acp_server::AcpServerError>>,
    ) {
        drop(self.handle);
        task.await.expect("server task joins").expect("server exits cleanly on EOF");
    }
}

/// One documented route per dialect, covering both surfaces.
const DIALECT_MATRIX: [(OpenCodeSurface, OpenCodeDialect); 3] = [
    (OpenCodeSurface::Zen, OpenCodeDialect::OpenAiResponses),
    (OpenCodeSurface::Go, OpenCodeDialect::AnthropicMessages),
    (OpenCodeSurface::Go, OpenCodeDialect::OpenAiChatCompletions),
];

/// First documented route for one surface and dialect.
fn route_for(surface: OpenCodeSurface, dialect: OpenCodeDialect) -> OpenCodeRoute {
    catalog()
        .find(|route| route.surface == surface && route.dialect == dialect)
        .expect("catalog documents a route for this surface and dialect")
}

/// Spawns the production provider over a scripted codec for one documented route.
fn spawn_agent(
    surface: OpenCodeSurface,
    dialect: OpenCodeDialect,
    answers: Vec<ScriptedAnswer>,
    dirs: &TestDirs,
) -> SpawnedAgent {
    let route = route_for(surface, dialect);
    spawn_agent_with_config(test_config(surface, route.model_id), answers, dirs)
}

/// Spawns the production provider over a scripted codec for an exact config, so
/// tests can change recovery and budget knobs the same way the binary does.
fn spawn_agent_with_config(
    config: Config,
    answers: Vec<ScriptedAnswer>,
    dirs: &TestDirs,
) -> SpawnedAgent {
    let route = config.route;
    let codec = Arc::new(ScriptedCodec::new(route.dialect, answers).with_label(route.label()));
    let (provider, warning) = opencode_multi_model_provider_with_codecs(
        &config,
        dirs.state(),
        (route, codec.clone() as Arc<dyn ee_agent_opencode::adapter::DialectCodec>),
        None,
    )
    .expect("root provider builds");
    assert!(warning.is_none(), "no critic is configured in this fixture: {warning:?}");
    let (harness, task) = spawn_provider(provider, dirs);
    (codec, harness, task)
}

/// Serves one already-built provider over the framework's memory transport.
fn spawn_provider(
    provider: OrchestratorProvider,
    dirs: &TestDirs,
) -> (Harness, tokio::task::JoinHandle<Result<(), ee_agent_acp_server::AcpServerError>>) {
    let server = AcpAgentServer::new(provider, AcpAgentServerConfig::default());
    let (transport, handle) = MemoryTransport::new();
    let task = tokio::spawn(async move { server.run_with_transport(transport).await });
    let harness = Harness {
        handle,
        pending: Arc::new(Mutex::new(VecDeque::new())),
        workspace: dirs.workspace(),
    };
    (harness, task)
}

type SpawnedAgent = (
    Arc<ScriptedCodec>,
    Harness,
    tokio::task::JoinHandle<Result<(), ee_agent_acp_server::AcpServerError>>,
);

fn request(id: i64, method: &str, params: Value) -> RawJsonRpcMessage {
    RawJsonRpcMessage::request(method.to_string(), params, RequestId::Number(id))
        .expect("test request builds")
}

fn notification(method: &str, params: Value) -> RawJsonRpcMessage {
    RawJsonRpcMessage::notification(method.to_string(), params).expect("test notification builds")
}

fn raw_params_to_value(params: Option<ee_agent_protocol::RawJsonRpcParams>) -> Value {
    match params {
        None => Value::Null,
        Some(ee_agent_protocol::RawJsonRpcParams::Object(map)) => Value::Object(map),
        Some(ee_agent_protocol::RawJsonRpcParams::Array(array)) => Value::Array(array),
    }
}

async fn initialize(harness: &Harness) {
    harness.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = result_of(harness.next_frame().await);

    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentInfo"]["name"], "ee-agent-opencode");
    assert_eq!(result["agentInfo"]["title"], "OpenCode");
}

/// Opens a session in the test workspace and drains the command advertisement
/// that follows it.
async fn new_session(harness: &Harness, id: i64) -> String {
    harness.send(request(
        id,
        "session/new",
        json!({
            "cwd": harness.workspace,
            "additionalDirectories": [],
            "mcpServers": [],
        }),
    ));
    let session_id = result_of(harness.next_frame().await)["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let update = notification_params(harness.next_frame().await);
    assert_eq!(update["update"]["sessionUpdate"], "available_commands_update");
    session_id
}

fn prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
    })
}

fn result_of(frame: RawJsonRpcMessage) -> Value {
    let RawJsonRpcMessage::Response(response) = frame else {
        panic!("expected a response frame, got {frame:?}");
    };
    match response {
        Response::Result { result, .. } => result,
        Response::Error { error, .. } => {
            panic!("expected a successful response, got {}", error.message)
        }
    }
}

fn notification_params(frame: RawJsonRpcMessage) -> Value {
    let RawJsonRpcMessage::Notification(notification) = frame else {
        panic!("expected a notification frame, got {frame:?}");
    };
    raw_params_to_value(notification.params.clone())
}

/// Drains the MCP diagnostics thought updates emitted at prompt start until the
/// summary message, so prompt assertions start from the plan update.
async fn drain_prompt_start(harness: &Harness) {
    loop {
        let params = notification_params(harness.next_frame().await);
        assert_eq!(
            params["update"]["sessionUpdate"], "agent_thought_chunk",
            "only thought updates precede the plan: {params}"
        );
        if params["update"]["messageId"] == "mcp-diagnostics" {
            return;
        }
    }
}

fn update_kinds(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .filter_map(|params| params["update"]["sessionUpdate"].as_str().map(String::from))
        .collect()
}

fn update_text(updates: &[Value], session_update: &str) -> String {
    update_text_chunks(updates, session_update).join("")
}

fn update_text_chunks(updates: &[Value], session_update: &str) -> Vec<String> {
    updates
        .iter()
        .filter(|params| params["update"]["sessionUpdate"] == session_update)
        .filter_map(|params| params["update"]["content"]["text"].as_str().map(String::from))
        .collect()
}

/// Whether one update kind carried `text` as its own chunk, unmodified.
fn has_text_chunk(updates: &[Value], session_update: &str, text: &str) -> bool {
    update_text_chunks(updates, session_update).iter().any(|chunk| chunk == text)
}

/// Paths directly inside `directory`, ignoring unreadable entries.
fn directory_entries(directory: &std::path::Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .map(|entries| entries.filter_map(Result::ok).map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

mod capability;
mod critic;
mod dialects;
mod redaction;
mod single_dialect;
