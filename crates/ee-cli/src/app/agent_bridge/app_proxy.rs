//! `impl App`: proxy tool invocation, buffer/file payloads, diagnostics.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ee_agent_host::{AgentError, ClientRequestResponse, ClientRequestResult};
use ee_agent_protocol::{ReadTextFileRequest, SessionId};
use tokio::sync::oneshot;

use super::super::*;

use crate::policy::{McpInvocation, TrustCategory};

use super::app_search::{PROXY_DIAGNOSTICS_LIMIT, paths_equivalent};
use super::approval::{PreparedWrite, ProxyWriteSpec, WriteExpectation, WriteReplyKind};
use super::prompt::ApprovalPrompt;
use super::read::read_text_window;
use super::write::buffer_revision_id;

impl App {
    /// Validated generic MCP invocation for an eligible proxy tool call
    /// (Phase 3): server identity `ee`, pinned manifest schema version,
    /// side-effect classification, canonical exact JSON arguments, and the
    /// delivering transport.  Returns `None` for tools that never qualify
    /// (content-bearing writes, terminal-create, read/unknown tools).
    pub(super) fn mcp_invocation_for_tool(
        &self,
        tool: &str,
        arguments: serde_json::Value,
        route: crate::app::agents_mcp::ProxyRoute,
    ) -> Option<McpInvocation> {
        if !ee_mcp::classify::exact_trust_eligible(tool) {
            return None;
        }
        let arguments_json =
            crate::policy::rules::canonicalize_arguments_json(&arguments.to_string()).ok()?;
        let category = match ee_mcp::classify::side_effect_class(tool) {
            ee_mcp::SideEffectClass::Read => TrustCategory::Read,
            ee_mcp::SideEffectClass::Write => TrustCategory::WriteModify,
            ee_mcp::SideEffectClass::Execute => TrustCategory::Execute,
            ee_mcp::SideEffectClass::Unknown => TrustCategory::Unknown,
        };
        Some(McpInvocation {
            workspace: self.primary_workspace_identity(),
            agent: None,
            transport: route.transport_kind(),
            transport_identity: route.transport_identity().to_string(),
            server: String::from("ee"),
            tool: tool.to_string(),
            tool_schema_version: ee_mcp::classify::EE_TOOL_SCHEMA_VERSION,
            category,
            arguments_json,
        })
    }

