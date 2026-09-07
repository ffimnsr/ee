//! [`EeProxyBackend`]: the editor-host contract behind the ee MCP proxy.
use super::errors::unavailable_proxy_tool;
use super::*;

/// Backend implementing the editor operations the ee MCP proxy exposes.
///
/// All methods are synchronous and cheap to call; the host decides its own
/// capability policy (which paths, which commands) and applies it inside each
/// method.
pub trait EeProxyBackend: Send + Sync + 'static {
    /// Configured workspace roots and active editor context.
    fn workspace_roots(&self) -> Result<WorkspaceRootsResult, ProxyToolError>;

    /// Lists one directory level for `path` (absolute).
    fn list_directory(&self, path: String) -> Result<ListDirectoryResult, ProxyToolError>;

    /// Lists one directory level including hidden and ignored entries.
    fn list_directory_all(&self, path: String) -> Result<ListDirectoryAllResult, ProxyToolError>;

    /// Searches files by path/glob pattern across allowed roots.
    fn search_files(&self, pattern: String) -> Result<SearchFilesResult, ProxyToolError>;

    /// Searches files including hidden and ignored paths.
    fn search_files_all(&self, pattern: String) -> Result<SearchFilesAllResult, ProxyToolError>;

    /// Searches file text literally and case-sensitively across allowed roots.
    fn search_text(&self, query: String) -> Result<SearchTextResult, ProxyToolError>;

    /// Searches file text with a regex across allowed roots.
    fn search_text_regex(&self, pattern: String) -> Result<SearchTextResult, ProxyToolError>;

    /// Searches a configured public index. Default implementation fails closed.
    fn web_search(&self, request: WebSearchRequest) -> Result<WebSearchResult, ProxyToolError> {
        let _ = request;
        Err(WebToolError::new(
            WebToolErrorCode::WebSearchUnavailable,
            "no configured web search backend",
        )
        .into())
    }

    /// Fetches configured public text content. Default implementation fails closed.
    fn fetch_url(&self, request: FetchUrlRequest) -> Result<FetchUrlResult, ProxyToolError> {
        let _ = request;
        Err(WebToolError::new(
            WebToolErrorCode::WebDisabled,
            "web fetching is unavailable in this proxy mode",
        )
        .into())
    }

    /// Runs one configured browser read operation. Default implementation fails closed.
    fn browser_run(&self, request: BrowserRunRequest) -> Result<BrowserRunResult, ProxyToolError> {
        let _ = request;
        Err(WebToolError::new(
            WebToolErrorCode::WebDisabled,
            "browser runs are unavailable in this proxy mode",
        )
        .into())
    }

    /// Searches file text literally and case-sensitively inside glob-matched files.
    fn search_text_in_files(
        &self,
        query: String,
        file_glob: String,
    ) -> Result<SearchTextResult, ProxyToolError>;

    /// Replaces exactly one literal match in `path` through buffer/save semantics.
    fn replace_text(
        &self,
        path: String,
        old_text: String,
        new_text: String,
    ) -> Result<EditTextResult, ProxyToolError>;

    /// Applies multiple literal text edits to `path` through buffer/save semantics.
    fn apply_patch(
        &self,
        path: String,
        edits: Vec<TextEdit>,
    ) -> Result<EditTextResult, ProxyToolError>;

    /// Creates a new text file and fails when it already exists.
    fn create_text_file(
        &self,
        path: String,
        content: String,
    ) -> Result<EditTextResult, ProxyToolError>;

    /// Overwrites an existing text file through buffer/save semantics.
    fn overwrite_text_file(
        &self,
        path: String,
        content: String,
    ) -> Result<EditTextResult, ProxyToolError>;

    /// Creates a directory, including missing parents. Default fails closed.
    fn create_directory(&self, path: String) -> Result<FilesystemResult, ProxyToolError> {
        let _ = path;
        unavailable_proxy_tool("filesystem writes")
    }

    /// Deletes a file or directory recursively. Default fails closed.
    fn delete_path(&self, path: String) -> Result<FilesystemResult, ProxyToolError> {
        let _ = path;
        unavailable_proxy_tool("filesystem writes")
    }

    /// Copies a file or directory recursively. Default fails closed.
    fn copy_path(
        &self,
        source_path: String,
        destination_path: String,
    ) -> Result<FilesystemResult, ProxyToolError> {
        let _ = (source_path, destination_path);
        unavailable_proxy_tool("filesystem writes")
    }

    /// Moves or renames a file or directory. Default fails closed.
    fn move_path(
        &self,
        source_path: String,
        destination_path: String,
    ) -> Result<FilesystemResult, ProxyToolError> {
        let _ = (source_path, destination_path);
        unavailable_proxy_tool("filesystem writes")
    }

    /// Reads current buffer content, including unsaved changes when open.
    fn read_buffer(&self, path: String) -> Result<String, ProxyToolError>;

    /// Reads a bounded line window from the current buffer content.
    fn read_buffer_lines(
        &self,
        path: String,
        line: u32,
        limit: u32,
    ) -> Result<String, ProxyToolError>;

    /// Summaries of currently open editor buffers.
    fn open_buffers(&self) -> Result<OpenBuffersResult, ProxyToolError>;

    /// Returns bounded workspace diagnostics from editor/LSP state.
    fn get_diagnostics(&self) -> Result<DiagnosticsResult, ProxyToolError>;

    /// Returns bounded diagnostics for one file from editor/LSP state.
    fn get_file_diagnostics(&self, path: String) -> Result<DiagnosticsResult, ProxyToolError>;

    /// Returns bounded document symbols for one file.
    fn document_symbols(&self, path: String) -> Result<DocumentSymbolsResult, ProxyToolError>;

    /// Returns bounded references for the symbol at `path:line:character`.
    fn references(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> Result<ReferencesResult, ProxyToolError>;

    /// Lists available code actions at `path:line:character` without applying them.
    fn list_code_actions(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> Result<CodeActionsResult, ProxyToolError>;

    /// Applies one previously listed code action through buffer/save semantics.
    fn apply_code_action(
        &self,
        path: String,
        action_id: String,
    ) -> Result<EditTextResult, ProxyToolError>;

    /// Formats one file through LSP/editor formatting and buffer/save semantics.
    fn format_file(&self, path: String) -> Result<EditTextResult, ProxyToolError>;

    /// Previews planned rename edits without applying them.
    fn preview_rename_symbol(
        &self,
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenamePreviewResult, ProxyToolError>;

    /// Applies a rename through buffer/save semantics after validation.
    fn rename_symbol(
        &self,
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<WorkspaceEditResult, ProxyToolError>;

    /// Reads `path` (absolute) and returns the file text.
    ///
    /// `line` (1-based) and `limit` are optional line-window hints; the host
    /// decides how strictly to honor them.
    fn read_text_file(
        &self,
        path: String,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, ProxyToolError>;

    /// Writes `content` to `path` (absolute).
    fn write_text_file(&self, path: String, content: String) -> Result<(), ProxyToolError>;

    /// Starts a terminal running `command` with `args` in `cwd` and `env`.
    ///
    /// Returns the terminal id.
    fn terminal_create(
        &self,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<String, ProxyToolError>;

    /// Returns the bounded retained output for a terminal owned by this proxy session.
    fn terminal_output(&self, terminal_id: String) -> Result<TerminalOutputResult, ProxyToolError>;

    /// Returns retained chunks after `since_seq` for a terminal owned by this proxy session.
    fn terminal_output_since(
        &self,
        terminal_id: String,
        since_seq: u64,
    ) -> Result<TerminalOutputResult, ProxyToolError> {
        let _ = (terminal_id, since_seq);
        Err(ProxyToolError {
            message: String::from("incremental terminal output is unavailable in this proxy mode"),
            is_permission_denied: false,
        })
    }

    /// Waits using the host default timeout for a terminal owned by this proxy session.
    fn terminal_wait(&self, terminal_id: String) -> Result<TerminalWaitResult, ProxyToolError>;

    /// Waits for at most `timeout_ms` for a terminal owned by this proxy session.
    fn terminal_wait_long(
        &self,
        terminal_id: String,
        timeout_ms: u64,
    ) -> Result<TerminalWaitResult, ProxyToolError> {
        let _ = (terminal_id, timeout_ms);
        Err(ProxyToolError {
            message: String::from("long terminal waits are unavailable in this proxy mode"),
            is_permission_denied: false,
        })
    }

    /// Kills a terminal owned by this proxy session.
    fn terminal_kill(&self, terminal_id: String) -> Result<(), ProxyToolError>;

    /// Releases a terminal owned by this proxy session.
    fn terminal_release(&self, terminal_id: String) -> Result<(), ProxyToolError>;

    /// Returns bounded repository status for active workspace context.
    fn git_status(&self) -> Result<GitStatusResult, ProxyToolError> {
        Err(ProxyToolError {
            message: String::from("Git status is unavailable in this proxy mode"),
            is_permission_denied: false,
        })
    }

    /// Returns bounded unstaged unified diff for active workspace context.
    fn git_diff(&self) -> Result<GitDiffResult, ProxyToolError> {
        Err(ProxyToolError {
            message: String::from("Git diff is unavailable in this proxy mode"),
            is_permission_denied: false,
        })
    }

    /// Returns bounded staged unified diff for active workspace context.
    fn git_diff_staged(&self) -> Result<GitDiffResult, ProxyToolError> {
        Err(ProxyToolError {
            message: String::from("Git staged diff is unavailable in this proxy mode"),
            is_permission_denied: false,
        })
    }

    /// Returns bounded unstaged unified diff for one absolute workspace file.
    fn git_diff_file(&self, path: String) -> Result<GitDiffResult, ProxyToolError> {
        let _ = path;
        Err(ProxyToolError {
            message: String::from("Git file diff is unavailable in this proxy mode"),
            is_permission_denied: false,
        })
    }

    /// Returns SCM state merged with editor dirty/saved state.
    fn changed_files(&self) -> Result<ChangedFilesResult, ProxyToolError> {
        Err(ProxyToolError {
            message: String::from("Changed-file context is unavailable in this proxy mode"),
            is_permission_denied: false,
        })
    }

    /// Returns bounded review context without running commands or tests.
    fn review_context(&self) -> Result<ReviewContextResult, ProxyToolError> {
        Err(ProxyToolError {
            message: String::from("Review context is unavailable in this proxy mode"),
            is_permission_denied: false,
        })
    }

    /// Returns a host-owned, transport-safe turn evidence summary.
    ///
    /// The serialized value must contain only the existing redacted
    /// `TurnEvidenceSummary` fields. Implementations must reject unknown,
    /// stale, foreign, or ambiguous session/turn targets rather than
    /// fabricating a summary.
    fn turn_evidence_summary(
        &self,
        session_id: Option<String>,
        turn_id: Option<u64>,
    ) -> Result<serde_json::Value, ProxyToolError> {
        let _ = (session_id, turn_id);
        Err(ProxyToolError {
            message: String::from(
                "evidence_unavailable: turn evidence is unavailable in this proxy mode",
            ),
            is_permission_denied: false,
        })
    }

    /// Whether this backend currently has a host-owned evidence summary.
    ///
    /// Evidence tool discovery stays unavailable until this is true, so callers
    /// cannot infer session state from an otherwise empty turn ledger.
    fn exposes_turn_evidence_summary(&self) -> bool {
        true
    }

    /// Returns bounded workspace-local instructions and safe config summaries.
    fn project_instructions(&self) -> Result<ProjectInstructionsResult, ProxyToolError> {
        unavailable_proxy_tool("Project instructions")
    }

    /// Stores one validated, non-secret note for this proxy connection scope.
    fn save_note(&self, key: String, content: String) -> Result<SessionNoteResult, ProxyToolError> {
        let _ = (key, content);
        unavailable_proxy_tool("Session notes")
    }

    /// Returns bounded notes for this proxy connection scope.
    fn read_notes(&self) -> Result<SessionNotesResult, ProxyToolError> {
        unavailable_proxy_tool("Session notes")
    }

    /// Returns one bounded note for this proxy connection scope.
    fn read_note(&self, key: String) -> Result<SessionNoteResult, ProxyToolError> {
        let _ = key;
        unavailable_proxy_tool("Session notes")
    }

    /// Persists one approved workspace fact. Default implementation fails closed.
    fn remember_workspace_fact(
        &self,
        key: String,
        value: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        let _ = (key, value);
        unavailable_proxy_tool("Workspace memory")
    }

    /// Promotes one exact host-evidence-derived fact after required approval.
    fn verify_workspace_fact(
        &self,
        session_id: String,
        turn_id: u64,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        let _ = (session_id, turn_id, key);
        unavailable_proxy_tool("Workspace memory verification")
    }

    /// Recalls bounded workspace facts as untrusted data. Default implementation fails closed.
    fn recall_workspace_facts(
        &self,
        query: String,
    ) -> Result<WorkspaceFactsResult, ProxyToolError> {
        let _ = query;
        unavailable_proxy_tool("Workspace memory")
    }

    /// Reads one workspace fact by exact key. Default implementation fails closed.
    fn read_workspace_fact(&self, key: String) -> Result<WorkspaceFact, ProxyToolError> {
        let _ = key;
        unavailable_proxy_tool("Workspace memory")
    }

    /// Forgets one approved workspace fact by exact key. Default implementation fails closed.
    fn forget_workspace_fact(
        &self,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        let _ = key;
        unavailable_proxy_tool("Workspace memory")
    }

    /// Lists bounded active workspace facts. Default implementation fails closed.
    fn list_workspace_facts(&self, limit: u32) -> Result<WorkspaceFactsResult, ProxyToolError> {
        let _ = limit;
        unavailable_proxy_tool("Workspace memory")
    }

    /// Retracts one approved active workspace fact. Default implementation fails closed.
    fn retract_workspace_fact(
        &self,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        let _ = key;
        unavailable_proxy_tool("Workspace memory")
    }

    /// Exports bounded workspace memory. Default implementation fails closed.
    fn export_workspace_memory(
        &self,
        include_values: bool,
    ) -> Result<serde_json::Value, ProxyToolError> {
        let _ = include_values;
        unavailable_proxy_tool("Workspace memory")
    }

    /// Imports one bounded versioned workspace-memory export. Default fails closed.
    fn import_workspace_memory(
        &self,
        export_json: String,
    ) -> Result<serde_json::Value, ProxyToolError> {
        let _ = export_json;
        unavailable_proxy_tool("Workspace memory")
    }

    /// Clears approved workspace memory. Default implementation fails closed.
    fn clear_workspace_memory(&self) -> Result<serde_json::Value, ProxyToolError> {
        unavailable_proxy_tool("Workspace memory")
    }

    /// Returns known file edges from an optional dependency index.
    fn file_dependency_map(&self, path: String) -> Result<FileDependencyMapResult, ProxyToolError> {
        let _ = path;
        unavailable_proxy_tool("File dependency map")
    }

    /// Returns bounded Tree-sitter dependency facts for one symbol position.
    fn symbol_dependency_map(
        &self,
        path: String,
        line: u32,
        character: u32,
    ) -> Result<SymbolDependencyMapResult, ProxyToolError> {
        let _ = (path, line, character);
        Err(ProxyToolError {
            message: String::from(
                "dependency_index_unavailable: symbol dependency map unavailable in this proxy mode",
            ),
            is_permission_denied: false,
        })
    }

    /// Optional exact supported-tool profile. `None` means every stable tool is supported.
    /// Hosts use this to avoid advertising partial implementations as complete.
    fn supported_tools(&self) -> Option<Vec<String>> {
        None
    }

    /// Recent stderr/diagnostic lines, bounded; never contains secrets.
    fn diagnostics(&self) -> Vec<String>;
}
