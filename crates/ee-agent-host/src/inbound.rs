//! Agent-to-client request surface: typed inbound requests and the handler
//! trait bridges (Phase 4) implement.
//!
//! The host dispatches every agent-to-client file, terminal, and elicitation
//! request to exactly one [`ClientRequestHandler`].  Handlers are UI-free and
//! run inside the connection task; they must never block the event loop
//! (slow work belongs in spawned tasks).  `session/request_permission` is not
//! part of this surface — it is answered by the host's permission broker
//! directly.
//!
//! Fail-closed rule: a handler that does not advertise a capability through
//! [`ClientRequestHandler::capabilities`] causes the host to answer the
//! request with a typed error before the handler is invoked.

use ee_agent_protocol::{
    CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};

use crate::error::AgentError;
use ee_agent_protocol::{
    ELICITATION_CREATE_METHOD_NAME, FS_READ_TEXT_FILE_METHOD_NAME, FS_WRITE_TEXT_FILE_METHOD_NAME,
    TERMINAL_CREATE_METHOD_NAME, TERMINAL_KILL_METHOD_NAME, TERMINAL_OUTPUT_METHOD_NAME,
    TERMINAL_RELEASE_METHOD_NAME, TERMINAL_WAIT_FOR_EXIT_METHOD_NAME,
};

/// Which client-side capabilities a handler implements.  The host advertises
/// exactly these in the ACP `initialize` request and rejects inbound
/// requests for unadvertised capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HandlerCapabilities {
    /// `fs/read_text_file`
    pub fs_read: bool,
    /// `fs/write_text_file`
    pub fs_write: bool,
    /// `terminal/create`, `terminal/output`, `terminal/wait_for_exit`,
    /// `terminal/kill`, `terminal/release`
    pub terminal: bool,
    /// `elicitation/create` with form mode
    pub elicitation_form: bool,
    /// `elicitation/create` with url mode
    pub elicitation_url: bool,
}

impl HandlerCapabilities {
    /// No capabilities (default handler; every inbound request fails closed).
    #[must_use]
    pub const fn none() -> Self {
        Self {
            fs_read: false,
            fs_write: false,
            terminal: false,
            elicitation_form: false,
            elicitation_url: false,
        }
    }

    /// All file, terminal, and elicitation capabilities (used by tests).
    #[must_use]
    pub const fn all() -> Self {
        Self {
            fs_read: true,
            fs_write: true,
            terminal: true,
            elicitation_form: true,
            elicitation_url: true,
        }
    }

    /// Whether this capability set covers `method`.
    #[must_use]
    pub fn supports(&self, method: &str) -> bool {
        match method {
            FS_READ_TEXT_FILE_METHOD_NAME => self.fs_read,
            FS_WRITE_TEXT_FILE_METHOD_NAME => self.fs_write,
            TERMINAL_CREATE_METHOD_NAME
            | TERMINAL_OUTPUT_METHOD_NAME
            | TERMINAL_WAIT_FOR_EXIT_METHOD_NAME
            | TERMINAL_KILL_METHOD_NAME
            | TERMINAL_RELEASE_METHOD_NAME => self.terminal,
            ELICITATION_CREATE_METHOD_NAME => {
                // Method-level gate is insufficient for elicitation modes; the
                // handler still validates the mode (see handler contract).
                self.elicitation_form || self.elicitation_url
            }
            _ => false,
        }
    }
}

/// One agent-to-client request the host forwards to the handler.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientRequest {
    ReadTextFile(ReadTextFileRequest),
    WriteTextFile(WriteTextFileRequest),
    CreateTerminal(CreateTerminalRequest),
    TerminalOutput(TerminalOutputRequest),
    WaitForTerminalExit(WaitForTerminalExitRequest),
    KillTerminal(KillTerminalRequest),
    ReleaseTerminal(ReleaseTerminalRequest),
    CreateElicitation(CreateElicitationRequest),
}

