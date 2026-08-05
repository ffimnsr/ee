//! Echo agent: a minimal ACP provider that exercises the framework API
//! without any network or editor dependency.
//!
//! The provider echoes each prompt's text blocks back through
//! [`UpdateSink::agent_message_chunk`] and returns an end-turn stop reason.
//! Prompts observe the framework's cancellation token before emitting, and
//! `EchoProvider::blocking` (used by the tests) blocks until cancelled to
//! exercise cancellation handling.
//!
//! Run it over stdio:
//!
//! ```sh
//! cargo run -p ee-acp-agent-server --example echo_agent
//! ```
//!
//! The integration tests below run the same provider over the in-memory
//! transport:
//!
//! ```sh
//! cargo test -p ee-acp-agent-server --examples
//! ```

use std::sync::Mutex;

use ee_acp_agent_server::{
    AcpAgentServer, AcpAgentServerConfig, AgentProvider, ClientBridge, LoadSessionContext,
    NewSessionContext, PromptContext, PromptResult, ProviderError, ProviderFuture, SessionInit,
    UpdateSink,
};
use ee_agent_protocol::{
    AgentCapabilities, ContentBlock, Implementation, PromptResponse, SessionId, StopReason,
};
use tokio::sync::watch;

/// Echoes the prompt's text blocks back through the update sink.
struct EchoProvider {
    /// Session ids handed out so far, as `echo-N`.
    next_session: Mutex<u64>,
    /// When set, prompts block until cancelled (exercises cancellation
    /// without network calls).
    block_prompts: bool,
}

impl EchoProvider {
    /// A provider whose prompts return immediately.
    fn new() -> Self {
        Self { next_session: Mutex::new(1), block_prompts: false }
    }

    /// A provider whose prompts block until cancelled.
    #[cfg(test)]
    fn blocking() -> Self {
        Self { next_session: Mutex::new(1), block_prompts: true }
    }
}

impl Default for EchoProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentProvider for EchoProvider {
    fn info(&self) -> Implementation {
        Implementation::new("echo-agent", env!("CARGO_PKG_VERSION")).title("Echo Agent")
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities::default()
    }

    fn new_session(
        &self,
        _ctx: NewSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let mut next = self.next_session.lock().expect("echo session counter poisoned");
        let session_id = SessionId::new(format!("echo-{}", *next));
        *next += 1;
        Box::pin(async move { Ok(SessionInit::new(session_id)) })
    }

