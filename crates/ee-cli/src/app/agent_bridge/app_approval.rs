//! `impl App`: approval prompt queueing, web/workspace-memory requests.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use ee_agent_host::{ClientRequestResponse, ClientRequestResult};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::super::*;

use crate::app::agents_mcp::ProxyRoute;

use crate::policy::{
    CommandRule, DecisionReason, FilesystemRule, HostMatchMode, MatchMode, McpDenyRule,
    NetworkRule, OperationIdentity, PathPrefix, ToolRule, ToolRuleIdentity, TrustCategory,
    TrustDecision, TrustEffect, TrustOperation, TrustOutcome, TrustRule, TrustRuleScope,
    WriteOperationKind, WriteRule, generate_command_rule_id, generate_filesystem_rule_id,
    generate_mcp_rule_id, generate_network_rule_id, generate_tool_rule_id, generate_write_rule_id,
};

use super::app_web::{NEXT_WEB_LIFECYCLE_ID, web_context_agent_error};
use super::approval::{
    ApprovalChoice, ApprovalKind, DenyScopePreview, MandatoryConfirmation, PersistentDenyCandidate,
    WebApprovalCall, WorkspaceMemoryApprovalOperation, WorkspaceMemoryApprovalTarget,
    approval_fingerprint,
};
use super::bridge_ui::BridgeUiMessage;
use super::prompt::{ApprovalPrompt, format_expiry_utc};

