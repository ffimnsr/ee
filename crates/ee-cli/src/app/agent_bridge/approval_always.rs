//! Always-allow approval flow: workspace-trusted candidate generation,
//! reuse-or-append persistence, and decision dispatch.
//!
//! Always-allow rules are workspace-wide (`agent: None`), carry no expiry and
//! no use budget, and live only in the host-local per-workspace trust store.
//! The option is offered and persisted only while the host-local workspace
//! trust decision is `trusted`; undecided, untrusted, or unreadable decisions
//! fail closed. Mandatory-confirm operations strip these options before the
//! prompt is shown (see `App::mark_mandatory_confirmation`).

use ee_agent_host::AgentError;

use super::super::*;

use crate::policy::{
    AlwaysRuleCandidate, OperationIdentity, TrustEffect, TrustOperation, TrustStoreError,
};

use super::approval::{ALWAYS_ALLOW_LABEL, ApprovalChoice, ApprovalKind};
use super::prompt::ApprovalPrompt;
use super::write::ActionLogEntry;

impl App {
    /// Attaches the always-allow candidates for one eligible operation.
    /// Fail closed: without a `trusted` workspace decision no candidate is
    /// generated and the option stays absent from the prompt.
    pub(super) fn attach_always_allows(
        &self,
        prompt: &mut ApprovalPrompt,
        operation: &TrustOperation,
    ) {
        prompt.options.retain(|(_, choice)| {
            !matches!(choice, ApprovalChoice::AllowAlways | ApprovalChoice::AllowAlwaysPrefix(_))
        });
        prompt.allow_candidates.clear();
        if !self.workspace_trusted() {
            return;
        }
        let mut candidates = Vec::new();
        match &prompt.kind {
            ApprovalKind::TerminalCreate { request } => {
                let Ok(invocation) = self.command_invocation_for_request(request) else {
                    return;
                };
                if let Ok(candidate) = AlwaysRuleCandidate::command_exact(&invocation) {
                    candidates.push((
                        ApprovalChoice::AllowAlways,
                        ALWAYS_ALLOW_LABEL.to_string(),
                        candidate,
                    ));
                }
                // Offer two deliberate token boundaries at most: first argument
                // and full argv. This keeps scope selection explicit without
                // allowing long requests to hide approval controls.
                if !invocation.argv.is_empty() {
                    for argument_count in [1, invocation.argv.len()] {
                        if argument_count == invocation.argv.len()
                            && argument_count == 1
                            && candidates.iter().any(|(choice, _, _)| {
                                *choice == ApprovalChoice::AllowAlwaysPrefix(1)
                            })
                        {
                            continue;
                        }
                        if let Ok(candidate) =
                            AlwaysRuleCandidate::command_prefix(&invocation, argument_count)
                        {
                            candidates.push((
                                ApprovalChoice::AllowAlwaysPrefix(argument_count),
                                format!("Allow always: prefix through argument {argument_count}"),
                                candidate,
                            ));
                        }
                    }
                }
            }
            ApprovalKind::Write { .. } | ApprovalKind::WriteBatch { .. }
                if prompt.mcp.is_some() =>
            {
                if let Some(invocation) = prompt.mcp.as_ref()
                    && let Ok(candidate) = AlwaysRuleCandidate::mcp_exact(invocation)
                {
                    candidates.push((
                        ApprovalChoice::AllowAlways,
                        ALWAYS_ALLOW_LABEL.to_string(),
                        candidate,
                    ));
                }
            }
            ApprovalKind::Write { path, content, expectation, .. } => {
                if let Some((write_operation, prefix, files, total, file)) =
                    self.native_single_write_rule_shape(path, content, expectation)
                    && let Ok(candidate) = AlwaysRuleCandidate::write_prefix(
                        operation.workspace,
                        write_operation,
                        prefix,
                        files,
                        total,
                        file,
                    )
                {
                    candidates.push((
                        ApprovalChoice::AllowAlways,
                        ALWAYS_ALLOW_LABEL.to_string(),
                        candidate,
                    ));
                }
            }
            ApprovalKind::WriteBatch { writes, .. } => {
                if let Some((write_operation, prefix, files, total, file)) =
                    self.native_batch_write_rule_shape(writes)
                    && let Ok(candidate) = AlwaysRuleCandidate::write_prefix(
                        operation.workspace,
                        write_operation,
                        prefix,
                        files,
                        total,
                        file,
                    )
                {
                    candidates.push((
                        ApprovalChoice::AllowAlways,
                        ALWAYS_ALLOW_LABEL.to_string(),
                        candidate,
                    ));
                }
            }
            ApprovalKind::Network { .. } => {
                if let OperationIdentity::Network { scheme, host, port, method, browser_action } =
                    &operation.identity
                    && let Ok(candidate) = AlwaysRuleCandidate::network_exact_read(
                        operation.workspace,
                        *scheme,
                        host.clone(),
                        *port,
                        *method,
                        *browser_action,
                    )
                {
                    candidates.push((
                        ApprovalChoice::AllowAlways,
                        ALWAYS_ALLOW_LABEL.to_string(),
                        candidate,
                    ));
                }
            }
            ApprovalKind::Filesystem { .. } | ApprovalKind::WorkspaceMemoryApproval { .. } => {}
        }
        for (choice, label, candidate) in candidates {
            prompt.options.push((label, choice));
            prompt.allow_candidates.push((choice, candidate));
        }
    }