    fn load_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        let session_id = ctx.session_id.clone();
        Box::pin(async move { Ok(SessionInit::new(session_id)) })
    }

    fn prompt(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        _client: ClientBridge,
        mut cancel: watch::Receiver<bool>,
    ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
        let block = self.block_prompts;
        Box::pin(async move {
            let text = extract_text(&ctx.prompt);
            if text.is_empty() {
                return Err(ProviderError::InvalidRequest("prompt has no text content".into()));
            }
            if block {
                // Block until the framework flips the cancellation token.
                let _ = cancel.changed().await;
            }
            // Observe cancellation before emitting the final update.
            if *cancel.borrow() {
                return Err(ProviderError::Cancellation);
            }
            sink.agent_message_chunk("echo", text).map_err(|error| {
                ProviderError::BackendFailure(format!("failed to emit echo update: {error}"))
            })?;
            Ok(PromptResponse::new(StopReason::EndTurn))
        })
    }

    fn cancel_session(&self, _session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        Box::pin(async { Ok(()) })
    }

    fn close_session(&self, _session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Concatenates the prompt's text blocks, space-separated; non-text blocks
/// are ignored.
fn extract_text(prompt: &[ContentBlock]) -> String {
    prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let provider = EchoProvider::new();
    if let Err(error) =
        AcpAgentServer::new(provider, AcpAgentServerConfig::default()).run_stdio().await
    {
        eprintln!("echo-agent: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use ee_acp_agent_server::{AcpServerError, MemoryTransport, MemoryTransportHandle};
    use ee_agent_protocol::{RawJsonRpcMessage, RawJsonRpcParams, RequestId, Response};
    use serde_json::{Value, json};

    /// Non-destructive outbound frame pump: frames accumulate across polls,
    /// so frames arriving in separate batches are never lost.
    struct Frames {
        pending: VecDeque<RawJsonRpcMessage>,
    }

    impl Frames {
        fn new() -> Self {
            Self { pending: VecDeque::new() }
        }

        async fn next(&mut self, handle: &MemoryTransportHandle) -> RawJsonRpcMessage {
            for _ in 0..5_000 {
                if let Some(frame) = self.pending.pop_front() {
                    return frame;
                }
                self.pending.extend(handle.take_outbound());
                tokio::task::yield_now().await;
            }
            panic!("no outbound frame within budget; remaining={:?}", handle.outbound());
        }
    }

    fn request(id: i64, method: &str, params: Value) -> RawJsonRpcMessage {
        RawJsonRpcMessage::request(method.to_string(), params, RequestId::Number(id))
            .expect("test request builds")
    }

    fn notification(method: &str, params: Value) -> RawJsonRpcMessage {
        RawJsonRpcMessage::notification(method.to_string(), params)
            .expect("test notification builds")
    }

    fn request_result(frame: RawJsonRpcMessage) -> Value {
        let RawJsonRpcMessage::Response(Response::Result { result, .. }) = frame else {
            panic!("expected a result response, got {frame:?}");
        };
        result
    }

    /// Extracts the `update` value from a `session/update` notification.
    fn update_of(frame: &RawJsonRpcMessage) -> Value {
        let RawJsonRpcMessage::Notification(notification) = frame else {
            panic!("expected a session/update notification, got {frame:?}");
        };
        assert_eq!(notification.method.as_ref(), "session/update");
        let RawJsonRpcParams::Object(params) =
            notification.params.as_ref().expect("params present")
        else {
            panic!("expected object params");
        };
        params.get("update").expect("update").clone()
    }

    /// Spawns the echo provider over the in-memory transport.
    async fn spawn_echo(
        provider: EchoProvider,
    ) -> (MemoryTransportHandle, tokio::task::JoinHandle<Result<(), AcpServerError>>) {
        let server = AcpAgentServer::new(provider, AcpAgentServerConfig::default());
        let (transport, handle) = MemoryTransport::new();
        let task = tokio::spawn(async move { server.run_with_transport(transport).await });
        (handle, task)
    }

    /// Starts a session and returns its id.
    async fn new_session(handle: &MemoryTransportHandle, frames: &mut Frames, id: i64) -> String {
        handle.send(request(
            id,
            "session/new",
            json!({ "cwd": "/work", "additionalDirectories": [], "mcpServers": [] }),
        ));
        let result = request_result(frames.next(handle).await);
        result["sessionId"].as_str().expect("session id").to_string()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initialize_negotiates_acp_v1() {
        let (handle, task) = spawn_echo(EchoProvider::new()).await;
        let mut frames = Frames::new();

        handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));

        let result = request_result(frames.next(&handle).await);
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentInfo"]["name"], "echo-agent");
        assert_eq!(result["agentInfo"]["title"], "Echo Agent");

        drop(handle);
        task.await.expect("server task joins").expect("clean EOF shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_new_returns_echo_session_id() {
        let (handle, task) = spawn_echo(EchoProvider::new()).await;
        let mut frames = Frames::new();

        handle.send(request(
            1,
            "session/new",
            json!({ "cwd": "/work", "additionalDirectories": [], "mcpServers": [] }),
        ));

        let result = request_result(frames.next(&handle).await);
        assert_eq!(result["sessionId"], "echo-1");

        drop(handle);
        task.await.expect("server task joins").expect("clean EOF shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prompt_echoes_text_as_update() {
        let (handle, task) = spawn_echo(EchoProvider::new()).await;
        let mut frames = Frames::new();
        let session_id = new_session(&handle, &mut frames, 1).await;

        handle.send(request(
            2,
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    { "type": "text", "text": "hello" },
                    { "type": "text", "text": "world" },
                ],
            }),
        ));

        // Update first, then the end-turn response, in order.
        let update = update_of(&frames.next(&handle).await);
        assert_eq!(update["sessionUpdate"], "agent_message_chunk");
        assert_eq!(update["content"]["text"], "hello world");

        let result = request_result(frames.next(&handle).await);
        assert_eq!(result["stopReason"], "end_turn");

        drop(handle);
        task.await.expect("server task joins").expect("clean EOF shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_during_blocked_prompt_cleans_up_state() {
        let (handle, task) = spawn_echo(EchoProvider::blocking()).await;
        let mut frames = Frames::new();
        let session_id = new_session(&handle, &mut frames, 1).await;

        // The prompt blocks on the cancellation token; cancel it.
        handle.send(request(
            2,
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "hello" }],
            }),
        ));
        handle.send(notification("session/cancel", json!({ "sessionId": session_id })));

        // Cancellation resolves deterministically to the `cancelled` stop
        // reason, and no echo update was emitted.
        let result = request_result(frames.next(&handle).await);
        assert_eq!(result["stopReason"], "cancelled");
        assert!(handle.outbound().is_empty(), "no update after cancellation");

        // Active-prompt state was cleaned up: a second prompt is accepted.
        handle.send(request(
            3,
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "again" }],
            }),
        ));
        handle.send(notification("session/cancel", json!({ "sessionId": session_id })));
        let result = request_result(frames.next(&handle).await);
        assert_eq!(result["stopReason"], "cancelled");

        drop(handle);
        task.await.expect("server task joins").expect("clean EOF shutdown");
    }
}
