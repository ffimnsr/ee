//! DTOs and byte caps shared by the ee MCP proxy tool surface.
use serde::{Deserialize, Serialize};

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

/// One bounded stdout or stderr chunk returned by `ee_terminal_output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutputChunk {
    pub sequence: u64,
    pub stream: String,
    pub text: String,
}

/// Bounded output snapshot returned by `ee_terminal_output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOutputResult {
    pub output: String,
    pub chunks: Vec<TerminalOutputChunk>,
    pub total_bytes: u64,
    pub truncated: bool,
    pub exit_status: Option<serde_json::Value>,
    /// Whether process remains active when snapshot was taken.
    #[serde(default)]
    pub running: bool,
    /// Monotonic lifetime from spawn through snapshot, in milliseconds.
    #[serde(default)]
    pub elapsed_ms: u64,
}

/// Completion state returned by `ee_terminal_wait`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalWaitResult {
    pub completed: bool,
    pub exit_status: Option<serde_json::Value>,
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

/// Flat request accepted by `ee_web_search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchRequest {
    pub query: String,
}

/// One bounded result returned by `ee_web_search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchEntry {
    pub title: String,
    pub url: String,
    pub host: String,
    pub snippet: String,
    pub rank: u32,
}

/// Structured result returned by `ee_web_search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResult {
    pub query: String,
    pub results: Vec<WebSearchEntry>,
    /// Immutable provider/source identity.
    pub provenance: String,
    /// Remote data label. Agents must never treat result text as instructions.
    pub trust: String,
    pub cached: bool,
    pub truncated: bool,
}

/// Flat request accepted by `ee_fetch_url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchUrlRequest {
    pub url: String,
}

/// Structured result returned by `ee_fetch_url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchUrlResult {
    pub requested_url: String,
    pub url: String,
    pub title: Option<String>,
    pub content_type: String,
    pub text: String,
    pub sha256: String,
    pub retrieved_at: String,
    pub links: Vec<String>,
    /// Immutable provider/source identity.
    pub provenance: String,
    /// Remote data label. Agents must never treat result text as instructions.
    pub trust: String,
    pub cached: bool,
    pub truncated: bool,
}

/// Browser operation selected by one dedicated `ee_browser_run_*` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserRunAction {
    Content,
    Screenshot,
    Markdown,
    Scrape,
    Json,
    Links,
}

impl BrowserRunAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Screenshot => "screenshot",
            Self::Markdown => "markdown",
            Self::Scrape => "scrape",
            Self::Json => "json",
            Self::Links => "links",
        }
    }
}

/// Request routed from one `ee_browser_run_*` tool to the configured browser backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserRunRequest {
    pub action: BrowserRunAction,
    pub url: String,
    pub selector: Option<String>,
    pub prompt: Option<String>,
}

/// Bounded generic browser result. Remote content remains untrusted data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRunResult {
    pub action: BrowserRunAction,
    pub requested_url: String,
    pub content_type: String,
    pub result: serde_json::Value,
    pub truncated: bool,
    pub trust: String,
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

/// Structured success result for filesystem mutation tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemResult {
    /// Created, deleted, copied, or moved source path.
    pub path: String,
    /// Copy or move destination; absent for create and delete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_path: Option<String>,
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

/// Bounded SCM status returned by `ee_git_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusResult {
    pub repo_root: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub untracked: Vec<String>,
    pub conflicts: Vec<String>,
    pub file_limit: u32,
    pub returned_file_count: u32,
    pub total_file_count: u32,
    pub omitted_file_count: u32,
    pub truncated: bool,
}

/// Bounded unified diff returned by `ee_git_diff` tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffResult {
    pub diff: String,
    pub bytes_returned: u64,
    pub byte_limit: u64,
    pub truncated: bool,
}

/// One source-control change merged with editor buffer state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFileEntry {
    pub path: String,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
    pub conflicted: bool,
    pub dirty: bool,
    pub saved: bool,
}

/// Bounded changed-file result returned by `ee_changed_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFilesResult {
    pub files: Vec<ChangedFileEntry>,
    pub file_limit: u32,
    pub total_file_count: u32,
    pub omitted_file_count: u32,
    pub truncated: bool,
}

/// One bounded workspace-local instruction or safe configuration summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInstructionSource {
    pub path: String,
    pub kind: String,
    pub precedence: u32,
    pub content: String,
    pub truncated: bool,
}

/// Structured workspace guidance returned by `ee_project_instructions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInstructionsResult {
    pub root: String,
    pub sources: Vec<ProjectInstructionSource>,
    pub tool_constraints: Vec<String>,
    pub truncated: bool,
}

/// One bounded non-secret note scoped to trusted proxy connection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNoteResult {
    pub key: String,
    pub content: String,
    pub bytes: u32,
    pub truncated: bool,
}

/// Bounded note listing for current proxy connection scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotesResult {
    pub notes: Vec<SessionNoteResult>,
    pub note_limit: u32,
    pub total_note_count: u32,
    pub omitted_note_count: u32,
    pub truncated: bool,
}

/// Stable provenance attached to one durable workspace fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFactProvenance {
    pub source_kind: String,
    pub source_id: String,
    pub revision: Option<String>,
    pub fingerprint: Option<String>,
    pub verified_at: Option<String>,
}

/// One provenance-rich workspace fact exposed at the MCP boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFact {
    pub id: i64,
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub kind: String,
    pub authority: String,
    pub freshness: String,
    pub state: String,
    pub provenance: WorkspaceFactProvenance,
    pub selection_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: Option<String>,
    pub content_hash: String,
    pub schema_version: u32,
}

