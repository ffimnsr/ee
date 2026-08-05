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
    AgentCapabilities, ContentBlock, Implementation, McpServer, Meta, SessionConfigOption,
    SessionId, SessionModeState,
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
        }
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
    /// Available command names for this session (the SDK has no wire field
    /// for these yet; carried for later phases).
    pub commands: Vec<String>,
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

    /// Sets the available command names for this session.
    #[must_use]
    pub fn commands(mut self, commands: Vec<String>) -> Self {
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

    /// Releases provider state for a cancelled session (prompt dispatch
    /// calls this when `session/cancel` arrives).
    fn cancel_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>>;

    /// Releases provider state for a closed session.
    fn close_session(&self, session_id: SessionId) -> ProviderFuture<Result<(), ProviderError>>;
}
