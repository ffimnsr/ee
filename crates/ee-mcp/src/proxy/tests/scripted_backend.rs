//! Scripted backend test double recording every invocation.
use std::sync::Mutex;

use super::*;

/// Records every backend invocation for assertions.
#[derive(Debug, Default)]
pub(crate) struct ScriptedBackend {
    calls: Mutex<Vec<String>>,
}

impl ScriptedBackend {
    fn record(&self, call: String) {
        self.calls.lock().expect("calls poisoned").push(call);
    }

    pub(crate) fn calls(&self) -> Vec<String> {
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

    fn list_directory_all(&self, path: String) -> Result<ListDirectoryAllResult, ProxyToolError> {
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
        Ok(SearchFilesResult { matches: vec![format!("/abs/work/{pattern}")], truncated: false })
    }

    fn search_files_all(&self, pattern: String) -> Result<SearchFilesAllResult, ProxyToolError> {
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

    fn web_search(&self, request: WebSearchRequest) -> Result<WebSearchResult, ProxyToolError> {
        self.record(format!("web_search:{}", request.query));
        Ok(WebSearchResult {
            query: request.query,
            results: vec![WebSearchEntry {
                title: String::from("Example documentation"),
                url: String::from("https://example.com/docs"),
                host: String::from("example.com"),
                snippet: String::from("Example provider snippet"),
                rank: 1,
            }],
            provenance: String::from("configured_search_backend"),
            trust: String::from("untrusted_external_content"),
            cached: true,
            truncated: false,
        })
    }

    fn fetch_url(&self, request: FetchUrlRequest) -> Result<FetchUrlResult, ProxyToolError> {
        self.record(format!("fetch_url:{}", request.url));
        Ok(FetchUrlResult {
            requested_url: request.url,
            url: String::from("https://example.com/docs"),
            title: Some(String::from("Example documentation")),
            content_type: String::from("text/html"),
            text: String::from("Example documentation body"),
            sha256: String::from("abc123"),
            retrieved_at: String::from("2026-08-25T00:00:00Z"),
            links: vec![String::from("https://example.com/next")],
            provenance: String::from("https://example.com/docs"),
            trust: String::from("untrusted_external_content"),
            cached: false,
            truncated: true,
        })
    }

    fn browser_run(&self, request: BrowserRunRequest) -> Result<BrowserRunResult, ProxyToolError> {
        let action = request.action;
        self.record(format!(
            "browser_run:{}:{}:{}:{}",
            action.as_str(),
            request.url,
            request.selector.as_deref().unwrap_or_default(),
            request.prompt.as_deref().unwrap_or_default(),
        ));
        Ok(BrowserRunResult {
            action,
            requested_url: request.url,
            content_type: match action {
                BrowserRunAction::Screenshot => String::from("image/png"),
                BrowserRunAction::Json => String::from("application/json"),
                _ => String::from("text/plain"),
            },
            result: json!({ "kind": action.as_str() }),
            truncated: false,
            trust: String::from("untrusted_external_content"),
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

    fn create_directory(&self, path: String) -> Result<FilesystemResult, ProxyToolError> {
        self.record(format!("create_directory:{path}"));
        Ok(FilesystemResult { path, destination_path: None })
    }

    fn delete_path(&self, path: String) -> Result<FilesystemResult, ProxyToolError> {
        self.record(format!("delete_path:{path}"));
        Ok(FilesystemResult { path, destination_path: None })
    }

    fn copy_path(
        &self,
        source_path: String,
        destination_path: String,
    ) -> Result<FilesystemResult, ProxyToolError> {
        self.record(format!("copy_path:{source_path}:{destination_path}"));
        Ok(FilesystemResult { path: source_path, destination_path: Some(destination_path) })
    }

    fn move_path(
        &self,
        source_path: String,
        destination_path: String,
    ) -> Result<FilesystemResult, ProxyToolError> {
        self.record(format!("move_path:{source_path}:{destination_path}"));
        Ok(FilesystemResult { path: source_path, destination_path: Some(destination_path) })
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

    fn git_status(&self) -> Result<GitStatusResult, ProxyToolError> {
        self.record(String::from("git_status"));
        Ok(GitStatusResult {
            repo_root: String::from("/abs/work"),
            branch: Some(String::from("main")),
            detached: false,
            staged: vec![String::from("staged.rs")],
            unstaged: vec![String::from("src/main.rs")],
            untracked: vec![String::from("new.rs")],
            conflicts: Vec::new(),
            file_limit: 512,
            returned_file_count: 3,
            total_file_count: 3,
            omitted_file_count: 0,
            truncated: false,
        })
    }

    fn git_diff(&self) -> Result<GitDiffResult, ProxyToolError> {
        self.record(String::from("git_diff"));
        Ok(GitDiffResult {
            diff: String::from("diff --git a/src/main.rs b/src/main.rs\n"),
            bytes_returned: 40,
            byte_limit: 1024,
            truncated: false,
        })
    }

    fn git_diff_staged(&self) -> Result<GitDiffResult, ProxyToolError> {
        self.record(String::from("git_diff_staged"));
        Ok(GitDiffResult {
            diff: String::from("diff --git a/staged.rs b/staged.rs\n"),
            bytes_returned: 38,
            byte_limit: 1024,
            truncated: false,
        })
    }

    fn git_diff_file(&self, path: String) -> Result<GitDiffResult, ProxyToolError> {
        self.record(format!("git_diff_file:{path}"));
        Ok(GitDiffResult {
            diff: format!("diff --git a/{path} b/{path}\n"),
            bytes_returned: 32,
            byte_limit: 1024,
            truncated: false,
        })
    }

    fn changed_files(&self) -> Result<ChangedFilesResult, ProxyToolError> {
        self.record(String::from("changed_files"));
        Ok(ChangedFilesResult {
            files: vec![ChangedFileEntry {
                path: String::from("/abs/work/src/main.rs"),
                staged: false,
                unstaged: true,
                untracked: false,
                conflicted: false,
                dirty: true,
                saved: false,
            }],
            file_limit: 512,
            total_file_count: 1,
            omitted_file_count: 0,
            truncated: false,
        })
    }

    fn review_context(&self) -> Result<ReviewContextResult, ProxyToolError> {
        self.record(String::from("review_context"));
        Ok(ReviewContextResult {
            changed_files: self.changed_files()?,
            diagnostics: DiagnosticsResult { diagnostics: Vec::new(), truncated: false, total: 0 },
            nearby_symbols: Vec::new(),
            symbols_truncated: false,
            test_suggestions: Vec::new(),
        })
    }

    fn turn_evidence_summary(
        &self,
        session_id: Option<String>,
        turn_id: Option<u64>,
    ) -> Result<serde_json::Value, ProxyToolError> {
        self.record(format!("turn_evidence_summary:{session_id:?}:{turn_id:?}"));
        if session_id.as_deref() == Some("foreign") || turn_id == Some(99) {
            return Err(ProxyToolError {
                message: String::from("evidence_unavailable: turn evidence is unavailable"),
                is_permission_denied: false,
            });
        }
        Ok(json!({
            "key": { "agent_id": "agent-1", "session_id": "session-1", "turn_id": 1 },
            "status": "unverified",
            "blocker": "missing_revision",
            "safe_follow_up": "collect_current_revision",
            "evidence_ids": ["turn:1:evidence:1"],
        }))
    }

    fn remember_workspace_fact(
        &self,
        key: String,
        value: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        self.record(format!("remember_workspace_fact:{key}:{value}"));
        Ok(WorkspaceFactMutationResult {
            operation: String::from("remembered"),
            key: key.clone(),
            affected: 1,
            fact: Some(workspace_fact(&key, None)),
        })
    }

    fn recall_workspace_facts(
        &self,
        query: String,
    ) -> Result<WorkspaceFactsResult, ProxyToolError> {
        self.record(format!("recall_workspace_facts:{query}"));
        Ok(WorkspaceFactsResult {
            facts: vec![workspace_fact("architecture.parser", Some("full_text"))],
            total: 1,
            omitted: 0,
            truncated: false,
        })
    }

    fn read_workspace_fact(&self, key: String) -> Result<WorkspaceFact, ProxyToolError> {
        self.record(format!("read_workspace_fact:{key}"));
        Ok(workspace_fact(&key, Some("exact_key")))
    }

    fn forget_workspace_fact(
        &self,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        self.record(format!("forget_workspace_fact:{key}"));
        Ok(WorkspaceFactMutationResult {
            operation: String::from("forgotten"),
            key,
            affected: 1,
            fact: None,
        })
    }

    fn list_workspace_facts(&self, limit: u32) -> Result<WorkspaceFactsResult, ProxyToolError> {
        self.record(format!("list_workspace_facts:{limit}"));
        Ok(WorkspaceFactsResult {
            facts: vec![workspace_fact("architecture.parser", Some("key_prefix"))],
            total: 1,
            omitted: 0,
            truncated: false,
        })
    }

    fn retract_workspace_fact(
        &self,
        key: String,
    ) -> Result<WorkspaceFactMutationResult, ProxyToolError> {
        self.record(format!("retract_workspace_fact:{key}"));
        Ok(WorkspaceFactMutationResult {
            operation: String::from("retracted"),
            key,
            affected: 1,
            fact: None,
        })
    }

    fn export_workspace_memory(
        &self,
        include_values: bool,
    ) -> Result<serde_json::Value, ProxyToolError> {
        self.record(format!("export_workspace_memory:{include_values}"));
        Ok(json!({ "schema_version": 1, "redacted": !include_values, "facts": [] }))
    }

    fn import_workspace_memory(
        &self,
        export_json: String,
    ) -> Result<serde_json::Value, ProxyToolError> {
        self.record(format!("import_workspace_memory:{}", export_json.len()));
        Ok(json!({ "operation": "imported", "affected": 1 }))
    }

    fn clear_workspace_memory(&self) -> Result<serde_json::Value, ProxyToolError> {
        self.record(String::from("clear_workspace_memory"));
        Ok(json!({ "operation": "cleared", "affected": 1 }))
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

    fn terminal_output(&self, terminal_id: String) -> Result<TerminalOutputResult, ProxyToolError> {
        self.record(format!("terminal_output:{terminal_id}"));
        Ok(TerminalOutputResult {
            output: String::from("output"),
            chunks: vec![TerminalOutputChunk {
                sequence: 1,
                stream: String::from("stdout"),
                text: String::from("output"),
            }],
            total_bytes: 6,
            truncated: false,
            exit_status: None,
            running: true,
            elapsed_ms: 1_000,
        })
    }

    fn terminal_wait(&self, terminal_id: String) -> Result<TerminalWaitResult, ProxyToolError> {
        self.record(format!("terminal_wait:{terminal_id}"));
        Ok(TerminalWaitResult { completed: true, exit_status: Some(json!({ "exitCode": 0 })) })
    }

    fn terminal_kill(&self, terminal_id: String) -> Result<(), ProxyToolError> {
        self.record(format!("terminal_kill:{terminal_id}"));
        Ok(())
    }

    fn terminal_release(&self, terminal_id: String) -> Result<(), ProxyToolError> {
        self.record(format!("terminal_release:{terminal_id}"));
        Ok(())
    }

    fn diagnostics(&self) -> Vec<String> {
        vec!["line one".to_owned(), "line two".to_owned()]
    }
}
