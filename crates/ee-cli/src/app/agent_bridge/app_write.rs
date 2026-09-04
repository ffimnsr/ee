//! `impl App`: write-lease acquisition and approved buffer writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ee_agent_host::{
    AgentError, ClientRequestResponse, ClientRequestResult, EvidenceCheck, EvidenceRevision,
    TurnObservation, WriteEvidenceOutcome, WriteTransactionStage,
};
use ee_agent_protocol::WriteTextFileResponse;
use tokio::sync::oneshot;

use super::super::*;

use crate::app::write_leases::WriteLeaseOwner;

use crate::policy::is_protected_relative_path;

use super::app_search::paths_equivalent;
use super::approval::{ApprovalKind, PreparedWrite, WriteExpectation, WriteReplyKind};
use super::prompt::ApprovalPrompt;
use super::write::{
    ActionLogEntry, BridgeWriteOutcome, buffer_revision_id, buffer_saved_state, diff_hunks,
    fingerprint, split_lines,
};

impl App {
    pub(super) fn record_write_lease_rejection(&self, prompt: &ApprovalPrompt) {
        let paths = match &prompt.kind {
            ApprovalKind::Write { path, .. } => vec![path.clone()],
            ApprovalKind::WriteBatch { writes, .. } => {
                writes.iter().map(|write| write.path.clone()).collect()
            }
            ApprovalKind::Filesystem { .. }
            | ApprovalKind::TerminalCreate { .. }
            | ApprovalKind::WorkspaceMemoryApproval { .. }
            | ApprovalKind::Network { .. } => return,
        };
        let revision = self
            .evidence_revision_for_paths(&paths)
            .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
        self.observe_active_turn(
            &prompt.session_id,
            TurnObservation::Revision { revision: revision.clone() },
        );
        self.observe_active_turn(
            &prompt.session_id,
            TurnObservation::Write {
                revision: revision.clone(),
                outcome: WriteEvidenceOutcome::Conflicted,
            },
        );
        self.observe_transaction_stage(
            &prompt.session_id,
            revision,
            WriteTransactionStage::Read,
            EvidenceCheck::Failed,
        );
    }

