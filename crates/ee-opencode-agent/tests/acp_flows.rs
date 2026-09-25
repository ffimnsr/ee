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

use ee_acp_agent_server::{
    AcpAgentServer, AcpAgentServerConfig, MemoryTransport, MemoryTransportHandle,
};
use ee_agent_orchestrator::{CritiqueReport, CritiqueTarget, OrchestratorProvider};
use ee_agent_protocol::{Error as RpcError, RawJsonRpcMessage, RequestId, Response};
use ee_opencode_agent::adapter::test_support::{ScriptedAnswer, ScriptedCodec, test_config};
use ee_opencode_agent::config::Config;
use ee_opencode_agent::critic::opencode_multi_model_provider_with_codecs;
use ee_opencode_agent::routes::{
    OpenCodeDialect, OpenCodeRoute, OpenCodeSurface, catalog, resolve_route,
};
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
    pending: Arc<Mutex<VecDeque<RawJsonRpcMessage>>>,
    workspace: PathBuf,
}

impl Harness {
    fn send(&self, frame: RawJsonRpcMessage) -> bool {
        self.handle.send(frame)
    }

    async fn next_frame(&self) -> RawJsonRpcMessage {
        self.next_frames(1).await.remove(0)
    }

    async fn next_frames(&self, count: usize) -> Vec<RawJsonRpcMessage> {
        for _ in 0..5_000 {
            let ready = {
                let mut pending = self.pending.lock().expect("harness pending poisoned");
                if pending.len() < count {
                    pending.extend(self.handle.take_outbound());
                }
                if pending.len() >= count { Some(pending.drain(..count).collect()) } else { None }
            };
            if let Some(frames) = ready {
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
        task: tokio::task::JoinHandle<Result<(), ee_acp_agent_server::AcpServerError>>,
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
    let codec = Arc::new(ScriptedCodec::new(route.dialect, answers));
    let (provider, warning) = opencode_multi_model_provider_with_codecs(
        &config,
        dirs.state(),
        (route, codec.clone() as Arc<dyn ee_opencode_agent::adapter::DialectCodec>),
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
) -> (Harness, tokio::task::JoinHandle<Result<(), ee_acp_agent_server::AcpServerError>>) {
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
    tokio::task::JoinHandle<Result<(), ee_acp_agent_server::AcpServerError>>,
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
    assert_eq!(result["agentInfo"]["name"], "ee-opencode-agent");
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

#[tokio::test]
async fn initialize_reports_the_opencode_agent_identity() {
    let dirs = TestDirs::new();
    let (_codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiResponses,
        vec![ScriptedAnswer::text("unused")],
        &dirs,
    );

    initialize(&harness).await;

    harness.shutdown(task).await;
}

#[tokio::test]
async fn prompt_streams_thought_then_answer_for_the_configured_route() {
    let dirs = TestDirs::new();
    let (codec, harness, task) = spawn_agent(
        OpenCodeSurface::Go,
        OpenCodeDialect::AnthropicMessages,
        vec![ScriptedAnswer::reasoning("weighing options", "the answer")],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
    drain_prompt_start(&harness).await;
    let (updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(update_text(&updates, "agent_thought_chunk"), "weighing options");
    assert!(has_text_chunk(&updates, "agent_message_chunk", "the answer"));
    // The host appends its own completion report after the model's answer.
    assert!(update_text(&updates, "agent_message_chunk").contains("the answer"));
    let recorded = codec.requests();
    assert_eq!(recorded.len(), 1, "one model round for one answer");
    assert!(!recorded[0].tools.is_empty(), "the routed tools reach the codec");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn tool_call_runs_through_the_client_bridge_and_continues_the_turn() {
    let dirs = TestDirs::new();
    let (codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiChatCompletions,
        vec![
            ScriptedAnswer::tool_call("call_1", "read_file", json!({ "path": "/tmp/notes.txt" })),
            ScriptedAnswer::text("read it"),
        ],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "read a file")));
    drain_prompt_start(&harness).await;
    let (updates, bridge_request) = harness.collect_until_client_request("fs/read_text_file").await;

    // The normalized tool call reached the client before the tool ran.
    assert!(update_kinds(&updates).contains(&String::from("tool_call")));
    let params = raw_params_to_value(match &bridge_request {
        RawJsonRpcMessage::Request(request) => request.params.clone(),
        other => panic!("expected the bridge request, got {other:?}"),
    });
    assert_eq!(params["sessionId"], session_id);
    assert_eq!(params["path"], "/tmp/notes.txt");

    let RawJsonRpcMessage::Request(request) = &bridge_request else {
        panic!("expected the bridge request");
    };
    harness.send(RawJsonRpcMessage::response(
        request.id.clone(),
        Ok(json!({ "content": "notes from the workspace" })),
    ));
    let (updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "end_turn");
    assert!(update_kinds(&updates).contains(&String::from("tool_call_update")));
    assert!(has_text_chunk(&updates, "agent_message_chunk", "read it"));

    // The second model round carried the tool observation, with the model tool
    // call id preserved for correlation.
    let recorded = codec.requests();
    assert_eq!(recorded.len(), 2, "the turn continued after the tool result");
    let second = recorded[1].json();
    let transcript = second["transcript"].to_string();
    assert!(transcript.contains("notes from the workspace"), "{transcript}");
    assert!(transcript.contains("call_1"), "tool-call identity is preserved: {transcript}");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn session_cancel_stops_a_pending_model_turn() {
    let dirs = TestDirs::new();
    let (codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiResponses,
        vec![ScriptedAnswer::Pending],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "wait")));
    drain_prompt_start(&harness).await;
    wait_for_requests(&codec, 1).await;
    harness.send(notification("session/cancel", json!({ "sessionId": session_id })));
    let (updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "cancelled");
    assert_eq!(codec.requests().len(), 1, "the call started once and was not retried");
    assert!(
        !update_kinds(&updates).contains(&String::from("agent_message_chunk")),
        "a cancelled turn emits no answer"
    );

    harness.shutdown(task).await;
}

#[tokio::test]
async fn missing_credential_fails_the_prompt_without_a_model_request() {
    let dirs = TestDirs::new();
    // The credential now comes from the configuration, exactly as in production.
    let mut config = test_config(OpenCodeSurface::Go, "kimi-k3");
    config.api_key = None;
    let (codec, harness, task) =
        spawn_agent_with_config(config, vec![ScriptedAnswer::text("never sent")], &dirs);
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
    drain_prompt_start(&harness).await;
    let (updates, error) = harness.collect_until_error(3).await;

    assert!(error.message.contains("OPENCODE_API_KEY is not set"), "{}", error.message);
    assert!(
        !update_kinds(&updates).contains(&String::from("agent_message_chunk")),
        "a failed turn emits no answer"
    );
    assert!(codec.requests().is_empty(), "no request exists without a credential");

    harness.shutdown(task).await;
}

#[tokio::test]
async fn session_close_ends_the_session_and_the_server_exits_on_eof() {
    let dirs = TestDirs::new();
    let (_codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiResponses,
        vec![ScriptedAnswer::text("unused")],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/close", json!({ "sessionId": session_id })));
    assert_eq!(result_of(harness.next_frame().await), json!({}));

    harness.shutdown(task).await;
}

// ── Per-dialect behavior matrix (Phase 6) ────────────────────────────────
//
// The single-dialect tests above assert detailed frame content. These tests run
// the same production path for one route of every dialect, so cancellation,
// stream ordering, policy admission, durable recovery, and session close are
// proven per codec rather than once.

fn directory_entries(directory: &std::path::Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .map(|entries| entries.filter_map(Result::ok).map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn every_dialect_streams_thought_then_answer_and_ends_the_turn() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        let (codec, harness, task) = spawn_agent(
            surface,
            dialect,
            vec![ScriptedAnswer::reasoning("thinking", "answer")],
            &dirs,
        );
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
        drain_prompt_start(&harness).await;
        let (updates, result) = harness.collect_until_response(3).await;

        let label = format!("{surface:?}/{dialect:?}");
        assert_eq!(result["stopReason"], "end_turn", "{label}");
        assert!(has_text_chunk(&updates, "agent_message_chunk", "answer"), "{label}");
        let kinds = update_kinds(&updates);
        let thought = kinds.iter().position(|kind| kind == "agent_thought_chunk");
        let answer = kinds.iter().position(|kind| kind == "agent_message_chunk");
        assert!(
            matches!((thought, answer), (Some(thought), Some(answer)) if thought < answer),
            "reasoning must stream before the answer for {label}: {kinds:?}"
        );
        assert_eq!(codec.requests().len(), 1, "one model round for one answer on {label}");

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn every_dialect_cancels_a_pending_model_call_without_retrying() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        let (codec, harness, task) =
            spawn_agent(surface, dialect, vec![ScriptedAnswer::Pending], &dirs);
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "wait")));
        drain_prompt_start(&harness).await;
        wait_for_requests(&codec, 1).await;
        harness.send(notification("session/cancel", json!({ "sessionId": session_id })));
        let (updates, result) = harness.collect_until_response(3).await;

        let label = format!("{surface:?}/{dialect:?}");
        assert_eq!(result["stopReason"], "cancelled", "{label}");
        assert_eq!(codec.requests().len(), 1, "a cancelled call is never retried on {label}");
        assert!(
            !update_kinds(&updates).contains(&String::from("agent_message_chunk")),
            "a cancelled turn emits no answer on {label}"
        );

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn every_dialect_denies_a_policy_blocked_tool_before_any_host_request() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        // `kill_terminal` is registered but carries the destructive
        // `TerminalKill` subclass, so the shared ee agent policy denies it and
        // the host is never asked to kill anything.
        let (codec, harness, task) = spawn_agent(
            surface,
            dialect,
            vec![
                ScriptedAnswer::tool_call(
                    "call_1",
                    "kill_terminal",
                    json!({ "terminal_id": "t-1" }),
                ),
                ScriptedAnswer::text("stopped"),
            ],
            &dirs,
        );
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "kill it")));
        drain_prompt_start(&harness).await;
        // `collect_until_response` panics on server-initiated requests, so a
        // clean result proves no host mutation request was made.
        let (updates, result) = harness.collect_until_response(3).await;

        let label = format!("{surface:?}/{dialect:?}");
        assert_eq!(result["stopReason"], "end_turn", "{label}");
        let failed = updates.iter().any(|params| {
            params["update"]["sessionUpdate"] == "tool_call_update"
                && params["update"]["status"] == "failed"
        });
        assert!(failed, "the denied tool reports a failed call on {label}: {updates:?}");
        assert!(has_text_chunk(&updates, "agent_message_chunk", "stopped"), "{label}");
        assert_eq!(codec.requests().len(), 2, "the turn continued after the denial on {label}");

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn every_dialect_recovers_a_durable_session_after_a_restart() {
    for (surface, dialect) in DIALECT_MATRIX {
        let label = format!("{surface:?}/{dialect:?}");
        let dirs = TestDirs::new();
        let (first_codec, harness, task) =
            spawn_agent(surface, dialect, vec![ScriptedAnswer::text("first answer")], &dirs);
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;
        harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
        drain_prompt_start(&harness).await;
        let (updates, result) = harness.collect_until_response(3).await;
        assert_eq!(result["stopReason"], "end_turn", "{label}");
        assert!(has_text_chunk(&updates, "agent_message_chunk", "first answer"), "{label}");
        assert_eq!(first_codec.requests().len(), 1, "{label} model round before the restart");
        harness.shutdown(task).await;

        // Durability is only real if the state survives the process: a fresh
        // provider over the same state directory must reload the session and
        // keep taking prompts on it.
        assert!(
            !directory_entries(&dirs.state()).is_empty(),
            "durable session state is written on {label}"
        );
        let (codec, harness, task) =
            spawn_agent(surface, dialect, vec![ScriptedAnswer::text("resumed answer")], &dirs);
        initialize(&harness).await;
        harness.send(request(
            2,
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": dirs.workspace(),
                "mcpServers": [],
            }),
        ));
        let (updates, loaded) = harness.collect_until_response(2).await;
        assert!(loaded.is_object(), "{label} load returns a result object: {loaded}");
        let commands = updates
            .iter()
            .find(|params| params["update"]["sessionUpdate"] == "available_commands_update")
            .unwrap_or_else(|| panic!("{label} advertises commands after load: {updates:?}"));
        assert_eq!(
            commands["sessionId"],
            json!(session_id),
            "{label} reloads the same durable session id"
        );

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "continue")));
        drain_prompt_start(&harness).await;
        let (updates, result) = harness.collect_until_response(3).await;
        assert_eq!(result["stopReason"], "end_turn", "{label} after reload");
        assert!(
            has_text_chunk(&updates, "agent_message_chunk", "resumed answer"),
            "{label} keeps prompting after a restart"
        );
        assert_eq!(codec.requests().len(), 1, "{label} model round after reload");

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn every_dialect_keeps_durable_recovery_state_wired_to_the_checkpoint_directory() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        let config = test_config(surface, route_for(surface, dialect).model_id);
        let mut durable = config;
        durable.checkpoint_dir = Some(dirs.checkpoints());

        let provider_config =
            ee_opencode_agent::adapter::opencode_orchestrator_config(&durable, dirs.state());

        assert!(
            provider_config.orchestrator.recovery.is_durable(),
            "{surface:?}/{dialect:?} keeps the configured durable recovery directory"
        );
        assert_eq!(
            provider_config.orchestrator.recovery.checkpoint_dir.as_deref(),
            Some(dirs.checkpoints().as_path())
        );
    }
}

