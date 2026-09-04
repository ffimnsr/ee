//! `impl App`: write verification, terminal validation, workspace path checks.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ee_agent_host::{
    AgentError, EvidenceCheck, EvidenceRevision, HostValidationRecord, TurnObservation,
    WriteEvidenceOutcome, WriteTransactionStage,
};
use ee_agent_protocol::CreateTerminalRequest;

use super::super::*;

use super::app_search::paths_equivalent;
use super::app_web::sha256_hex;
use super::approval::{ApprovalKind, WriteExpectation};
use super::terminal::{TerminalCompletion, TerminalValidationRun, terminal_command_line};
use super::write::{buffer_revision_id, text_revision_id};

impl App {
    /// Appends one host-owned observation only while this exact ACP session
    /// owns an active turn. Generic stdio MCP proxy calls use the synthetic
    /// `proxy` session and deliberately cannot borrow a pane turn's evidence.
    pub(super) fn observe_active_turn(&self, session_id: &str, observation: TurnObservation) {
        let Some(index) = self.session_thread_by_id(session_id) else {
            return;
        };
        let thread = &self.agents.threads[index].host;
        let Some(turn) = thread.active_turn_key() else {
            return;
        };
        if let Err(error) = thread.observe_turn_evidence(turn.turn_id(), observation) {
            tracing::warn!(
                session_id,
                turn_id = turn.turn_id(),
                ?error,
                "bridge evidence was not recorded"
            );
        }
    }

    pub(super) fn validation_run_for_terminal(
        &mut self,
        session_id: &str,
        request: &CreateTerminalRequest,
    ) -> Option<TerminalValidationRun> {
        let index = self.session_thread_by_id(session_id)?;
        let (revision, scope) = {
            let thread = &self.agents.threads[index];
            (thread.verification_revision.clone()?, thread.verification_paths.clone())
        };
        Some(TerminalValidationRun {
            revision,
            selector: terminal_command_line(request),
            diagnostics_before: self.refresh_diagnostic_error_count(&scope).ok(),
        })
    }

    pub(super) fn observe_transaction_stage(
        &self,
        session_id: &str,
        revision: EvidenceRevision,
        stage: WriteTransactionStage,
        outcome: EvidenceCheck,
    ) {
        self.observe_active_turn(
            session_id,
            TurnObservation::WriteTransaction { revision, stage, outcome },
        );
    }

    pub(super) fn record_unavailable_validation(
        &self,
        session_id: &str,
        request: &CreateTerminalRequest,
        validation: TerminalValidationRun,
    ) {
        self.observe_active_turn(
            session_id,
            TurnObservation::ValidationRecord {
                revision: validation.revision.clone(),
                selected: true,
                record: HostValidationRecord {
                    run_id: format!("unavailable:{}", terminal_command_line(request)),
                    command_id: terminal_command_line(request),
                    command: terminal_command_line(request),
                    tool: Some(String::from("terminal")),
                    selector: Some(validation.selector),
                    outcome: EvidenceCheck::Unavailable,
                    exit_status: None,
                    elapsed_ms: None,
                    affected_tests: Vec::new(),
                    diagnostics_delta: 0,
                    output_truncated: false,
                    skip_or_denial: Some(String::from("terminal_unavailable")),
                },
            },
        );
        self.observe_transaction_stage(
            session_id,
            validation.revision,
            WriteTransactionStage::Validation,
            EvidenceCheck::Unavailable,
        );
    }

    pub(super) fn record_denied_validation(&self, session_id: &str, kind: &ApprovalKind) {
        let ApprovalKind::TerminalCreate { request } = kind else {
            return;
        };
        let Some(index) = self.session_thread_by_id(session_id) else {
            return;
        };
        let Some(revision) = self.agents.threads[index].verification_revision.clone() else {
            return;
        };
        self.observe_active_turn(
            session_id,
            TurnObservation::ValidationRecord {
                revision: revision.clone(),
                selected: true,
                record: HostValidationRecord {
                    run_id: format!("denied:{}", terminal_command_line(request)),
                    command_id: terminal_command_line(request),
                    command: terminal_command_line(request),
                    tool: Some(String::from("terminal")),
                    selector: Some(String::from("approved_terminal")),
                    outcome: EvidenceCheck::Denied,
                    exit_status: None,
                    elapsed_ms: None,
                    affected_tests: Vec::new(),
                    diagnostics_delta: 0,
                    output_truncated: false,
                    skip_or_denial: Some(String::from("approval_denied")),
                },
            },
        );
        self.observe_transaction_stage(
            session_id,
            revision,
            WriteTransactionStage::Validation,
            EvidenceCheck::Denied,
        );
    }

