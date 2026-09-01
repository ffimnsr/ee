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
    /// `clientCapabilities.session.configOptions.boolean`
    pub session_config_boolean: bool,
    /// Proxy-only ee MCP discovery tools (`_ee/*` local bridge methods).
    pub proxy_discovery: bool,
    /// Host UI can approve durable workspace-memory mutations.
    pub workspace_memory_mutation_approval: bool,
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
            session_config_boolean: false,
            proxy_discovery: false,
            workspace_memory_mutation_approval: false,
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
            session_config_boolean: true,
            proxy_discovery: true,
            workspace_memory_mutation_approval: true,
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
            ELICITATION_CREATE_METHOD_NAME => self.elicitation_form || self.elicitation_url,
            "_ee/approve_workspace_memory_mutation" => self.workspace_memory_mutation_approval,
            "_ee/workspace_roots"
            | "_ee/list_directory"
            | "_ee/list_directory_all"
            | "_ee/search_files"
            | "_ee/search_files_all"
            | "_ee/search_text"
            | "_ee/search_text_regex"
            | "_ee/search_text_in_files"
            | "_ee/replace_text"
            | "_ee/apply_patch"
            | "_ee/create_text_file"
            | "_ee/overwrite_text_file"
            | "_ee/create_directory"
            | "_ee/delete_path"
            | "_ee/copy_path"
            | "_ee/move_path"
            | "_ee/read_buffer"
            | "_ee/read_buffer_lines"
            | "_ee/open_buffers"
            | "_ee/get_diagnostics"
            | "_ee/get_file_diagnostics"
            | "_ee/document_symbols"
            | "_ee/references"
            | "_ee/list_code_actions"
            | "_ee/apply_code_action"
            | "_ee/format_file"
            | "_ee/preview_rename_symbol"
            | "_ee/rename_symbol"
            | "_ee/terminal_output"
            | "_ee/git_status"
            | "_ee/git_diff"
            | "_ee/git_diff_staged"
            | "_ee/git_diff_file"
            | "_ee/changed_files"
            | "_ee/review_context"
            | "_ee/project_instructions"
            | "_ee/save_note"
            | "_ee/read_notes"
            | "_ee/read_note"
            | "_ee/file_dependency_map"
            | "_ee/symbol_dependency_map"
            | "_ee/web_search"
            | "_ee/fetch_url"
            | "_ee/browser_run" => self.proxy_discovery,
            _ => false,
        }
    }

    /// Whether this capability set covers one fully-typed request.
    #[must_use]
    pub fn supports_request(&self, request: &ClientRequest) -> bool {
        match request {
            ClientRequest::CreateElicitation(request) => match &request.mode {
                ee_agent_protocol::ElicitationMode::Form(_) => self.elicitation_form,
                ee_agent_protocol::ElicitationMode::Url(_) => self.elicitation_url,
                _ => false,
            },
            other => self.supports(other.method()),
        }
    }
}

/// One agent-to-client request the host forwards to the handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyTextEdit {
    pub old_text: String,
    pub new_text: String,
}