#[tokio::test]
async fn every_dialect_closes_the_session_and_exits_on_eof() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        let (_codec, harness, task) =
            spawn_agent(surface, dialect, vec![ScriptedAnswer::text("unused")], &dirs);
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/close", json!({ "sessionId": session_id })));
        assert_eq!(
            result_of(harness.next_frame().await),
            json!({}),
            "{surface:?}/{dialect:?} closes cleanly"
        );

        harness.shutdown(task).await;
    }
}

/// Reads every durable state file under `directory` as text.
fn read_state_files(directory: &std::path::Path) -> String {
    let mut text = String::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for path in directory_entries(&directory) {
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(contents) = fs::read_to_string(&path) {
                text.push_str(&contents);
            }
        }
    }
    text
}

#[tokio::test]
async fn a_live_turn_never_leaks_the_configured_key_into_frames_or_state() {
    let secret = "sk-opencode-release-gate-secret";

    for (surface, dialect) in DIALECT_MATRIX {
        let label = format!("{surface:?}/{dialect:?}");
        let dirs = TestDirs::new();
        let mut config = test_config(surface, route_for(surface, dialect).model_id);
        config.api_key = Some(secret.to_string());
        let (codec, harness, task) = spawn_agent_with_config(
            config,
            vec![
                ScriptedAnswer::reasoning("plan", "answer"),
                ScriptedAnswer::Failure(String::from("OpenCode request failed: HTTP 500")),
            ],
            &dirs,
        );
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
        drain_prompt_start(&harness).await;
        let (updates, result) = harness.collect_until_response(3).await;
        assert_eq!(result["stopReason"], "end_turn", "{label}");

        let frames = updates.iter().map(Value::to_string).collect::<String>();
        let requests = codec
            .requests()
            .iter()
            .map(ee_opencode_agent::adapter::test_support::RecordedRequest::json)
            .collect::<Vec<_>>();
        let transcripts = Value::Array(requests).to_string();
        let state = read_state_files(&dirs.state());

        assert!(!frames.contains(secret), "{label} update payloads must not carry the key");
        assert!(!result.to_string().contains(secret), "{label} prompt result must not carry it");
        assert!(!transcripts.contains(secret), "{label} transcripts must not carry it");
        assert!(!state.contains(secret), "{label} durable session state must not carry it");

        // The failing path is the one that most often leaks: a provider error
        // must name the failure, never the credential.
        harness.send(request(4, "session/prompt", prompt_params(&session_id, "again")));
        drain_prompt_start(&harness).await;
        let (updates, error) = harness.collect_until_error(4).await;

        assert!(error.message.contains("HTTP 500"), "{label}: {}", error.message);
        assert!(!error.message.contains(secret), "{label} error text must not carry the key");
        let frames = updates.iter().map(Value::to_string).collect::<String>();
        assert!(!frames.contains(secret), "{label} failure updates must not carry the key");
        assert!(!read_state_files(&dirs.state()).contains(secret), "{label} checkpoints too");

        harness.shutdown(task).await;
    }
}

