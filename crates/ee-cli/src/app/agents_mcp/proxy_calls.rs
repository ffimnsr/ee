//! Proxy call/reply wire types.
use super::*;

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub(crate) enum ProxyCall {
    RememberWorkspaceFact {
        key: String,
        value: String,
    },
    RecallWorkspaceFacts {
        query: String,
    },
    ReadWorkspaceFact {
        key: String,
    },
    ForgetWorkspaceFact {
        key: String,
    },
    ListWorkspaceFacts {
        limit: u32,
    },
    RetractWorkspaceFact {
        key: String,
    },
    ExportWorkspaceMemory {
        include_values: bool,
    },
    ImportWorkspaceMemory {
        export_json: String,
    },
    ClearWorkspaceMemory,
    WorkspaceRoots,
    ListDirectory {
        path: String,
    },
    ListDirectoryAll {
        path: String,
    },
    SearchFiles {
        pattern: String,
    },
    SearchFilesAll {
        pattern: String,
    },
    SearchText {
        query: String,
    },
    SearchTextRegex {
        pattern: String,
    },
    WebSearch {
        query: String,
    },
    FetchUrl {
        url: String,
    },
    BrowserRun {
        request: ee_mcp::BrowserRunRequest,
    },
    SearchTextInFiles {
        query: String,
        file_glob: String,
    },
    ReplaceText {
        path: String,
        old_text: String,
        new_text: String,
    },
    ApplyPatch {
        path: String,
        edits: Vec<ee_mcp::TextEdit>,
    },
    CreateTextFile {
        path: String,
        content: String,
    },
    OverwriteTextFile {
        path: String,
        content: String,
    },
    CreateDirectory {
        path: String,
    },
    DeletePath {
        path: String,
    },
    CopyPath {
        source_path: String,
        destination_path: String,
    },
    MovePath {
        source_path: String,
        destination_path: String,
    },
    ReadBuffer {
        path: String,
    },
    ReadBufferLines {
        path: String,
        line: u32,
        limit: u32,
    },
    OpenBuffers,
    GetDiagnostics,
    GetFileDiagnostics {
        path: String,
    },
    DocumentSymbols {
        path: String,
    },
    References {
        path: String,
        line: u32,
        character: u32,
    },
    ListCodeActions {
        path: String,
        line: u32,
        character: u32,
    },
    ApplyCodeAction {
        path: String,
        action_id: String,
    },
    FormatFile {
        path: String,
    },
    PreviewRenameSymbol {
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    },
    RenameSymbol {
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    },
    GitStatus,
    GitDiff,
    GitDiffStaged,
    GitDiffFile {
        path: String,
    },
    ChangedFiles,
    ReviewContext,
    ProjectInstructions,
    SaveNote {
        key: String,
        content: String,
    },
    ReadNotes,
    ReadNote {
        key: String,
    },
    FileDependencyMap {
        path: String,
    },
    SymbolDependencyMap {
        path: String,
        line: u32,
        character: u32,
    },
    ReadTextFile {
        path: String,
        line: Option<u32>,
        limit: Option<u32>,
    },
    WriteTextFile {
        path: String,
        content: String,
    },
    TerminalCreate {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        cwd: Option<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    Diagnostics,
}

/// The reply to one proxy tool call.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum ProxyReply {
    Ok { value: serde_json::Value },
    Err { error: ProxyErrorBody },
}

/// Error body of a denied/failed proxy tool call.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct ProxyErrorBody {
    pub(crate) message: String,
    pub(crate) denied: bool,
}

impl ProxyReply {
    pub(super) fn from_client_result(result: ClientRequestResult) -> Self {
        match result {
            Ok(ClientRequestResponse::ProxyValue(response)) => Self::Ok { value: response },
            Ok(ClientRequestResponse::ReadTextFile(response)) => {
                Self::Ok { value: serde_json::Value::String(response.content) }
            }
            Ok(ClientRequestResponse::WriteTextFile(_)) => {
                Self::Ok { value: serde_json::Value::String(String::from("ok")) }
            }
            Ok(ClientRequestResponse::CreateTerminal(response)) => {
                Self::Ok { value: serde_json::Value::String(response.terminal_id.0.to_string()) }
            }
            // Diagnostics are carried as terminal-output text internally
            // (transport-only mapping; never crosses the ACP wire).
            Ok(ClientRequestResponse::TerminalOutput(response)) => {
                Self::Ok { value: serde_json::Value::String(response.output) }
            }
            Ok(_) => Self::Ok { value: serde_json::Value::String(String::from("ok")) },
            Err(error) => Self::Err {
                error: ProxyErrorBody {
                    message: error.to_string(),
                    denied: matches!(
                        error,
                        AgentError::PermissionDenied { .. }
                            | AgentError::NonOverridableDenied { .. }
                    ),
                },
            },
        }
    }
}