    /// Reuses an identical enabled always-allow rule or appends a new one.
    /// A matching but disabled rule fails closed: the operation is not
    /// silently re-enabled, and no duplicate bypasses the explicit disable.
    fn persist_always_candidate(
        &mut self,
        candidate: &AlwaysRuleCandidate,
    ) -> Result<String, TrustStoreError> {
        if candidate.rule.effect() != TrustEffect::Allow
            || candidate.rule.scope().expires_at.is_some()
            || candidate.rule.scope().max_uses.is_some()
        {
            return Err(TrustStoreError::ValidationFailure(
                "always-allow candidate must not carry expiry or use budget".into(),
            ));
        }
        if !self.workspace_trusted() {
            return Err(TrustStoreError::ValidationFailure(
                "workspace trust decision is not trusted".into(),
            ));
        }
        let store = self.workspace_trust_store().ok_or(TrustStoreError::StateDirUnavailable)?;
        let managed = store.load_for_management_at(self.trust_clock.now())?;
        if let Some(existing) = managed.document.rules.iter().find(|rule| {
            rule.effect() == TrustEffect::Allow
                && rule.scope().expires_at.is_none()
                && rule.scope().max_uses.is_none()
                && rule.scope().workspace == candidate.rule.scope().workspace
                && rule.scope().agent == candidate.rule.scope().agent
                && rule.same_matcher(&candidate.rule)
        }) {
            let rule_id = existing.id().to_string();
            if managed.state(&rule_id).is_some_and(|state| !state.enabled) {
                return Err(TrustStoreError::ValidationFailure(format!(
                    "always-allow rule {rule_id} is disabled; enable it with /permissions enable {rule_id}"
                )));
            }
            self.agents.action_log.push(ActionLogEntry::TrustRuleMutation {
                rule_id: Some(rule_id.clone()),
                action: "reuse".into(),
                source: "approval-always-allow".into(),
            });
            return Ok(rule_id);
        }
        let rule_id = candidate.rule.id().to_string();
        store.add_rule(candidate.rule.clone())?;
        self.reload_workspace_trust_store()?;
        self.agents.action_log.push(ActionLogEntry::TrustRuleMutation {
            rule_id: Some(rule_id.clone()),
            action: "create".into(),
            source: "approval-always-allow".into(),
        });
        Ok(rule_id)
    }

    /// Resolves one always-allow choice: persist (or reuse) the previewed
    /// candidate, then dispatch the original request through the existing
    /// persistent-allow pipeline.
    pub(super) fn resolve_always_choice(
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
                reason: "always-allow approval has no previewed candidate".into(),
            }));
            return;
        };
        let rule_id = match self.persist_always_candidate(&candidate) {
            Ok(rule_id) => rule_id,
            Err(error) => {
                self.record_denied_write(&prompt.session_id, &prompt.kind);
                self.release_prompt_write_lease(&mut prompt);
                let _ = prompt.reply.send(Err(AgentError::PermissionDenied {
                    reason: format!("always-allow approval unavailable: {error}"),
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
}