// ── Critic (rubber-duck) second opinion ──────────────────────────────────

/// Serialized clean critique report the critic model must return.
fn clean_critique_report() -> String {
    let report = CritiqueReport::clean(CritiqueTarget::Implementation);
    serde_json::to_string(&report).expect("clean report serializes")
}

#[tokio::test]
async fn rubber_duck_second_opinion_runs_the_configured_critic() {
    for (root_model, critic_model) in [("gpt-5.5", "kimi-k3"), ("qwen3.7-max", "kimi-k3")] {
        let surface = OpenCodeSurface::Zen;
        let label = format!("{root_model} + {critic_model}");
        let dirs = TestDirs::new();
        let root_route = resolve_route(surface, root_model).expect("documented root route");
        let critic_route = resolve_route(surface, critic_model).expect("documented critic route");
        assert_ne!(root_route.dialect, critic_route.dialect, "{label} spans two dialects");

        let root_codec = Arc::new(ScriptedCodec::new(
            root_route.dialect,
            vec![ScriptedAnswer::text("synthesis after critique")],
        ));
        let critic_codec = Arc::new(ScriptedCodec::new(
            critic_route.dialect,
            vec![ScriptedAnswer::text(&clean_critique_report())],
        ));
        let mut config = test_config(surface, root_model);
        config.critic_model = Some(critic_model.to_string());

        let (provider, warning) = opencode_multi_model_provider_with_codecs(
            &config,
            dirs.state(),
            (root_route, root_codec.clone() as Arc<dyn ee_opencode_agent::adapter::DialectCodec>),
            Some((
                critic_route,
                critic_codec.clone() as Arc<dyn ee_opencode_agent::adapter::DialectCodec>,
            )),
        )
        .expect("provider builds with a critic");
        assert!(warning.is_none(), "{label}: {warning:?}");

        let (harness, task) = spawn_provider(provider, &dirs);
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "/rubber-duck")));
        let (updates, result) = harness.collect_until_response(3).await;

        assert_eq!(result["stopReason"], "end_turn", "{label}");
        let selection = updates
            .iter()
            .find(|params| params["update"]["messageId"] == "rubber-duck-selected")
            .unwrap_or_else(|| panic!("{label} reports the selected critic: {updates:?}"));
        // The runtime reports registry identities: the critic is the
        // `rubber_duck` route, and the root is the `default` route.
        assert!(
            selection["update"]["content"]["text"].as_str().is_some_and(|text| {
                text.contains("rubber_duck") && text.contains("root default")
            }),
            "{label} reports the selected critic route: {selection}"
        );
        assert!(
            has_text_chunk(&updates, "agent_message_chunk", "synthesis after critique"),
            "{label} streams the root synthesis"
        );

        // The critic ran exactly once, on its own route and dialect, and the
        // root then synthesized once.
        assert_eq!(critic_codec.requests().len(), 1, "{label} critic call");
        assert_eq!(root_codec.requests().len(), 1, "{label} root synthesis call");
        let critic_request = critic_codec.requests()[0].json().to_string();
        assert!(
            critic_request.contains("rubber-duck critic"),
            "{label} critic received the critique contract: {critic_request}"
        );
        let root_request = root_codec.requests()[0].json().to_string();
        assert!(
            root_request.contains("read it") || root_request.contains("final decision owner"),
            "{label} root received the synthesis contract: {root_request}"
        );

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn rubber_duck_without_a_critic_reports_contrast_unavailable() {
    let dirs = TestDirs::new();
    let (_codec, harness, task) = spawn_agent(
        OpenCodeSurface::Zen,
        OpenCodeDialect::OpenAiResponses,
        vec![ScriptedAnswer::text("unused")],
        &dirs,
    );
    initialize(&harness).await;
    let session_id = new_session(&harness, 2).await;

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "/rubber-duck")));
    let (updates, result) = harness.collect_until_response(3).await;

    assert_eq!(result["stopReason"], "end_turn");
    let reported = updates
        .iter()
        .find(|params| params["update"]["messageId"] == "rubber-duck-result")
        .unwrap_or_else(|| {
            panic!("root-only agent explains why contrast is unavailable: {updates:?}")
        });
    assert!(
        reported["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("rubber duck skipped")),
        "the root-only agent explains why the critic is unavailable: {reported}"
    );

    harness.shutdown(task).await;
}