/// Reads one bounded line from a buffered stream.
pub(super) async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let read = tokio::io::AsyncBufReadExt::read_line(reader, &mut line).await?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame exceeds the {cap}-byte cap"),
        ));
    }
    Ok(Some(line.trim_end().to_string()))
}

/// A proxy tool call forwarded to the pane (bridge message payload).  The
/// requests use the same ACP wire types as direct client methods so the
/// approval and bridge paths are shared verbatim.
pub(crate) enum ProxyToolCall {
    RememberWorkspaceFact {
        key: String,
        value: String,
    },
    RecallWorkspaceFacts {
        query: String,
    },
    ReadWorkspaceFact {
        key: String,
    },
    ForgetWorkspaceFact {
        key: String,
    },
    ListWorkspaceFacts {
        limit: u32,
    },
    RetractWorkspaceFact {
        key: String,
    },
    ExportWorkspaceMemory {
        include_values: bool,
    },
    ImportWorkspaceMemory {
        export_json: String,
    },
    ClearWorkspaceMemory,
    WorkspaceRoots,
    ListDirectory {
        path: String,
    },
    ListDirectoryAll {
        path: String,
    },
    SearchFiles {
        pattern: String,
    },
    SearchFilesAll {
        pattern: String,
    },
    SearchText {
        query: String,
    },
    SearchTextRegex {
        pattern: String,
    },
    WebSearch {
        query: String,
        approval_scope: String,
        cancellation: tokio_util::sync::CancellationToken,
    },
    FetchUrl {
        url: String,
        approval_scope: String,
        cancellation: tokio_util::sync::CancellationToken,
    },
    BrowserRun {
        request: ee_mcp::BrowserRunRequest,
        approval_scope: String,
        cancellation: tokio_util::sync::CancellationToken,
    },
    SearchTextInFiles {
        query: String,
        file_glob: String,
    },
    ReplaceText {
        path: String,
        old_text: String,
        new_text: String,
    },
    ApplyPatch {
        path: String,
        edits: Vec<ee_agent_host::ProxyTextEdit>,
    },
    CreateTextFile {
        path: String,
        content: String,
    },
    OverwriteTextFile {
        path: String,
        content: String,
    },
    CreateDirectory {
        path: String,
    },
    DeletePath {
        path: String,
    },
    CopyPath {
        source_path: String,
        destination_path: String,
    },
    MovePath {
        source_path: String,
        destination_path: String,
    },
    ReadBuffer {
        path: String,
    },
    ReadBufferLines {
        path: String,
        line: u32,
        limit: u32,
    },
    OpenBuffers,
    GetDiagnostics,
    GetFileDiagnostics {
        path: String,
    },
    DocumentSymbols {
        path: String,
    },
    References {
        path: String,
        line: u32,
        character: u32,
    },
    ListCodeActions {
        path: String,
        line: u32,
        character: u32,
    },
    ApplyCodeAction {
        path: String,
        action_id: String,
    },
    FormatFile {
        path: String,
    },
    PreviewRenameSymbol {
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    },
    RenameSymbol {
        path: String,
        line: u32,
        character: u32,
        new_name: String,
    },
    GitStatus,
    GitDiff,
    GitDiffStaged,
    GitDiffFile {
        path: String,
    },
    ChangedFiles,
    ReviewContext,
    ProjectInstructions,
    SaveNote {
        scope: String,
        key: String,
        content: String,
    },
    ReadNotes {
        scope: String,
    },
    ReadNote {
        scope: String,
        key: String,
    },
    FileDependencyMap {
        path: String,
    },
    SymbolDependencyMap {
        path: String,
        line: u32,
        character: u32,
    },
    Read(ReadTextFileRequest),
    Write(WriteTextFileRequest),
    Terminal(CreateTerminalRequest),
    Diagnostics,
}