    pub(super) fn record_denied_write(&self, session_id: &str, kind: &ApprovalKind) {
        let paths = match kind {
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
            session_id,
            TurnObservation::Write {
                revision: revision.clone(),
                outcome: WriteEvidenceOutcome::Denied,
            },
        );
        self.observe_transaction_stage(
            session_id,
            revision,
            WriteTransactionStage::Approval,
            EvidenceCheck::Denied,
        );
    }

    /// Hashes current buffer/disk revisions for exactly the write sequence.
    /// Raw paths and buffer contents never enter the evidence store.
    pub(super) fn evidence_revision_for_paths(
        &self,
        paths: &[PathBuf],
    ) -> Result<EvidenceRevision, AgentError> {
        let mut members = Vec::with_capacity(paths.len());
        for path in paths {
            let revision =
                self.current_text_revision(path)?.unwrap_or_else(|| String::from("missing"));
            let dirty = self
                .backend
                .all_bufs()
                .iter()
                .find(|buffer| {
                    buffer
                        .path
                        .as_deref()
                        .is_some_and(|candidate| paths_equivalent(candidate, path))
                })
                .is_some_and(|buffer| !buffer.pristine);
            members.push(format!("{}:{revision}:{dirty}", path.display()));
        }
        members.sort();
        Ok(EvidenceRevision::new(format!("sha256:{}", sha256_hex(members.join("\n").as_bytes()))))
    }

    pub(super) fn has_dirty_buffer(&self, paths: &[PathBuf]) -> bool {
        self.backend.all_bufs().iter().any(|buffer| {
            !buffer.pristine
                && buffer.path.as_deref().is_some_and(|candidate| {
                    paths.iter().any(|path| paths_equivalent(candidate, path))
                })
        })
    }

    pub(super) fn refresh_diagnostic_error_count(
        &mut self,
        paths: &[PathBuf],
    ) -> Result<u32, AgentError> {
        // Drain pending editor/LSP events before reading diagnostics. If the
        // host cannot produce a complete current snapshot, callers record an
        // unavailable fact rather than treating cached diagnostics as passing.
        let _ = self.backend.drain_events();
        let value = self.proxy_get_diagnostics(None)?;
        let diagnostics = serde_json::from_value::<ee_mcp::DiagnosticsResult>(value)
            .map_err(|error| AgentError::HandlerError(error.to_string()))?;
        if diagnostics.truncated {
            return Err(AgentError::HandlerError(String::from(
                "current diagnostics snapshot is truncated",
            )));
        }
        let mut path_set = paths.to_vec();
        path_set.sort();
        path_set.dedup();
        Ok(u32::try_from(
            diagnostics
                .diagnostics
                .into_iter()
                .filter(|entry| {
                    entry.severity == "error"
                        && path_set
                            .iter()
                            .any(|path| paths_equivalent(path, Path::new(&entry.path)))
                })
                .count(),
        )
        .unwrap_or(u32::MAX))
    }