    pub(super) fn acquire_prompt_write_lease(
        &mut self,
        prompt: &mut ApprovalPrompt,
    ) -> Result<(), AgentError> {
        let scopes = match &prompt.kind {
            ApprovalKind::Write { path, .. } => {
                vec![self.canonical_workspace_write_target(path).ok_or_else(|| {
                    AgentError::invalid_params("write target has no canonical workspace identity")
                })?]
            }
            ApprovalKind::WriteBatch { writes, .. } => writes
                .iter()
                .map(|write| {
                    self.canonical_workspace_write_target(&write.path).ok_or_else(|| {
                        AgentError::invalid_params(
                            "write target has no canonical workspace identity",
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            ApprovalKind::Filesystem { operation } => operation
                .canonical_write_scopes(&self.allowed_fs_roots())
                .map_err(|error| AgentError::invalid_params(error.to_string()))?,
            ApprovalKind::TerminalCreate { .. }
            | ApprovalKind::WorkspaceMemoryApproval { .. }
            | ApprovalKind::Network { .. } => return Ok(()),
        };

        let blocks_dirty = match &prompt.kind {
            ApprovalKind::Write { expectation, .. } => {
                matches!(expectation, WriteExpectation::Blind | WriteExpectation::MustNotExist)
            }
            ApprovalKind::WriteBatch { writes, .. } => writes.iter().any(|write| {
                matches!(
                    write.expectation,
                    WriteExpectation::Blind | WriteExpectation::MustNotExist
                )
            }),
            ApprovalKind::Filesystem { .. } => true,
            ApprovalKind::TerminalCreate { .. }
            | ApprovalKind::WorkspaceMemoryApproval { .. }
            | ApprovalKind::Network { .. } => false,
        } && self.has_dirty_buffer(&scopes);
        if blocks_dirty {
            return Err(AgentError::invalid_params(
                "dirty editor buffer conflicts with requested agent write scope",
            ));
        }

        if prompt.agent_id.is_none() {
            prompt.agent_id = prompt
                .thread_index
                .and_then(|index| self.agents.threads.get(index))
                .map(|thread| thread.agent_id.clone());
        }
        let connection_id = prompt.agent_id.clone().unwrap_or_else(|| String::from("proxy"));
        let turn_id = match &prompt.kind {
            ApprovalKind::Write { tool_call_id: Some(id), .. } => id.clone(),
            ApprovalKind::Write { tool_call_id: None, .. } => {
                format!("write-{}", self.agents.next_write_turn_id)
            }
            ApprovalKind::WriteBatch { writes, .. } => writes
                .iter()
                .find_map(|write| write.tool_call_id.clone())
                .unwrap_or_else(|| format!("write-{}", self.agents.next_write_turn_id)),
            ApprovalKind::Filesystem { .. } => {
                format!("filesystem-{}", self.agents.next_write_turn_id)
            }
            ApprovalKind::TerminalCreate { .. }
            | ApprovalKind::WorkspaceMemoryApproval { .. }
            | ApprovalKind::Network { .. } => unreachable!(),
        };
        self.agents.next_write_turn_id = self.agents.next_write_turn_id.wrapping_add(1);
        let owner =
            WriteLeaseOwner { connection_id, session_id: prompt.session_id.clone(), turn_id };
        let revisions = self.write_scope_revisions(&scopes)?;
        let id =
            self.agents.write_leases.acquire(owner.clone(), scopes, revisions).map_err(
                |conflict| AgentError::PermissionDenied { reason: conflict.to_string() },
            )?;
        prompt.write_lease = Some(id);
        prompt.write_lease_owner = Some(owner);
        Ok(())
    }

    fn write_scope_revisions(
        &self,
        scopes: &[PathBuf],
    ) -> Result<BTreeMap<PathBuf, String>, AgentError> {
        scopes.iter().map(|path| Ok((path.clone(), self.write_scope_revision(path)?))).collect()
    }

    fn write_scope_revision(&self, path: &Path) -> Result<String, AgentError> {
        let dirty = self.backend.all_bufs().iter().any(|buffer| {
            !buffer.pristine
                && buffer.path.as_deref().is_some_and(|candidate| paths_equivalent(candidate, path))
        });
        if path.is_file() {
            let revision =
                self.current_text_revision(path)?.unwrap_or_else(|| String::from("missing"));
            return Ok(format!("file:{revision}:dirty={dirty}"));
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(String::from("missing"));
            }
            Err(error) => {
                return Err(AgentError::Io(format!(
                    "cannot inspect write scope {}: {error}",
                    path.display()
                )));
            }
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Ok(format!(
            "metadata:{}:{}:{}:{modified}:dirty={dirty}",
            metadata.is_dir(),
            metadata.is_file(),
            metadata.len()
        ))
    }

    pub(super) fn validate_prompt_write_lease(
        &self,
        prompt: &ApprovalPrompt,
    ) -> Result<(), AgentError> {
        let Some(id) = prompt.write_lease else {
            return Ok(());
        };
        let owner = prompt.write_lease_owner.as_ref().ok_or_else(|| {
            AgentError::PermissionDenied { reason: String::from("write lease owner is missing") }
        })?;
        let scopes = self.agents.write_leases.scopes(id).ok_or_else(|| {
            AgentError::PermissionDenied { reason: String::from("write lease is no longer active") }
        })?;
        let revisions = self.write_scope_revisions(scopes)?;
        self.agents
            .write_leases
            .validate(id, owner, &revisions)
            .map_err(|reason| AgentError::PermissionDenied { reason: reason.to_string() })
    }

    pub(super) fn release_prompt_write_lease(&mut self, prompt: &mut ApprovalPrompt) {
        if let Some(id) = prompt.write_lease.take() {
            self.agents.write_leases.release(id);
        }
        prompt.write_lease_owner = None;
    }

    /// Performs an approved buffer write: open/reuse buffer, diff, edit,
    /// verify, save — all through existing buffer/save semantics.  A matched
    /// persistent rule consumes one use only after the write succeeds.
    pub(super) fn apply_bridge_write(
        &mut self,
        prepared: PreparedWrite,
        session_id: &str,
        rule_id: Option<&str>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let path = prepared.path.as_path();
        let content = prepared.content.as_str();
        if let Err(error) = self.validate_workspace_write_path(path) {
            let _ = reply.send(Err(error));
            return;
        }
        if let Err(error) = self.validate_write_expectation(path, &prepared.expectation) {
            let _ = reply.send(Err(error));
            return;
        }

        let paths = vec![path.to_path_buf()];
        let pre_write_revision = self
            .evidence_revision_for_paths(&paths)
            .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
        self.observe_active_turn(
            session_id,
            TurnObservation::Revision { revision: pre_write_revision.clone() },
        );
        let blocks_dirty_buffer = self.has_dirty_buffer(&paths)
            && matches!(
                prepared.expectation,
                WriteExpectation::Blind | WriteExpectation::MustNotExist
            );
        self.observe_transaction_stage(
            session_id,
            pre_write_revision.clone(),
            WriteTransactionStage::Read,
            if blocks_dirty_buffer { EvidenceCheck::Failed } else { EvidenceCheck::Passed },
        );
        if blocks_dirty_buffer {
            self.observe_active_turn(
                session_id,
                TurnObservation::Write {
                    revision: pre_write_revision,
                    outcome: WriteEvidenceOutcome::Conflicted,
                },
            );
            let _ = reply.send(Err(AgentError::invalid_params(
                "dirty editor buffer requires explicit user handoff before blind agent write",
            )));
            return;
        }
        self.observe_transaction_stage(
            session_id,
            pre_write_revision.clone(),
            WriteTransactionStage::Preview,
            EvidenceCheck::Passed,
        );
        self.observe_active_turn(
            session_id,
            TurnObservation::Write {
                revision: pre_write_revision.clone(),
                outcome: WriteEvidenceOutcome::Approved,
            },
        );
        self.observe_transaction_stage(
            session_id,
            pre_write_revision,
            WriteTransactionStage::Approval,
            EvidenceCheck::Passed,
        );
        let diagnostics_before = self.refresh_diagnostic_error_count(&paths).ok();
        match self.write_through_buffer(path, content) {
            Ok(outcome) => {
                if let Some(rule_id) = rule_id {
                    self.agents.usage_ledger.record_use(
                        self.primary_workspace_identity(),
                        session_id,
                        rule_id,
                    );
                }
                let changed = outcome.old_content != content;
                self.agents.action_log.push(ActionLogEntry::Write {
                    path: path.to_path_buf(),
                    old_fingerprint: fingerprint(&outcome.old_content),
                    new_fingerprint: fingerprint(content),
                    tool_call_id: prepared.tool_call_id,
                    session_id: session_id.to_string(),
                });
                let response = match prepared.reply_kind {
                    WriteReplyKind::FsWrite => {
                        ClientRequestResponse::WriteTextFile(WriteTextFileResponse::new())
                    }
                    WriteReplyKind::ProxyStructured => {
                        ClientRequestResponse::ProxyValue(json!(ee_mcp::EditTextResult {
                            changed_file: path.display().to_string(),
                            byte_count: outcome.byte_count,
                            edit_count: prepared.proxy_edit_count,
                            new_revision: outcome.new_revision.clone(),
                            saved: outcome.saved,
                            dirty: outcome.dirty,
                        }))
                    }
                };
                // Publish host-owned revision and verification evidence before replying.
                // A fast provider may complete the turn as soon as it receives the response.
                let post_write_revision = self
                    .evidence_revision_for_paths(&paths)
                    .unwrap_or_else(|_| EvidenceRevision::new(&outcome.new_revision));
                self.observe_active_turn(
                    session_id,
                    TurnObservation::Revision { revision: post_write_revision.clone() },
                );
                self.observe_active_turn(
                    session_id,
                    TurnObservation::Write {
                        revision: post_write_revision.clone(),
                        outcome: if changed {
                            WriteEvidenceOutcome::Applied
                        } else {
                            WriteEvidenceOutcome::NoOp
                        },
                    },
                );
                self.observe_transaction_stage(
                    session_id,
                    post_write_revision,
                    WriteTransactionStage::Apply,
                    EvidenceCheck::Passed,
                );
                if changed {
                    #[cfg(test)]
                    self.run_pre_write_verification_test_hook();
                    self.collect_post_write_verification(session_id, &paths, diagnostics_before);
                    #[cfg(test)]
                    {
                        self.run_post_write_test_hook();
                        self.observe_post_write_test_revision(session_id, &paths);
                    }
                }
                if let Some(thread) = self.session_thread_by_id(session_id) {
                    self.agents.threads[thread]
                        .push_system(format!("agent wrote: {}", path.display()));
                }
                let _ = reply.send(Ok(response));
            }
            Err(error) => {
                let revision = self
                    .evidence_revision_for_paths(&paths)
                    .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
                let outcome = if error.to_string().contains("conflict") {
                    WriteEvidenceOutcome::Conflicted
                } else {
                    WriteEvidenceOutcome::Failed
                };
                self.observe_active_turn(
                    session_id,
                    TurnObservation::Revision { revision: revision.clone() },
                );
                self.observe_active_turn(
                    session_id,
                    TurnObservation::Write { revision: revision.clone(), outcome },
                );
                self.observe_transaction_stage(
                    session_id,
                    revision.clone(),
                    WriteTransactionStage::Apply,
                    EvidenceCheck::Failed,
                );
                self.observe_transaction_stage(
                    session_id,
                    revision,
                    WriteTransactionStage::RollbackSafety,
                    EvidenceCheck::Unavailable,
                );
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn apply_bridge_write_batch(
        &mut self,
        writes: Vec<PreparedWrite>,
        total_edit_count: u32,
        session_id: &str,
        rule_id: Option<&str>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        for prepared in &writes {
            if let Err(error) = self.validate_workspace_write_path(prepared.path.as_path()) {
                let _ = reply.send(Err(error));
                return;
            }
            if let Err(error) =
                self.validate_write_expectation(prepared.path.as_path(), &prepared.expectation)
            {
                let _ = reply.send(Err(error));
                return;
            }
        }
        let paths = writes.iter().map(|prepared| prepared.path.clone()).collect::<Vec<_>>();
        let blocks_dirty_buffer = self.has_dirty_buffer(&paths)
            && writes.iter().any(|prepared| {
                matches!(
                    prepared.expectation,
                    WriteExpectation::Blind | WriteExpectation::MustNotExist
                )
            });
        let pre_write_revision = self
            .evidence_revision_for_paths(&paths)
            .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
        self.observe_active_turn(
            session_id,
            TurnObservation::Revision { revision: pre_write_revision.clone() },
        );
        self.observe_transaction_stage(
            session_id,
            pre_write_revision.clone(),
            WriteTransactionStage::Read,
            if blocks_dirty_buffer { EvidenceCheck::Failed } else { EvidenceCheck::Passed },
        );
        if blocks_dirty_buffer {
            self.observe_active_turn(
                session_id,
                TurnObservation::Write {
                    revision: pre_write_revision,
                    outcome: WriteEvidenceOutcome::Conflicted,
                },
            );
            let _ = reply.send(Err(AgentError::invalid_params(
                "dirty editor buffer requires explicit user handoff before agent write",
            )));
            return;
        }
        self.observe_transaction_stage(
            session_id,
            pre_write_revision.clone(),
            WriteTransactionStage::Preview,
            EvidenceCheck::Passed,
        );
        self.observe_active_turn(
            session_id,
            TurnObservation::Write {
                revision: pre_write_revision.clone(),
                outcome: WriteEvidenceOutcome::Approved,
            },
        );
        self.observe_transaction_stage(
            session_id,
            pre_write_revision,
            WriteTransactionStage::Approval,
            EvidenceCheck::Passed,
        );
        let diagnostics_before = self.refresh_diagnostic_error_count(&paths).ok();
        let mut changed = false;
        let mut files = Vec::new();
        for prepared in writes {
            let path = prepared.path.clone();
            match self.write_through_buffer(path.as_path(), prepared.content.as_str()) {
                Ok(outcome) => {
                    changed |= outcome.old_content != prepared.content;
                    self.agents.action_log.push(ActionLogEntry::Write {
                        path: path.clone(),
                        old_fingerprint: fingerprint(&outcome.old_content),
                        new_fingerprint: fingerprint(prepared.content.as_str()),
                        tool_call_id: prepared.tool_call_id.clone(),
                        session_id: session_id.to_string(),
                    });
                    files.push(ee_mcp::EditTextResult {
                        changed_file: path.display().to_string(),
                        byte_count: outcome.byte_count,
                        edit_count: prepared.proxy_edit_count,
                        new_revision: outcome.new_revision,
                        saved: outcome.saved,
                        dirty: outcome.dirty,
                    });
                }
                Err(error) => {
                    let revision = self
                        .evidence_revision_for_paths(&paths)
                        .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
                    let outcome = if error.to_string().contains("conflict") {
                        WriteEvidenceOutcome::Conflicted
                    } else {
                        WriteEvidenceOutcome::Failed
                    };
                    self.observe_active_turn(
                        session_id,
                        TurnObservation::Revision { revision: revision.clone() },
                    );
                    self.observe_active_turn(
                        session_id,
                        TurnObservation::Write { revision: revision.clone(), outcome },
                    );
                    self.observe_transaction_stage(
                        session_id,
                        revision.clone(),
                        WriteTransactionStage::Apply,
                        EvidenceCheck::Failed,
                    );
                    if !files.is_empty() {
                        self.observe_transaction_stage(
                            session_id,
                            revision,
                            WriteTransactionStage::RollbackSafety,
                            EvidenceCheck::Unavailable,
                        );
                    }
                    let _ = reply.send(Err(error));
                    return;
                }
            }
        }
        let post_write_revision = self
            .evidence_revision_for_paths(&paths)
            .unwrap_or_else(|_| EvidenceRevision::new("unavailable"));
        self.observe_active_turn(
            session_id,
            TurnObservation::Revision { revision: post_write_revision.clone() },
        );
        self.observe_active_turn(
            session_id,
            TurnObservation::Write {
                revision: post_write_revision.clone(),
                outcome: if changed {
                    WriteEvidenceOutcome::Applied
                } else {
                    WriteEvidenceOutcome::NoOp
                },
            },
        );
        self.observe_transaction_stage(
            session_id,
            post_write_revision,
            WriteTransactionStage::Apply,
            EvidenceCheck::Passed,
        );
        if changed {
            #[cfg(test)]
            self.run_pre_write_verification_test_hook();
            self.collect_post_write_verification(session_id, &paths, diagnostics_before);
            #[cfg(test)]
            {
                self.run_post_write_test_hook();
                self.observe_post_write_test_revision(session_id, &paths);
            }
        }
        if let Some(rule_id) = rule_id {
            self.agents.usage_ledger.record_use(
                self.primary_workspace_identity(),
                session_id,
                rule_id,
            );
        }
        let _ =
            reply.send(Ok(ClientRequestResponse::ProxyValue(json!(ee_mcp::WorkspaceEditResult {
                file_count: u32::try_from(files.len()).unwrap_or(u32::MAX),
                edit_count: total_edit_count,
                files,
            }))));
    }

    /// Opens/reuses the buffer, applies the minimal diff, verifies, saves.
    fn write_through_buffer(
        &mut self,
        path: &Path,
        content: &str,
    ) -> Result<BridgeWriteOutcome, AgentError> {
        let target_lines = split_lines(content);
        let buf_id = match self.buffer_id_for_path(path) {
            Some(id) => id,
            None => self.backend.open_buffer(Some(path.to_path_buf())).map_err(|error| {
                AgentError::Io(format!("cannot open {}: {error}", path.display()))
            })?,
        };
        let snapshot = |backend: &crate::buffer::BufferManager| -> Option<String> {
            backend.all_bufs().iter().find(|buf| buf.id == buf_id).and_then(|buf| buf.whole_text())
        };
        if let Some(buf) = self.backend.all_bufs().iter().find(|buf| buf.id == buf_id)
            && buf.is_vlf
        {
            return Err(AgentError::invalid_params(
                "writes are not supported for very large file buffers",
            ));
        }
        self.backend
            .flush_all_pending_edits()
            .map_err(|error| AgentError::Io(format!("flush failed: {error}")))?;
        let old_content = snapshot(&self.backend).unwrap_or_default();

        // Line edits target the active view only, so transiently switch to
        // the target buffer and restore the previous one afterwards.  The
        // switch is invisible to the renderer (no pump happens in between).
        let previous_idx = self.backend.current_idx();
        if self.backend.active().id != buf_id {
            self.backend.switch_to_id(buf_id).map_err(|error| {
                AgentError::Io(format!("cannot switch to {}: {error}", path.display()))
            })?;
        }
        let result =
            self.apply_bridge_write_edits(&target_lines, buf_id, path, &snapshot, &old_content);
        if self.backend.current_idx() != previous_idx {
            self.backend.switch_to_idx(previous_idx);
        }
        result?;
        let Some(buf) = self.backend.all_bufs().iter().find(|buf| buf.id == buf_id) else {
            return Err(AgentError::HandlerError(String::from("buffer disappeared after save")));
        };
        Ok(BridgeWriteOutcome {
            old_content,
            byte_count: u64::try_from(content.len()).unwrap_or(u64::MAX),
            new_revision: buffer_revision_id(buf),
            saved: buffer_saved_state(buf),
            dirty: !buf.pristine,
        })
    }

    /// Diff-applies `target_lines` to the active buffer, polls for the edit
    /// updates, verifies convergence, and saves.
    fn apply_bridge_write_edits(
        &mut self,
        target_lines: &[String],
        buf_id: crate::buffer::BufferId,
        path: &Path,
        snapshot: &impl Fn(&crate::buffer::BufferManager) -> Option<String>,
        old_content: &str,
    ) -> Result<(), AgentError> {
        let mut current_content = old_content.to_string();
        for _ in 0..2 {
            let current_lines = split_lines(&current_content);
            if current_lines == target_lines {
                break;
            }
            let hunks = diff_hunks(&current_lines, target_lines);
            if hunks.is_empty() {
                break;
            }
            for (start, end, new_lines) in hunks.into_iter().rev() {
                self.apply_diff_hunk(&current_lines, start, end, &new_lines)?;
            }
            self.backend
                .flush_all_pending_edits()
                .map_err(|error| AgentError::Io(format!("flush failed: {error}")))?;
            // xi-core applies edits asynchronously; poll until the update
            // lands (bounded, so a hung backend fails closed).
            let deadline = Instant::now() + Duration::from_millis(2000);
            current_content = loop {
                self.backend
                    .drain_events()
                    .map_err(|error| AgentError::Io(format!("drain failed: {error}")))?;
                let next = snapshot(&self.backend).unwrap_or_default();
                if split_lines(&next) == target_lines || Instant::now() >= deadline {
                    break next;
                }
                thread::sleep(Duration::from_millis(10));
            };
        }
        if split_lines(&current_content) != target_lines {
            return Err(AgentError::invalid_params(
                "buffer changed concurrently; agent write conflicts with user edits",
            ));
        }

        self.backend.save_buffer(buf_id).map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                AgentError::PermissionDenied {
                    reason: format!("save permission denied for {}", path.display()),
                }
            } else {
                AgentError::Io(format!("save failed: {error}"))
            }
        })?;
        Ok(())
    }

    /// Applies one diff hunk against the backend's inclusive line-range edit.
    ///
    /// `end` is exclusive; a pure insertion (`start == end`) anchors on the
    /// current line so nothing is overwritten.
    fn apply_diff_hunk(
        &mut self,
        current_lines: &[String],
        start: usize,
        end: usize,
        new_lines: &[String],
    ) -> Result<(), AgentError> {
        if start == end {
            let anchor = current_lines
                .get(start)
                .or_else(|| current_lines.last())
                .cloned()
                .unwrap_or_default();
            let mut replacement = new_lines.to_vec();
            replacement.push(anchor);
            let last = current_lines.len().saturating_sub(1);
            self.backend
                .replace_line_range(start.min(last), start.min(last), &replacement)
                .map_err(|error| AgentError::Io(format!("edit failed: {error}")))?;
            return Ok(());
        }
        self.backend
            .replace_line_range(start, end.saturating_sub(1), new_lines)
            .map_err(|error| AgentError::Io(format!("edit failed: {error}")))
    }

    pub(super) fn buffer_id_for_path(&self, path: &Path) -> Option<crate::buffer::BufferId> {
        self.backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, path)))
            .map(|buf| buf.id)
    }

    /// Captures one explicitly selected, primary-workspace context file.
    /// Open buffers win so unsaved user edits are the snapshot sent to agent.
    pub(crate) fn agent_context_file_snapshot(
        &self,
        relative_path: &Path,
        max_bytes: usize,
    ) -> Result<(PathBuf, String, String), String> {
        if relative_path.as_os_str().is_empty() || relative_path.is_absolute() {
            return Err(String::from("context path must be workspace-relative"));
        }
        let root = std::fs::canonicalize(&self.working_dir)
            .map_err(|error| format!("cannot access workspace: {error}"))?;
        let canonical = std::fs::canonicalize(root.join(relative_path)).map_err(|error| {
            format!("cannot access context file {}: {error}", relative_path.display())
        })?;
        let relative = canonical
            .strip_prefix(&root)
            .map_err(|_| String::from("context file outside primary workspace"))?
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if relative.is_empty()
            || is_protected_relative_path(&relative)
            || self.is_secret_store_path(&canonical)
        {
            return Err(format!("context file is protected: {relative}"));
        }
        let metadata = std::fs::metadata(&canonical)
            .map_err(|error| format!("cannot inspect context file {relative}: {error}"))?;
        if !metadata.is_file() {
            return Err(format!("context path is not a regular file: {relative}"));
        }

        let content = if let Some(buffer) = self.backend.all_bufs().iter().find(|buffer| {
            buffer.path.as_deref().is_some_and(|path| paths_equivalent(path, &canonical))
        }) {
            if buffer.is_vlf {
                return Err(format!("context file is too large to snapshot: {relative}"));
            }
            buffer
                .whole_text()
                .ok_or_else(|| format!("cannot snapshot context file: {relative}"))?
        } else {
            if metadata.len() > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
                return Err(format!("context file exceeds {max_bytes} byte limit: {relative}"));
            }
            std::fs::read_to_string(&canonical)
                .map_err(|error| format!("cannot read context file {relative}: {error}"))?
        };
        if content.len() > max_bytes {
            return Err(format!("context file exceeds {max_bytes} byte limit: {relative}"));
        }
        let secrets = self.agents_secret_values();
        let content = ee_agent_host::redact::redact_secret_values(&content, &secrets);
        Ok((canonical, relative, content))
    }

    pub(super) fn session_thread_by_id(&self, session_id: &str) -> Option<usize> {
        self.agents.thread_index(session_id)
    }
}
