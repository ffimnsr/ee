//! In-process MCP server surface ("ee MCP proxy").
//!
//! The proxy exposes ee editor operations as MCP tools over protocol
//! `2026-07-28` only. Tool execution is delegated to a caller-provided
//! [`EeProxyBackend`] (the editor host implements it; this crate stays
//! UI-free). Tools are namespaced under the server id `ee`, so every tool
//! name literally starts with `ee.`.
//!
//! Wire handling is entirely rmcp's ([`rmcp::ServerHandler`]); ee-owned code
//! here is limited to the tool surface, argument validation, and the
//! fail-closed protocol-version pin.

use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, DiscoverResult,
    ErrorCode, ErrorData, Implementation, InitializeRequestParams, InitializeResult, JsonObject,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The single protocol version this server implements.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];

/// Error produced by an [`EeProxyBackend`] operation.
///
/// Backend failures never become JSON-RPC protocol errors: they surface as
/// `isError` tool results so the caller sees the message. The
/// `is_permission_denied` flag lets hosts distinguish host policy denials
/// from ordinary operation failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyToolError {
    /// Human-readable failure description surfaced as tool content.
    pub message: String,
    /// Whether the failure was a permission denial (host policy).
    pub is_permission_denied: bool,
}

impl std::fmt::Display for ProxyToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProxyToolError {}

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

    /// Recent stderr/diagnostic lines, bounded; never contains secrets.
    fn diagnostics(&self) -> Vec<String>;
}

/// Structured result of `ee_workspace_roots`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRootsResult {
    pub roots: Vec<String>,
    pub active_root: Option<String>,
    pub active_file: Option<String>,
    #[serde(default)]
    pub additional_directories: Vec<String>,
}

/// One entry returned by `ee_list_directory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub path: String,
    pub kind: String,
    pub size: u64,
}

/// Structured result of `ee_list_directory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListDirectoryResult {
    pub entries: Vec<DirectoryEntry>,
    pub truncated: bool,
}

/// One entry returned by `ee_list_directory_all`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntryAll {
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub hidden: bool,
    pub ignored: bool,
}

/// Structured result of `ee_list_directory_all`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListDirectoryAllResult {
    pub entries: Vec<DirectoryEntryAll>,
    pub truncated: bool,
}

/// Structured result of `ee_search_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilesResult {
    pub matches: Vec<String>,
    pub truncated: bool,
}

/// One path match returned by `ee_search_files_all`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMatch {
    pub path: String,
    pub hidden: bool,
    pub ignored: bool,
}

/// Structured result of `ee_search_files_all`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFilesAllResult {
    pub matches: Vec<FileMatch>,
    pub truncated: bool,
}

/// One literal text match returned by `ee_search_text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMatch {
    pub path: String,
    pub line: u32,
    pub context: String,
}

/// Structured result of `ee_search_text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchTextResult {
    pub matches: Vec<TextMatch>,
    pub truncated: bool,
}

/// One literal text edit for `ee_apply_patch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub old_text: String,
    pub new_text: String,
}

/// Structured success result for patch-oriented write tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditTextResult {
    pub changed_file: String,
    pub byte_count: u64,
    pub edit_count: u32,
    pub new_revision: String,
    pub saved: bool,
    pub dirty: bool,
}

/// One open buffer summary from `ee_open_buffers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenBufferEntry {
    pub path: String,
    pub dirty: bool,
    pub revision_id: String,
    pub cursor_summary: String,
    pub selection_summary: String,
    pub language_id: Option<String>,
    pub active: bool,
}

/// Structured result of `ee_open_buffers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenBuffersResult {
    pub buffers: Vec<OpenBufferEntry>,
}

/// One 1-based text range exposed by Phase 3 tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextRange {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

/// One diagnostic returned by `ee_get_diagnostics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticEntry {
    pub path: String,
    pub range: TextRange,
    pub severity: String,
    pub source: Option<String>,
    pub code: Option<String>,
    pub message: String,
}

/// Structured result of diagnostics tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsResult {
    pub diagnostics: Vec<DiagnosticEntry>,
    pub truncated: bool,
    pub total: u32,
}

/// One document symbol returned by `ee_document_symbols`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbolEntry {
    pub name: String,
    pub kind: String,
    pub range: TextRange,
    pub selection_range: TextRange,
    pub container_path: String,
}

/// Structured result of `ee_document_symbols`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbolsResult {
    pub symbols: Vec<DocumentSymbolEntry>,
    pub truncated: bool,
    pub total: u32,
}

/// One reference location returned by `ee_references`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceEntry {
    pub path: String,
    pub range: TextRange,
}

/// Structured result of `ee_references`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferencesResult {
    pub references: Vec<ReferenceEntry>,
    pub truncated: bool,
    pub total: u32,
}

/// One listed code action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeActionEntry {
    pub action_id: String,
    pub title: String,
    pub kind: Option<String>,
}

/// Structured result of `ee_list_code_actions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeActionsResult {
    pub actions: Vec<CodeActionEntry>,
    pub truncated: bool,
    pub total: u32,
}

/// One planned text edit in a rename preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedTextEdit {
    pub range: TextRange,
    pub new_text: String,
}

/// One file touched by a rename preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedFileEdit {
    pub path: String,
    pub edits: Vec<PlannedTextEdit>,
}

/// Structured result of `ee_preview_rename_symbol`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamePreviewResult {
    pub files: Vec<PlannedFileEdit>,
    pub truncated: bool,
    pub total_files: u32,
    pub total_edits: u32,
}

/// Structured success result for multi-file workspace edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceEditResult {
    pub files: Vec<EditTextResult>,
    pub file_count: u32,
    pub edit_count: u32,
}

/// An in-process MCP server exposing ee editor operations as MCP tools.
///
/// The server speaks MCP `2026-07-28` only and advertises the fixed
/// `ee_*` tool set returned by [`list_tools`](rmcp::ServerHandler::list_tools).
/// Tool execution is delegated to the [`EeProxyBackend`] supplied at
/// construction.
pub struct EeMcpProxy {
    backend: Arc<dyn EeProxyBackend>,
}

impl EeMcpProxy {
    /// Creates a proxy delegating tool execution to `backend`.
    #[must_use]
    pub fn new(backend: Arc<dyn EeProxyBackend>) -> Self {
        Self { backend }
    }

    /// The server capabilities advertised in `initialize` and `discover`.
    fn capabilities() -> ServerCapabilities {
        ServerCapabilities::builder().enable_tools().build()
    }

    /// The server implementation identity advertised in `initialize`.
    fn server_info() -> Implementation {
        Implementation::new(crate::CLIENT_NAME, crate::CLIENT_VERSION).with_title("ee MCP proxy")
    }

