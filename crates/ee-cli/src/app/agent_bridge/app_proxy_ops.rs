//! `impl App`: git, review-context, code-action, rename, and dependency-map proxies.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ee_agent_host::{AgentError, ClientRequestResponse, ClientRequestResult};
use tokio::sync::oneshot;

use super::super::*;

use super::app_search::{
    PROXY_CODE_ACTIONS_LIMIT, PROXY_DIAGNOSTICS_LIMIT, PROXY_DOCUMENT_SYMBOLS_LIMIT,
    PROXY_REFERENCES_LIMIT, PROXY_RENAME_EDITS_LIMIT, PROXY_RENAME_FILES_LIMIT,
    PROXY_REVIEW_SYMBOL_FILE_LIMIT, PROXY_REVIEW_SYMBOLS_LIMIT, paths_equivalent,
};
use super::approval::{
    PERSISTENT_TERMINAL_OPTION_LABEL, PreparedWrite, ProxyWriteSpec, WriteExpectation,
    WriteReplyKind,
};
use super::prompt::ApprovalPrompt;
use super::write::{
    AgentCodeActionPayload, AgentDocumentSymbolsPayload, AgentReferencesPayload,
    AgentRenamePayload, AgentTextEditsPayload, apply_planned_text_edits_to_content,
    buffer_revision_id, buffer_saved_state, text_revision_id,
};