impl App {
    /// Queues or dispatches one network tool call. Trusted global hosts bypass
    /// UI; all other hosts require an isolated route-and-host session decision.
    pub(super) fn queue_web_approval(
        &mut self,
        route: ProxyRoute,
        approval_scope: String,
        call: WebApprovalCall,
        cancellation: CancellationToken,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let (host, provider_label) = {
            let service = match self.web_context_service() {
                Ok(service) => service,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            match &call {
                WebApprovalCall::Search { .. } => (
                    service.search_initial_host(),
                    Some(service.search_provider_approval_label().to_owned()),
                ),
                WebApprovalCall::Fetch { url } => (
                    service
                        .fetch_initial_host(&ee_agent_host::WebFetchRequest { url: url.clone() }),
                    None,
                ),
                WebApprovalCall::BrowserRun { request } => (
                    service.browser_run_initial_host(request),
                    Some(String::from("Cloudflare Browser Run")),
                ),
            }
        };
        let host = match host {
            Ok(host) => host,
            Err(error) => {
                let _ = reply.send(Err(web_context_agent_error(error)));
                return;
            }
        };
        let preapproved =
            self.web_context_service().is_ok_and(|service| service.is_preapproved_host(&host));
        let network_session_id =
            format!("proxy-network:{}:{approval_scope}", route.transport_identity());
        if preapproved {
            self.dispatch_web_call(
                route,
                network_session_id,
                host,
                call,
                BTreeSet::new(),
                cancellation,
                reply,
            );
        } else {
            self.request_web_approval(ApprovalPrompt::web(
                route,
                network_session_id,
                host.clone(),
                host,
                provider_label.as_deref(),
                call,
                BTreeSet::new(),
                cancellation,
                reply,
            ));
        }
    }

    fn attach_persistent_deny(&self, prompt: &mut ApprovalPrompt, operation: &TrustOperation) {
        let Some(candidate) = self.persistent_deny_candidate(prompt, operation) else {
            return;
        };
        prompt.options.push((
            ApprovalChoice::DenyPersistent.label().to_string(),
            ApprovalChoice::DenyPersistent,
        ));
        prompt.deny_candidate = Some(candidate);
    }

    fn persistent_deny_candidate(
        &self,
        prompt: &ApprovalPrompt,
        operation: &TrustOperation,
    ) -> Option<PersistentDenyCandidate> {
        let scope = TrustRuleScope {
            workspace: operation.workspace,
            agent: prompt.agent_id.clone(),
            expires_at: None,
            max_uses: None,
        };
        let (rule, matcher_fields) = match &operation.identity {
            OperationIdentity::Command { executable, argv } => {
                let rule = TrustRule::Command(CommandRule {
                    id: generate_command_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    executable: executable.clone(),
                    match_mode: MatchMode::ArgvExact,
                    argv: argv.clone(),
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "command".into()),
                        ("executable".into(), executable.clone()),
                        ("arguments".into(), format!("exact · {} tokens", argv.len())),
                    ],
                )
            }
            OperationIdentity::Mcp {
                server,
                transport_identity,
                tool,
                tool_schema_version,
                ..
            }
            | OperationIdentity::McpRead {
                server,
                transport_identity,
                tool,
                tool_schema_version,
                ..
            } => {
                let rule = TrustRule::mcp_deny(McpDenyRule {
                    id: generate_mcp_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    server: server.clone(),
                    transport_identity: transport_identity.clone(),
                    tool: tool.clone(),
                    tool_schema_version: *tool_schema_version,
                    category: Some(operation.category),
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "mcp".into()),
                        ("server".into(), server.clone()),
                        ("transport".into(), transport_identity.clone()),
                        ("tool".into(), tool.clone()),
                        ("schema".into(), tool_schema_version.to_string()),
                        ("category".into(), operation.category.as_str().into()),
                    ],
                )
            }
            OperationIdentity::Write { relative_path, .. } => {
                let operation_kind = match operation.category {
                    TrustCategory::WriteCreate => WriteOperationKind::Create,
                    TrustCategory::WriteModify => WriteOperationKind::Modify,
                    _ => return None,
                };
                let rule = TrustRule::Write(WriteRule {
                    id: generate_write_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    operation: operation_kind,
                    path_prefix: PathPrefix::parse(relative_path).ok()?,
                    max_files: 0,
                    max_total_bytes: 0,
                    max_file_bytes: 0,
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "filesystem write".into()),
                        ("operation".into(), format!("{operation_kind:?}").to_ascii_lowercase()),
                        ("path prefix".into(), relative_path.clone()),
                    ],
                )
            }
            OperationIdentity::Filesystem {
                operation: filesystem_operation,
                source_path,
                destination_path,
            } => {
                let path = source_path.as_ref().or(destination_path.as_ref())?;
                let rule = TrustRule::filesystem(FilesystemRule {
                    id: generate_filesystem_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    operations: vec![*filesystem_operation],
                    path_prefix: PathPrefix::parse(path).ok()?,
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "filesystem".into()),
                        (
                            "operation".into(),
                            format!("{filesystem_operation:?}").to_ascii_lowercase(),
                        ),
                        ("path prefix".into(), path.clone()),
                    ],
                )
            }
            OperationIdentity::Network { scheme, host, port, method, browser_action } => {
                let rule = TrustRule::Network(
                    NetworkRule::deny(
                        generate_network_rule_id(),
                        scope,
                        *scheme,
                        host.clone(),
                        HostMatchMode::Exact,
                        *port,
                        *method,
                        *browser_action,
                    )
                    .ok()?,
                );
                (
                    rule,
                    vec![
                        ("kind".into(), "network".into()),
                        ("scheme".into(), format!("{scheme:?}").to_ascii_lowercase()),
                        ("host".into(), host.clone()),
                        ("port".into(), port.to_string()),
                        ("method class".into(), format!("{method:?}").to_ascii_lowercase()),
                        (
                            "browser action".into(),
                            format!("{browser_action:?}").to_ascii_lowercase(),
                        ),
                    ],
                )
            }
            OperationIdentity::NativeTool { tool } => {
                let rule = TrustRule::tool(ToolRule {
                    id: generate_tool_rule_id(),
                    effect: TrustEffect::Deny,
                    scope,
                    identity: ToolRuleIdentity::Native { tool: tool.clone() },
                    category: Some(operation.category),
                });
                (
                    rule,
                    vec![
                        ("kind".into(), "native tool/category".into()),
                        ("tool".into(), tool.clone()),
                        ("category".into(), operation.category.as_str().into()),
                    ],
                )
            }
            _ => return None,
        };
        Some(PersistentDenyCandidate {
            rule,
            preview: DenyScopePreview {
                workspace: operation.workspace.as_string(),
                agent: prompt.agent_id.clone().unwrap_or_else(|| "all agents".into()),
                matcher_fields,
                expires: "never".into(),
            },
        })
    }

    /// Network approvals use typed persistent deny and session policy, but
    /// never persistent allow or approval-mode bypass.
    pub(super) fn request_web_approval(&mut self, mut prompt: ApprovalPrompt) {
        let fingerprint = approval_fingerprint(&prompt.kind);
        let operation = self.trust_operation_for_prompt(&prompt);
        self.attach_always_allows(&mut prompt, &operation);
        self.attach_persistent_deny(&mut prompt, &operation);
        let decision = self.evaluate_operation(&operation, &prompt.session_id, &fingerprint);
        self.mark_mandatory_confirmation(&mut prompt, &operation, &decision);
        self.push_trust_audit(&operation, &decision, &prompt.session_id);
        match &decision {
            TrustDecision {
                outcome: TrustOutcome::Allow,
                reason: DecisionReason::PersistentAllow,
                rule_id: Some(rule_id),
            } => {
                self.resolve_persistent_allow(prompt, rule_id.clone());
                return;
            }
            TrustDecision { outcome: TrustOutcome::Allow, .. } => {
                self.resolve_approval(prompt, ApprovalChoice::AllowSession);
                return;
            }
            TrustDecision { outcome: TrustOutcome::Deny, .. } => {
                self.resolve_policy_deny(prompt, &decision);
                return;
            }
            TrustDecision { outcome: TrustOutcome::Confirm, .. } => {}
        }
        if let ApprovalKind::Network { current_host, call, .. } = &prompt.kind {
            let action = match call {
                WebApprovalCall::Search { .. } => "search",
                WebApprovalCall::Fetch { .. } => "fetch",
                WebApprovalCall::BrowserRun { request } => request.action.as_str(),
            };
            self.record_web_failure(action, current_host, "approval required");
        }
        self.agents.approvals.push_back(prompt);
        self.backend.status_message = Some(String::from("network approval required"));
    }

    pub(super) fn clear_proxy_network_scope(&mut self, scope: &str) {
        let prefix = format!("proxy-network:{}:{scope}", ProxyRoute::Stdio.transport_identity());
        self.agents.approval_policy.invalidate_session(&prefix);
        // Dropping each sender makes any caller resolve as cancelled. Sending
        // from `retain` would require moving out of a borrowed prompt.
        self.agents.approvals.retain(|prompt| prompt.session_id != prefix);
    }

    pub(super) fn record_web_failure(&mut self, action: &str, host: &str, status: &str) {
        let lifecycle_id = NEXT_WEB_LIFECYCLE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.record_web_lifecycle(
            &format!("web-{lifecycle_id}"),
            &format!("web/{action}"),
            status,
            &format!(
                "kind: fetch · host: {host} · outcome: {status} · trust: untrusted external content"
            ),
        );
    }

    pub(crate) fn prune_cancelled_bridge_approvals(&mut self) {
        let mut retained = VecDeque::with_capacity(self.agents.approvals.len());
        while let Some(mut prompt) = self.agents.approvals.pop_front() {
            if prompt.reply.is_closed() {
                self.release_prompt_write_lease(&mut prompt);
            } else {
                retained.push_back(prompt);
            }
        }
        self.agents.approvals = retained;
    }

    pub(crate) fn queue_workspace_memory_forget(&mut self, key: String) {
        let (reply, receiver) = oneshot::channel();
        self.request_workspace_memory_approval(ApprovalPrompt::workspace_memory(
            ee_agent_host::WorkspaceMemoryMutationOperation::Forget,
            key,
            WorkspaceMemoryApprovalTarget::Forget,
            reply,
        ));
        self.forward_workspace_memory_slash_result("forget", receiver);
    }

    pub(crate) fn queue_workspace_memory_retract(&mut self, key: String) {
        self.queue_workspace_memory_management(
            WorkspaceMemoryApprovalOperation::Retract,
            key,
            WorkspaceMemoryApprovalTarget::Retract,
        );
    }

    pub(crate) fn queue_workspace_memory_clear(&mut self) {
        self.queue_workspace_memory_management(
            WorkspaceMemoryApprovalOperation::Clear,
            String::from("all facts in primary canonical workspace"),
            WorkspaceMemoryApprovalTarget::Clear,
        );
    }

    pub(crate) fn queue_workspace_memory_disable_delete(&mut self, config_path: PathBuf) {
        self.queue_workspace_memory_management(
            WorkspaceMemoryApprovalOperation::DisableDelete,
            String::from(
                "all facts in primary canonical workspace; persist enabled = false only after clear",
            ),
            WorkspaceMemoryApprovalTarget::DisableDelete { config_path },
        );
    }

    pub(crate) fn queue_workspace_memory_export(&mut self, include_values: bool) {
        let scope = if include_values {
            "all facts including values"
        } else {
            "all facts with values redacted"
        };
        self.queue_workspace_memory_management(
            WorkspaceMemoryApprovalOperation::Export,
            scope.to_string(),
            WorkspaceMemoryApprovalTarget::Export { include_values },
        );
    }

    pub(crate) fn queue_workspace_memory_import(
        &mut self,
        path: &Path,
        export: ee_agent_host::WorkspaceMemoryExportDto,
    ) {
        self.queue_workspace_memory_management(
            WorkspaceMemoryApprovalOperation::Import,
            path.display().to_string(),
            WorkspaceMemoryApprovalTarget::Import { export: Box::new(export) },
        );
    }

    fn queue_workspace_memory_management(
        &mut self,
        operation: WorkspaceMemoryApprovalOperation,
        key: String,
        target: WorkspaceMemoryApprovalTarget,
    ) {
        let (reply, receiver) = oneshot::channel();
        self.request_workspace_memory_approval(ApprovalPrompt::workspace_memory_management(
            operation, key, target, reply,
        ));
        self.forward_workspace_memory_slash_result(operation.label(), receiver);
    }

    fn forward_workspace_memory_slash_result(
        &self,
        operation: &'static str,
        receiver: oneshot::Receiver<ClientRequestResult>,
    ) {
        let bridge = self.agents.bridge_tx.clone();
        let thread_name = format!("ee-workspace-memory-{operation}");
        let _ = std::thread::Builder::new().name(thread_name).spawn(move || {
            let text = match receiver.blocking_recv() {
                Ok(Ok(ClientRequestResponse::ProxyValue(value))) => {
                    format!("workspace memory {operation} completed: {value}")
                }
                Ok(Ok(_)) => format!("workspace memory {operation} completed"),
                Ok(Err(error)) => format!("workspace memory {operation} failed: {error}"),
                Err(_) => format!("workspace memory {operation} cancelled"),
            };
            let _ = bridge.send(BridgeUiMessage::WorkspaceMemorySlashResult { text });
        });
    }

    pub(super) fn request_workspace_memory_approval(&mut self, prompt: ApprovalPrompt) {
        self.agents.approvals.push_back(prompt);
        self.backend.status_message = Some(if self.agents.layout == AgentPaneLayout::Closed {
            String::from("workspace memory approval required (open :agents)")
        } else {
            String::from("workspace memory approval required")
        });
    }

    /// Queues an approval prompt (front of the queue wins) and notifies,
    /// unless the shared policy (session state first, then persistent
    /// rules) already resolves it without UI.
    pub(super) fn request_bridge_approval(&mut self, mut prompt: ApprovalPrompt) {
        let thread_index = prompt.thread_index;
        let session_id = prompt.session_id.clone();
        let fingerprint = approval_fingerprint(&prompt.kind);
        let operation = self.trust_operation_for_prompt(&prompt);
        let safeguard = self.built_in_safeguard_for_prompt(&prompt);
        self.attach_always_allows(&mut prompt, &operation);
        self.attach_persistent_deny(&mut prompt, &operation);
        let mut decision = self.evaluate_operation_with_safeguard(
            &operation,
            &session_id,
            &fingerprint,
            safeguard,
        );
        // Phase 4 curated-profile fallback: a terminal request that matches
        // a fixed registry entry is evaluated as its profile when the exact
        // command grant did not cover it.  The narrower exact grant always
        // wins; the profile grant fills the gap.
        let mut audited_operation = operation.clone();
        if matches!(decision.outcome, TrustOutcome::Confirm)
            && matches!(
                decision.reason,
                DecisionReason::NoMatchingRule
                    | DecisionReason::WorkspaceDisabled
                    | DecisionReason::ToolDefaultConfirm
                    | DecisionReason::CategoryDefaultConfirm
                    | DecisionReason::GlobalDefaultConfirm
            )
            && let ApprovalKind::TerminalCreate { request } = &prompt.kind
            && let Some(profile) = self.profile_id_for_request(request)
        {
            let profile_operation = TrustOperation {
                workspace: audited_operation.workspace,
                agent: audited_operation.agent.clone(),
                transport: audited_operation.transport,
                category: TrustCategory::Execute,
                identity: OperationIdentity::Profile { profile: profile.to_string() },
            };
            decision = self.evaluate_operation(&profile_operation, &session_id, &fingerprint);
            audited_operation = profile_operation;
        }
        self.mark_mandatory_confirmation(&mut prompt, &audited_operation, &decision);
        // Phase 6 audit: every automatic decision (allow or prompt fallback)
        // records redacted rule/category/reason/remaining-use metadata.
        self.push_trust_audit(&audited_operation, &decision, &session_id);
        if matches!(decision.outcome, TrustOutcome::Deny) {
            self.resolve_policy_deny(prompt, &decision);
            return;
        }
        if let Err(error) = self.acquire_prompt_write_lease(&mut prompt) {
            self.record_write_lease_rejection(&prompt);
            let _ = prompt.reply.send(Err(error));
            return;
        }
        match &decision {
            TrustDecision {
                outcome: TrustOutcome::Allow,
                reason: DecisionReason::PersistentAllow,
                rule_id: Some(rule_id),
            } => {
                // Persistent rules auto-dispatch terminal creates (phases 2
                // and 4 profiles), eligible generic MCP invocations (phase
                // 3), and bounded native writes (phase 5).  Operations
                // without a matched rule stay on the UI path.
                self.resolve_persistent_allow(prompt, rule_id.clone());
                // Redacted grant metadata on the status surfaces once the
                // dispatch settled: the remaining-use count then reflects
                // the successful use, and async save alerts cannot clobber
                // the transcript notice.
                if let Some(status) =
                    self.matched_grant_status(&audited_operation, &decision, &session_id)
                {
                    let summary = match (status.remaining_uses, status.expires_at) {
                        (Some(remaining), Some(expires)) => format!(
                            "trusted by {rule_id} · {remaining} uses left · expires {}",
                            format_expiry_utc(expires)
                        ),
                        (Some(remaining), None) => {
                            format!("trusted by {rule_id} · {remaining} uses left")
                        }
                        _ => format!("trusted by {rule_id}"),
                    };
                    if let Some(thread_index) = thread_index
                        && let Some(thread) = self.agents.threads.get_mut(thread_index)
                    {
                        thread.push_system(summary.clone());
                    }
                    self.backend.status_message = Some(summary);
                }
                return;
            }
            TrustDecision { outcome: TrustOutcome::Allow, reason, .. } => {
                if *reason == DecisionReason::BuiltInAllow {
                    // Workspace-trusted read-only allowlist: dispatch silently,
                    // never record a session decision, and keep the audit
                    // trail entry for the automatic allow.
                    let summary = String::from("trusted built-in read-only command");
                    if let Some(thread_index) = thread_index
                        && let Some(thread) = self.agents.threads.get_mut(thread_index)
                    {
                        thread.push_system(summary.clone());
                    }
                    self.backend.status_message = Some(summary);
                    self.resolve_approval(prompt, ApprovalChoice::AllowOnce);
                    return;
                }
                // Session allow: resolve silently, no UI.
                self.resolve_approval(prompt, ApprovalChoice::AllowSession);
                return;
            }
            TrustDecision { outcome: TrustOutcome::Deny, .. } => unreachable!(),
            _ => {}
        }
        let approval_mode =
            self.agents.approval_modes.get(&session_id).copied().unwrap_or_default();
        if decision.reason != DecisionReason::MandatoryConfirm
            && self.tool_approval_mode_allows(approval_mode, &prompt, &operation)
        {
            let summary = format!("tool auto-approved ({})", approval_mode.label());
            if let Some(thread_index) = thread_index
                && let Some(thread) = self.agents.threads.get_mut(thread_index)
            {
                thread.push_system(summary.clone());
            }
            self.backend.status_message = Some(summary);
            self.resolve_approval(prompt, ApprovalChoice::AllowOnce);
            return;
        }
        self.agents.approvals.push_back(prompt);
        self.backend.status_message = Some(if self.agents.layout == AgentPaneLayout::Closed {
            String::from("agent approval required (open :agents)")
        } else {
            String::from("agent approval required")
        });
    }

    fn mark_mandatory_confirmation(
        &self,
        prompt: &mut ApprovalPrompt,
        operation: &TrustOperation,
        decision: &TrustDecision,
    ) {
        let TrustDecision {
            outcome: TrustOutcome::Confirm,
            reason: DecisionReason::MandatoryConfirm,
            rule_id: Some(rule_id),
        } = decision
        else {
            return;
        };
        let template_id = self
            .effective_trust_document(operation.workspace)
            .rules
            .iter()
            .find(|rule| rule.id() == rule_id)
            .and_then(TrustRule::template_id)
            .map(str::to_string);
        prompt.mandatory_confirmation =
            Some(MandatoryConfirmation { rule_id: rule_id.clone(), template_id });
        prompt.options.retain(|(_, choice)| {
            !matches!(
                choice,
                ApprovalChoice::AllowSession
                    | ApprovalChoice::AllowAlways
                    | ApprovalChoice::AllowAlwaysPrefix(_)
            )
        });
        prompt.allow_candidates.clear();
        prompt.selected = prompt.selected.min(prompt.options.len().saturating_sub(1));
    }
}