    /// The fixed tool list (tool names are namespaced under `ee.`).
    fn tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "ee_workspace_roots",
                "Return canonical workspace roots plus active root and active file. Result is bounded to session-advertised roots only.",
                schema(json!({ "type": "object", "properties": {} })),
            ),
            Tool::new(
                "ee_list_directory",
                "List one directory level from the editor workspace (absolute path). Hidden/ignored entries are skipped and results are bounded by the host default cap.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                    },
                    "required": ["path"],
                })),
            ),
            Tool::new(
                "ee_list_directory_all",
                "List one directory level including hidden and ignored entries. Results are bounded by the host default cap.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                    },
                    "required": ["path"],
                })),
            ),
            Tool::new(
                "ee_search_files",
                "Search workspace files by path or glob pattern. Results are bounded by the host default cap and respect ignore rules.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                    },
                    "required": ["pattern"],
                })),
            ),
            Tool::new(
                "ee_search_files_all",
                "Search workspace files including hidden and ignored paths. Results are bounded by the host default cap.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                    },
                    "required": ["pattern"],
                })),
            ),
            Tool::new(
                "ee_search_text",
                "Perform literal case-sensitive text search across workspace files. Results are bounded by the host default cap.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                    },
                    "required": ["query"],
                })),
            ),
            Tool::new(
                "ee_search_text_regex",
                "Perform regex text search across workspace files. Results are bounded by the host default cap and regex execution is safety-limited by the host.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                    },
                    "required": ["pattern"],
                })),
            ),
            Tool::new(
                "ee_search_text_in_files",
                "Perform literal case-sensitive text search inside glob-matched files. Results are bounded by the host default cap.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "file_glob": { "type": "string" },
                    },
                    "required": ["query", "file_glob"],
                })),
            ),
            Tool::new(
                "ee_replace_text",
                "Replace exactly one literal match in an editor file. Requires absolute path, approval before mutation, and fails when old_text is missing or ambiguous.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_text": { "type": "string" },
                        "new_text": { "type": "string" },
                    },
                    "required": ["path", "old_text", "new_text"],
                })),
            ),
            Tool::new(
                "ee_apply_patch",
                "Apply multiple literal old_text/new_text edits to one file. Each edit uses the same simple shape; range or hunk patches are rejected.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_text": { "type": "string" },
                                    "new_text": { "type": "string" },
                                },
                                "required": ["old_text", "new_text"],
                            }
                        },
                    },
                    "required": ["path", "edits"],
                })),
            ),
            Tool::new(
                "ee_create_text_file",
                "Create a new text file in the editor workspace. Fails when the file already exists and requires approval before mutation.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                    },
                    "required": ["path", "content"],
                })),
            ),
            Tool::new(
                "ee_overwrite_text_file",
                "Overwrite an existing text file in the editor workspace. Requires approval and reports the replacement as structured success.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                    },
                    "required": ["path", "content"],
                })),
            ),
            Tool::new(
                "ee_read_buffer",
                "Read current editor buffer content, including unsaved changes. Falls back to disk only when no buffer is open and policy allows.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                    },
                    "required": ["path"],
                })),
            ),
            Tool::new(
                "ee_read_buffer_lines",
                "Read a bounded line window from current editor buffer content using 1-based line and explicit limit.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer" },
                        "limit": { "type": "integer" },
                    },
                    "required": ["path", "line", "limit"],
                })),
            ),
            Tool::new(
                "ee_open_buffers",
                "Return open buffer paths, dirty flags, revision ids, cursor or selection summaries, and language ids without exposing full content.",
                schema(json!({ "type": "object", "properties": {} })),
            ),
            Tool::new(
                "ee_get_diagnostics",
                "Return bounded workspace diagnostics from editor and LSP state. Paths are absolute, ranges are 1-based, and results include truncation metadata when capped.",
                schema(json!({ "type": "object", "properties": {} })),
            ),
            Tool::new(
                "ee_get_file_diagnostics",
                "Return bounded diagnostics for one file from editor and LSP state. Requires an absolute path.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                })),
            ),
            Tool::new(
                "ee_document_symbols",
                "Return bounded document symbols for one file with 1-based ranges and stable container paths.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                })),
            ),
            Tool::new(
                "ee_references",
                "Return bounded references for symbol at absolute path and 1-based line and character.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer" },
                        "character": { "type": "integer" }
                    },
                    "required": ["path", "line", "character"]
                })),
            ),
            Tool::new(
                "ee_list_code_actions",
                "List bounded code actions at absolute path and 1-based line and character without applying them.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer" },
                        "character": { "type": "integer" }
                    },
                    "required": ["path", "line", "character"]
                })),
            ),
            Tool::new(
                "ee_apply_code_action",
                "Apply one previously listed code action by action_id. Requires approval and uses buffer edit semantics.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "action_id": { "type": "string" }
                    },
                    "required": ["path", "action_id"]
                })),
            ),
            Tool::new(
                "ee_format_file",
                "Format one file through configured formatter or LSP formatting. Requires approval when it changes the buffer.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                })),
            ),
            Tool::new(
                "ee_preview_rename_symbol",
                "Preview planned rename edits for symbol at absolute path and 1-based line and character without applying them.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer" },
                        "character": { "type": "integer" },
                        "new_name": { "type": "string" }
                    },
                    "required": ["path", "line", "character", "new_name"]
                })),
            ),
            Tool::new(
                "ee_rename_symbol",
                "Apply a rename through buffer edit semantics after validating every touched file is inside allowed roots. Requires approval.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer" },
                        "character": { "type": "integer" },
                        "new_name": { "type": "string" }
                    },
                    "required": ["path", "line", "character", "new_name"]
                })),
            ),
            Tool::new(
                "ee_read_text_file",
                "Read a text file from the editor workspace (absolute path).",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer" },
                        "limit": { "type": "integer" },
                    },
                    "required": ["path"],
                })),
            ),
            Tool::new(
                "ee_write_text_file",
                "Write text content to a file in the editor workspace (absolute path).",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                    },
                    "required": ["path", "content"],
                })),
            ),
            Tool::new(
                "ee_terminal_create",
                "Start a terminal in the editor workspace running a command.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "args": { "type": "array", "items": { "type": "string" } },
                        "cwd": { "type": "string" },
                        "env": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                        },
                    },
                    "required": ["command"],
                })),
            ),
            Tool::new(
                "ee_diagnostics",
                "Recent editor diagnostics (stderr lines); never contains secrets.",
                schema(json!({ "type": "object", "properties": {} })),
            ),
        ]
    }

    /// Validates and dispatches one `tools/call` request.
    fn dispatch_tool(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            "ee_workspace_roots" => Ok(self
                .backend
                .workspace_roots()
                .map(|roots| complete(CallToolResult::structured(json!(roots))))
                .unwrap_or_else(backend_error_result)),
            "ee_list_directory" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .list_directory(path.to_owned())
                    .map(|listing| complete(CallToolResult::structured(json!(listing))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_list_directory_all" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .list_directory_all(path.to_owned())
                    .map(|listing| complete(CallToolResult::structured(json!(listing))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_files" => {
                let arguments = require_arguments(request)?;
                let pattern = require_nonempty_string(arguments, "pattern")?;
                Ok(self
                    .backend
                    .search_files(pattern.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_files_all" => {
                let arguments = require_arguments(request)?;
                let pattern = require_nonempty_string(arguments, "pattern")?;
                Ok(self
                    .backend
                    .search_files_all(pattern.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_text" => {
                let arguments = require_arguments(request)?;
                let query = require_nonempty_string(arguments, "query")?;
                Ok(self
                    .backend
                    .search_text(query.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_text_regex" => {
                let arguments = require_arguments(request)?;
                let pattern = require_nonempty_string(arguments, "pattern")?;
                Ok(self
                    .backend
                    .search_text_regex(pattern.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_text_in_files" => {
                let arguments = require_arguments(request)?;
                let query = require_nonempty_string(arguments, "query")?;
                let file_glob = require_nonempty_string(arguments, "file_glob")?;
                Ok(self
                    .backend
                    .search_text_in_files(query.to_owned(), file_glob.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_replace_text" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let old_text = require_string(arguments, "old_text")?;
                let new_text = require_string(arguments, "new_text")?;
                Ok(self
                    .backend
                    .replace_text(path.to_owned(), old_text.to_owned(), new_text.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_apply_patch" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let edits = text_edits(arguments, "edits")?;
                Ok(self
                    .backend
                    .apply_patch(path.to_owned(), edits)
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_create_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let content = require_string(arguments, "content")?;
                Ok(self
                    .backend
                    .create_text_file(path.to_owned(), content.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_overwrite_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let content = require_string(arguments, "content")?;
                Ok(self
                    .backend
                    .overwrite_text_file(path.to_owned(), content.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_read_buffer" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .read_buffer(path.to_owned())
                    .map(|text| complete(CallToolResult::success(vec![ContentBlock::text(text)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_read_buffer_lines" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let limit = require_positive_u32(arguments, "limit")?;
                Ok(self
                    .backend
                    .read_buffer_lines(path.to_owned(), line, limit)
                    .map(|text| complete(CallToolResult::success(vec![ContentBlock::text(text)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_open_buffers" => Ok(self
                .backend
                .open_buffers()
                .map(|buffers| complete(CallToolResult::structured(json!(buffers))))
                .unwrap_or_else(backend_error_result)),
            "ee_get_diagnostics" => Ok(self
                .backend
                .get_diagnostics()
                .map(|diagnostics| complete(CallToolResult::structured(json!(diagnostics))))
                .unwrap_or_else(backend_error_result)),
            "ee_get_file_diagnostics" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .get_file_diagnostics(path.to_owned())
                    .map(|diagnostics| complete(CallToolResult::structured(json!(diagnostics))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_document_symbols" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .document_symbols(path.to_owned())
                    .map(|symbols| complete(CallToolResult::structured(json!(symbols))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_references" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let character = require_positive_u32(arguments, "character")?;
                Ok(self
                    .backend
                    .references(path.to_owned(), line, character)
                    .map(|references| complete(CallToolResult::structured(json!(references))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_list_code_actions" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let character = require_positive_u32(arguments, "character")?;
                Ok(self
                    .backend
                    .list_code_actions(path.to_owned(), line, character)
                    .map(|actions| complete(CallToolResult::structured(json!(actions))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_apply_code_action" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let action_id = require_nonempty_string(arguments, "action_id")?;
                Ok(self
                    .backend
                    .apply_code_action(path.to_owned(), action_id.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_format_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .format_file(path.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_preview_rename_symbol" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let character = require_positive_u32(arguments, "character")?;
                let new_name = require_nonempty_string(arguments, "new_name")?;
                Ok(self
                    .backend
                    .preview_rename_symbol(path.to_owned(), line, character, new_name.to_owned())
                    .map(|preview| complete(CallToolResult::structured(json!(preview))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_rename_symbol" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let character = require_positive_u32(arguments, "character")?;
                let new_name = require_nonempty_string(arguments, "new_name")?;
                Ok(self
                    .backend
                    .rename_symbol(path.to_owned(), line, character, new_name.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_read_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = optional_u32(arguments, "line")?;
                let limit = optional_u32(arguments, "limit")?;
                Ok(self
                    .backend
                    .read_text_file(path.to_owned(), line, limit)
                    .map(|text| complete(CallToolResult::success(vec![ContentBlock::text(text)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_write_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let content = require_string(arguments, "content")?;
                Ok(self
                    .backend
                    .write_text_file(path.to_owned(), content.to_owned())
                    .map(|()| {
                        complete(CallToolResult::success(vec![ContentBlock::text(format!(
                            "wrote {} bytes to {path}",
                            content.len()
                        ))]))
                    })
                    .unwrap_or_else(backend_error_result))
            }
            "ee_terminal_create" => {
                let arguments = require_arguments(request)?;
                let command = require_string(arguments, "command")?;
                if command.is_empty() {
                    return Err(ErrorData::invalid_params(
                        "argument 'command' must not be empty",
                        None,
                    ));
                }
                let args = string_array(arguments, "args")?;
                let cwd = match optional_string(arguments, "cwd")? {
                    Some(cwd) => {
                        require_absolute(&cwd)?;
                        Some(cwd)
                    }
                    None => None,
                };
                let env = env_pairs(arguments)?;
                Ok(self
                    .backend
                    .terminal_create(command.to_owned(), args, cwd, env)
                    .map(|id| complete(CallToolResult::success(vec![ContentBlock::text(id)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_diagnostics" => {
                let lines = self.backend.diagnostics();
                Ok(complete(CallToolResult::success(vec![ContentBlock::text(lines.join("\n"))])))
            }
            _ => Err(ErrorData::method_not_found::<rmcp::model::CallToolRequestMethod>()),
        }
    }
}

impl rmcp::ServerHandler for EeMcpProxy {
    /// Accepts `2026-07-28` only; anything else fails closed.
    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<InitializeResult, ErrorData>> + MaybeSendFuture + '_ {
        context.peer.set_peer_info(request.clone());
        if request.protocol_version != ProtocolVersion::V_2026_07_28 {
            return std::future::ready(Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("unsupported protocol version: {}", request.protocol_version),
                None,
            )));
        }
        std::future::ready(Ok(InitializeResult::new(Self::capabilities())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(Self::server_info())))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }

    /// Advertises `2026-07-28` with the tools capability.
    fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<DiscoverResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(DiscoverResult::new(
            SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
            Self::capabilities(),
        )))
    }

    fn ping(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(()))
    }

    /// The fixed tool list; pagination is ignored (no cursor support).
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(Self::tools())))
    }

    /// Dispatches a tool call to the backend; see [`EeMcpProxy::dispatch_tool`].
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(self.dispatch_tool(&request))
    }
}

/// Converts a complete tool result into a `tools/call` response.
fn complete(result: CallToolResult) -> CallToolResponse {
    CallToolResponse::from(result)
}

/// Converts a backend failure into an `isError` tool result.
///
/// Backend errors are tool-level failures the caller must see, not JSON-RPC
/// protocol errors; permission denials are prefixed so hosts can distinguish
/// them at a glance.
fn backend_error_result(error: ProxyToolError) -> CallToolResponse {
    let message = if error.is_permission_denied {
        format!("denied: {}", error.message)
    } else {
        error.message
    };
    complete(CallToolResult::error(vec![ContentBlock::text(message)]))
}

/// Converts a `serde_json::json!` literal into a tool input schema object.
fn schema(value: serde_json::Value) -> JsonObject {
    value.as_object().expect("tool schema must be a JSON object").clone()
}

/// Requires the tool call to carry arguments.
fn require_arguments(request: &CallToolRequestParams) -> Result<&JsonObject, ErrorData> {
    request
        .arguments
        .as_ref()
        .ok_or_else(|| ErrorData::invalid_params("missing tool arguments", None))
}

/// Reads a required string argument.
fn require_string<'a>(arguments: &'a JsonObject, key: &str) -> Result<&'a str, ErrorData> {
    arguments.get(key).and_then(serde_json::Value::as_str).ok_or_else(|| {
        ErrorData::invalid_params(format!("missing or non-string argument '{key}'"), None)
    })
}

/// Reads a required non-empty string argument.
fn require_nonempty_string<'a>(arguments: &'a JsonObject, key: &str) -> Result<&'a str, ErrorData> {
    let value = require_string(arguments, key)?;
    if value.is_empty() {
        return Err(ErrorData::invalid_params(format!("argument '{key}' must not be empty"), None));
    }
    Ok(value)
}

/// Reads an optional string argument.
fn optional_string(arguments: &JsonObject, key: &str) -> Result<Option<String>, ErrorData> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => value.as_str().map(ToOwned::to_owned).map(Some).ok_or_else(|| {
            ErrorData::invalid_params(format!("argument '{key}' must be a string"), None)
        }),
    }
}

/// Reads an optional non-negative integer argument.
fn optional_u32(arguments: &JsonObject, key: &str) -> Result<Option<u32>, ErrorData> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => {
            value.as_u64().and_then(|number| u32::try_from(number).ok()).map(Some).ok_or_else(
                || {
                    ErrorData::invalid_params(
                        format!("argument '{key}' must be a non-negative integer"),
                        None,
                    )
                },
            )
        }
    }
}

/// Reads a required positive integer argument.
fn require_positive_u32(arguments: &JsonObject, key: &str) -> Result<u32, ErrorData> {
    let value = optional_u32(arguments, key)?
        .ok_or_else(|| ErrorData::invalid_params(format!("missing argument '{key}'"), None))?;
    if value == 0 {
        return Err(ErrorData::invalid_params(
            format!("argument '{key}' must be greater than zero"),
            None,
        ));
    }
    Ok(value)
}

/// Reads a required array of simple `old_text`/`new_text` edits.
fn text_edits(arguments: &JsonObject, key: &str) -> Result<Vec<TextEdit>, ErrorData> {
    let items = arguments.get(key).and_then(serde_json::Value::as_array).ok_or_else(|| {
        ErrorData::invalid_params(format!("argument '{key}' must be an array of edits"), None)
    })?;
    if items.is_empty() {
        return Err(ErrorData::invalid_params(format!("argument '{key}' must not be empty"), None));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let object = item.as_object().ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("edit {index} in '{key}' must be an object with old_text/new_text"),
                    None,
                )
            })?;
            if object.len() != 2
                || !object.contains_key("old_text")
                || !object.contains_key("new_text")
            {
                return Err(ErrorData::invalid_params(
                    format!("edit {index} in '{key}' must contain only old_text and new_text"),
                    None,
                ));
            }
            let old_text =
                object.get("old_text").and_then(serde_json::Value::as_str).ok_or_else(|| {
                    ErrorData::invalid_params(
                        format!("edit {index} in '{key}' is missing string old_text"),
                        None,
                    )
                })?;
            let new_text =
                object.get("new_text").and_then(serde_json::Value::as_str).ok_or_else(|| {
                    ErrorData::invalid_params(
                        format!("edit {index} in '{key}' is missing string new_text"),
                        None,
                    )
                })?;
            Ok(TextEdit { old_text: old_text.to_owned(), new_text: new_text.to_owned() })
        })
        .collect()
}

/// Reads an optional array-of-strings argument (defaults to empty).
fn string_array(arguments: &JsonObject, key: &str) -> Result<Vec<String>, ErrorData> {
    match arguments.get(key) {
        None => Ok(Vec::new()),
        Some(value) => {
            let items = value.as_array().ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("argument '{key}' must be an array of strings"),
                    None,
                )
            })?;
            items
                .iter()
                .map(|item| {
                    item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        ErrorData::invalid_params(
                            format!("argument '{key}' must be an array of strings"),
                            None,
                        )
                    })
                })
                .collect()
        }
    }
}

/// Reads the optional `env` object (string values), rejecting secret-like
/// keys before they ever reach the backend.
fn env_pairs(arguments: &JsonObject) -> Result<Vec<(String, String)>, ErrorData> {
    let Some(value) = arguments.get("env") else {
        return Ok(Vec::new());
    };
    let object = value.as_object().ok_or_else(|| {
        ErrorData::invalid_params("argument 'env' must be an object with string values", None)
    })?;
    let mut pairs = Vec::with_capacity(object.len());
    for (key, value) in object {
        if crate::handler::is_secret_field_name(key) {
            return Err(ErrorData::invalid_params(
                "secret-like environment keys are not allowed in the ee proxy",
                None,
            ));
        }
        let value = value.as_str().ok_or_else(|| {
            ErrorData::invalid_params(format!("env value for {key:?} must be a string"), None)
        })?;
        pairs.push((key.clone(), value.to_owned()));
    }
    Ok(pairs)
}

/// Requires an absolute path (relative paths fail closed).
fn require_absolute(path: &str) -> Result<(), ErrorData> {
    if std::path::Path::new(path).is_absolute() {
        Ok(())
    } else {
        Err(ErrorData::invalid_params(format!("path must be absolute: {path:?}"), None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ClientHandler;
    use rmcp::model::ClientCapabilities;

    use rmcp::service::{ClientLifecycleMode, RoleClient, RunningService};
    use std::sync::Mutex;
    use std::time::Duration;

    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// Minimal client handler: 2026-07-28 only, no extra capabilities.
    #[derive(Debug, Clone, Copy, Default)]
    struct TestClientHandler;

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> rmcp::model::InitializeRequestParams {
            rmcp::model::InitializeRequestParams::new(
                ClientCapabilities::builder().build(),
                rmcp::model::Implementation::new("proxy-test", "0.1"),
            )
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
        }
    }

    /// Records every backend invocation for assertions.
    #[derive(Debug, Default)]
    struct ScriptedBackend {
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedBackend {
        fn record(&self, call: String) {
            self.calls.lock().expect("calls poisoned").push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls poisoned").clone()
        }
    }

    impl EeProxyBackend for ScriptedBackend {
        fn workspace_roots(&self) -> Result<WorkspaceRootsResult, ProxyToolError> {
            self.record(String::from("workspace_roots"));
            Ok(WorkspaceRootsResult {
                roots: vec![String::from("/abs/work"), String::from("/abs/extra")],
                active_root: Some(String::from("/abs/work")),
                active_file: Some(String::from("/abs/work/src/main.rs")),
                additional_directories: vec![String::from("/abs/extra")],
            })
        }

        fn list_directory(&self, path: String) -> Result<ListDirectoryResult, ProxyToolError> {
            self.record(format!("list_directory:{path}"));
            Ok(ListDirectoryResult {
                entries: vec![DirectoryEntry {
                    path: format!("{path}/src"),
                    kind: String::from("directory"),
                    size: 4096,
                }],
                truncated: false,
            })
        }

        fn list_directory_all(
            &self,
            path: String,
        ) -> Result<ListDirectoryAllResult, ProxyToolError> {
            self.record(format!("list_directory_all:{path}"));
            Ok(ListDirectoryAllResult {
                entries: vec![DirectoryEntryAll {
                    path: format!("{path}/.git"),
                    kind: String::from("directory"),
                    size: 4096,
                    hidden: true,
                    ignored: true,
                }],
                truncated: false,
            })
        }

        fn search_files(&self, pattern: String) -> Result<SearchFilesResult, ProxyToolError> {
            self.record(format!("search_files:{pattern}"));
            Ok(SearchFilesResult {
                matches: vec![format!("/abs/work/{pattern}")],
                truncated: false,
            })
        }

        fn search_files_all(
            &self,
            pattern: String,
        ) -> Result<SearchFilesAllResult, ProxyToolError> {
            self.record(format!("search_files_all:{pattern}"));
            Ok(SearchFilesAllResult {
                matches: vec![FileMatch {
                    path: format!("/abs/work/{pattern}"),
                    hidden: true,
                    ignored: true,
                }],
                truncated: false,
            })
        }

        fn search_text(&self, query: String) -> Result<SearchTextResult, ProxyToolError> {
            self.record(format!("search_text:{query}"));
            Ok(SearchTextResult {
                matches: vec![TextMatch {
                    path: String::from("/abs/work/src/main.rs"),
                    line: 7,
                    context: format!("found {query}"),
                }],
                truncated: false,
            })
        }

        fn search_text_regex(&self, pattern: String) -> Result<SearchTextResult, ProxyToolError> {
            self.record(format!("search_text_regex:{pattern}"));
            Ok(SearchTextResult {
                matches: vec![TextMatch {
                    path: String::from("/abs/work/src/lib.rs"),
                    line: 9,
                    context: format!("regex {pattern}"),
                }],
                truncated: false,
            })
        }

        fn search_text_in_files(
            &self,
            query: String,
            file_glob: String,
        ) -> Result<SearchTextResult, ProxyToolError> {
            self.record(format!("search_text_in_files:{query}:{file_glob}"));
            Ok(SearchTextResult {
                matches: vec![TextMatch {
                    path: format!("/abs/work/{file_glob}"),
                    line: 11,
                    context: format!("scoped {query}"),
                }],
                truncated: false,
            })
        }

        fn replace_text(
            &self,
            path: String,
            old_text: String,
            new_text: String,
        ) -> Result<EditTextResult, ProxyToolError> {
            self.record(format!("replace_text:{path}:{old_text}:{new_text}"));
            Ok(EditTextResult {
                changed_file: path,
                byte_count: 12,
                edit_count: 1,
                new_revision: String::from("rev-replace"),
                saved: true,
                dirty: false,
            })
        }

        fn apply_patch(
            &self,
            path: String,
            edits: Vec<TextEdit>,
        ) -> Result<EditTextResult, ProxyToolError> {
            self.record(format!("apply_patch:{path}:{}", edits.len()));
            Ok(EditTextResult {
                changed_file: path,
                byte_count: 24,
                edit_count: u32::try_from(edits.len()).unwrap_or(u32::MAX),
                new_revision: String::from("rev-patch"),
                saved: true,
                dirty: false,
            })
        }

        fn create_text_file(
            &self,
            path: String,
            content: String,
        ) -> Result<EditTextResult, ProxyToolError> {
            self.record(format!("create_text_file:{path}:{content}"));
            Ok(EditTextResult {
                changed_file: path,
                byte_count: 7,
                edit_count: 1,
                new_revision: String::from("rev-create"),
                saved: true,
                dirty: false,
            })
        }

        fn overwrite_text_file(
            &self,
            path: String,
            content: String,
        ) -> Result<EditTextResult, ProxyToolError> {
            self.record(format!("overwrite_text_file:{path}:{content}"));
            Ok(EditTextResult {
                changed_file: path,
                byte_count: 8,
                edit_count: 1,
                new_revision: String::from("rev-overwrite"),
                saved: true,
                dirty: false,
            })
        }

        fn read_buffer(&self, path: String) -> Result<String, ProxyToolError> {
            self.record(format!("read_buffer:{path}"));
            Ok(format!("buffer of {path}"))
        }

        fn read_buffer_lines(
            &self,
            path: String,
            line: u32,
            limit: u32,
        ) -> Result<String, ProxyToolError> {
            self.record(format!("read_buffer_lines:{path}:{line}:{limit}"));
            Ok(format!("buffer lines of {path}"))
        }

        fn open_buffers(&self) -> Result<OpenBuffersResult, ProxyToolError> {
            self.record(String::from("open_buffers"));
            Ok(OpenBuffersResult {
                buffers: vec![OpenBufferEntry {
                    path: String::from("/abs/work/src/main.rs"),
                    dirty: true,
                    revision_id: String::from("rev-open"),
                    cursor_summary: String::from("line 3, column 7"),
                    selection_summary: String::from("cursor at 3:7"),
                    language_id: Some(String::from("rust")),
                    active: true,
                }],
            })
        }

        fn get_diagnostics(&self) -> Result<DiagnosticsResult, ProxyToolError> {
            self.record(String::from("get_diagnostics"));
            Ok(DiagnosticsResult {
                diagnostics: vec![DiagnosticEntry {
                    path: String::from("/abs/work/src/main.rs"),
                    range: TextRange {
                        start_line: 1,
                        start_character: 1,
                        end_line: 1,
                        end_character: 5,
                    },
                    severity: String::from("error"),
                    source: Some(String::from("rust-analyzer")),
                    code: Some(String::from("E0001")),
                    message: String::from("boom"),
                }],
                truncated: false,
                total: 1,
            })
        }

        fn get_file_diagnostics(&self, path: String) -> Result<DiagnosticsResult, ProxyToolError> {
            self.record(format!("get_file_diagnostics:{path}"));
            Ok(DiagnosticsResult {
                diagnostics: vec![DiagnosticEntry {
                    path,
                    range: TextRange {
                        start_line: 2,
                        start_character: 1,
                        end_line: 2,
                        end_character: 4,
                    },
                    severity: String::from("warning"),
                    source: Some(String::from("rust-analyzer")),
                    code: None,
                    message: String::from("careful"),
                }],
                truncated: false,
                total: 1,
            })
        }

        fn document_symbols(&self, path: String) -> Result<DocumentSymbolsResult, ProxyToolError> {
            self.record(format!("document_symbols:{path}"));
            Ok(DocumentSymbolsResult {
                symbols: vec![DocumentSymbolEntry {
                    name: String::from("main"),
                    kind: String::from("function"),
                    range: TextRange {
                        start_line: 1,
                        start_character: 1,
                        end_line: 3,
                        end_character: 1,
                    },
                    selection_range: TextRange {
                        start_line: 1,
                        start_character: 4,
                        end_line: 1,
                        end_character: 8,
                    },
                    container_path: path,
                }],
                truncated: false,
                total: 1,
            })
        }

        fn references(
            &self,
            path: String,
            line: u32,
            character: u32,
        ) -> Result<ReferencesResult, ProxyToolError> {
            self.record(format!("references:{path}:{line}:{character}"));
            Ok(ReferencesResult {
                references: vec![ReferenceEntry {
                    path,
                    range: TextRange {
                        start_line: line,
                        start_character: character,
                        end_line: line,
                        end_character: character.saturating_add(2),
                    },
                }],
                truncated: false,
                total: 1,
            })
        }

        fn list_code_actions(
            &self,
            path: String,
            line: u32,
            character: u32,
        ) -> Result<CodeActionsResult, ProxyToolError> {
            self.record(format!("list_code_actions:{path}:{line}:{character}"));
            Ok(CodeActionsResult {
                actions: vec![CodeActionEntry {
                    action_id: String::from("action-1"),
                    title: String::from("Fix thing"),
                    kind: Some(String::from("quickfix")),
                }],
                truncated: false,
                total: 1,
            })
        }

        fn apply_code_action(
            &self,
            path: String,
            action_id: String,
        ) -> Result<EditTextResult, ProxyToolError> {
            self.record(format!("apply_code_action:{path}:{action_id}"));
            Ok(EditTextResult {
                changed_file: path,
                byte_count: 10,
                edit_count: 1,
                new_revision: String::from("rev-action"),
                saved: true,
                dirty: false,
            })
        }

        fn format_file(&self, path: String) -> Result<EditTextResult, ProxyToolError> {
            self.record(format!("format_file:{path}"));
            Ok(EditTextResult {
                changed_file: path,
                byte_count: 10,
                edit_count: 2,
                new_revision: String::from("rev-format"),
                saved: true,
                dirty: false,
            })
        }

        fn preview_rename_symbol(
            &self,
            path: String,
            line: u32,
            character: u32,
            new_name: String,
        ) -> Result<RenamePreviewResult, ProxyToolError> {
            self.record(format!("preview_rename_symbol:{path}:{line}:{character}:{new_name}"));
            Ok(RenamePreviewResult {
                files: vec![PlannedFileEdit {
                    path,
                    edits: vec![PlannedTextEdit {
                        range: TextRange {
                            start_line: line,
                            start_character: character,
                            end_line: line,
                            end_character: character.saturating_add(3),
                        },
                        new_text: new_name,
                    }],
                }],
                truncated: false,
                total_files: 1,
                total_edits: 1,
            })
        }

        fn rename_symbol(
            &self,
            path: String,
            _line: u32,
            _character: u32,
            _new_name: String,
        ) -> Result<WorkspaceEditResult, ProxyToolError> {
            self.record(format!("rename_symbol:{path}"));
            Ok(WorkspaceEditResult {
                file_count: 1,
                edit_count: 1,
                files: vec![EditTextResult {
                    changed_file: path,
                    byte_count: 10,
                    edit_count: 1,
                    new_revision: String::from("rev-rename"),
                    saved: true,
                    dirty: false,
                }],
            })
        }

        fn read_text_file(
            &self,
            path: String,
            line: Option<u32>,
            limit: Option<u32>,
        ) -> Result<String, ProxyToolError> {
            self.record(format!("read:{path}:{line:?}:{limit:?}"));
            Ok(format!("content of {path}"))
        }

        fn write_text_file(&self, path: String, content: String) -> Result<(), ProxyToolError> {
            self.record(format!("write:{path}:{content}"));
            Ok(())
        }

        fn terminal_create(
            &self,
            command: String,
            args: Vec<String>,
            cwd: Option<String>,
            env: Vec<(String, String)>,
        ) -> Result<String, ProxyToolError> {
            self.record(format!("terminal:{command}:{args:?}:{cwd:?}:{env:?}"));
            Ok("term-1".to_owned())
        }

        fn diagnostics(&self) -> Vec<String> {
            vec!["line one".to_owned(), "line two".to_owned()]
        }
    }

    /// Always denies writes, for isError-result assertions.
    #[derive(Debug, Clone, Copy, Default)]
    struct DenyWriteBackend;

    impl EeProxyBackend for DenyWriteBackend {
        fn workspace_roots(&self) -> Result<WorkspaceRootsResult, ProxyToolError> {
            Ok(WorkspaceRootsResult {
                roots: vec![String::from("/abs/work")],
                active_root: Some(String::from("/abs/work")),
                active_file: None,
                additional_directories: Vec::new(),
            })
        }

        fn list_directory(&self, _path: String) -> Result<ListDirectoryResult, ProxyToolError> {
            Ok(ListDirectoryResult { entries: Vec::new(), truncated: false })
        }

        fn list_directory_all(
            &self,
            _path: String,
        ) -> Result<ListDirectoryAllResult, ProxyToolError> {
            Ok(ListDirectoryAllResult { entries: Vec::new(), truncated: false })
        }

        fn search_files(&self, _pattern: String) -> Result<SearchFilesResult, ProxyToolError> {
            Ok(SearchFilesResult { matches: Vec::new(), truncated: false })
        }

        fn search_files_all(
            &self,
            _pattern: String,
        ) -> Result<SearchFilesAllResult, ProxyToolError> {
            Ok(SearchFilesAllResult { matches: Vec::new(), truncated: false })
        }

        fn search_text(&self, _query: String) -> Result<SearchTextResult, ProxyToolError> {
            Ok(SearchTextResult { matches: Vec::new(), truncated: false })
        }

        fn search_text_regex(&self, _pattern: String) -> Result<SearchTextResult, ProxyToolError> {
            Ok(SearchTextResult { matches: Vec::new(), truncated: false })
        }

        fn search_text_in_files(
            &self,
            _query: String,
            _file_glob: String,
        ) -> Result<SearchTextResult, ProxyToolError> {
            Ok(SearchTextResult { matches: Vec::new(), truncated: false })
        }

        fn replace_text(
            &self,
            _path: String,
            _old_text: String,
            _new_text: String,
        ) -> Result<EditTextResult, ProxyToolError> {
            Err(ProxyToolError {
                message: "no write access".to_owned(),
                is_permission_denied: true,
            })
        }

        fn apply_patch(
            &self,
            _path: String,
            _edits: Vec<TextEdit>,
        ) -> Result<EditTextResult, ProxyToolError> {
            Err(ProxyToolError {
                message: "no write access".to_owned(),
                is_permission_denied: true,
            })
        }

        fn create_text_file(
            &self,
            _path: String,
            _content: String,
        ) -> Result<EditTextResult, ProxyToolError> {
            Err(ProxyToolError {
                message: "no write access".to_owned(),
                is_permission_denied: true,
            })
        }

        fn overwrite_text_file(
            &self,
            _path: String,
            _content: String,
        ) -> Result<EditTextResult, ProxyToolError> {
            Err(ProxyToolError {
                message: "no write access".to_owned(),
                is_permission_denied: true,
            })
        }

        fn read_buffer(&self, _path: String) -> Result<String, ProxyToolError> {
            Ok(String::new())
        }

        fn read_buffer_lines(
            &self,
            _path: String,
            _line: u32,
            _limit: u32,
        ) -> Result<String, ProxyToolError> {
            Ok(String::new())
        }

        fn open_buffers(&self) -> Result<OpenBuffersResult, ProxyToolError> {
            Ok(OpenBuffersResult { buffers: Vec::new() })
        }

        fn get_diagnostics(&self) -> Result<DiagnosticsResult, ProxyToolError> {
            Ok(DiagnosticsResult { diagnostics: Vec::new(), truncated: false, total: 0 })
        }

        fn get_file_diagnostics(&self, _path: String) -> Result<DiagnosticsResult, ProxyToolError> {
            Ok(DiagnosticsResult { diagnostics: Vec::new(), truncated: false, total: 0 })
        }

        fn document_symbols(&self, _path: String) -> Result<DocumentSymbolsResult, ProxyToolError> {
            Ok(DocumentSymbolsResult { symbols: Vec::new(), truncated: false, total: 0 })
        }

        fn references(
            &self,
            _path: String,
            _line: u32,
            _character: u32,
        ) -> Result<ReferencesResult, ProxyToolError> {
            Ok(ReferencesResult { references: Vec::new(), truncated: false, total: 0 })
        }

        fn list_code_actions(
            &self,
            _path: String,
            _line: u32,
            _character: u32,
        ) -> Result<CodeActionsResult, ProxyToolError> {
            Ok(CodeActionsResult { actions: Vec::new(), truncated: false, total: 0 })
        }

        fn apply_code_action(
            &self,
            _path: String,
            _action_id: String,
        ) -> Result<EditTextResult, ProxyToolError> {
            Err(ProxyToolError {
                message: "no write access".to_owned(),
                is_permission_denied: true,
            })
        }

        fn format_file(&self, _path: String) -> Result<EditTextResult, ProxyToolError> {
            Err(ProxyToolError {
                message: "no write access".to_owned(),
                is_permission_denied: true,
            })
        }

        fn preview_rename_symbol(
            &self,
            _path: String,
            _line: u32,
            _character: u32,
            _new_name: String,
        ) -> Result<RenamePreviewResult, ProxyToolError> {
            Ok(RenamePreviewResult {
                files: Vec::new(),
                truncated: false,
                total_files: 0,
                total_edits: 0,
            })
        }

        fn rename_symbol(
            &self,
            _path: String,
            _line: u32,
            _character: u32,
            _new_name: String,
        ) -> Result<WorkspaceEditResult, ProxyToolError> {
            Err(ProxyToolError {
                message: "no write access".to_owned(),
                is_permission_denied: true,
            })
        }

        fn read_text_file(
            &self,
            _path: String,
            _line: Option<u32>,
            _limit: Option<u32>,
        ) -> Result<String, ProxyToolError> {
            Ok("unused".to_owned())
        }

        fn write_text_file(&self, _path: String, _content: String) -> Result<(), ProxyToolError> {
            Err(ProxyToolError {
                message: "no write access".to_owned(),
                is_permission_denied: true,
            })
        }

        fn terminal_create(
            &self,
            _command: String,
            _args: Vec<String>,
            _cwd: Option<String>,
            _env: Vec<(String, String)>,
        ) -> Result<String, ProxyToolError> {
            Ok("unused".to_owned())
        }

        fn diagnostics(&self) -> Vec<String> {
            Vec::new()
        }
    }

    /// Converts a `json!` literal into tool-call arguments.
    fn arguments(value: serde_json::Value) -> JsonObject {
        value.as_object().expect("arguments must be a JSON object").clone()
    }

    /// Spawns the proxy server and a 2026-07-28 Discover-mode client over one
    /// duplex transport; both handshakes must complete.
    async fn connect(
        backend: Arc<dyn EeProxyBackend>,
    ) -> (RunningService<RoleClient, TestClientHandler>, RunningService<RoleServer, EeMcpProxy>)
    {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(rmcp::serve_server(EeMcpProxy::new(backend), server_side));
        let client = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            rmcp::serve_client_with_lifecycle(
                TestClientHandler,
                client_side,
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                },
            ),
        )
        .await
        .expect("client handshake timed out")
        .expect("client handshake failed");
        let server = tokio::time::timeout(HANDSHAKE_TIMEOUT, server_task)
            .await
            .expect("server handshake timed out")
            .expect("server task panicked")
            .expect("server handshake failed");
        (client, server)
    }

    /// Shuts both services down so tests never leave tasks behind.
    fn shutdown(
        client: &RunningService<RoleClient, TestClientHandler>,
        server: &RunningService<RoleServer, EeMcpProxy>,
    ) {
        client.cancellation_token().cancel();
        server.cancellation_token().cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_list_exposes_ee_namespaced_tools() {
        let backend: Arc<dyn EeProxyBackend> = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend).await;

        let tools = tokio::time::timeout(REQUEST_TIMEOUT, client.list_all_tools())
            .await
            .expect("list tools timed out")
            .expect("list tools failed");

        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            vec![
                "ee_workspace_roots",
                "ee_list_directory",
                "ee_list_directory_all",
                "ee_search_files",
                "ee_search_files_all",
                "ee_search_text",
                "ee_search_text_regex",
                "ee_search_text_in_files",
                "ee_replace_text",
                "ee_apply_patch",
                "ee_create_text_file",
                "ee_overwrite_text_file",
                "ee_read_buffer",
                "ee_read_buffer_lines",
                "ee_open_buffers",
                "ee_get_diagnostics",
                "ee_get_file_diagnostics",
                "ee_document_symbols",
                "ee_references",
                "ee_list_code_actions",
                "ee_apply_code_action",
                "ee_format_file",
                "ee_preview_rename_symbol",
                "ee_rename_symbol",
                "ee_read_text_file",
                "ee_write_text_file",
                "ee_terminal_create",
                "ee_diagnostics",
            ]
        );
        assert!(tools.iter().all(|tool| tool.name.starts_with("ee_")));
        assert!(tools.iter().all(|tool| !tool.name.contains('.')));
        assert!(tools.iter().all(|tool| tool.input_schema.contains_key("properties")));
        assert!(tools.iter().find(|tool| tool.name == "ee_list_directory").is_some_and(|tool| {
            tool.description
                .as_ref()
                .is_some_and(|description| description.contains("host default cap"))
        }));
        assert!(tools.iter().find(|tool| tool.name == "ee_search_text_regex").is_some_and(
            |tool| {
                tool.description
                    .as_ref()
                    .is_some_and(|description| description.contains("safety-limited"))
            }
        ));

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn workspace_roots_returns_structured_content() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let result = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(CallToolRequestParams::new("ee_workspace_roots")),
        )
        .await
        .expect("call timed out")
        .expect("call failed");
        assert_eq!(result.is_error, Some(false));
        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["roots"], json!(["/abs/work", "/abs/extra"]));
        assert_eq!(structured["activeRoot"], json!("/abs/work"));
        assert_eq!(backend.calls(), vec![String::from("workspace_roots")]);

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_directory_returns_structured_content() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee_list_directory")
            .with_arguments(arguments(json!({ "path": "/abs/work" })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("call failed");
        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["entries"][0]["path"], json!("/abs/work/src"));
        assert_eq!(structured["entries"][0]["kind"], json!("directory"));
        assert_eq!(backend.calls(), vec![String::from("list_directory:/abs/work")]);

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_tools_return_structured_content() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let files = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(
                CallToolRequestParams::new("ee_search_files")
                    .with_arguments(arguments(json!({ "pattern": "src/*.rs" }))),
            ),
        )
        .await
        .expect("search_files timed out")
        .expect("search_files failed");
        assert_eq!(
            files.structured_content.expect("structured")["matches"],
            json!(["/abs/work/src/*.rs"])
        );

        let files_all = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(
                CallToolRequestParams::new("ee_search_files_all")
                    .with_arguments(arguments(json!({ "pattern": ".git/*" }))),
            ),
        )
        .await
        .expect("search_files_all timed out")
        .expect("search_files_all failed");
        assert_eq!(
            files_all.structured_content.expect("structured")["matches"][0]["hidden"],
            json!(true)
        );

        let text = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(
                CallToolRequestParams::new("ee_search_text")
                    .with_arguments(arguments(json!({ "query": "needle" }))),
            ),
        )
        .await
        .expect("search_text timed out")
        .expect("search_text failed");
        let structured = text.structured_content.expect("structured");
        assert_eq!(structured["matches"][0]["line"], json!(7));
        assert_eq!(structured["matches"][0]["context"], json!("found needle"));

        let regex = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(
                CallToolRequestParams::new("ee_search_text_regex")
                    .with_arguments(arguments(json!({ "pattern": "main" }))),
            ),
        )
        .await
        .expect("search_text_regex timed out")
        .expect("search_text_regex failed");
        assert_eq!(regex.structured_content.expect("structured")["matches"][0]["line"], json!(9));

        let scoped = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(CallToolRequestParams::new("ee_search_text_in_files").with_arguments(
                arguments(json!({ "query": "needle", "file_glob": "src/main.rs" })),
            )),
        )
        .await
        .expect("search_text_in_files timed out")
        .expect("search_text_in_files failed");
        assert_eq!(scoped.structured_content.expect("structured")["matches"][0]["line"], json!(11));

        assert_eq!(
            backend.calls(),
            vec![
                String::from("search_files:src/*.rs"),
                String::from("search_files_all:.git/*"),
                String::from("search_text:needle"),
                String::from("search_text_regex:main"),
                String::from("search_text_in_files:needle:src/main.rs"),
            ]
        );

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn phase2_tools_return_structured_content() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let replace = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(CallToolRequestParams::new("ee_replace_text").with_arguments(
                arguments(json!({
                    "path": "/abs/work/src/main.rs",
                    "old_text": "old",
                    "new_text": "new"
                })),
            )),
        )
        .await
        .expect("replace_text timed out")
        .expect("replace_text failed");
        assert_eq!(
            replace.structured_content.expect("structured")["newRevision"],
            json!("rev-replace")
        );

        let patch = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(CallToolRequestParams::new("ee_apply_patch").with_arguments(
                arguments(json!({
                    "path": "/abs/work/src/main.rs",
                    "edits": [{ "old_text": "a", "new_text": "b" }]
                })),
            )),
        )
        .await
        .expect("apply_patch timed out")
        .expect("apply_patch failed");
        assert_eq!(patch.structured_content.expect("structured")["editCount"], json!(1));

        let open_buffers = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(CallToolRequestParams::new("ee_open_buffers")),
        )
        .await
        .expect("open_buffers timed out")
        .expect("open_buffers failed");
        assert_eq!(
            open_buffers.structured_content.expect("structured")["buffers"][0]["languageId"],
            json!("rust")
        );

        let _read_buffer = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(
                CallToolRequestParams::new("ee_read_buffer")
                    .with_arguments(arguments(json!({ "path": "/abs/work/src/main.rs" }))),
            ),
        )
        .await
        .expect("read_buffer timed out")
        .expect("read_buffer failed");

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn phase2_argument_validation_rejects_invalid_patch_shapes() {
        let backend: Arc<dyn EeProxyBackend> = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend).await;

        let error = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(CallToolRequestParams::new("ee_apply_patch").with_arguments(
                arguments(json!({
                    "path": "/abs/work/src/main.rs",
                    "edits": [{ "range": [1, 2], "new_text": "x" }]
                })),
            )),
        )
        .await
        .expect("apply_patch invalid timed out")
        .expect_err("invalid patch shape must fail");
        assert!(format!("{error:?}").contains("old_text and new_text"));

        let error = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(CallToolRequestParams::new("ee_read_buffer_lines").with_arguments(
                arguments(json!({ "path": "/abs/work/src/main.rs", "line": 0, "limit": 1 })),
            )),
        )
        .await
        .expect("read_buffer_lines invalid timed out")
        .expect_err("line zero must fail");
        assert!(format!("{error:?}").contains("greater than zero"));

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_directory_all_returns_structured_content() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee_list_directory_all")
            .with_arguments(arguments(json!({ "path": "/abs/work" })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("call failed");
        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["entries"][0]["hidden"], json!(true));
        assert_eq!(structured["entries"][0]["ignored"], json!(true));
        assert_eq!(backend.calls(), vec![String::from("list_directory_all:/abs/work")]);

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_text_file_routes_to_backend() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee_read_text_file")
            .with_arguments(arguments(json!({ "path": "/abs/notes.txt", "line": 3, "limit": 10 })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("call failed");
        assert_eq!(result.is_error, Some(false));
        let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
        assert_eq!(text.text, "content of /abs/notes.txt");

        assert_eq!(backend.calls(), vec!["read:/abs/notes.txt:Some(3):Some(10)".to_owned()]);

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relative_path_is_rejected() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee_read_text_file")
            .with_arguments(arguments(json!({ "path": "relative.txt" })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out");
        assert!(result.is_err(), "relative path must surface as a protocol error");

        assert!(backend.calls().is_empty(), "backend must not be invoked");

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_text_file_result() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee_write_text_file")
            .with_arguments(arguments(json!({ "path": "/abs/out.txt", "content": "hello" })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("call failed");
        assert_eq!(result.is_error, Some(false));
        let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
        assert!(text.text.contains("wrote 5 bytes to /abs/out.txt"));

        assert_eq!(backend.calls(), vec!["write:/abs/out.txt:hello".to_owned()]);

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permission_denied_surfaces_as_is_error_result() {
        let backend: Arc<dyn EeProxyBackend> = Arc::new(DenyWriteBackend);
        let (client, server) = connect(backend).await;

        let params = CallToolRequestParams::new("ee_write_text_file")
            .with_arguments(arguments(json!({ "path": "/abs/out.txt", "content": "hello" })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("denials are tool results, not protocol errors");
        assert_eq!(result.is_error, Some(true));
        let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
        assert!(text.text.contains("no write access"), "message must reach the caller");
        assert!(text.text.starts_with("denied:"), "denials are prefixed");

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_create_rejects_secret_env() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params = CallToolRequestParams::new("ee_terminal_create").with_arguments(arguments(
            json!({ "command": "cargo", "env": { "API_TOKEN": "sekrit" } }),
        ));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out");
        assert!(result.is_err(), "secret-like env keys must be rejected");

        assert!(backend.calls().is_empty(), "backend must not be invoked");

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_create_routes_to_backend() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;

        let params =
            CallToolRequestParams::new("ee_terminal_create").with_arguments(arguments(json!({
                "command": "cargo",
                "args": ["check"],
                "cwd": "/abs/work",
                "env": { "RUST_BACKTRACE": "1" },
            })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("call failed");
        assert_eq!(result.is_error, Some(false));
        let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
        assert_eq!(text.text, "term-1");

        assert_eq!(
            backend.calls(),
            vec![
                "terminal:cargo:[\"check\"]:Some(\"/abs/work\"):[(\"RUST_BACKTRACE\", \"1\")]"
                    .to_owned()
            ]
        );

        shutdown(&client, &server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_tool_fails_closed() {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend).await;

        let params = CallToolRequestParams::new("ee_nope");
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out");
        assert!(result.is_err(), "unknown tools must be method-not-found errors");

        shutdown(&client, &server);
    }
}
