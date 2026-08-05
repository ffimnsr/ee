//! Reusable ACP v1 **agent-side** server framework.
//!
//! Provider binaries embed this crate to serve ACP v1 over stdio or
//! in-memory transports instead of handrolling JSON-RPC loops.  This crate
//! is the counterpart of `ee-agent-host`, which stays the editor/client-side
//! host only; this crate never depends on `ee-agent-host`.
//!
//! All ACP wire types come from [`ee_agent_protocol`] re-exports (backed by
//! the official `agent-client-protocol` SDK).  This crate defines no
//! ee-owned ACP wire structs; the provider-facing API ([`provider`]) and
//! session store ([`session`]) are framework boundaries, not wire types.
//!
//! Phase 1 scope: crate skeleton, shared config and errors, ID generation,
//! and the transport boundary ([`config`], [`error`], [`ids`],
//! [`transport`]).
//!
//! Phase 2 scope: provider trait ([`provider`]), session store ([`session`]),
//! server runtime ([`server`]) and request dispatch ([`dispatch`]) for
//! `initialize` and the session lifecycle (`session/new`, `session/load`,
//! `session/list`, `session/close`).
//!
//! Phase 3 scope: prompt execution (`session/prompt`), typed update
//! emission ([`updates`]), and cancellation (`session/cancel`, active-prompt
//! tracking in [`server`]).
//!
//! Phase 4 scope: the agent → client request bridge ([`client`]) — typed
//! fs/terminal/elicitation methods with framework-owned request ids,
//! response correlation, timeouts, and cleanup.
//!
//! Phase 5 scope: protocol validation ([`validate`]), dispatch hardening
//! (parse errors answered per JSON-RPC, provider output rejected when
//! malformed), transport-lifecycle hardening (writer path closed, active
//! prompts cancelled, pending requests failed on transport close), and
//! conformance fixtures.
//!
//! # Example: a minimal provider
//!
//! Providers implement [`AgentProvider`]; the framework owns JSON-RPC
//! dispatch, version negotiation, the session store, updates, and agent →
//! client requests.  This example — compile-tested by `cargo test --doc` —
//! echoes prompt text back through the update sink:
//!
//! ```
//! use std::sync::Mutex;
//!
//! use ee_acp_agent_server::{
//!     AcpAgentServer, AcpAgentServerConfig, AgentProvider, ClientBridge,
//!     LoadSessionContext, NewSessionContext, PromptContext, PromptResult, ProviderError,
//!     ProviderFuture, SessionInit, UpdateSink,
//! };
//! use ee_agent_protocol::{
//!     AgentCapabilities, ContentBlock, Implementation, PromptResponse, SessionId, StopReason,
//! };
//! use tokio::sync::watch;
//!
//! /// Minimal provider: echoes the prompt's text blocks back as one message
//! /// chunk.
//! struct EchoProvider {
//!     next_session: Mutex<u64>,
//! }
//!
//! impl AgentProvider for EchoProvider {
//!     fn info(&self) -> Implementation {
//!         Implementation::new("doc-echo", env!("CARGO_PKG_VERSION")).title("Doc Echo")
//!     }
//!
//!     fn capabilities(&self) -> AgentCapabilities {
//!         AgentCapabilities::default()
//!     }
//!
//!     fn new_session(
//!         &self,
//!         _ctx: NewSessionContext,
//!     ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
//!         let mut next = self.next_session.lock().expect("echo session counter poisoned");
//!         let session_id = SessionId::new(format!("doc-echo-{}", *next));
//!         *next += 1;
//!         Box::pin(async move { Ok(SessionInit::new(session_id)) })
//!     }
//!
//!     fn load_session(
//!         &self,
//!         ctx: LoadSessionContext,
//!     ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
//!         let session_id = ctx.session_id.clone();
//!         Box::pin(async move { Ok(SessionInit::new(session_id)) })
//!     }
//!
//!     fn prompt(
//!         &self,
//!         ctx: PromptContext,
//!         sink: UpdateSink,
//!         _client: ClientBridge,
//!         cancel: watch::Receiver<bool>,
//!     ) -> ProviderFuture<Result<PromptResult, ProviderError>> {
//!         Box::pin(async move {
//!             let text: String = ctx
//!                 .prompt
//!                 .iter()
//!                 .filter_map(|block| match block {
//!                     ContentBlock::Text(text) => Some(text.text.clone()),
//!                     _ => None,
//!                 })
//!                 .collect::<Vec<_>>()
//!                 .join(" ");
//!             if *cancel.borrow() {
//!                 return Err(ProviderError::Cancellation);
//!             }
//!             sink.agent_message_chunk("echo", text).map_err(|error| {
//!                 ProviderError::BackendFailure(format!("failed to emit echo update: {error}"))
//!             })?;
//!             Ok(PromptResponse::new(StopReason::EndTurn))
//!         })
//!     }
//!
//!     fn cancel_session(&self, _session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
//!         Box::pin(async { Ok(()) })
//!     }
//!
//!     fn close_session(&self, _session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>> {
//!         Box::pin(async { Ok(()) })
//!     }
//! }
//!
//! let provider = EchoProvider { next_session: Mutex::new(1) };
//! // The server is ready; a real binary drives it with `run_stdio().await`.
//! let _server = AcpAgentServer::new(provider, AcpAgentServerConfig::default());
//! ```
//!
//! # Running over stdio
//!
//! A binary only needs to build the provider and run the server over the
//! stdio transport; `no_run` because it blocks on stdin:
//!
//! ```no_run
//! use ee_acp_agent_server::{
//!     AcpAgentServer, AcpAgentServerConfig, AgentProvider, ClientBridge, LoadSessionContext,
//!     NewSessionContext, PromptContext, PromptResult, ProviderError, ProviderFuture,
//!     SessionInit, UpdateSink,
//! };
//! use ee_agent_protocol::{Implementation, SessionId};
//! use tokio::sync::watch;
//!
//! # struct Stub;
//! # impl AgentProvider for Stub {
//! #     fn info(&self) -> Implementation { todo!() }
//! #     fn capabilities(&self) -> ee_agent_protocol::AgentCapabilities { todo!() }
//! #     fn new_session(&self, _: NewSessionContext) -> ProviderFuture<Result<SessionInit, ProviderError>> { todo!() }
//! #     fn load_session(&self, _: LoadSessionContext) -> ProviderFuture<Result<SessionInit, ProviderError>> { todo!() }
//! #     fn prompt(&self, _: PromptContext, _: UpdateSink, _: ClientBridge, _: watch::Receiver<bool>) -> ProviderFuture<Result<PromptResult, ProviderError>> { todo!() }
//! #     fn cancel_session(&self, _: SessionId) -> ProviderFuture<Result<(), ProviderError>> { todo!() }
//! #     fn close_session(&self, _: SessionId) -> ProviderFuture<Result<(), ProviderError>> { todo!() }
//! # }
//! # fn provider() -> impl AgentProvider { Stub }
//! # async fn run() {
//! let server = AcpAgentServer::new(provider(), AcpAgentServerConfig::default());
//! server.run_stdio().await.expect("stdio transport serves");
//! # }
//! ```
//!
//! A complete runnable provider with integration tests lives in
//! `examples/echo_agent.rs`; its tests run under
//! `cargo test -p ee-acp-agent-server --examples`.