/// Durable workspace-memory mutation requiring explicit user approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMemoryMutationOperation {
    Remember,
    Verify,
    Forget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientRequest {
    /// Approval request contains operation and key only. Fact value never crosses this boundary.
    ApproveWorkspaceMemoryMutation {
        operation: WorkspaceMemoryMutationOperation,
        key: String,
    },
    ProxyWorkspaceRoots,
    ProxyListDirectory {
        path: String,
    },
    ProxyListDirectoryAll {
        path: String,
    },
    ProxySearchFiles {
        pattern: String,
    },
    ProxySearchFilesAll {
        pattern: String,
    },
    ProxySearchText {
        query: String,
    },
    ProxySearchTextRegex {
        pattern: String,
    },
    ProxySearchTextInFiles {
        query: String,
        file_glob: String,
    },
    ProxyReplaceText {
        path: String,
        old_text: String,
        new_text: String,
    },
    ProxyApplyPatch {
        path: String,
        edits: Vec<ProxyTextEdit>,
    },
    ProxyCreateTextFile {
        path: String,
        content: String,
    },
    ProxyOverwriteTextFile {
        path: String,
        content: String,
    },
    ProxyCreateDirectory {
        path: String,
    },
    ProxyDeletePath {
        path: String,
    },
    ProxyCopyPath {
        source_path: String,
        destination_path: String,
    },
    ProxyMovePath {
        source_path: String,
        destination_path: String,
    },
    ProxyReadBuffer {
        path: String,
    },
    ProxyReadBufferLines {
        path: String,
        line: u32,
        limit: u32,
    },
    ProxyOpenBuffers,
    ProxyGetDiagnostics,
    ProxyGetFileDiagnostics {
        path: String,
    },
    ProxyDocumentSymbols {
        path: String,
    },
    ProxyReferences {
        path: String,
        line: u32,
        character: u32,
    },
    ProxyListCodeActions {
        path: String,
        line: u32,
        character: u32,
    },
    ProxyApplyCodeAction {
        path: String,
        action_id: String,
    },
    ProxyFormatFile {
        path: String,
    },
    ProxyPreviewRenameSymbol {
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    },
    ProxyRenameSymbol {
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    },
    ProxyGitStatus,
    ProxyGitDiff,
    ProxyGitDiffStaged,
    ProxyGitDiffFile {
        path: String,
    },
    ProxyChangedFiles,
    ProxyReviewContext,
    ProxyProjectInstructions,
    ProxySaveNote {
        scope: String,
        key: String,
        content: String,
    },
    ProxyReadNotes {
        scope: String,
    },
    ProxyReadNote {
        scope: String,
        key: String,
    },
    ProxyFileDependencyMap {
        path: String,
    },
    ProxySymbolDependencyMap {
        path: String,
        line: u32,
        character: u32,
    },
    ProxyWebSearch {
        query: String,
        /// Opaque MCP logical-connection identity; never agent controlled.
        scope: String,
    },
    ProxyFetchUrl {
        url: String,
        /// Opaque MCP logical-connection identity; never agent controlled.
        scope: String,
    },
    ProxyBrowserRun {
        request: ee_mcp::BrowserRunRequest,
        /// Opaque MCP logical-connection identity; never agent controlled.
        scope: String,
    },
    /// Internal MCP proxy request. Unlike ACP `terminal/output`, returns the
    /// proxy's structured terminal-output schema in `ProxyValue`.
    ProxyTerminalOutput(TerminalOutputRequest),
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
            Self::ApproveWorkspaceMemoryMutation { .. } => "_ee/approve_workspace_memory_mutation",
            Self::ProxyWorkspaceRoots => "_ee/workspace_roots",
            Self::ProxyListDirectory { .. } => "_ee/list_directory",
            Self::ProxyListDirectoryAll { .. } => "_ee/list_directory_all",
            Self::ProxySearchFiles { .. } => "_ee/search_files",
            Self::ProxySearchFilesAll { .. } => "_ee/search_files_all",
            Self::ProxySearchText { .. } => "_ee/search_text",
            Self::ProxySearchTextRegex { .. } => "_ee/search_text_regex",
            Self::ProxySearchTextInFiles { .. } => "_ee/search_text_in_files",
            Self::ProxyReplaceText { .. } => "_ee/replace_text",
            Self::ProxyApplyPatch { .. } => "_ee/apply_patch",
            Self::ProxyCreateTextFile { .. } => "_ee/create_text_file",
            Self::ProxyOverwriteTextFile { .. } => "_ee/overwrite_text_file",
            Self::ProxyCreateDirectory { .. } => "_ee/create_directory",
            Self::ProxyDeletePath { .. } => "_ee/delete_path",
            Self::ProxyCopyPath { .. } => "_ee/copy_path",
            Self::ProxyMovePath { .. } => "_ee/move_path",
            Self::ProxyReadBuffer { .. } => "_ee/read_buffer",
            Self::ProxyReadBufferLines { .. } => "_ee/read_buffer_lines",
            Self::ProxyOpenBuffers => "_ee/open_buffers",
            Self::ProxyGetDiagnostics => "_ee/get_diagnostics",
            Self::ProxyGetFileDiagnostics { .. } => "_ee/get_file_diagnostics",
            Self::ProxyDocumentSymbols { .. } => "_ee/document_symbols",
            Self::ProxyReferences { .. } => "_ee/references",
            Self::ProxyListCodeActions { .. } => "_ee/list_code_actions",
            Self::ProxyApplyCodeAction { .. } => "_ee/apply_code_action",
            Self::ProxyFormatFile { .. } => "_ee/format_file",
            Self::ProxyPreviewRenameSymbol { .. } => "_ee/preview_rename_symbol",
            Self::ProxyRenameSymbol { .. } => "_ee/rename_symbol",
            Self::ProxyGitStatus => "_ee/git_status",
            Self::ProxyGitDiff => "_ee/git_diff",
            Self::ProxyGitDiffStaged => "_ee/git_diff_staged",
            Self::ProxyGitDiffFile { .. } => "_ee/git_diff_file",
            Self::ProxyChangedFiles => "_ee/changed_files",
            Self::ProxyReviewContext => "_ee/review_context",
            Self::ProxyProjectInstructions => "_ee/project_instructions",
            Self::ProxySaveNote { .. } => "_ee/save_note",
            Self::ProxyReadNotes { .. } => "_ee/read_notes",
            Self::ProxyReadNote { .. } => "_ee/read_note",
            Self::ProxyFileDependencyMap { .. } => "_ee/file_dependency_map",
            Self::ProxySymbolDependencyMap { .. } => "_ee/symbol_dependency_map",
            Self::ProxyWebSearch { .. } => "_ee/web_search",
            Self::ProxyFetchUrl { .. } => "_ee/fetch_url",
            Self::ProxyBrowserRun { .. } => "_ee/browser_run",
            Self::ProxyTerminalOutput(_) => "_ee/terminal_output",
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
            Self::ApproveWorkspaceMemoryMutation { .. }
            | Self::ProxyWorkspaceRoots
            | Self::ProxyListDirectory { .. }
            | Self::ProxyListDirectoryAll { .. }
            | Self::ProxySearchFiles { .. }
            | Self::ProxySearchFilesAll { .. }
            | Self::ProxySearchText { .. }
            | Self::ProxySearchTextRegex { .. }
            | Self::ProxySearchTextInFiles { .. }
            | Self::ProxyReplaceText { .. }
            | Self::ProxyApplyPatch { .. }
            | Self::ProxyCreateTextFile { .. }
            | Self::ProxyOverwriteTextFile { .. }
            | Self::ProxyCreateDirectory { .. }
            | Self::ProxyDeletePath { .. }
            | Self::ProxyCopyPath { .. }
            | Self::ProxyMovePath { .. }
            | Self::ProxyReadBuffer { .. }
            | Self::ProxyReadBufferLines { .. }
            | Self::ProxyOpenBuffers
            | Self::ProxyGetDiagnostics
            | Self::ProxyGetFileDiagnostics { .. }
            | Self::ProxyDocumentSymbols { .. }
            | Self::ProxyReferences { .. }
            | Self::ProxyListCodeActions { .. }
            | Self::ProxyApplyCodeAction { .. }
            | Self::ProxyFormatFile { .. }
            | Self::ProxyPreviewRenameSymbol { .. }
            | Self::ProxyRenameSymbol { .. }
            | Self::ProxyGitStatus
            | Self::ProxyGitDiff
            | Self::ProxyGitDiffStaged
            | Self::ProxyGitDiffFile { .. }
            | Self::ProxyChangedFiles
            | Self::ProxyReviewContext
            | Self::ProxyProjectInstructions
            | Self::ProxySaveNote { .. }
            | Self::ProxyReadNotes { .. }
            | Self::ProxyReadNote { .. }
            | Self::ProxyFileDependencyMap { .. }
            | Self::ProxySymbolDependencyMap { .. }
            | Self::ProxyWebSearch { .. }
            | Self::ProxyFetchUrl { .. }
            | Self::ProxyBrowserRun { .. } => None,
            Self::ProxyTerminalOutput(request) => Some(&request.session_id),
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
    /// Typed answer for one workspace-memory mutation approval request.
    WorkspaceMemoryApproval {
        approved: bool,
    },
    ProxyValue(serde_json::Value),
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
            Self::WorkspaceMemoryApproval { approved } => {
                serde_json::to_value(serde_json::json!({ "approved": approved }))
            }
            Self::ProxyValue(response) => Ok(response),
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
    use ee_agent_protocol::{ElicitationSchema, SessionId, TerminalId};

    #[test]
    fn capabilities_supports_maps_methods() {
        let caps = HandlerCapabilities::all();
        for method in [
            "fs/read_text_file",
            "fs/write_text_file",
            "_ee/create_directory",
            "_ee/delete_path",
            "_ee/copy_path",
            "_ee/move_path",
            "terminal/create",
            "terminal/output",
            "terminal/wait_for_exit",
            "terminal/kill",
            "terminal/release",
            "elicitation/create",
            "_ee/workspace_roots",
            "_ee/list_directory",
            "_ee/list_directory_all",
            "_ee/search_files",
            "_ee/search_files_all",
            "_ee/search_text",
            "_ee/search_text_regex",
            "_ee/search_text_in_files",
            "_ee/replace_text",
            "_ee/apply_patch",
            "_ee/create_text_file",
            "_ee/overwrite_text_file",
            "_ee/read_buffer",
            "_ee/read_buffer_lines",
            "_ee/open_buffers",
            "_ee/git_status",
            "_ee/git_diff",
            "_ee/git_diff_staged",
            "_ee/git_diff_file",
            "_ee/changed_files",
            "_ee/review_context",
            "_ee/terminal_output",
        ] {
            assert!(caps.supports(method), "{method} should be supported");
        }
        assert!(!caps.supports("session/request_permission"));
        assert!(!HandlerCapabilities::none().supports("fs/read_text_file"));
    }

    #[test]
    fn supports_request_gates_elicitation_modes_independently() {
        let form_request = ClientRequest::CreateElicitation(CreateElicitationRequest::new(
            ee_agent_protocol::ElicitationFormMode::new(
                ee_agent_protocol::ElicitationSessionScope::new("s1"),
                ElicitationSchema::new(),
            ),
            "fill",
        ));
        let url_request = ClientRequest::CreateElicitation(CreateElicitationRequest::new(
            ee_agent_protocol::ElicitationUrlMode::new(
                ee_agent_protocol::ElicitationSessionScope::new("s1"),
                "el-1",
                "https://example.com",
            ),
            "open",
        ));

        let form_only =
            HandlerCapabilities { elicitation_form: true, ..HandlerCapabilities::none() };
        assert!(form_only.supports_request(&form_request));
        assert!(!form_only.supports_request(&url_request));

        let url_only = HandlerCapabilities { elicitation_url: true, ..HandlerCapabilities::none() };
        assert!(url_only.supports_request(&url_request));
        assert!(!url_only.supports_request(&form_request));
    }

    #[test]
    fn request_method_names_match_wire() {
        let session = SessionId::new("s1");
        let read = ClientRequest::ReadTextFile(ReadTextFileRequest::new(session.clone(), "/tmp/x"));
        assert_eq!(read.method(), "fs/read_text_file");
        assert_eq!(read.session_id(), Some(&session));

        let proxy = ClientRequest::ProxySearchFiles { pattern: String::from("src/*.rs") };
        assert_eq!(proxy.method(), "_ee/search_files");
        assert_eq!(proxy.session_id(), None);

        let create_directory =
            ClientRequest::ProxyCreateDirectory { path: String::from("/tmp/new") };
        assert_eq!(create_directory.method(), "_ee/create_directory");
        assert_eq!(create_directory.session_id(), None);
        assert!(HandlerCapabilities::all().supports_request(&create_directory));

        let move_path = ClientRequest::ProxyMovePath {
            source_path: String::from("/tmp/old"),
            destination_path: String::from("/tmp/new"),
        };
        assert_eq!(move_path.method(), "_ee/move_path");
        assert_eq!(move_path.session_id(), None);

        let proxy_regex = ClientRequest::ProxySearchTextRegex { pattern: String::from("main") };
        assert_eq!(proxy_regex.method(), "_ee/search_text_regex");
        assert_eq!(proxy_regex.session_id(), None);

        let replace = ClientRequest::ProxyReplaceText {
            path: String::from("/tmp/x"),
            old_text: String::from("old"),
            new_text: String::from("new"),
        };
        assert_eq!(replace.method(), "_ee/replace_text");
        assert_eq!(replace.session_id(), None);

        let terminal =
            ClientRequest::CreateTerminal(CreateTerminalRequest::new(session.clone(), "echo"));
        assert_eq!(terminal.method(), "terminal/create");
        assert_eq!(terminal.session_id(), Some(&session));

        let proxy_terminal = ClientRequest::ProxyTerminalOutput(TerminalOutputRequest::new(
            session.clone(),
            TerminalId::new("t1"),
        ));
        assert_eq!(proxy_terminal.method(), "_ee/terminal_output");
        assert_eq!(proxy_terminal.session_id(), Some(&session));

        let git_status = ClientRequest::ProxyGitStatus;
        assert_eq!(git_status.method(), "_ee/git_status");
        assert_eq!(git_status.session_id(), None);
        assert!(HandlerCapabilities::all().supports(git_status.method()));

        let git_diff_staged = ClientRequest::ProxyGitDiffStaged;
        assert_eq!(git_diff_staged.method(), "_ee/git_diff_staged");
        assert_eq!(git_diff_staged.session_id(), None);
        assert!(HandlerCapabilities::all().supports(git_diff_staged.method()));

        let git_diff_file = ClientRequest::ProxyGitDiffFile { path: String::from("/tmp/x") };
        assert_eq!(git_diff_file.method(), "_ee/git_diff_file");
        assert_eq!(git_diff_file.session_id(), None);
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