    pub(super) fn queue_proxy_replace_text(
        &mut self,
        path: &str,
        old_text: &str,
        new_text: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = PathBuf::from(path);
        match self.prepare_replace_text(&path, old_text, new_text) {
            Ok((content, expectation)) => {
                let persistent_label =
                    self.native_write_persistent_label(&path, &content, &expectation);
                let spec = ProxyWriteSpec {
                    title: String::from("ee_replace_text"),
                    detail: format!("{} ({} bytes, 1 edit)", path.display(), content.len()),
                    prepared: PreparedWrite {
                        path,
                        content,
                        tool_call_id: None,
                        expectation,
                        reply_kind: WriteReplyKind::ProxyStructured,
                        proxy_edit_count: 1,
                    },
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    None,
                    persistent_label,
                    reply,
                ))
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn queue_proxy_apply_patch(
        &mut self,
        path: &str,
        edits: &[ee_agent_host::ProxyTextEdit],
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = PathBuf::from(path);
        match self.prepare_apply_patch(&path, edits) {
            Ok((content, expectation)) => {
                let edit_count = u32::try_from(edits.len()).unwrap_or(u32::MAX);
                let detail = format!(
                    "{} ({} bytes, {} edit{})",
                    path.display(),
                    content.len(),
                    edit_count,
                    if edit_count == 1 { "" } else { "s" }
                );
                let persistent_label =
                    self.native_write_persistent_label(&path, &content, &expectation);
                let spec = ProxyWriteSpec {
                    title: String::from("ee_apply_patch"),
                    detail,
                    prepared: PreparedWrite {
                        path,
                        content,
                        tool_call_id: None,
                        expectation,
                        reply_kind: WriteReplyKind::ProxyStructured,
                        proxy_edit_count: edit_count,
                    },
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    None,
                    persistent_label,
                    reply,
                ))
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn queue_proxy_filesystem(
        &mut self,
        operation: crate::app::agent_filesystem::FilesystemOperation,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        // Application safeguards inspect typed paths before ordinary validation;
        // executor validates again immediately before mutation.
        if let Some(path) = self
            .backend
            .all_bufs()
            .iter()
            .filter_map(|buffer| buffer.path.as_deref())
            .find(|path| operation.affected_open_path(path))
        {
            let _ = reply.send(Err(AgentError::invalid_params(format!(
                "filesystem operation affects open buffer: {}",
                path.display()
            ))));
            return;
        }
        self.request_bridge_approval(ApprovalPrompt::filesystem(operation, reply));
    }

    pub(super) fn apply_proxy_filesystem(
        &mut self,
        operation: crate::app::agent_filesystem::FilesystemOperation,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        if reply.is_closed() {
            return;
        }
        if let Some(path) = self
            .backend
            .all_bufs()
            .iter()
            .filter_map(|buffer| buffer.path.as_deref())
            .find(|path| operation.affected_open_path(path))
        {
            let _ = reply.send(Err(AgentError::invalid_params(format!(
                "filesystem operation affects open buffer: {}",
                path.display()
            ))));
            return;
        }
        match crate::app::agent_filesystem::execute(&operation, &self.allowed_fs_roots()) {
            Ok(result) => match serde_json::to_value(result) {
                Ok(value) => {
                    let _ = reply.send(Ok(ClientRequestResponse::ProxyValue(value)));
                }
                Err(error) => {
                    let _ = reply.send(Err(AgentError::HandlerError(format!(
                        "filesystem result serialization failed: {error}"
                    ))));
                }
            },
            Err(error) => {
                let _ = reply.send(Err(AgentError::Io(format!(
                    "{} failed: {error}",
                    operation.tool_name()
                ))));
            }
        }
    }

    pub(super) fn queue_proxy_create_text_file(
        &mut self,
        path: &str,
        content: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = PathBuf::from(path);
        if let Err(error) = self.validate_workspace_write_path(&path) {
            let _ = reply.send(Err(error));
            return;
        }
        match self.current_text_revision(&path) {
            Ok(Some(_)) => {
                let _ = reply.send(Err(AgentError::invalid_params(format!(
                    "target already exists: {}",
                    path.display()
                ))));
            }
            Ok(None) => {
                let created = content.to_string();
                let persistent_label = self.native_write_persistent_label(
                    &path,
                    &created,
                    &WriteExpectation::MustNotExist,
                );
                let spec = ProxyWriteSpec {
                    title: String::from("ee_create_text_file"),
                    detail: format!("{} ({} bytes, 1 edit)", path.display(), created.len()),
                    prepared: PreparedWrite {
                        path,
                        content: created,
                        tool_call_id: None,
                        expectation: WriteExpectation::MustNotExist,
                        reply_kind: WriteReplyKind::ProxyStructured,
                        proxy_edit_count: 1,
                    },
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    None,
                    persistent_label,
                    reply,
                ))
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn queue_proxy_overwrite_text_file(
        &mut self,
        path: &str,
        content: &str,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = PathBuf::from(path);
        if let Err(error) = self.validate_workspace_write_path(&path) {
            let _ = reply.send(Err(error));
            return;
        }
        match self.current_text_revision(&path) {
            Ok(Some(revision)) => {
                let updated = content.to_string();
                let expectation = WriteExpectation::ExpectRevision(revision);
                let persistent_label =
                    self.native_write_persistent_label(&path, &updated, &expectation);
                let spec = ProxyWriteSpec {
                    title: String::from("ee_overwrite_text_file"),
                    detail: format!("{} ({} bytes, 1 edit)", path.display(), updated.len()),
                    prepared: PreparedWrite {
                        path,
                        content: updated,
                        tool_call_id: None,
                        expectation,
                        reply_kind: WriteReplyKind::ProxyStructured,
                        proxy_edit_count: 1,
                    },
                };
                self.request_bridge_approval(ApprovalPrompt::proxy_write(
                    spec,
                    None,
                    persistent_label,
                    reply,
                ))
            }
            Ok(None) => {
                let _ = reply.send(Err(AgentError::invalid_params(format!(
                    "target does not exist: {}",
                    path.display()
                ))));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn proxy_read_buffer(
        &mut self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<serde_json::Value, AgentError> {
        let mut request = ReadTextFileRequest::new(SessionId::new("proxy"), path.to_path_buf());
        request.line = line;
        request.limit = limit;
        let text = if let Some(buf) = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, path)))
        {
            self.read_from_buffer(buf, &request)?.0
        } else {
            if !path.is_absolute() {
                return Err(AgentError::invalid_params("path must be absolute"));
            }
            if !self.path_in_workspace(path) {
                return Err(AgentError::invalid_params(format!(
                    "path outside allowed workspace: {}",
                    path.display()
                )));
            }
            let content = std::fs::read_to_string(path).map_err(|error| {
                AgentError::Io(format!("cannot read {}: {error}", path.display()))
            })?;
            if let Some(line) = line {
                read_text_window(&content, Some(line), limit)?
            } else {
                read_text_window(&content, None, limit)?
            }
        };
        Ok(serde_json::Value::String(text))
    }

    pub(super) fn buffer_language_id(&self, buf: &crate::buffer::BufState) -> Option<String> {
        self.syntax_overrides
            .get(&buf.id)
            .cloned()
            .or_else(|| {
                buf.path
                    .as_deref()
                    .and_then(xi_core_lib::tree_sitter_support::language_name_for_path)
            })
            .or_else(|| self.highlighter.syntax_name_for_path(buf.path.as_deref()))
    }

    fn buffer_selection_summary(&mut self, buf_id: crate::buffer::BufferId) -> String {
        let previous_idx = self.backend.current_idx();
        let switched = self.backend.active().id != buf_id;
        if switched && self.backend.switch_to_id(buf_id).is_err() {
            return String::from("selection unavailable");
        }
        let summary = match self.primary_selection_preview() {
            Ok(Some(selection)) => {
                let start = selection.start.min(selection.end);
                let end = selection.start.max(selection.end);
                if start == end {
                    format!("cursor at offset {start}")
                } else {
                    format!("offsets {start}..{end}")
                }
            }
            Ok(None) => String::from("cursor only"),
            Err(_) => String::from("selection unavailable"),
        };
        if switched && self.backend.current_idx() != previous_idx {
            self.backend.switch_to_idx(previous_idx);
        }
        summary
    }

    pub(super) fn proxy_open_buffers(&mut self) -> Result<serde_json::Value, AgentError> {
        let active_id = self.backend.active().id;
        let snapshot = self
            .backend
            .all_bufs()
            .iter()
            .filter_map(|buf| {
                let path = buf.path.as_ref()?;
                Some((
                    buf.id,
                    path.display().to_string(),
                    !buf.pristine,
                    buffer_revision_id(buf),
                    format!("line {}, column {}", buf.cursor_line + 1, buf.cursor_col + 1),
                    self.buffer_language_id(buf),
                    buf.id == active_id,
                ))
            })
            .collect::<Vec<_>>();
        let buffers = snapshot
            .into_iter()
            .map(|(id, path, dirty, revision_id, cursor_summary, language_id, active)| {
                ee_mcp::OpenBufferEntry {
                    path,
                    dirty,
                    revision_id,
                    cursor_summary,
                    selection_summary: self.buffer_selection_summary(id),
                    language_id,
                    active,
                }
            })
            .collect();
        serde_json::to_value(ee_mcp::OpenBuffersResult { buffers })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn ensure_proxy_buffer(
        &mut self,
        path: &Path,
    ) -> Result<crate::buffer::BufferId, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        if let Some(id) = self.buffer_id_for_path(path) {
            return Ok(id);
        }
        if !self.path_in_workspace(path) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )));
        }
        self.backend
            .open_buffer(Some(path.to_path_buf()))
            .map_err(|error| AgentError::Io(format!("cannot open {}: {error}", path.display())))
    }

    pub(super) fn proxy_agent_tool_payload(
        &mut self,
        path: &Path,
        line: Option<u32>,
        character: Option<u32>,
        method: &str,
        kind: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, AgentError> {
        let buf_id = self.ensure_proxy_buffer(path)?;
        let previous_idx = self.backend.current_idx();
        let switched = self.backend.active().id != buf_id;
        if switched {
            self.backend.switch_to_id(buf_id).map_err(|error| {
                AgentError::Io(format!("cannot switch to {}: {error}", path.display()))
            })?;
        }
        let saved_selections =
            if !switched { self.backend.selections_preview().ok() } else { None };
        if let Some(line) = line {
            let character = character.unwrap_or(1);
            self.move_cursor_to(
                (line.saturating_sub(1)) as usize,
                character.saturating_sub(1) as usize,
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                self.backend
                    .drain_events()
                    .map_err(|error| AgentError::Io(format!("drain failed: {error}")))?;
                if self.backend.cursor_line == (line.saturating_sub(1)) as usize {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        self.backend
            .send_edit(method, params)
            .map_err(|error| AgentError::Io(format!("{method} failed: {error}")))?;
        let active_view_id = self.backend.active().view_id.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            self.backend
                .drain_events()
                .map_err(|error| AgentError::Io(format!("drain failed: {error}")))?;
            let pending = self.backend.drain_pending_agent_tool_results();
            let mut remainder = Vec::new();
            let mut matched = None;
            for entry in pending {
                if matched.is_none() && entry.0 == active_view_id && entry.1 == kind {
                    matched = Some(entry.2);
                } else {
                    remainder.push(entry);
                }
            }
            self.backend.pending_agent_tool_results.extend(remainder);
            if let Some(payload) = matched {
                break Ok(payload);
            }
            if Instant::now() >= deadline {
                break Err(AgentError::Io(format!("{method} timed out")));
            }
            thread::sleep(Duration::from_millis(10));
        };
        if !switched && let Some(selections) = saved_selections {
            let _ = self.backend.set_selections(&selections);
        }
        if switched && self.backend.current_idx() != previous_idx {
            self.backend.switch_to_idx(previous_idx);
        }
        result
    }

    pub(super) fn proxy_diagnostic_entries(
        &self,
        path: Option<&Path>,
    ) -> Vec<ee_mcp::DiagnosticEntry> {
        self.backend
            .all_bufs()
            .iter()
            .filter_map(|buf| {
                let buf_path = buf.path.as_ref()?;
                if let Some(target) = path
                    && !paths_equivalent(buf_path, target)
                {
                    return None;
                }
                Some((buf_path, &buf.lines, &buf.diagnostics))
            })
            .flat_map(|(buf_path, lines, diagnostics)| {
                diagnostics.iter().map(move |diagnostic| {
                    let (start_line, start_col) =
                        line_col_for_offset(lines, diagnostic.range.start);
                    let (end_line, end_col) = line_col_for_offset(lines, diagnostic.range.end);
                    ee_mcp::DiagnosticEntry {
                        path: buf_path.display().to_string(),
                        range: ee_mcp::TextRange {
                            start_line: u32::try_from(start_line + 1).unwrap_or(u32::MAX),
                            start_character: u32::try_from(start_col + 1).unwrap_or(u32::MAX),
                            end_line: u32::try_from(end_line + 1).unwrap_or(u32::MAX),
                            end_character: u32::try_from(end_col + 1).unwrap_or(u32::MAX),
                        },
                        severity: match diagnostic.severity {
                            xi_core_lib::plugin_rpc::DiagnosticSeverity::Error => {
                                String::from("error")
                            }
                            xi_core_lib::plugin_rpc::DiagnosticSeverity::Warning => {
                                String::from("warning")
                            }
                            xi_core_lib::plugin_rpc::DiagnosticSeverity::Information => {
                                String::from("information")
                            }
                            xi_core_lib::plugin_rpc::DiagnosticSeverity::Hint => {
                                String::from("hint")
                            }
                        },
                        source: diagnostic.source.clone(),
                        code: diagnostic.code.clone(),
                        message: diagnostic.message.clone(),
                    }
                })
            })
            .collect()
    }

    pub(super) fn proxy_get_diagnostics(
        &mut self,
        path: Option<&Path>,
    ) -> Result<serde_json::Value, AgentError> {
        if let Some(path) = path
            && self.buffer_id_for_path(path).is_none()
            && path.exists()
        {
            let _ = self.ensure_proxy_buffer(path)?;
            let _ = self.backend.drain_events();
        }
        let mut diagnostics = self.proxy_diagnostic_entries(path);
        diagnostics.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.range.start_line.cmp(&right.range.start_line))
                .then(left.range.start_character.cmp(&right.range.start_character))
        });
        let total = u32::try_from(diagnostics.len()).unwrap_or(u32::MAX);
        let truncated = diagnostics.len() > PROXY_DIAGNOSTICS_LIMIT;
        diagnostics.truncate(PROXY_DIAGNOSTICS_LIMIT);
        serde_json::to_value(ee_mcp::DiagnosticsResult { diagnostics, truncated, total })
            .map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    pub(super) fn proxy_git_repository(&self) -> Result<crate::git::GitRepository, AgentError> {
        let root = self
            .active_root_path()
            .or_else(|| std::fs::canonicalize(&self.working_dir).ok())
            .ok_or_else(|| {
                AgentError::HandlerError(String::from("active workspace root is unavailable"))
            })?;
        crate::git::GitRepository::discover(&root)
            .map_err(|error| {
                AgentError::HandlerError(format!("Git repository discovery failed: {error}"))
            })?
            .ok_or_else(|| {
                AgentError::HandlerError(String::from("active workspace is not a Git repository"))
            })
    }
}