    /// Collects current editor/Git facts after a successful buffer write. A
    /// missing tool, dirty user buffer, conflict, truncated response, or
    /// unavailable Git diff leaves the turn blocked/unverified instead of
    /// treating model prose or ACP completion as verification.
    pub(super) fn collect_post_write_verification(
        &mut self,
        session_id: &str,
        paths: &[PathBuf],
        diagnostics_before: Option<u32>,
    ) {
        let Some(index) = self.session_thread_by_id(session_id) else {
            return;
        };
        let scope = {
            let thread = &mut self.agents.threads[index];
            for path in paths {
                if !thread.verification_paths.iter().any(|known| paths_equivalent(known, path)) {
                    thread.verification_paths.push(path.clone());
                }
            }
            thread.verification_paths.clone()
        };
        // Apply pending editor updates before snapshotting one revision for every
        // verification fact collected below. Otherwise later host observations can
        // correctly invalidate evidence that was already stale at collection time.
        let _ = self.backend.drain_events();
        let Ok(revision) = self.evidence_revision_for_paths(&scope) else {
            return;
        };
        self.agents.threads[index].verification_revision = Some(revision.clone());
        self.observe_active_turn(
            session_id,
            TurnObservation::Revision { revision: revision.clone() },
        );

        let changed_files = match self.proxy_changed_files_result() {
            Ok(result) => result,
            Err(_) => return,
        };
        let expected_present = scope.iter().all(|expected| {
            changed_files
                .files
                .iter()
                .any(|entry| paths_equivalent(expected, Path::new(&entry.path)))
        });
        let has_unsafe_buffer = changed_files.files.iter().any(|entry| {
            scope.iter().any(|expected| paths_equivalent(expected, Path::new(&entry.path)))
                && (entry.conflicted || entry.dirty || !entry.saved)
        });
        self.observe_active_turn(
            session_id,
            TurnObservation::ChangedFiles {
                revision: revision.clone(),
                files: changed_files.files.iter().map(|entry| entry.path.clone()).collect(),
                truncated: changed_files.truncated || !expected_present || has_unsafe_buffer,
            },
        );

        let diagnostics_outcome =
            match (diagnostics_before, self.refresh_diagnostic_error_count(&scope)) {
                (Some(before), Ok(after)) if after <= before => EvidenceCheck::Passed,
                (Some(_), Ok(_)) => EvidenceCheck::Failed,
                _ => EvidenceCheck::Unavailable,
            };
        self.observe_active_turn(
            session_id,
            TurnObservation::Diagnostics {
                revision: revision.clone(),
                outcome: diagnostics_outcome,
            },
        );
        self.observe_transaction_stage(
            session_id,
            revision.clone(),
            WriteTransactionStage::Diagnostics,
            diagnostics_outcome,
        );

        let diff_outcome = self.proxy_git_diff().and_then(|value| {
            serde_json::from_value::<ee_mcp::GitDiffResult>(value)
                .map_err(|error| AgentError::HandlerError(error.to_string()))
        });
        let review_passed = diff_outcome.is_ok_and(|diff| {
            !diff.truncated
                && !diff.diff.is_empty()
                && !has_unsafe_buffer
                && changed_files.files.iter().all(|entry| !entry.conflicted)
        });
        let diff_outcome =
            if review_passed { EvidenceCheck::Passed } else { EvidenceCheck::Unavailable };
        self.observe_active_turn(
            session_id,
            TurnObservation::DiffReview { revision: revision.clone(), outcome: diff_outcome },
        );
        self.observe_transaction_stage(
            session_id,
            revision.clone(),
            WriteTransactionStage::FinalDiff,
            diff_outcome,
        );

        // Do not synthesize a pending validation command. A selected terminal
        // is registered only after its approved spawn and contributes evidence
        // only after its observed lifecycle completes.
    }