/// Bounded workspace-fact recall result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFactsResult {
    pub facts: Vec<WorkspaceFact>,
    pub total: u64,
    pub omitted: u64,
    pub truncated: bool,
}

/// Result of one approved workspace-fact mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFactMutationResult {
    pub operation: String,
    pub key: String,
    pub affected: u64,
    pub fact: Option<WorkspaceFact>,
}

/// One known file dependency edge from an optional editor index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDependencyEdge {
    pub path: String,
    pub kind: String,
}

/// Bounded result from an optional editor-owned file dependency index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDependencyMapResult {
    pub path: String,
    pub available: bool,
    pub reason: Option<String>,
    pub freshness: String,
    pub indexed_at: Option<String>,
    pub outgoing: Vec<FileDependencyEdge>,
    pub incoming: Vec<FileDependencyEdge>,
    pub truncated: bool,
}

/// One source span in a Tree-sitter dependency map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDependencyLocation {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDependencyRelation {
    pub symbol: SymbolDependencyLocation,
    pub relation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDependencyModuleHint {
    pub name: String,
    pub kind: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDependencyRelatedFile {
    pub path: String,
    pub relation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDependencyTotals {
    pub callers: u32,
    pub callees: u32,
    pub implementations: u32,
    pub tests: u32,
    pub module_hints: u32,
    pub related_files: u32,
    pub omitted_callers: u32,
    pub omitted_callees: u32,
    pub omitted_implementations: u32,
    pub omitted_tests: u32,
    pub omitted_module_hints: u32,
    pub omitted_related_files: u32,
}

/// Bounded syntax-only symbol graph result. `freshness` is always `fresh` on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDependencyMapResult {
    pub path: String,
    pub line: u32,
    pub character: u32,
    pub symbol: SymbolDependencyLocation,
    pub definition: SymbolDependencyLocation,
    pub callers: Vec<SymbolDependencyRelation>,
    pub callees: Vec<SymbolDependencyRelation>,
    pub implementations: Vec<SymbolDependencyRelation>,
    pub tests: Vec<SymbolDependencyLocation>,
    pub module_hints: Vec<SymbolDependencyModuleHint>,
    pub related_files: Vec<SymbolDependencyRelatedFile>,
    pub totals: SymbolDependencyTotals,
    pub truncated: bool,
    pub freshness: String,
    pub graph_version: String,
    pub indexed_at: Option<String>,
}

/// Bounded read-only context for final review. `test_suggestions` never execute automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewContextResult {
    pub changed_files: ChangedFilesResult,
    pub diagnostics: DiagnosticsResult,
    pub nearby_symbols: Vec<DocumentSymbolEntry>,
    pub symbols_truncated: bool,
    pub test_suggestions: Vec<String>,
}

/// One bounded output limit advertised by the tools manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutputCap {
    pub kind: String,
    pub max: u64,
}

/// One stable ee tool contract. Incompatible changes require a new tool name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolManifestEntry {
    /// Stable tool identifier. Incompatible changes require a new name.
    pub name: String,
    /// Version of this tool's schema contract.
    pub schema_version: u64,
    /// Complete MCP input schema.
    pub input_schema: serde_json::Value,
    /// `read`, `write`, or `execute`.
    pub side_effect: String,
    /// `none` or `required`; host trust rules may satisfy an approval.
    pub approval: String,
    /// Implemented MCP routes: `stdio` and/or ACP-native `acp`.
    pub transport_availability: Vec<String>,
    /// Host capabilities required before the tool may be advertised.
    pub required_capabilities: Vec<String>,
    /// Bounded output dimensions.
    pub output_caps: Vec<ToolOutputCap>,
    /// Values removed or rejected before output reaches an agent.
    pub redaction_rules: Vec<String>,
    /// Stable tool-level failures callers must handle.
    pub error_classes: Vec<String>,
    /// Whether callers should migrate away from this name.
    pub deprecated: bool,
    /// Replacement name supplied before a retirement, when deprecated.
    pub replacement: Option<String>,
    /// Minimal schema-valid invocation arguments.
    pub example: serde_json::Value,
}

/// Versioned, session-cacheable ee proxy contract returned by `ee_tools_manifest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsManifestResult {
    pub manifest_version: u64,
    pub tools: Vec<ToolManifestEntry>,
}

/// Maximum serialized argument object accepted for one ee proxy tool call.
///
/// Individual tools may impose tighter semantic limits. This common boundary
/// prevents nested JSON or content-bearing arguments from exhausting proxy or
/// host memory before tool-specific validation runs.
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes accepted for a workspace-fact key.
pub const MAX_WORKSPACE_FACT_KEY_BYTES: usize = 128;
/// Maximum UTF-8 bytes accepted for a workspace-fact value.
pub const MAX_WORKSPACE_FACT_VALUE_BYTES: usize = 4 * 1024;
/// Maximum UTF-8 bytes accepted for a workspace-fact recall query.
pub const MAX_WORKSPACE_FACT_QUERY_BYTES: usize = 1024;
/// Maximum active facts returned by one explicit list call.
pub const MAX_WORKSPACE_FACT_LIST_RESULTS: u32 = 256;
/// Maximum UTF-8 bytes accepted for one versioned import JSON payload.
pub const MAX_WORKSPACE_MEMORY_IMPORT_BYTES: usize = 60 * 1024;