impl ClientRequest {
    /// The JSON-RPC method name of this request.
    #[must_use]
    pub fn method(&self) -> &'static str {
        match self {
            Self::ReadTextFile(_) => FS_READ_TEXT_FILE_METHOD_NAME,
            Self::WriteTextFile(_) => FS_WRITE_TEXT_FILE_METHOD_NAME,
            Self::CreateTerminal(_) => TERMINAL_CREATE_METHOD_NAME,
            Self::TerminalOutput(_) => TERMINAL_OUTPUT_METHOD_NAME,
            Self::WaitForTerminalExit(_) => TERMINAL_WAIT_FOR_EXIT_METHOD_NAME,
            Self::KillTerminal(_) => TERMINAL_KILL_METHOD_NAME,
            Self::ReleaseTerminal(_) => TERMINAL_RELEASE_METHOD_NAME,
            Self::CreateElicitation(_) => ELICITATION_CREATE_METHOD_NAME,
        }
    }

    /// The session this request targets, when it is session-scoped.
    ///
    /// Elicitation requests may be request-scoped (outside any session).
    #[must_use]
    pub fn session_id(&self) -> Option<&ee_agent_protocol::SessionId> {
        match self {
            Self::ReadTextFile(request) => Some(&request.session_id),
            Self::WriteTextFile(request) => Some(&request.session_id),
            Self::CreateTerminal(request) => Some(&request.session_id),
            Self::TerminalOutput(request) => Some(&request.session_id),
            Self::WaitForTerminalExit(request) => Some(&request.session_id),
            Self::KillTerminal(request) => Some(&request.session_id),
            Self::ReleaseTerminal(request) => Some(&request.session_id),
            Self::CreateElicitation(request) => match &request.mode {
                ee_agent_protocol::ElicitationMode::Form(mode) => match &mode.scope {
                    ee_agent_protocol::ElicitationScope::Session(scope) => Some(&scope.session_id),
                    ee_agent_protocol::ElicitationScope::Request(_) => None,
                    _ => None,
                },
                ee_agent_protocol::ElicitationMode::Url(mode) => match &mode.scope {
                    ee_agent_protocol::ElicitationScope::Session(scope) => Some(&scope.session_id),
                    ee_agent_protocol::ElicitationScope::Request(_) => None,
                    _ => None,
                },
                // Non-exhaustive upstream; unknown modes carry no session.
                _ => None,
            },
        }
    }
}

/// The typed response for a handled [`ClientRequest`].
#[derive(Debug, Clone, PartialEq)]
pub enum ClientRequestResponse {
    ReadTextFile(ReadTextFileResponse),
    WriteTextFile(WriteTextFileResponse),
    CreateTerminal(CreateTerminalResponse),
    TerminalOutput(TerminalOutputResponse),
    WaitForTerminalExit(WaitForTerminalExitResponse),
    KillTerminal(KillTerminalResponse),
    ReleaseTerminal(ReleaseTerminalResponse),
    CreateElicitation(CreateElicitationResponse),
}

impl ClientRequestResponse {
    /// Serializes the response payload into its wire shape (without the
    /// enum tag).
    pub fn into_value(self) -> Result<serde_json::Value, serde_json::Error> {
        match self {
            Self::ReadTextFile(response) => serde_json::to_value(response),
            Self::WriteTextFile(response) => serde_json::to_value(response),
            Self::CreateTerminal(response) => serde_json::to_value(response),
            Self::TerminalOutput(response) => serde_json::to_value(response),
            Self::WaitForTerminalExit(response) => serde_json::to_value(response),
            Self::KillTerminal(response) => serde_json::to_value(response),
            Self::ReleaseTerminal(response) => serde_json::to_value(response),
            Self::CreateElicitation(response) => serde_json::to_value(response),
        }
    }
}

/// Result of handling one inbound request.
pub type ClientRequestResult = std::result::Result<ClientRequestResponse, AgentError>;