    pub(super) fn record_terminal_validation(&mut self, completion: TerminalCompletion) {
        let Some(validation) = completion.validation else {
            return;
        };
        let Some(index) = self.session_thread_by_id(&completion.session_id) else {
            return;
        };
        let scope = self.agents.threads[index].verification_paths.clone();
        let diagnostics_after = self.refresh_diagnostic_error_count(&scope);
        let (diagnostics_outcome, diagnostics_delta) =
            match (validation.diagnostics_before, diagnostics_after) {
                (Some(before), Ok(after)) if after <= before => {
                    (EvidenceCheck::Passed, i64::from(after) - i64::from(before))
                }
                (Some(before), Ok(after)) => {
                    (EvidenceCheck::Failed, i64::from(after) - i64::from(before))
                }
                _ => (EvidenceCheck::Unavailable, 0),
            };
        self.observe_active_turn(
            &completion.session_id,
            TurnObservation::Diagnostics {
                revision: validation.revision.clone(),
                outcome: diagnostics_outcome,
            },
        );
        let outcome = if matches!(diagnostics_outcome, EvidenceCheck::Passed) {
            match completion.exit_code {
                Some(0) => EvidenceCheck::Passed,
                Some(_) => EvidenceCheck::Failed,
                None => EvidenceCheck::Unavailable,
            }
        } else {
            EvidenceCheck::Unavailable
        };
        self.observe_active_turn(
            &completion.session_id,
            TurnObservation::ValidationRecord {
                revision: validation.revision.clone(),
                selected: true,
                record: HostValidationRecord {
                    run_id: completion.terminal_id.clone(),
                    command_id: completion.terminal_id,
                    command: completion.command.clone(),
                    tool: Some(String::from("terminal")),
                    selector: Some(validation.selector),
                    outcome,
                    exit_status: completion.exit_code,
                    elapsed_ms: Some(completion.elapsed_ms),
                    affected_tests: Vec::new(),
                    diagnostics_delta,
                    output_truncated: completion.output_truncated,
                    skip_or_denial: None,
                },
            },
        );
        self.observe_transaction_stage(
            &completion.session_id,
            validation.revision,
            WriteTransactionStage::Validation,
            outcome,
        );
        self.agents.threads[index].push_system(format!(
            "validation terminal completed (exit: {}; {}ms; output truncated: {})",
            completion.exit_code.map_or_else(|| String::from("unknown"), |code| code.to_string()),
            completion.elapsed_ms,
            completion.output_truncated,
        ));
    }

    pub(super) fn path_in_workspace(&self, path: &Path) -> bool {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.canonical_workspace_roots().iter().any(|root| canonical.starts_with(root))
    }

    pub(super) fn path_in_effective_workspace(&self, path: &Path) -> bool {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.allowed_fs_roots().iter().any(|root| canonical.starts_with(root))
    }

    pub(crate) fn canonical_workspace_roots(&self) -> Vec<PathBuf> {
        let mut roots = BTreeSet::new();
        for root in self.agents_workspace_roots() {
            if !root.is_absolute() {
                continue;
            }
            if let Ok(canonical) = std::fs::canonicalize(&root) {
                roots.insert(canonical);
            }
        }
        roots.into_iter().collect()
    }

    pub(super) fn active_file_path(&self) -> Option<PathBuf> {
        self.backend.active().path.clone()
    }

    pub(super) fn active_root_path(&self) -> Option<PathBuf> {
        let roots = self.canonical_workspace_roots();
        let active_file = self
            .active_file_path()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .or_else(|| std::fs::canonicalize(&self.working_dir).ok());
        active_file.and_then(|path| roots.into_iter().find(|root| path.starts_with(root)))
    }

    pub(super) fn allowed_fs_roots(&self) -> Vec<PathBuf> {
        self.active_root_path().map_or_else(|| self.canonical_workspace_roots(), |root| vec![root])
    }

