//! Provider-facing API.
//!
//! Providers implement [`AgentProvider`] to supply business logic; the
//! framework owns JSON-RPC dispatch, version negotiation, the session store,
//! and typed response shaping.  The trait is deliberately independent of
//! OpenRouter or any specific backend.
//!
//! Context structs carry validated request data; [`SessionInit`] and
//! [`PromptResult`] are the provider-side results the server converts into
//! SDK wire responses.  None of these are ACP wire structs — all wire types
//! stay SDK-backed via [`ee_agent_protocol`] re-exports.

use std::future::Future;
use std::pin::Pin;

use ee_agent_protocol::{
    AgentCapabilities, AvailableCommand, ContentBlock, Implementation, McpServer, Meta,
    SessionConfigOption, SessionId, SessionModeId, SessionModeState,
};
use tokio::sync::watch;

use crate::client::ClientBridge;
use crate::error::ProviderError;
use crate::updates::UpdateSink;

/// Boxed future returned by provider trait methods.
///
/// The box keeps the trait object-safe without depending on `async-trait`.
pub type ProviderFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Validated input for [`AgentProvider::new_session`].
///
/// Constructed by the framework; providers only read it.  Non-exhaustive so
/// future request metadata can be added without breaking providers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NewSessionContext {
    /// The session working directory.  Always absolute (validated before the
    /// provider is invoked).
    pub cwd: std::path::PathBuf,
    /// Additional workspace roots.  Always absolute (validated before the
    /// provider is invoked).
    pub additional_directories: Vec<std::path::PathBuf>,
    /// MCP servers the client asked the session to connect to.
    pub mcp_servers: Vec<McpServer>,
    /// Raw `_meta` from the `session/new` request, if present.
    pub metadata: Option<Meta>,
}

impl NewSessionContext {
    /// Creates a context with the given working directory and no additional
    /// directories, MCP servers, or metadata.
    #[must_use]
    pub fn new(cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            metadata: None,
        }
    }
}

/// Validated input for [`AgentProvider::load_session`].
///
/// Constructed by the framework; providers only read it.  Non-exhaustive so
/// future request metadata can be added without breaking providers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LoadSessionContext {
    /// The session id the client wants to load.
    pub session_id: SessionId,
    /// The session working directory.  Always absolute (validated before the
    /// provider is invoked).
    pub cwd: std::path::PathBuf,
    /// Additional workspace roots.  Always absolute (validated before the
    /// provider is invoked).
    pub additional_directories: Vec<std::path::PathBuf>,
    /// MCP servers the client asked the session to connect to.
    pub mcp_servers: Vec<McpServer>,
    /// Raw `_meta` from the `session/load` request, if present.
    pub metadata: Option<Meta>,
    /// Sink for ACP v1 conversation replay: the provider streams the whole
    /// conversation as `session/update` notifications before the load
    /// response is sent.  `session/resume` (which must NOT replay) leaves
    /// this unset.
    pub replay_sink: Option<UpdateSink>,
}

impl LoadSessionContext {
    /// Creates a context with the given session id and working directory and
    /// no additional directories, MCP servers, or metadata.
    #[must_use]
    pub fn new(session_id: impl Into<SessionId>, cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            session_id: session_id.into(),
            cwd: cwd.into(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            metadata: None,
            replay_sink: None,
        }
    }

    /// Sets the conversation-replay sink (ACP v1 `session/load`).
    #[must_use]
    pub fn with_replay_sink(mut self, sink: UpdateSink) -> Self {
        self.replay_sink = Some(sink);
        self
    }
}

/// Input for [`AgentProvider::prompt`] (prompt dispatch lands in a later
/// phase).
///
/// Constructed by the framework; providers only read it.  Non-exhaustive so
/// future prompt metadata can be added without breaking providers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PromptContext {
    /// The session receiving the prompt.
    pub session_id: SessionId,
    /// The content blocks composing the user's message.
    pub prompt: Vec<ContentBlock>,
    /// Raw `_meta` from the `session/prompt` request, if present.
    pub metadata: Option<Meta>,
}

impl PromptContext {
    /// Creates a context with the given session id and prompt content and no
    /// metadata.
    #[must_use]
    pub fn new(session_id: impl Into<SessionId>, prompt: Vec<ContentBlock>) -> Self {
        Self { session_id: session_id.into(), prompt, metadata: None }
    }
}

/// Validated input for [`AgentProvider::set_mode`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SetModeContext {
    /// Session receiving the selected mode.
    pub session_id: SessionId,
    /// Advertised mode identifier selected by the client.
    pub mode_id: SessionModeId,
    /// Raw `_meta` from the `session/set_mode` request, if present.
    pub metadata: Option<Meta>,
}

