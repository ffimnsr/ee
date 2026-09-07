//! Provider-adapter tests: shared fakes, harness, and plumbing.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ee_acp_agent_server::{
    AcpAgentServer, AcpAgentServerConfig, AcpServerError, MemoryTransport, MemoryTransportHandle,
};
use ee_agent_protocol::{
    Error as RpcError, RawJsonRpcMessage, RawJsonRpcParams, RequestId, Response,
};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::*;
use crate::config::RecoveryConfig;
use crate::model::{ModelAdapter, ModelError, ModelFuture, ModelRequest, ModelResponse, ModelRole};
use crate::test_support::FakeModel;

fn plan_response(text: &str) -> String {
    format!(
        "{text}\n\n## Plan\n1. Inspect implementation\n\n## Validation\n- Review the affected code paths.\n\n## Open questions\n- None\n\n{PLAN_PAYLOAD}"
    )
}
#[derive(Clone)]
struct DelayedModel {
    delays: Arc<Mutex<VecDeque<std::time::Duration>>>,
    default_delay: std::time::Duration,
    inner: FakeModel,
}

impl DelayedModel {
    fn new(
        hang_first: std::time::Duration,
        default_delay: std::time::Duration,
        inner: FakeModel,
    ) -> Self {
        let mut delays = VecDeque::new();
        delays.push_back(hang_first);
        Self { delays: Arc::new(Mutex::new(delays)), default_delay, inner }
    }
}

impl ModelAdapter for DelayedModel {
    fn complete(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let delay =
            self.delays.lock().expect("delays poisoned").pop_front().unwrap_or(self.default_delay);
        let inner = self.inner.clone();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            inner.complete(request, cancel).await
        })
    }
}
fn recovery_provider(model: Arc<DelayedModel>, auto_resume_max: u32) -> OrchestratorProvider {
    let mut recovery = RecoveryConfig::memory_only();
    recovery.auto_resume_max = auto_resume_max;
    OrchestratorProvider::with_policy(
        OrchestratorProviderConfig {
            orchestrator: OrchestratorConfig {
                turn_timeout: std::time::Duration::from_millis(500),
                recovery,
                // Scripted text-only responses make no task-graph
                // progress; disable the no-progress rule for the test.
                stuck: crate::stuck::StuckConfig {
                    max_no_progress_iterations: 100,
                    ..crate::stuck::StuckConfig::default()
                },
                ..OrchestratorConfig::default()
            },
            ..OrchestratorProviderConfig::default()
        },
        model,
        PolicyEngine::default(),
    )
}
fn resume_script() -> FakeModel {
    let mut responses = Vec::new();
    for index in 0..12 {
        let response = if index == 11 {
            ModelResponse::new().text("done").completed()
        } else {
            ModelResponse::new().text(format!("step {index}"))
        };
        responses.push(response);
    }
    FakeModel::new(responses)
}
fn durable_recovery_provider(
    model: Arc<DelayedModel>,
    dir: &std::path::Path,
) -> OrchestratorProvider {
    let mut recovery = RecoveryConfig::durable(dir.to_path_buf());
    recovery.auto_resume_max = 0;
    OrchestratorProvider::with_policy(
        OrchestratorProviderConfig {
            orchestrator: OrchestratorConfig {
                turn_timeout: std::time::Duration::from_millis(500),
                recovery,
                stuck: crate::stuck::StuckConfig {
                    max_no_progress_iterations: 100,
                    ..crate::stuck::StuckConfig::default()
                },
                ..OrchestratorConfig::default()
            },
            ..OrchestratorProviderConfig::default()
        },
        model,
        PolicyEngine::default(),
    )
}
async fn next_response_frame(handle: &Harness) -> RawJsonRpcMessage {
    loop {
        let frame = handle.next_frame_real().await;
        if let RawJsonRpcMessage::Response(_) = &frame {
            return frame;
        }
    }
}
async fn next_response_with_updates(handle: &Harness) -> (RawJsonRpcMessage, Vec<Value>) {
    let mut updates = Vec::new();
    loop {
        let frame = handle.next_frame_real().await;
        match &frame {
            RawJsonRpcMessage::Notification(update) => {
                updates.push(raw_params_to_value(update.params.clone()));
            }
            RawJsonRpcMessage::Response(_) => return (frame, updates),
            _ => {}
        }
    }
}
/// inbound frames, read outbound frames in order.
struct Harness {
    handle: MemoryTransportHandle,
    pending: Arc<Mutex<VecDeque<RawJsonRpcMessage>>>,
}

impl Harness {
    fn new(handle: MemoryTransportHandle) -> Self {
        Self { handle, pending: Arc::new(Mutex::new(VecDeque::new())) }
    }

    fn send(&self, frame: RawJsonRpcMessage) -> bool {
        self.handle.send(frame)
    }

    async fn next_frame(&self) -> RawJsonRpcMessage {
        self.next_frames(1).await.remove(0)
    }