pub mod client;
pub mod config;
pub mod dispatch;
pub mod error;
pub mod ids;
pub mod provider;
pub mod server;
pub mod session;
pub mod transport;
pub mod updates;
pub mod validate;

// ── Primary public types ────────────────────────────────────────────────

pub use client::ClientBridge;
pub use config::AcpAgentServerConfig;
pub use error::{AcpServerError, ProviderError};
pub use ids::{RequestIdGenerator, SessionIdGenerator};
pub use provider::{
    AgentProvider, LoadSessionContext, NewSessionContext, PromptContext, PromptResult,
    ProviderFuture, SessionInit,
};
pub use server::AcpAgentServer;
pub use session::{ServerSession, SessionStore, SessionStoreError};
pub use transport::{AcpTransport, JsonRpcFrame, StdioTransport};
#[cfg(feature = "test-utils")]
pub use transport::{MemoryTransport, MemoryTransportHandle};
pub use updates::{UpdateSink, UpdateSinkError};

// ── Workspace guardrails ─────────────────────────────────────────────────

#[cfg(test)]
mod compile_checks {
    use crate::PromptResult;
    use ee_agent_protocol::{
        ACP_PROTOCOL_VERSION, AvailableCommandsUpdate, PromptResponse, ProtocolVersion,
        ReadTextFileRequest, ReadTextFileResponse, SessionId, SessionUpdate, StopReason,
    };