impl App {
    pub(super) fn proxy_git_status(&self) -> Result<serde_json::Value, AgentError> {
        let report = self
            .proxy_git_repository()?
            .status(crate::git::GitReadLimits::default())
            .map_err(|error| AgentError::HandlerError(format!("Git status failed: {error}")))?;
        let result = ee_mcp::GitStatusResult {
            repo_root: report.repo_root.display().to_string(),
            branch: report.branch,
            detached: report.detached,
            staged: report
                .staged
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            unstaged: report
                .unstaged
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            untracked: report
                .untracked
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            conflicts: report
                .conflicts
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            file_limit: u32::try_from(report.file_limit).unwrap_or(u32::MAX),
            returned_file_count: u32::try_from(report.returned_file_count).unwrap_or(u32::MAX),
            total_file_count: u32::try_from(report.total_file_count).unwrap_or(u32::MAX),
            omitted_file_count: u32::try_from(report.omitted_file_count).unwrap_or(u32::MAX),
            truncated: report.truncated,
        };
        serde_json::to_value(result).map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_git_diff(&self) -> Result<serde_json::Value, AgentError> {
        let diff = self
            .proxy_git_repository()?
            .unstaged_diff(crate::git::GitReadLimits::default())
            .map_err(|error| AgentError::HandlerError(format!("Git diff failed: {error}")))?;
        self.proxy_git_diff_value(diff)
    }

    pub(super) fn proxy_git_diff_staged(&self) -> Result<serde_json::Value, AgentError> {
        let diff = self
            .proxy_git_repository()?
            .staged_diff(crate::git::GitReadLimits::default())
            .map_err(|error| {
            AgentError::HandlerError(format!("Git staged diff failed: {error}"))
        })?;
        self.proxy_git_diff_value(diff)
    }

    pub(super) fn proxy_git_diff_file(&self, path: &Path) -> Result<serde_json::Value, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        if !self.path_in_effective_workspace(path) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )));
        }
        let diff = self
            .proxy_git_repository()?
            .unstaged_diff_for_path(path, crate::git::GitReadLimits::default())
            .map_err(|error| AgentError::HandlerError(format!("Git file diff failed: {error}")))?;
        self.proxy_git_diff_value(diff)
    }

    fn proxy_git_diff_value(
        &self,
        diff: crate::git::GitDiff,
    ) -> Result<serde_json::Value, AgentError> {
        serde_json::to_value(ee_mcp::GitDiffResult {
            diff: diff.text,
            bytes_returned: u64::try_from(diff.bytes_returned).unwrap_or(u64::MAX),
            byte_limit: u64::try_from(diff.byte_limit).unwrap_or(u64::MAX),
            truncated: diff.truncated,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_changed_files_result(
        &self,
    ) -> Result<ee_mcp::ChangedFilesResult, AgentError> {
        let repository = self.proxy_git_repository()?;
        let report = repository
            .status(crate::git::GitReadLimits::default())
            .map_err(|error| AgentError::HandlerError(format!("Git status failed: {error}")))?;
        let mut files = BTreeMap::<PathBuf, ee_mcp::ChangedFileEntry>::new();
        let mut insert_status =
            |path: &Path, staged: bool, unstaged: bool, untracked: bool, conflicted: bool| {
                let path = report.repo_root.join(path);
                let entry = files.entry(path.clone()).or_insert_with(|| ee_mcp::ChangedFileEntry {
                    path: path.display().to_string(),
                    staged: false,
                    unstaged: false,
                    untracked: false,
                    conflicted: false,
                    dirty: false,
                    saved: true,
                });
                entry.staged |= staged;
                entry.unstaged |= unstaged;
                entry.untracked |= untracked;
                entry.conflicted |= conflicted;
            };
        for path in &report.staged {
            insert_status(path, true, false, false, false);
        }
        for path in &report.unstaged {
            insert_status(path, false, true, false, false);
        }
        for path in &report.untracked {
            insert_status(path, false, false, true, false);
        }
        for path in &report.conflicts {
            insert_status(path, false, false, false, true);
        }
        for buffer in self.backend.all_bufs() {
            let Some(path) = &buffer.path else { continue };
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            if !canonical.starts_with(repository.root()) {
                continue;
            }
            let entry =
                files.entry(canonical.clone()).or_insert_with(|| ee_mcp::ChangedFileEntry {
                    path: canonical.display().to_string(),
                    staged: false,
                    unstaged: false,
                    untracked: false,
                    conflicted: false,
                    dirty: false,
                    saved: true,
                });
            entry.dirty = !buffer.pristine;
            entry.saved = buffer_saved_state(buffer);
        }
        let file_limit = report.file_limit;
        let mut files = files.into_values().collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        files.retain(|entry| {
            entry.staged || entry.unstaged || entry.untracked || entry.conflicted || entry.dirty
        });
        let total_file_count = report.total_file_count.max(files.len());
        files.truncate(file_limit);
        let omitted_file_count = total_file_count.saturating_sub(files.len());
        Ok(ee_mcp::ChangedFilesResult {
            files,
            file_limit: u32::try_from(file_limit).unwrap_or(u32::MAX),
            total_file_count: u32::try_from(total_file_count).unwrap_or(u32::MAX),
            omitted_file_count: u32::try_from(omitted_file_count).unwrap_or(u32::MAX),
            truncated: report.truncated || omitted_file_count > 0,
        })
    }

    pub(super) fn proxy_changed_files(&self) -> Result<serde_json::Value, AgentError> {
        serde_json::to_value(self.proxy_changed_files_result()?)
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(crate) fn proxy_review_context(&mut self) -> Result<serde_json::Value, AgentError> {
        let changed_files = self.proxy_changed_files_result()?;
        let changed_paths =
            changed_files.files.iter().map(|entry| entry.path.as_str()).collect::<BTreeSet<_>>();
        let mut diagnostics = self
            .proxy_diagnostic_entries(None)
            .into_iter()
            .filter(|entry| changed_paths.contains(entry.path.as_str()))
            .collect::<Vec<_>>();
        diagnostics.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.range.start_line.cmp(&right.range.start_line))
                .then(left.range.start_character.cmp(&right.range.start_character))
        });
        let diagnostic_total = u32::try_from(diagnostics.len()).unwrap_or(u32::MAX);
        let diagnostics_truncated = diagnostics.len() > PROXY_DIAGNOSTICS_LIMIT;
        diagnostics.truncate(PROXY_DIAGNOSTICS_LIMIT);

        let mut nearby_symbols = Vec::new();
        let mut symbols_truncated = false;
        for entry in changed_files.files.iter().take(PROXY_REVIEW_SYMBOL_FILE_LIMIT) {
            if nearby_symbols.len() >= PROXY_REVIEW_SYMBOLS_LIMIT {
                symbols_truncated = true;
                break;
            }
            let path = Path::new(&entry.path);
            if self.buffer_id_for_path(path).is_none() {
                continue;
            }
            let payload = match self.proxy_document_symbols(path) {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            let Ok(result) = serde_json::from_value::<ee_mcp::DocumentSymbolsResult>(payload)
            else {
                continue;
            };
            symbols_truncated |= result.truncated;
            nearby_symbols.extend(
                result
                    .symbols
                    .into_iter()
                    .take(PROXY_REVIEW_SYMBOLS_LIMIT.saturating_sub(nearby_symbols.len())),
            );
        }
        symbols_truncated |= changed_files.files.len() > PROXY_REVIEW_SYMBOL_FILE_LIMIT;

        let test_suggestions = self.proxy_git_repository().map_or_else(
            |_| Vec::new(),
            |repository| {
                if repository.root().join("Cargo.toml").is_file() {
                    vec![String::from("cargo test --quiet")]
                } else {
                    Vec::new()
                }
            },
        );
        serde_json::to_value(ee_mcp::ReviewContextResult {
            changed_files,
            diagnostics: ee_mcp::DiagnosticsResult {
                diagnostics,
                truncated: diagnostics_truncated,
                total: diagnostic_total,
            },
            nearby_symbols,
            symbols_truncated,
            // Only suggest a quiet Cargo validation when workspace shape proves it applies.
            test_suggestions,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_document_symbols(
        &mut self,
        path: &Path,
    ) -> Result<serde_json::Value, AgentError> {
        let payload = self.proxy_agent_tool_payload(
            path,
            None,
            None,
            "ee.agent.document_symbols",
            "document_symbols",
            json!({}),
        )?;
        let mut symbols = serde_json::from_value::<AgentDocumentSymbolsPayload>(payload)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?
            .symbols;
        let total = u32::try_from(symbols.len()).unwrap_or(u32::MAX);
        let truncated = symbols.len() > PROXY_DOCUMENT_SYMBOLS_LIMIT;
        symbols.truncate(PROXY_DOCUMENT_SYMBOLS_LIMIT);
        serde_json::to_value(ee_mcp::DocumentSymbolsResult { symbols, truncated, total })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_references(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<serde_json::Value, AgentError> {
        let payload = self.proxy_agent_tool_payload(
            path,
            Some(line),
            Some(character),
            "ee.agent.references",
            "references",
            json!({}),
        )?;
        let mut references = serde_json::from_value::<AgentReferencesPayload>(payload)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?
            .references;
        let total = u32::try_from(references.len()).unwrap_or(u32::MAX);
        let truncated = references.len() > PROXY_REFERENCES_LIMIT;
        references.truncate(PROXY_REFERENCES_LIMIT);
        serde_json::to_value(ee_mcp::ReferencesResult { references, truncated, total })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_list_code_actions(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<serde_json::Value, AgentError> {
        let payload = self.proxy_agent_tool_payload(
            path,
            Some(line),
            Some(character),
            "ee.agent.list_code_actions",
            "list_code_actions",
            json!({}),
        )?;
        let actions = serde_json::from_value::<AgentCodeActionPayload>(payload)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?
            .actions;
        let total = u32::try_from(actions.len()).unwrap_or(u32::MAX);
        let truncated = actions.len() > PROXY_CODE_ACTIONS_LIMIT;
        let path_text = path.display().to_string();
        let mut listed = Vec::new();
        for action in actions.into_iter().take(PROXY_CODE_ACTIONS_LIMIT) {
            let action_id = format!("proxy-action-{}", self.agents.mcp.next_proxy_action_id);
            self.agents.mcp.next_proxy_action_id =
                self.agents.mcp.next_proxy_action_id.saturating_add(1);
            self.agents.mcp.proxy_code_actions.insert(
                action_id.clone(),
                crate::app::agents_mcp::CachedProxyCodeAction {
                    path: path_text.clone(),
                    has_command: action.has_command,
                    edits: action.edits.clone(),
                },
            );
            listed.push(ee_mcp::CodeActionEntry {
                action_id,
                title: action.title,
                kind: action.kind,
            });
        }
        serde_json::to_value(ee_mcp::CodeActionsResult { actions: listed, truncated, total })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn current_proxy_edit_result(
        &self,
        path: &Path,
        edit_count: u32,
    ) -> Result<ee_mcp::EditTextResult, AgentError> {
        if let Some(buf) = self.backend.all_bufs().iter().find(|buf| {
            buf.path.as_deref().is_some_and(|candidate| paths_equivalent(candidate, path))
        }) {
            let content = buf.whole_text().unwrap_or_default();
            return Ok(ee_mcp::EditTextResult {
                changed_file: path.display().to_string(),
                byte_count: u64::try_from(content.len()).unwrap_or(u64::MAX),
                edit_count,
                new_revision: buffer_revision_id(buf),
                saved: buffer_saved_state(buf),
                dirty: !buf.pristine,
            });
        }
        let content = std::fs::read_to_string(path)
            .map_err(|error| AgentError::Io(format!("cannot read {}: {error}", path.display())))?;
        Ok(ee_mcp::EditTextResult {
            changed_file: path.display().to_string(),
            byte_count: u64::try_from(content.len()).unwrap_or(u64::MAX),
            edit_count,
            new_revision: text_revision_id(&content),
            saved: true,
            dirty: false,
        })
    }

    fn prepare_planned_file_write(
        &mut self,
        path: &Path,
        edits: &[ee_mcp::PlannedTextEdit],
    ) -> Result<PreparedWrite, AgentError> {
        let (content, revision) = self.read_current_text(path)?;
        let next = apply_planned_text_edits_to_content(&content, edits)?;
        Ok(PreparedWrite {
            path: path.to_path_buf(),
            content: next,
            tool_call_id: None,
            expectation: WriteExpectation::ExpectRevision(revision),
            reply_kind: WriteReplyKind::ProxyStructured,
            proxy_edit_count: u32::try_from(edits.len()).unwrap_or(u32::MAX),
        })
    }

    pub(super) fn proxy_preview_rename_symbol(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<serde_json::Value, AgentError> {
        let payload = self.proxy_agent_tool_payload(
            path,
            Some(line),
            Some(character),
            "ee.agent.preview_rename",
            "preview_rename",
            json!({ "new_name": new_name }),
        )?;
        let mut files = serde_json::from_value::<AgentRenamePayload>(payload)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?
            .files;
        for file in &files {
            self.validate_workspace_write_path(Path::new(&file.path))?;
        }
        let total_files = u32::try_from(files.len()).unwrap_or(u32::MAX);
        let total_edits = u32::try_from(files.iter().map(|file| file.edits.len()).sum::<usize>())
            .unwrap_or(u32::MAX);
        let mut seen_edits = 0usize;
        let mut truncated = files.len() > PROXY_RENAME_FILES_LIMIT;
        files.truncate(PROXY_RENAME_FILES_LIMIT);
        for file in &mut files {
            if seen_edits >= PROXY_RENAME_EDITS_LIMIT {
                file.edits.clear();
                truncated = true;
                continue;
            }
            let remaining = PROXY_RENAME_EDITS_LIMIT.saturating_sub(seen_edits);
            if file.edits.len() > remaining {
                file.edits.truncate(remaining);
                truncated = true;
            }
            seen_edits = seen_edits.saturating_add(file.edits.len());
        }
        serde_json::to_value(ee_mcp::RenamePreviewResult {
            files,
            truncated,
            total_files,
            total_edits,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn queue_proxy_apply_code_action(
        &mut self,
        path: &str,
        action_id: &str,
        route: crate::app::agents_mcp::ProxyRoute,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let Some(cached) = self.agents.mcp.proxy_code_actions.get(action_id).cloned() else {
            let _ = reply
                .send(Err(AgentError::invalid_params(format!("unknown action_id: {action_id}"))));
            return;
        };
        if cached.path != path {
            let _ = reply
                .send(Err(AgentError::invalid_params("action_id was listed for a different path")));
            return;
        }
        if cached.has_command {
            let _ = reply.send(Err(AgentError::invalid_params(
                "code actions that require executeCommand are not supported yet",
            )));
            return;
        }
        let path = PathBuf::from(path);
        match self.prepare_planned_file_write(&path, &cached.edits) {
            Ok(prepared) => {
                if prepared.content
                    == self.read_current_text(&path).map(|(text, _)| text).unwrap_or_default()
                {
                    let result = self.current_proxy_edit_result(&path, prepared.proxy_edit_count);
                    let _ = reply
                        .send(result.map(|value| ClientRequestResponse::ProxyValue(json!(value))));
                    return;
                }
                let detail = format!(
                    "{} ({} bytes, {} edit{})",
                    path.display(),
                    prepared.content.len(),
                    prepared.proxy_edit_count,
                    if prepared.proxy_edit_count == 1 { "" } else { "s" }
                );
                let spec = ProxyWriteSpec {
                    title: String::from("ee_apply_code_action"),
                    detail,
                    prepared,
                };
                let mcp = self.mcp_invocation_for_tool(
                    "ee_apply_code_action",
                    json!({ "action_id": action_id, "path": path }),
                    route,
                );
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    mcp,
                    Some(PERSISTENT_TERMINAL_OPTION_LABEL),
                    reply,
                ));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn queue_proxy_format_file(
        &mut self,
        path: &str,
        route: crate::app::agents_mcp::ProxyRoute,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path_buf = PathBuf::from(path);
        match self.proxy_agent_tool_payload(
            &path_buf,
            None,
            None,
            "ee.agent.format_preview",
            "format_preview",
            json!({}),
        ) {
            Ok(payload) => match serde_json::from_value::<AgentTextEditsPayload>(payload) {
                Ok(payload) => match self.prepare_planned_file_write(&path_buf, &payload.edits) {
                    Ok(prepared) => {
                        if payload.edits.is_empty() {
                            let result = self.current_proxy_edit_result(&path_buf, 0);
                            let _ = reply.send(
                                result.map(|value| ClientRequestResponse::ProxyValue(json!(value))),
                            );
                            return;
                        }
                        let detail = format!(
                            "{} ({} bytes, {} edit{})",
                            path_buf.display(),
                            prepared.content.len(),
                            prepared.proxy_edit_count,
                            if prepared.proxy_edit_count == 1 { "" } else { "s" }
                        );
                        let spec = ProxyWriteSpec {
                            title: String::from("ee_format_file"),
                            detail,
                            prepared,
                        };
                        let mcp = self.mcp_invocation_for_tool(
                            "ee_format_file",
                            json!({ "path": path }),
                            route,
                        );
                        self.request_bridge_approval(ApprovalPrompt::proxy_write(
                            spec,
                            mcp,
                            Some(PERSISTENT_TERMINAL_OPTION_LABEL),
                            reply,
                        ));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                },
                Err(error) => {
                    let _ = reply.send(Err(AgentError::HandlerError(error.to_string())));
                }
            },
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn queue_proxy_rename_symbol(
        &mut self,
        path: &str,
        line: u32,
        character: u32,
        new_name: &str,
        route: crate::app::agents_mcp::ProxyRoute,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path_buf = PathBuf::from(path);
        match self.proxy_agent_tool_payload(
            &path_buf,
            Some(line),
            Some(character),
            "ee.agent.preview_rename",
            "preview_rename",
            json!({ "new_name": new_name }),
        ) {
            Ok(payload) => match serde_json::from_value::<AgentRenamePayload>(payload) {
                Ok(payload) => {
                    let mut writes = Vec::new();
                    let mut total_edits = 0u32;
                    for file in payload.files {
                        let file_path = PathBuf::from(&file.path);
                        if let Err(error) = self.validate_workspace_write_path(&file_path) {
                            let _ = reply.send(Err(error));
                            return;
                        }
                        match self.prepare_planned_file_write(&file_path, &file.edits) {
                            Ok(prepared) => {
                                total_edits = total_edits.saturating_add(prepared.proxy_edit_count);
                                writes.push(prepared);
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error));
                                return;
                            }
                        }
                    }
                    if writes.is_empty() {
                        let _ = reply.send(Ok(ClientRequestResponse::ProxyValue(json!(
                            ee_mcp::WorkspaceEditResult {
                                files: Vec::new(),
                                file_count: 0,
                                edit_count: 0
                            }
                        ))));
                        return;
                    }
                    let detail = format!(
                        "{} file{}, {} edit{}",
                        writes.len(),
                        if writes.len() == 1 { "" } else { "s" },
                        total_edits,
                        if total_edits == 1 { "" } else { "s" }
                    );
                    let mcp = self.mcp_invocation_for_tool(
                        "ee_rename_symbol",
                        json!({ "character": character, "line": line, "new_name": new_name, "path": path }),
                        route,
                    );
                    self.request_bridge_approval(ApprovalPrompt::proxy_write_batch(
                        String::from("ee_rename_symbol"),
                        detail,
                        writes,
                        total_edits,
                        mcp,
                        Some(PERSISTENT_TERMINAL_OPTION_LABEL),
                        reply,
                    ));
                }
                Err(error) => {
                    let _ = reply.send(Err(AgentError::HandlerError(error.to_string())));
                }
            },
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn proxy_symbol_dependency_map(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<serde_json::Value, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        let canonical = std::fs::canonicalize(path).map_err(|error| {
            AgentError::Io(format!("cannot resolve symbol-dependency path: {error}"))
        })?;
        if !self.path_in_workspace(&canonical) {
            return Err(AgentError::invalid_params("path outside allowed workspace"));
        }
        let buffer_id = self.ensure_proxy_buffer(&canonical)?;
        let buffer =
            self.backend.all_bufs().iter().find(|buffer| buffer.id == buffer_id).ok_or_else(
                || AgentError::HandlerError("opened buffer is unavailable".to_string()),
            )?;
        let language_id = self.buffer_language_id(buffer).ok_or_else(|| {
            AgentError::HandlerError(
                "dependency_index_unavailable: buffer language is unavailable".to_string(),
            )
        })?;
        self.backend
            .symbol_dependency_map(
                buffer_id,
                canonical.display().to_string(),
                line,
                character,
                language_id,
            )
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_file_dependency_map(
        &self,
        path: &Path,
    ) -> Result<serde_json::Value, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        let canonical = std::fs::canonicalize(path).map_err(|error| {
            AgentError::Io(format!("cannot resolve dependency-map path: {error}"))
        })?;
        if !self.path_in_workspace(&canonical) {
            return Err(AgentError::invalid_params("path outside allowed workspace"));
        }
        serde_json::to_value(crate::app::agent_knowledge::unavailable_dependency_map(canonical))
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_workspace_roots(&self) -> Result<serde_json::Value, AgentError> {
        let roots = self
            .canonical_workspace_roots()
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let active_root = self.active_root_path().map(|path| path.display().to_string());
        let active_file = self.active_file_path().and_then(|path| {
            std::fs::canonicalize(&path)
                .ok()
                .or_else(|| path.is_absolute().then_some(path))
                .map(|path| path.display().to_string())
        });
        let additional_directories = roots.iter().skip(1).cloned().collect::<Vec<_>>();
        serde_json::to_value(ee_mcp::WorkspaceRootsResult {
            roots,
            active_root,
            active_file,
            additional_directories,
        })
        .map_err(|error| AgentError::HandlerError(error.to_string()))
    }
}