/// Handles agent-to-client file, terminal, and elicitation requests.
///
/// Implementations are provided by the editor integration (Phase 4 bridges)
/// and must enforce approvals through the permission broker before any file
/// write or terminal execution.
///
/// Implementers return a boxed future (the `async_trait` pattern): the host
/// stores the handler behind `Arc<dyn ClientRequestHandler>`, which rules
/// out async-fn-in-trait syntax.
pub trait ClientRequestHandler: Send + Sync + 'static {
    /// The capabilities this handler implements; advertised during
    /// `initialize`.
    fn capabilities(&self) -> HandlerCapabilities;

    /// Handles one inbound request.
    ///
    /// Must return the response matching the request variant; returning a
    /// different variant is a host-side programming error and becomes an
    /// internal error response.  Handlers may be slow (I/O, approvals) but
    /// should spawn long work rather than blocking the connection task.
    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>>;
}

/// Default handler: advertises nothing and denies everything.
///
/// Used when the host is built without a bridge (fail closed).
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAllHandler;

impl ClientRequestHandler for DenyAllHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        HandlerCapabilities::none()
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            Err(AgentError::CapabilityUnsupported { method: request.method().to_string() })
        })
    }
}

/// Records inbound requests for tests and audits without executing them.
#[derive(Debug, Clone, Default)]
pub struct RecordingHandler {
    pub capabilities: HandlerCapabilities,
    pub seen: std::sync::Arc<std::sync::Mutex<Vec<ClientRequest>>>,
}

impl RecordingHandler {
    /// Creates a handler that records every request.
    #[must_use]
    pub fn new(capabilities: HandlerCapabilities) -> Self {
        Self { capabilities, seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())) }
    }

    /// The requests received so far, in order.
    #[must_use]
    pub fn seen(&self) -> Vec<ClientRequest> {
        self.seen.lock().expect("recording handler poisoned").clone()
    }
}

impl ClientRequestHandler for RecordingHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        self.capabilities
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            self.seen.lock().expect("recording handler poisoned").push(request.clone());
            Err(AgentError::PermissionDenied { reason: "recording handler denies".into() })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::{SessionId, TerminalId};

    #[test]
    fn capabilities_supports_maps_methods() {
        let caps = HandlerCapabilities::all();
        for method in [
            "fs/read_text_file",
            "fs/write_text_file",
            "terminal/create",
            "terminal/output",
            "terminal/wait_for_exit",
            "terminal/kill",
            "terminal/release",
            "elicitation/create",
        ] {
            assert!(caps.supports(method), "{method} should be supported");
        }
        assert!(!caps.supports("session/request_permission"));
        assert!(!HandlerCapabilities::none().supports("fs/read_text_file"));
    }

    #[test]
    fn request_method_names_match_wire() {
        let session = SessionId::new("s1");
        let read = ClientRequest::ReadTextFile(ReadTextFileRequest::new(session.clone(), "/tmp/x"));
        assert_eq!(read.method(), "fs/read_text_file");
        assert_eq!(read.session_id(), Some(&session));

        let terminal =
            ClientRequest::CreateTerminal(CreateTerminalRequest::new(session.clone(), "echo"));
        assert_eq!(terminal.method(), "terminal/create");
        assert_eq!(terminal.session_id(), Some(&session));
    }

    #[tokio::test]
    async fn recording_handler_denies_and_records() {
        let handler = RecordingHandler::new(HandlerCapabilities::all());
        let request = ClientRequest::KillTerminal(KillTerminalRequest::new(
            SessionId::new("s1"),
            TerminalId::new("t1"),
        ));
        let result = handler.handle(request.clone()).await;
        assert_eq!(
            result,
            Err(AgentError::PermissionDenied { reason: "recording handler denies".into() })
        );
        assert_eq!(handler.seen(), vec![request]);
    }
}