impl SetModeContext {
    /// Creates a context with the selected mode and no metadata.
    #[must_use]
    pub fn new(session_id: impl Into<SessionId>, mode_id: impl Into<SessionModeId>) -> Self {
        Self { session_id: session_id.into(), mode_id: mode_id.into(), metadata: None }
    }
}

/// Result of creating or loading a session.
///
/// The server converts this into the SDK `NewSessionResponse` /
/// `LoadSessionResponse` wire shape and registers the session in its store.
/// The SDK has no `SessionInit` wire struct, so this is a provider-contract
/// type, not a wire type.  Non-exhaustive so future session metadata can be
/// added without breaking providers; construct with [`SessionInit::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionInit {
    /// The resolved session id, used in all subsequent requests.
    pub session_id: SessionId,
    /// Human-readable session title (surfaced in `session/list`).
    pub title: Option<String>,
    /// Available commands for this session, advertised through an
    /// `available_commands_update` after the session is registered.
    pub commands: Vec<AvailableCommand>,
    /// Initial mode state, when the SDK advertises mode support.
    pub modes: Option<SessionModeState>,
    /// Initial session configuration options, when the SDK advertises them.
    pub config_options: Option<Vec<SessionConfigOption>>,
}

impl SessionInit {
    /// Builds a session init with the resolved session id.
    #[must_use]
    pub fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            title: None,
            commands: Vec::new(),
            modes: None,
            config_options: None,
        }
    }

    /// Sets the human-readable session title.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Sets the available commands for this session (advertised through
    /// `available_commands_update` after the session is registered).
    #[must_use]
    pub fn commands(mut self, commands: Vec<AvailableCommand>) -> Self {
        self.commands = commands;
        self
    }

    /// Sets the initial mode state.
    #[must_use]
    pub fn modes(mut self, modes: impl Into<Option<SessionModeState>>) -> Self {
        self.modes = modes.into();
        self
    }

    /// Sets the initial session configuration options.
    #[must_use]
    pub fn config_options(
        mut self,
        config_options: impl Into<Option<Vec<SessionConfigOption>>>,
    ) -> Self {
        self.config_options = config_options.into();
        self
    }
}

/// Result of one prompt turn.
///
/// The SDK's `PromptResponse` carries exactly the ACP stop reason and (when
/// enabled) usage metadata, so the provider returns it directly; the server
/// shapes the wire response in the prompt-dispatch phase.
pub type PromptResult = ee_agent_protocol::PromptResponse;

/// Business logic for one agent.
///
/// All methods are async and boxed ([`ProviderFuture`]); the framework
/// applies timeouts and converts failures into JSON-RPC errors.  Providers
/// must be `Send + Sync + 'static` so the server can move them across tasks.
pub trait AgentProvider: Send + Sync + 'static {
    /// Agent identity advertised in `initialize` responses.
    fn info(&self) -> Implementation;

    /// Agent capabilities advertised in `initialize` responses.
    fn capabilities(&self) -> AgentCapabilities;

    /// Creates a new session from validated request data.
    fn new_session(
        &self,
        ctx: NewSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>>;

    /// Loads an existing session from validated request data.
    fn load_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>>;

    /// Resumes an existing session without replaying the conversation (ACP
    /// v1 `session/resume`): restores the session context so the client can
    /// continue sending prompts.  Providers with no distinct resume path may
    /// rely on the default, which restores exactly like [`Self::load_session`]
    /// (minus replay).
    fn resume_session(
        &self,
        ctx: LoadSessionContext,
    ) -> ProviderFuture<Result<SessionInit, ProviderError>> {
        self.load_session(ctx)
    }

    /// Processes one prompt turn, streaming updates through `sink` and
    /// making agent → client requests through `client` (see
    /// [`ClientBridge`]); `cancel` flips when the session is closed or the
    /// prompt is cancelled.
    fn prompt(
        &self,
        ctx: PromptContext,
        sink: UpdateSink,
        client: ClientBridge,
        cancel: watch::Receiver<bool>,
    ) -> ProviderFuture<Result<PromptResult, ProviderError>>;

    /// Applies an advertised session mode. Providers that do not implement
    /// mode behavior reject mode changes rather than accepting inert state.
    fn set_mode(&self, _ctx: SetModeContext) -> ProviderFuture<Result<(), ProviderError>> {
        Box::pin(async {
            Err(ProviderError::InvalidRequest(
                "session/set_mode is not supported by this provider".to_string(),
            ))
        })
    }

    /// Releases provider state for a cancelled session (prompt dispatch
    /// calls this when `session/cancel` arrives).
    fn cancel_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>>;

    /// Releases provider state for a closed session.
    fn close_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>>;
}