    /// Real-time frame poll: parks on a timer between polls so spawned
    /// server tasks are never starved by a busy yield loop.  Overflow
    /// frames stay queued for the next call (never dropped).
    async fn next_frame_real(&self) -> RawJsonRpcMessage {
        loop {
            let ready = {
                let mut pending = self.pending.lock().expect("harness pending poisoned");
                if pending.is_empty() {
                    pending.extend(self.handle.take_outbound());
                }
                pending.pop_front()
            };
            if let Some(frame) = ready {
                return frame;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Waits for exactly `count` outbound frames, keeping overflow
    /// queued for the next call.
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

    async fn shutdown(self, task: tokio::task::JoinHandle<Result<(), AcpServerError>>) {
        drop(self.handle);
        task.await.expect("server task joins").expect("server exits cleanly on EOF");
    }

    /// Snapshot of outbound frames not yet consumed by the harness.
    fn outbound(&self) -> Vec<RawJsonRpcMessage> {
        self.handle.outbound()
    }
}
fn spawn_server(
    provider: OrchestratorProvider,
) -> (Harness, tokio::task::JoinHandle<Result<(), AcpServerError>>) {
    let server = AcpAgentServer::new(provider, AcpAgentServerConfig::default());
    let (transport, handle) = MemoryTransport::new();
    let task = tokio::spawn(async move { server.run_with_transport(transport).await });
    (Harness::new(handle), task)
}
fn request(id: i64, method: &str, params: Value) -> RawJsonRpcMessage {
    RawJsonRpcMessage::request(method.to_string(), params, RequestId::Number(id))
        .expect("test request builds")
}
fn notification(method: &str, params: Value) -> RawJsonRpcMessage {
    RawJsonRpcMessage::notification(method.to_string(), params).expect("test notification builds")
}
fn request_result(frame: RawJsonRpcMessage) -> Value {
    let Response::Result { result, .. } = unwrap_response(frame.clone()) else {
        panic!("expected a result response, got {frame:?}");
    };
    result
}
fn unwrap_response(frame: RawJsonRpcMessage) -> Response<Value> {
    let RawJsonRpcMessage::Response(response) = frame else {
        panic!("expected a response frame, got {frame:?}");
    };
    response
}
fn raw_params_to_value(params: Option<RawJsonRpcParams>) -> Value {
    match params {
        None => Value::Null,
        Some(RawJsonRpcParams::Object(map)) => Value::Object(map),
        Some(RawJsonRpcParams::Array(array)) => Value::Array(array),
    }
}
fn session_new_params(cwd: &str) -> Value {
    json!({
        "cwd": cwd,
        "additionalDirectories": [],
        "mcpServers": [],
    })
}
fn session_new_params_with_additional(cwd: &str, additional: &[&str]) -> Value {
    json!({
        "cwd": cwd,
        "additionalDirectories": additional,
        "mcpServers": [],
    })
}
fn prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
    })
}
async fn new_session(handle: &Harness, id: i64) -> String {
    handle.send(request(id, "session/new", session_new_params("/work")));
    let result = request_result(handle.next_frame().await);
    let session_id = result["sessionId"].as_str().expect("session id").to_string();
    // The provider advertises its initial slash commands after the
    // session/new response; drain the update before prompt flows.
    let frame = handle.next_frame().await;
    let RawJsonRpcMessage::Notification(update) = &frame else {
        panic!("expected the available_commands_update, got {frame:?}");
    };
    assert_eq!(
        raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
        "available_commands_update"
    );
    session_id
}
async fn drain_mcp_diagnostics(handle: &Harness) {
    loop {
        let frame = handle.next_frame().await;
        let RawJsonRpcMessage::Notification(update) = &frame else {
            panic!("expected an update while draining, got {frame:?}");
        };
        let params = raw_params_to_value(update.params.clone());
        assert_eq!(
            params["update"]["sessionUpdate"], "agent_thought_chunk",
            "only thought updates precede the plan"
        );
        if params["update"]["messageId"] == "mcp-diagnostics" {
            return;
        }
    }
}
async fn wait_until(condition: impl Fn() -> bool) {
    for _ in 0..10_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition never satisfied");
}
/// cancellation; proves framework cancellation reaches the running turn.
struct CancelAwaitingModel {
    calls: Arc<Mutex<usize>>,
}

impl ModelAdapter for CancelAwaitingModel {
    fn complete(
        &self,
        _request: ModelRequest,
        mut cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            *calls.lock().expect("calls poisoned") += 1;
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            let _ = cancel.changed().await;
            Err(ModelError::Cancelled)
        })
    }
}
fn request_error(frame: RawJsonRpcMessage) -> RpcError {
    let Response::Error { error, .. } = unwrap_response(frame) else {
        panic!("expected an error response");
    };
    error
}
const PLAN_PAYLOAD: &str = r#"<!-- ee-plan
[
  {
"title": "Inspect implementation",
"action": "inspect relevant implementation details",
"scope": "workspace",
"expected_result": "implementation approach is identified",
"verification": "review the affected code paths",
"depends_on": []
  }
]
-->"#;

mod lifecycle_tests;
mod mcp_tests;
mod session_tests;
