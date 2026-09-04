//! `impl App`: approval decision resolution and persistent rules.

use std::path::{Path, PathBuf};

use ee_agent_host::{AgentError, ClientRequestResponse, ClientRequestResult};
use ee_agent_protocol::CreateTerminalRequest;
use tokio::sync::oneshot;

use super::super::*;

use crate::policy::{
    BoundedRuleCandidate, DecisionReason, MAX_WRITE_FILE_BYTES, MAX_WRITE_FILES,
    MAX_WRITE_TOTAL_BYTES, OperationIdentity, PathPrefix, TrustCategory, TrustDecision,
    TrustEffect, TrustStoreError, WriteOperationKind, is_protected_relative_path,
};

use super::approval::{
    ApprovalChoice, ApprovalKind, PERSISTENT_WRITE_OPTION_LABEL, PreparedWrite, WebApprovalCall,
    WorkspaceMemoryApprovalTarget, WriteExpectation, approval_fingerprint, session_decision,
};
use super::prompt::ApprovalPrompt;
use super::write::ActionLogEntry;

impl App {
    pub(super) fn resolve_policy_deny(
        &mut self,
        mut prompt: ApprovalPrompt,
        decision: &TrustDecision,
    ) {
        let summary = if decision.reason == DecisionReason::BuiltInDeny {
            let rule_id = decision.rule_id.as_deref().unwrap_or("builtin.unknown");
            format!("blocked by non-overridable safeguard {rule_id}")
        } else {
            decision
                .rule_id
                .as_deref()
                .map(|rule_id| format!("blocked by workspace deny rule {rule_id}"))
                .unwrap_or_else(|| format!("operation denied ({})", decision.reason.as_str()))
        };
        self.record_denied_write(&prompt.session_id, &prompt.kind);
        self.record_denied_validation(&prompt.session_id, &prompt.kind);
        if let ApprovalKind::Network { current_host, call, .. } = &prompt.kind {
            let action = match call {
                WebApprovalCall::Search { .. } => "search",
                WebApprovalCall::Fetch { .. } => "fetch",
                WebApprovalCall::BrowserRun { request } => request.action.as_str(),
            };
            self.record_web_failure(action, current_host, "denied");
        }
        let error = if decision.reason == DecisionReason::BuiltInDeny {
            AgentError::NonOverridableDenied {
                rule_id: decision.rule_id.clone().unwrap_or_else(|| "builtin.unknown".into()),
                category: self
                    .built_in_safeguard_for_prompt(&prompt)
                    .map(|matched| matched.category.as_str().to_string())
                    .unwrap_or_else(|| "unknown".into()),
            }
        } else {
            AgentError::PermissionDenied { reason: summary.clone() }
        };
        self.release_prompt_write_lease(&mut prompt);
        let _ = prompt.reply.send(Err(error));
        if let Some(thread_index) = prompt.thread_index
            && let Some(thread) = self.agents.threads.get_mut(thread_index)
        {
            thread.push_system(summary.clone());
        }
        self.backend.status_message = Some(summary);
    }

    fn persist_deny_rule(&mut self, prompt: &ApprovalPrompt) -> Result<String, TrustStoreError> {
        let candidate = prompt.deny_candidate.as_ref().ok_or_else(|| {
            TrustStoreError::ValidationFailure(
                "approval has no narrow persistent deny scope".into(),
            )
        })?;
        let rule_id = candidate.rule.id().to_string();
        let store = self.workspace_trust_store().ok_or(TrustStoreError::StateDirUnavailable)?;
        store.add_rule(candidate.rule.clone())?;
        self.reload_workspace_trust_store()?;
        self.agents.action_log.push(ActionLogEntry::TrustRuleMutation {
            rule_id: Some(rule_id.clone()),
            action: "create".into(),
            source: "approval-deny".into(),
        });
        Ok(rule_id)
    }

