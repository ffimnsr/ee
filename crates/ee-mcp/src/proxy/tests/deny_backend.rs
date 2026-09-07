//! Write-denying backend double for isError permission assertions.
use super::*;

/// Always denies writes, for isError-result assertions.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DenyWriteBackend;

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

    fn list_directory_all(&self, _path: String) -> Result<ListDirectoryAllResult, ProxyToolError> {
        Ok(ListDirectoryAllResult { entries: Vec::new(), truncated: false })
    }

    fn search_files(&self, _pattern: String) -> Result<SearchFilesResult, ProxyToolError> {
        Ok(SearchFilesResult { matches: Vec::new(), truncated: false })
    }

    fn search_files_all(&self, _pattern: String) -> Result<SearchFilesAllResult, ProxyToolError> {
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
        Err(ProxyToolError { message: "no write access".to_owned(), is_permission_denied: true })
    }

    fn apply_patch(
        &self,
        _path: String,
        _edits: Vec<TextEdit>,
    ) -> Result<EditTextResult, ProxyToolError> {
        Err(ProxyToolError { message: "no write access".to_owned(), is_permission_denied: true })
    }

    fn create_text_file(
        &self,
        _path: String,
        _content: String,
    ) -> Result<EditTextResult, ProxyToolError> {
        Err(ProxyToolError { message: "no write access".to_owned(), is_permission_denied: true })
    }

    fn overwrite_text_file(
        &self,
        _path: String,
        _content: String,
    ) -> Result<EditTextResult, ProxyToolError> {
        Err(ProxyToolError { message: "no write access".to_owned(), is_permission_denied: true })
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
        Err(ProxyToolError { message: "no write access".to_owned(), is_permission_denied: true })
    }

    fn format_file(&self, _path: String) -> Result<EditTextResult, ProxyToolError> {
        Err(ProxyToolError { message: "no write access".to_owned(), is_permission_denied: true })
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
        Err(ProxyToolError { message: "no write access".to_owned(), is_permission_denied: true })
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
        Err(ProxyToolError { message: "no write access".to_owned(), is_permission_denied: true })
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

    fn terminal_output(
        &self,
        _terminal_id: String,
    ) -> Result<TerminalOutputResult, ProxyToolError> {
        Err(ProxyToolError {
            message: String::from("no terminal access"),
            is_permission_denied: true,
        })
    }

    fn terminal_wait(&self, _terminal_id: String) -> Result<TerminalWaitResult, ProxyToolError> {
        Err(ProxyToolError {
            message: String::from("no terminal access"),
            is_permission_denied: true,
        })
    }

    fn terminal_kill(&self, _terminal_id: String) -> Result<(), ProxyToolError> {
        Err(ProxyToolError {
            message: String::from("no terminal access"),
            is_permission_denied: true,
        })
    }

    fn terminal_release(&self, _terminal_id: String) -> Result<(), ProxyToolError> {
        Err(ProxyToolError {
            message: String::from("no terminal access"),
            is_permission_denied: true,
        })
    }

    fn diagnostics(&self) -> Vec<String> {
        Vec::new()
    }
}