    /// Compiles only when both arguments have exactly the same type; used to
    /// prove the public API exposes the SDK's own types, not lookalikes.
    fn assert_same_type<T>(_: &T, _: &T) {}

    #[test]
    fn public_session_id_is_the_sdk_type() {
        let ours: SessionId = SessionId::new("s-1");
        let sdk: ee_agent_protocol::SessionId = ee_agent_protocol::SessionId::new("s-1");
        assert_same_type(&ours, &sdk);
    }

    #[test]
    fn update_sink_uses_the_sdk_session_update_type() {
        let update = AvailableCommandsUpdate::new(Vec::new());
        let ours: SessionUpdate = SessionUpdate::AvailableCommandsUpdate(update.clone());
        let sdk: ee_agent_protocol::SessionUpdate =
            ee_agent_protocol::SessionUpdate::AvailableCommandsUpdate(update);
        assert_same_type(&ours, &sdk);
    }

    #[test]
    fn client_bridge_uses_sdk_request_and_response_types() {
        let request: ReadTextFileRequest = ReadTextFileRequest::new("s-1", "/tmp/a.txt");
        let sdk_request: ee_agent_protocol::ReadTextFileRequest =
            ee_agent_protocol::ReadTextFileRequest::new("s-1", "/tmp/a.txt");
        assert_same_type(&request, &sdk_request);

        let response: ReadTextFileResponse = ReadTextFileResponse::new("contents");
        let sdk_response: ee_agent_protocol::ReadTextFileResponse =
            ee_agent_protocol::ReadTextFileResponse::new("contents");
        assert_same_type(&response, &sdk_response);
    }

    #[test]
    fn prompt_result_is_the_sdk_prompt_response() {
        let ours: PromptResult = PromptResult::new(StopReason::EndTurn);
        let sdk: PromptResponse = PromptResponse::new(StopReason::EndTurn);
        assert_same_type(&ours, &sdk);
    }

    #[test]
    fn framework_supports_exactly_the_protocol_crates_version() {
        // The dispatcher negotiates ACP v1; the protocol facade's supported
        // version must stay in lockstep.
        assert_eq!(ProtocolVersion::V1, ACP_PROTOCOL_VERSION);
        assert!(ee_agent_protocol::protocol_version_supported(ProtocolVersion::V1));
    }

    #[test]
    fn framework_depends_on_protocol_but_never_on_the_host() {
        // Boundary guardrail (Phase 1 rule): the agent-side framework must
        // stay host-free; a future refactor that pulls in `ee-agent-host`
        // fails here.
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("framework manifest readable");
        // Dependency keys appear as `name.workspace = ...` or `name = ...`;
        // comments may mention the host crate and must not count.
        let has_dependency = |name: &str| {
            manifest.lines().any(|line| {
                let line = line.trim_start();
                line == name
                    || line.starts_with(&format!("{name}."))
                    || line.starts_with(&format!("{name} ="))
                    || line.starts_with(&format!("{name} "))
            })
        };
        assert!(has_dependency("ee-agent-protocol"), "framework must depend on ee-agent-protocol");
        assert!(!has_dependency("ee-agent-host"), "framework must not depend on ee-agent-host");
    }
}