    fn resolve_persistent_deny_choice(&mut self, mut prompt: ApprovalPrompt) {
        self.record_denied_write(&prompt.session_id, &prompt.kind);
        self.record_denied_validation(&prompt.session_id, &prompt.kind);
        if let ApprovalKind::Network { current_host, call, .. } = &prompt.kind {
            let action = match call {
                WebApprovalCall::Search { .. } => "search",
                WebApprovalCall::Fetch { .. } => "fetch",
                WebApprovalCall::BrowserRun { request } => request.action.as_str(),
            };
            self.record_web_failure(action, current_host, "denied");
        }
        let (reason, summary) = match self.persist_deny_rule(&prompt) {
            Ok(rule_id) => (
                format!("denied and saved workspace rule {rule_id}"),
                format!("workspace deny rule saved: {rule_id}"),
            ),
            Err(_) => (
                "user denied the operation; workspace deny rule was not saved".to_string(),
                "operation denied; workspace deny rule was not saved".to_string(),
            ),
        };
        self.release_prompt_write_lease(&mut prompt);
        let _ = prompt.reply.send(Err(AgentError::PermissionDenied { reason }));
        if let Some(thread_index) = prompt.thread_index
            && let Some(thread) = self.agents.threads.get_mut(thread_index)
        {
            thread.push_system(summary.clone());
        }
        self.backend.status_message = Some(summary);
    }

    fn persist_allow_candidate(
        &mut self,
        candidate: &BoundedRuleCandidate,
    ) -> Result<String, TrustStoreError> {
        if candidate.rule.effect() != TrustEffect::Allow
            || candidate.rule.scope().expires_at.is_none()
            || candidate.rule.scope().max_uses.is_none()
        {
            return Err(TrustStoreError::ValidationFailure(
                "bounded allow candidate lacks mandatory limits".into(),
            ));
        }
        let rule_id = candidate.rule.id().to_string();
        let store = self.workspace_trust_store().ok_or(TrustStoreError::StateDirUnavailable)?;
        store.add_rule(candidate.rule.clone())?;
        self.reload_workspace_trust_store()?;
        self.agents.action_log.push(ActionLogEntry::TrustRuleMutation {
            rule_id: Some(rule_id.clone()),
            action: "create".into(),
            source: "approval-bounded-allow".into(),
        });
        Ok(rule_id)
    }