    pub(super) fn validate_workspace_write_path(&self, path: &Path) -> Result<(), AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        let candidate = if path.exists() {
            std::fs::canonicalize(path).map_err(|error| {
                AgentError::Io(format!("cannot access {}: {error}", path.display()))
            })?
        } else {
            let Some(parent) = path.parent() else {
                return Err(AgentError::invalid_params(format!(
                    "path has no parent directory: {}",
                    path.display()
                )));
            };
            let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
                AgentError::Io(format!("cannot access parent {}: {error}", parent.display()))
            })?;
            let Some(name) = path.file_name() else {
                return Err(AgentError::invalid_params(format!(
                    "path has no file name: {}",
                    path.display()
                )));
            };
            canonical_parent.join(name)
        };
        if self.allowed_fs_roots().iter().any(|root| candidate.starts_with(root)) {
            Ok(())
        } else {
            Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )))
        }
    }

    pub(super) fn current_text_revision(&self, path: &Path) -> Result<Option<String>, AgentError> {
        if let Some(buf) = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, path)))
        {
            return Ok(Some(buffer_revision_id(buf)));
        }
        if !path.exists() {
            return Ok(None);
        }
        if !self.path_in_effective_workspace(path) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|error| AgentError::Io(format!("cannot read {}: {error}", path.display())))?;
        Ok(Some(text_revision_id(&content)))
    }

    pub(super) fn validate_write_expectation(
        &self,
        path: &Path,
        expectation: &WriteExpectation,
    ) -> Result<(), AgentError> {
        match expectation {
            WriteExpectation::Blind => Ok(()),
            WriteExpectation::MustNotExist => {
                if self.current_text_revision(path)?.is_some() {
                    Err(AgentError::invalid_params(format!(
                        "target already exists or was created before approval: {}",
                        path.display()
                    )))
                } else {
                    Ok(())
                }
            }
            WriteExpectation::ExpectRevision(expected) => {
                let actual = self.current_text_revision(path)?;
                if actual.as_deref() == Some(expected.as_str()) {
                    Ok(())
                } else {
                    Err(AgentError::invalid_params(format!(
                        "buffer changed after tool prepared edit for {}; re-read and retry",
                        path.display()
                    )))
                }
            }
        }
    }

    pub(super) fn read_current_text(
        &mut self,
        path: &Path,
    ) -> Result<(String, String), AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::invalid_params("path must be absolute"));
        }
        if let Some(buf) = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, path)))
        {
            if buf.is_vlf {
                return Err(AgentError::invalid_params(
                    "full-buffer edits are not supported for very large file buffers",
                ));
            }
            let content = buf.whole_text().unwrap_or_default();
            let revision = text_revision_id(&content);
            return Ok((content, revision));
        }
        if !self.path_in_effective_workspace(path) {
            return Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                path.display()
            )));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|error| AgentError::Io(format!("cannot read {}: {error}", path.display())))?;
        let revision = text_revision_id(&content);
        Ok((content, revision))
    }

    pub(super) fn prepare_replace_text(
        &mut self,
        path: &Path,
        old_text: &str,
        new_text: &str,
    ) -> Result<(String, WriteExpectation), AgentError> {
        if old_text.is_empty() {
            return Err(AgentError::invalid_params("old_text must not be empty"));
        }
        let (content, revision) = self.read_current_text(path)?;
        let matches = content.match_indices(old_text).count();
        match matches {
            1 => Ok((
                content.replacen(old_text, new_text, 1),
                WriteExpectation::ExpectRevision(revision),
            )),
            0 => Err(AgentError::invalid_params(format!(
                "old_text was not found in {}",
                path.display()
            ))),
            count => Err(AgentError::invalid_params(format!(
                "old_text matched {count} times in {}; expected exactly one match",
                path.display()
            ))),
        }
    }

    pub(super) fn prepare_apply_patch(
        &mut self,
        path: &Path,
        edits: &[ee_agent_host::ProxyTextEdit],
    ) -> Result<(String, WriteExpectation), AgentError> {
        if edits.is_empty() {
            return Err(AgentError::invalid_params("edits must not be empty"));
        }
        let (mut content, revision) = self.read_current_text(path)?;
        for (index, edit) in edits.iter().enumerate() {
            if edit.old_text.is_empty() {
                return Err(AgentError::invalid_params(format!(
                    "edit {} old_text must not be empty",
                    index + 1
                )));
            }
            let matches = content.match_indices(edit.old_text.as_str()).count();
            match matches {
                1 => {
                    content = content.replacen(edit.old_text.as_str(), edit.new_text.as_str(), 1);
                }
                0 => {
                    return Err(AgentError::invalid_params(format!(
                        "edit {} old_text was not found in {}",
                        index + 1,
                        path.display()
                    )));
                }
                count => {
                    return Err(AgentError::invalid_params(format!(
                        "edit {} old_text matched {count} times in {}; expected exactly one match",
                        index + 1,
                        path.display()
                    )));
                }
            }
        }
        Ok((content, WriteExpectation::ExpectRevision(revision)))
    }
}