    fn resolve_persistent_allow_choice(
        &mut self,
        mut prompt: ApprovalPrompt,
        choice: ApprovalChoice,
    ) {
        let Some(candidate) =
            prompt.allow_candidates.iter().find_map(|(candidate_choice, candidate)| {
                (*candidate_choice == choice).then_some(candidate.clone())
            })
        else {
            self.release_prompt_write_lease(&mut prompt);
            let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                reason: "persistent approval has no previewed bounded candidate".into(),
            }));
            return;
        };
        let rule_id = match self.persist_allow_candidate(&candidate) {
            Ok(rule_id) => rule_id,
            Err(error) => {
                self.record_denied_write(&prompt.session_id, &prompt.kind);
                self.release_prompt_write_lease(&mut prompt);
                let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                    reason: format!("persistent approval unavailable: {error}"),
                }));
                if let Some(thread) = prompt.thread_index
                    && let Some(thread) = self.agents.threads.get_mut(thread)
                {
                    thread.push_system("approval denied");
                }
                return;
            }
        };
        self.resolve_persistent_allow(prompt, rule_id);
    }

    /// Resolves one approval with the chosen policy decision.
    pub(super) fn resolve_approval(&mut self, mut prompt: ApprovalPrompt, choice: ApprovalChoice) {
        // A disconnected proxy client has dropped its receiver. Do not record
        // approval state or dispatch a side effect without a live requester.
        if prompt.reply.is_closed() {
            self.release_prompt_write_lease(&mut prompt);
            return;
        }

        if matches!(prompt.kind, ApprovalKind::WorkspaceMemoryApproval { .. }) {
            let approved = choice == ApprovalChoice::AllowOnce;
            let ApprovalKind::WorkspaceMemoryApproval { key, target, .. } = prompt.kind else {
                unreachable!()
            };
            let result = if !approved {
                match target {
                    WorkspaceMemoryApprovalTarget::ApprovalOnly => {
                        Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: false })
                    }
                    WorkspaceMemoryApprovalTarget::Remember { .. }
                    | WorkspaceMemoryApprovalTarget::Forget
                    | WorkspaceMemoryApprovalTarget::Retract
                    | WorkspaceMemoryApprovalTarget::RetractKey { .. }
                    | WorkspaceMemoryApprovalTarget::Clear
                    | WorkspaceMemoryApprovalTarget::DisableDelete { .. }
                    | WorkspaceMemoryApprovalTarget::Export { .. }
                    | WorkspaceMemoryApprovalTarget::ExportValue { .. }
                    | WorkspaceMemoryApprovalTarget::Import { .. } => {
                        Err(AgentError::PermissionDenied {
                            reason: String::from("user denied workspace memory operation"),
                        })
                    }
                }
            } else {
                match target {
                    WorkspaceMemoryApprovalTarget::ApprovalOnly => {
                        Ok(ClientRequestResponse::WorkspaceMemoryApproval { approved: true })
                    }
                    WorkspaceMemoryApprovalTarget::Remember { value } => self
                        .workspace_memory_remember(&key, &value)
                        .map(ClientRequestResponse::ProxyValue),
                    WorkspaceMemoryApprovalTarget::Forget => {
                        self.workspace_memory_forget(&key).map(ClientRequestResponse::ProxyValue)
                    }
                    WorkspaceMemoryApprovalTarget::Retract => {
                        self.workspace_memory_retract(&key).map(ClientRequestResponse::ProxyValue)
                    }
                    WorkspaceMemoryApprovalTarget::RetractKey { key } => {
                        self.workspace_memory_retract(&key).map(ClientRequestResponse::ProxyValue)
                    }
                    WorkspaceMemoryApprovalTarget::Clear => {
                        self.workspace_memory_clear().map(ClientRequestResponse::ProxyValue)
                    }
                    WorkspaceMemoryApprovalTarget::DisableDelete { config_path } => self
                        .workspace_memory_disable_delete(&config_path)
                        .map(ClientRequestResponse::ProxyValue),
                    WorkspaceMemoryApprovalTarget::Export { include_values } => self
                        .workspace_memory_export(include_values)
                        .map(ClientRequestResponse::ProxyValue),
                    WorkspaceMemoryApprovalTarget::ExportValue { include_values } => self
                        .workspace_memory_export_value(include_values)
                        .map(ClientRequestResponse::ProxyValue),
                    WorkspaceMemoryApprovalTarget::Import { export } => {
                        self.workspace_memory_import(*export).map(ClientRequestResponse::ProxyValue)
                    }
                }
            };
            let _ = prompt.reply.send(result);
            self.backend.status_message = Some(if approved {
                String::from("workspace memory mutation approved once")
            } else {
                String::from("workspace memory mutation denied")
            });
            return;
        }

        if choice == ApprovalChoice::DenyPersistent {
            self.resolve_persistent_deny_choice(prompt);
            return;
        }

        let fingerprint = approval_fingerprint(&prompt.kind);
        if let Some(decision) = session_decision(choice) {
            self.agents.approval_policy.record(&prompt.session_id, &fingerprint, decision);
        }
        if matches!(
            choice,
            ApprovalChoice::AllowPersistent
                | ApprovalChoice::AllowPersistentShort
                | ApprovalChoice::AllowPersistentPrefix(_)
                | ApprovalChoice::AllowPersistentPrefixShort(_)
        ) {
            self.resolve_persistent_allow_choice(prompt, choice);
            return;
        }
        let allow = choice.allows();
        if !allow {
            self.record_denied_write(&prompt.session_id, &prompt.kind);
            self.record_denied_validation(&prompt.session_id, &prompt.kind);
            if let ApprovalKind::Network { current_host, call, .. } = &prompt.kind {
                let action = match call {
                    WebApprovalCall::Search { .. } => "search",
                    WebApprovalCall::Fetch { .. } => "fetch",
                    WebApprovalCall::BrowserRun { request } => request.action.as_str(),
                };
                self.record_web_failure(action, current_host, "denied");
            }
            self.release_prompt_write_lease(&mut prompt);
            let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                reason: String::from("user denied the operation"),
            }));
            if let Some(thread) = prompt.thread_index
                && let Some(thread) = self.agents.threads.get_mut(thread)
            {
                thread.push_system("approval denied");
            }
            return;
        }
        if let Err(error) = self.validate_prompt_write_lease(&prompt) {
            self.release_prompt_write_lease(&mut prompt);
            let _ = prompt.reply.send(Err(error));
            return;
        }
        let write_lease = prompt.write_lease.take();
        prompt.write_lease_owner = None;
        match prompt.kind {
            ApprovalKind::Write {
                path,
                content,
                tool_call_id,
                expectation,
                reply_kind,
                proxy_edit_count,
            } => {
                self.apply_bridge_write(
                    PreparedWrite {
                        path,
                        content,
                        tool_call_id,
                        expectation,
                        reply_kind,
                        proxy_edit_count,
                    },
                    &prompt.session_id,
                    None,
                    prompt.reply,
                );
            }
            ApprovalKind::WriteBatch { writes, total_edit_count } => {
                self.apply_bridge_write_batch(
                    writes,
                    total_edit_count,
                    &prompt.session_id,
                    None,
                    prompt.reply,
                );
            }
            ApprovalKind::TerminalCreate { request } => {
                self.spawn_trusted_terminal(
                    &request,
                    &prompt.session_id,
                    prompt.agent_id.as_deref(),
                    None,
                    prompt.reply,
                );
            }
            ApprovalKind::Filesystem { operation } => {
                self.apply_proxy_filesystem(operation, prompt.reply);
            }
            ApprovalKind::WorkspaceMemoryApproval { .. } => unreachable!(),
            ApprovalKind::Network {
                route,
                requested_host,
                current_host,
                call,
                mut approved_hosts,
                cancellation,
            } => {
                approved_hosts.insert(current_host);
                self.dispatch_web_call(
                    route,
                    prompt.session_id,
                    requested_host,
                    call,
                    approved_hosts,
                    cancellation,
                    prompt.reply,
                );
            }
        }
        if let Some(id) = write_lease {
            self.agents.write_leases.release(id);
        }
    }

    /// Auto-resolves a prompt matched by a persisted host-local rule: the
    /// operation dispatches through the existing pipeline and the successful
    /// dispatch consumes one rule use.
    pub(super) fn resolve_persistent_allow(&mut self, mut prompt: ApprovalPrompt, rule_id: String) {
        let session_id = prompt.session_id.clone();
        match &prompt.kind {
            ApprovalKind::TerminalCreate { .. } => match prompt.kind {
                ApprovalKind::TerminalCreate { request } => self.spawn_trusted_terminal(
                    &request,
                    &session_id,
                    prompt.agent_id.as_deref(),
                    Some(rule_id),
                    prompt.reply,
                ),
                _ => unreachable!(),
            },
            ApprovalKind::Write { .. } | ApprovalKind::WriteBatch { .. } => {
                self.dispatch_write_prompt(prompt, Some(rule_id));
            }
            ApprovalKind::Network { .. } => match prompt.kind {
                ApprovalKind::Network {
                    route,
                    requested_host,
                    current_host,
                    call,
                    mut approved_hosts,
                    cancellation,
                } => {
                    // Network dispatch completion is asynchronous. Consume before
                    // dispatch so failed/cancelled attempts cannot expand authority.
                    self.agents.usage_ledger.record_use(
                        self.primary_workspace_identity(),
                        &session_id,
                        &rule_id,
                    );
                    approved_hosts.insert(current_host);
                    self.dispatch_web_call(
                        route,
                        session_id,
                        requested_host,
                        call,
                        approved_hosts,
                        cancellation,
                        prompt.reply,
                    );
                }
                _ => unreachable!(),
            },
            ApprovalKind::WorkspaceMemoryApproval { .. } => unreachable!(),
            ApprovalKind::Filesystem { .. } => {
                self.release_prompt_write_lease(&mut prompt);
                let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                    reason: String::from("persistent filesystem approval is not supported"),
                }));
            }
        }
    }

    /// Dispatches an approved write or write batch and consumes the matched
    /// persistent rule use only after the write succeeds.
    fn dispatch_write_prompt(&mut self, mut prompt: ApprovalPrompt, rule_id: Option<String>) {
        if let Err(error) = self.validate_prompt_write_lease(&prompt) {
            self.release_prompt_write_lease(&mut prompt);
            let _ = prompt.reply.send(Err(error));
            return;
        }
        let session_id = prompt.session_id.clone();
        let write_lease = prompt.write_lease.take();
        prompt.write_lease_owner = None;
        match prompt.kind {
            ApprovalKind::Write {
                path,
                content,
                tool_call_id,
                expectation,
                reply_kind,
                proxy_edit_count,
            } => self.apply_bridge_write(
                PreparedWrite {
                    path,
                    content,
                    tool_call_id,
                    expectation,
                    reply_kind,
                    proxy_edit_count,
                },
                &session_id,
                rule_id.as_deref(),
                prompt.reply,
            ),
            ApprovalKind::WriteBatch { writes, total_edit_count } => {
                self.apply_bridge_write_batch(
                    writes,
                    total_edit_count,
                    &session_id,
                    rule_id.as_deref(),
                    prompt.reply,
                );
            }
            _ => {
                let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                    reason: String::from("persistent approval not available for this operation"),
                }));
            }
        }
        if let Some(id) = write_lease {
            self.agents.write_leases.release(id);
        }
    }

    // ── Phase 5: native write normalization and bounded write grants ─────

    /// Canonical path identity shared by workspace validation, write leases,
    /// and policy normalization.
    pub(super) fn canonical_workspace_write_target(&self, path: &Path) -> Option<PathBuf> {
        let candidate = if path.exists() {
            std::fs::canonicalize(path).ok()?
        } else {
            let parent = std::fs::canonicalize(path.parent()?).ok()?;
            parent.join(path.file_name()?)
        };
        self.workspace_relative_segments(&candidate)?;
        Some(candidate)
    }

    /// Canonical in-workspace target eligible for bounded native-write trust.
    /// Protected and secret-store targets never qualify.
    pub(super) fn canonical_native_write_target(&self, path: &Path) -> Option<PathBuf> {
        let candidate = self.canonical_workspace_write_target(path)?;
        let relative = self.workspace_relative_segments(&candidate)?;
        if is_protected_relative_path(&relative) || self.is_secret_store_path(&candidate) {
            return None;
        }
        Some(candidate)
    }

    /// Normalized native write operation: canonical in-workspace target,
    /// create/modify category from the file-existence expectation, and the
    /// exact file count and byte deltas of this request.  Ineligible targets
    /// (external, traversal, symlink escape, protected) normalize to `None`
    /// and can never match a persistent rule.
    pub(super) fn native_write_operation(
        &self,
        path: &Path,
        content: &str,
        expectation: &WriteExpectation,
    ) -> Option<(TrustCategory, OperationIdentity)> {
        let candidate = self.canonical_native_write_target(path)?;
        let relative_path = self.workspace_relative_segments(&candidate)?;
        let exists = candidate.is_file();
        let category = match expectation {
            WriteExpectation::MustNotExist => TrustCategory::WriteCreate,
            WriteExpectation::ExpectRevision(_) => TrustCategory::WriteModify,
            WriteExpectation::Blind if exists => TrustCategory::WriteModify,
            WriteExpectation::Blind => TrustCategory::WriteCreate,
        };
        let bytes = content.len() as u64;
        Some((
            category,
            OperationIdentity::Write {
                relative_path,
                file_count: 1,
                total_bytes: Some(bytes),
                max_file_bytes: Some(bytes),
            },
        ))
    }

    /// Normalized native write-batch operation: every target must resolve
    /// canonically in-workspace, share one canonical parent directory, and
    /// agree on create vs modify; otherwise the batch is unknown and can
    /// never match a persistent rule.
    pub(crate) fn native_write_batch_operation(
        &self,
        writes: &[PreparedWrite],
    ) -> Option<(TrustCategory, OperationIdentity)> {
        if writes.is_empty() {
            return None;
        }
        let mut parent_dir: Option<PathBuf> = None;
        let mut relative_dir: Option<String> = None;
        let mut total_bytes = 0u64;
        let mut max_file_bytes = 0u64;
        let mut all_existing = true;
        let mut all_new = true;
        for write in writes {
            let candidate = self.canonical_native_write_target(&write.path)?;
            let dir = candidate.parent()?;
            match &parent_dir {
                None => {
                    parent_dir = Some(dir.to_path_buf());
                    relative_dir = self.workspace_relative_segments(dir);
                }
                Some(known) if known != dir => return None,
                _ => {}
            }
            total_bytes = total_bytes.saturating_add(write.content.len() as u64);
            max_file_bytes = max_file_bytes.max(write.content.len() as u64);
            let exists = candidate.is_file();
            all_existing &= exists;
            all_new &= !exists;
        }
        let category = if all_existing {
            TrustCategory::WriteModify
        } else if all_new {
            TrustCategory::WriteCreate
        } else {
            return None;
        };
        Some((
            category,
            OperationIdentity::Write {
                relative_path: relative_dir?,
                file_count: writes.len() as u64,
                total_bytes: Some(total_bytes),
                max_file_bytes: Some(max_file_bytes),
            },
        ))
    }

    /// Bounded persistent write rule derivable from one native write
    /// request: canonical directory prefix, exact request sizes, and the
    /// create/modify operation kind — all within the application safety
    /// maxima.  Root-level targets (no directory prefix) and over-maximum
    /// requests are ineligible.
    pub(super) fn native_single_write_rule_shape(
        &self,
        path: &Path,
        content: &str,
        expectation: &WriteExpectation,
    ) -> Option<(WriteOperationKind, PathPrefix, u64, u64, u64)> {
        let (category, identity) = self.native_write_operation(path, content, expectation)?;
        self.write_rule_shape_from(category, identity, path)
    }

    pub(crate) fn native_batch_write_rule_shape(
        &self,
        writes: &[PreparedWrite],
    ) -> Option<(WriteOperationKind, PathPrefix, u64, u64, u64)> {
        let (category, identity) = self.native_write_batch_operation(writes)?;
        self.write_rule_shape_from(category, identity, &writes.first()?.path)
    }

    /// Shared shape derivation: directory prefix from the first target and
    /// request bounds checked against the application safety maxima.
    fn write_rule_shape_from(
        &self,
        category: TrustCategory,
        identity: OperationIdentity,
        first_target: &Path,
    ) -> Option<(WriteOperationKind, PathPrefix, u64, u64, u64)> {
        let OperationIdentity::Write { file_count, total_bytes, max_file_bytes, .. } = identity
        else {
            return None;
        };
        let candidate = self.canonical_native_write_target(first_target)?;
        let dir = self.workspace_relative_segments(candidate.parent()?)?;
        let prefix = PathPrefix::parse(&dir).ok()?;
        let operation = match category {
            TrustCategory::WriteCreate => WriteOperationKind::Create,
            TrustCategory::WriteModify => WriteOperationKind::Modify,
            _ => return None,
        };
        if file_count == 0 || file_count > MAX_WRITE_FILES {
            return None;
        }
        let total = total_bytes?;
        let max_file = max_file_bytes?;
        if total == 0 || total > MAX_WRITE_TOTAL_BYTES || max_file > MAX_WRITE_FILE_BYTES {
            return None;
        }
        Some((operation, prefix, file_count, total, max_file))
    }

    /// Persistent option label for one eligible native write; `None` keeps
    /// the prompt on the four-choice UI (protected, external, root-level,
    /// and over-maximum requests never get a persistent grant).
    pub(super) fn native_write_persistent_label(
        &self,
        path: &Path,
        content: &str,
        expectation: &WriteExpectation,
    ) -> Option<&'static str> {
        self.native_single_write_rule_shape(path, content, expectation)
            .map(|_| PERSISTENT_WRITE_OPTION_LABEL)
    }

    /// Spawns an approved terminal through the existing pipeline and records
    /// the matched persistent rule use only after a successful spawn.
    fn spawn_trusted_terminal(
        &mut self,
        request: &CreateTerminalRequest,
        session_id: &str,
        agent_id: Option<&str>,
        rule_id: Option<String>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let validation = self.validation_run_for_terminal(session_id, request);
        let result = self.agents.terminals.spawn(request, agent_id);
        match (&result, validation) {
            (Ok(response), Some(validation)) => {
                if let Err(error) = self.agents.terminals.register_validation_run(
                    &response.terminal_id,
                    &request.session_id,
                    validation,
                ) {
                    tracing::warn!(
                        session_id,
                        ?error,
                        "cannot record terminal validation lifecycle"
                    );
                }
            }
            (Err(_), Some(validation)) => {
                self.record_unavailable_validation(session_id, request, validation);
            }
            _ => {}
        }
        if result.is_ok()
            && let Some(rule_id) = rule_id
        {
            self.agents.usage_ledger.record_use(
                self.primary_workspace_identity(),
                session_id,
                &rule_id,
            );
        }
        let _ = reply.send(result.map(ClientRequestResponse::CreateTerminal));
    }
}
