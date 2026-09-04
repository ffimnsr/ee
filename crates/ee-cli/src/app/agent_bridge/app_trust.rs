//! `impl App`: trust-store evaluation and policy resolution.

use std::path::{Path, PathBuf};

use ee_agent_protocol::{CreateTerminalRequest, ReadTextFileRequest};

use super::super::*;

use crate::policy::{
    BrowserActionClass, CATASTROPHIC_DELETE_RULE_ID, CommandInvocation, DecisionReason,
    FilesystemOperationKind, NetworkMethodClass, NetworkScheme, OperationIdentity, PolicyInput,
    SafeguardCategory, SafeguardMatch, TERMINAL_READONLY_PROFILE, TransportKind, TrustCategory,
    TrustDecision, TrustOperation, TrustOutcome, TrustRule, TrustStore, TrustStoreDocument,
    TrustStoreError, WorkspaceIdentity, evaluate, inspect_path_escape,
    inspect_protected_state_path, inspect_special_file, inspect_terminal_command,
    is_protected_relative_path, match_profile_entry, resolve_command_cwd, validate_argv_tokens,
    validate_command_tokens,
};

use super::app_search::paths_equivalent;
use super::approval::{ApprovalKind, ToolApprovalMode, WebApprovalCall, WriteExpectation};
use super::prompt::{ApprovalPrompt, TrustGrantStatus};
use super::read::{READ_FINGERPRINT, READ_SESSION};
use super::write::ActionLogEntry;

impl App {
    /// Whether a pending bridge operation is eligible for the active local
    /// approval mode. Invalid or unnormalizable operations always stay on the
    /// explicit approval path.
    pub(super) fn tool_approval_mode_allows(
        &self,
        mode: ToolApprovalMode,
        prompt: &ApprovalPrompt,
        operation: &TrustOperation,
    ) -> bool {
        if matches!(prompt.kind, ApprovalKind::WorkspaceMemoryApproval { .. }) {
            return false;
        }
        if operation.is_unknown() {
            return false;
        }
        if matches!(prompt.kind, ApprovalKind::Network { .. }) {
            return false;
        }
        if let ApprovalKind::TerminalCreate { request } = &prompt.kind
            && self.command_invocation_for_request(request).is_err()
        {
            return false;
        }
        match mode {
            ToolApprovalMode::Default => false,
            ToolApprovalMode::Autopilot => match &prompt.kind {
                ApprovalKind::Write { .. } | ApprovalKind::WriteBatch { .. } => {
                    prompt.mcp.is_none()
                        && matches!(
                            operation.category,
                            TrustCategory::WriteCreate | TrustCategory::WriteModify
                        )
                }
                ApprovalKind::TerminalCreate { request } => {
                    self.profile_id_for_request(request).is_some()
                }
                ApprovalKind::Filesystem { .. }
                | ApprovalKind::WorkspaceMemoryApproval { .. }
                | ApprovalKind::Network { .. } => false,
            },
            ToolApprovalMode::Bypass => true,
        }
    }

    /// Normalizes one pending approval into the shared policy operation
    /// (Phase 1 foundation).  Session lookups still key on the legacy
    /// fingerprint; the normalized operation is what persistent rules match.
    /// Terminal requests carry a validated command invocation; invalid
    /// requests (shell wrappers, bad cwd) normalize to `Unknown` and can
    /// never match a persistent rule.
    pub(super) fn trust_operation_for_prompt(&self, prompt: &ApprovalPrompt) -> TrustOperation {
        let workspace = self.primary_workspace_identity();
        let (category, identity) = match &prompt.kind {
            ApprovalKind::WorkspaceMemoryApproval { .. } => {
                unreachable!("workspace memory approvals bypass trust policy")
            }
            ApprovalKind::Write { .. } | ApprovalKind::WriteBatch { .. } => {
                // Phase 3: eligible proxy tool calls carry a validated MCP
                // invocation; everything else normalizes as a native write.
                if let Some(invocation) = &prompt.mcp {
                    (invocation.category, invocation.to_identity())
                } else {
                    match &prompt.kind {
                        ApprovalKind::Write { path, content, expectation, .. } => self
                            .native_write_operation(path, content, expectation)
                            .unwrap_or_else(|| {
                                let category = match expectation {
                                    WriteExpectation::MustNotExist => TrustCategory::WriteCreate,
                                    WriteExpectation::ExpectRevision(_) => {
                                        TrustCategory::WriteModify
                                    }
                                    WriteExpectation::Blind if !path.exists() => {
                                        TrustCategory::WriteCreate
                                    }
                                    WriteExpectation::Blind => TrustCategory::WriteModify,
                                };
                                (
                                    category,
                                    OperationIdentity::native_tool("fs/write_text_file")
                                        .unwrap_or(OperationIdentity::Unknown),
                                )
                            }),
                        ApprovalKind::WriteBatch { writes, .. } => {
                            self.native_write_batch_operation(writes).unwrap_or_else(|| {
                                let category = if writes.iter().all(|write| {
                                    matches!(write.expectation, WriteExpectation::MustNotExist)
                                        || (matches!(write.expectation, WriteExpectation::Blind)
                                            && !write.path.exists())
                                }) {
                                    TrustCategory::WriteCreate
                                } else {
                                    TrustCategory::WriteModify
                                };
                                (
                                    category,
                                    OperationIdentity::native_tool("fs/write_text_file_batch")
                                        .unwrap_or(OperationIdentity::Unknown),
                                )
                            })
                        }
                        _ => unreachable!(),
                    }
                }
            }
            ApprovalKind::TerminalCreate { request } => {
                let identity = self
                    .command_identity_for_policy_request(request)
                    .unwrap_or(OperationIdentity::Unknown);
                (TrustCategory::Execute, identity)
            }
            ApprovalKind::Filesystem { operation } => (
                match operation {
                    crate::app::agent_filesystem::FilesystemOperation::CreateDirectory {
                        ..
                    }
                    | crate::app::agent_filesystem::FilesystemOperation::CopyPath { .. } => {
                        TrustCategory::WriteCreate
                    }
                    crate::app::agent_filesystem::FilesystemOperation::DeletePath { .. } => {
                        TrustCategory::Delete
                    }
                    crate::app::agent_filesystem::FilesystemOperation::MovePath { .. } => {
                        TrustCategory::WriteModify
                    }
                },
                self.filesystem_policy_identity(operation).unwrap_or_else(|| {
                    OperationIdentity::native_tool(operation.tool_name())
                        .unwrap_or(OperationIdentity::Unknown)
                }),
            ),
            ApprovalKind::Network { route, current_host, call, .. } => {
                let browser_action = match call {
                    WebApprovalCall::Search { .. } | WebApprovalCall::Fetch { .. } => {
                        BrowserActionClass::Fetch
                    }
                    WebApprovalCall::BrowserRun { .. } => BrowserActionClass::Navigate,
                };
                let identity = OperationIdentity::network(
                    NetworkScheme::Https,
                    current_host,
                    443,
                    NetworkMethodClass::Read,
                    browser_action,
                )
                .unwrap_or(OperationIdentity::Unknown);
                let mut operation = TrustOperation {
                    workspace,
                    agent: prompt.agent_id.clone(),
                    transport: route.transport_kind(),
                    category: TrustCategory::Network,
                    identity,
                };
                if operation.is_unknown() {
                    operation.category = TrustCategory::Unknown;
                }
                return operation;
            }
        };
        TrustOperation {
            workspace,
            agent: prompt.agent_id.clone(),
            transport: TransportKind::Acp,
            category,
            identity,
        }
    }

    /// Runs application-owned safeguards against raw typed request fields before
    /// configurable policy. Returned metadata is redacted and versioned.
    pub(super) fn built_in_safeguard_for_prompt(
        &self,
        prompt: &ApprovalPrompt,
    ) -> Option<SafeguardMatch> {
        match &prompt.kind {
            ApprovalKind::TerminalCreate { request } => {
                let cwd = request.cwd.as_deref().unwrap_or(&self.working_dir);
                inspect_terminal_command(
                    &request.command,
                    &request.args,
                    cwd,
                    &self.canonical_workspace_roots(),
                    dirs::home_dir().as_deref(),
                    &self.protected_state_paths(),
                )
            }
            ApprovalKind::Write { path, .. } => self.inspect_mutation_path(path),
            ApprovalKind::WriteBatch { writes, .. } => {
                writes.iter().find_map(|write| self.inspect_mutation_path(&write.path))
            }
            ApprovalKind::Filesystem { operation } => {
                use crate::app::agent_filesystem::FilesystemOperation;
                let roots = self.canonical_workspace_roots();
                let paths: Vec<&Path> = match operation {
                    FilesystemOperation::CreateDirectory { path }
                    | FilesystemOperation::DeletePath { path } => vec![path],
                    FilesystemOperation::CopyPath { source, destination }
                    | FilesystemOperation::MovePath { source, destination } => {
                        vec![source, destination]
                    }
                };
                if matches!(operation, FilesystemOperation::DeletePath { .. })
                    && paths.iter().any(|path| {
                        std::fs::canonicalize(path)
                            .ok()
                            .is_some_and(|candidate| roots.contains(&candidate))
                    })
                {
                    return Some(SafeguardMatch::new(
                        CATASTROPHIC_DELETE_RULE_ID,
                        SafeguardCategory::CatastrophicDeletion,
                    ));
                }
                paths.into_iter().find_map(|path| self.inspect_mutation_path(path))
            }
            ApprovalKind::WorkspaceMemoryApproval { .. } | ApprovalKind::Network { .. } => None,
        }
    }

    fn protected_state_paths(&self) -> Vec<PathBuf> {
        let mut protected = Vec::new();
        if let Some(store) = self.workspace_trust_store() {
            protected.push(store.path().to_path_buf());
        }
        if let Ok(vault) = crate::secrets::default_vault_path() {
            protected.push(vault);
        }
        protected
    }

    fn inspect_mutation_path(&self, path: &Path) -> Option<SafeguardMatch> {
        inspect_protected_state_path(path, &self.protected_state_paths())
            .or_else(|| inspect_special_file(path))
            .or_else(|| inspect_path_escape(path, &self.canonical_workspace_roots()))
    }

    fn command_identity_for_policy_request(
        &self,
        request: &CreateTerminalRequest,
    ) -> Result<OperationIdentity, String> {
        if request.command.is_empty()
            || request.command.chars().any(|character| character.is_control())
        {
            return Err("invalid executable token".into());
        }
        validate_argv_tokens(&request.args)?;
        let primary =
            std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone());
        let raw_cwd = request.cwd.as_deref().unwrap_or(&primary);
        resolve_command_cwd(raw_cwd, &self.canonical_workspace_roots())?;
        Ok(OperationIdentity::Command {
            executable: request.command.clone(),
            argv: request.args.clone(),
        })
    }

    fn filesystem_policy_identity(
        &self,
        operation: &crate::app::agent_filesystem::FilesystemOperation,
    ) -> Option<OperationIdentity> {
        let relative = |path: &Path| {
            let canonical = self.canonical_native_write_target(path)?;
            self.workspace_relative_segments(&canonical)
        };
        match operation {
            crate::app::agent_filesystem::FilesystemOperation::CreateDirectory { path } => {
                let path = relative(path)?;
                OperationIdentity::filesystem(FilesystemOperationKind::Create, Some(&path), None)
                    .ok()
            }
            crate::app::agent_filesystem::FilesystemOperation::DeletePath { path } => {
                let path = relative(path)?;
                OperationIdentity::filesystem(FilesystemOperationKind::Delete, Some(&path), None)
                    .ok()
            }
            crate::app::agent_filesystem::FilesystemOperation::CopyPath { destination, .. } => {
                let destination = relative(destination)?;
                OperationIdentity::filesystem(
                    FilesystemOperationKind::Create,
                    None,
                    Some(&destination),
                )
                .ok()
            }
            crate::app::agent_filesystem::FilesystemOperation::MovePath { source, destination } => {
                let source = relative(source)?;
                let destination = relative(destination)?;
                OperationIdentity::filesystem(
                    FilesystemOperationKind::Rename,
                    Some(&source),
                    Some(&destination),
                )
                .ok()
            }
        }
    }

    /// Validated command invocation for a terminal request: structured
    /// executable + argv tokens, canonical in-workspace cwd, and the
    /// canonical workspace identity.  Invalid requests (shell wrappers,
    /// control characters, empty tokens, external/relative/traversal/
    /// symlink-escape cwd) return an error and are never eligible for
    /// persistent trust.
    pub(super) fn command_invocation_for_request(
        &self,
        request: &CreateTerminalRequest,
    ) -> Result<CommandInvocation, String> {
        let primary =
            std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone());
        if request
            .command
            .chars()
            .any(|character| character.is_whitespace() || ";&|()<>$`'\"\\".contains(character))
        {
            return Err("shell command text is ineligible for persistent allow".into());
        }
        validate_command_tokens(&request.command, &request.args)?;
        let raw_cwd = request.cwd.as_deref().unwrap_or(&primary);
        let canonical_cwd = resolve_command_cwd(raw_cwd, &self.canonical_workspace_roots())?;
        Ok(CommandInvocation {
            workspace: WorkspaceIdentity::from_canonical_root_bytes(
                primary.as_os_str().as_encoded_bytes(),
            ),
            executable: request.command.clone(),
            argv: request.args.clone(),
            canonical_cwd,
        })
    }

    /// The host-local trust store for the primary workspace, or `None` when
    /// no state directory is available (fail closed: empty effective rules).
    pub(crate) fn workspace_trust_store(&self) -> Option<TrustStore> {
        #[cfg(test)]
        if let Some(base) = self.agents.test_trust_store_base.as_deref() {
            return TrustStore::at(base, &self.working_dir).ok();
        }
        TrustStore::default_for(&self.working_dir).ok()
    }

    fn empty_trust_document(&self, workspace: WorkspaceIdentity) -> TrustStoreDocument {
        TrustStoreDocument {
            workspace,
            workspace_enabled: false,
            rules: Vec::new(),
            tool_defaults: Vec::new(),
            category_defaults: Vec::new(),
            global_default: crate::policy::FallbackEffect::Confirm,
        }
    }

    pub(super) fn effective_trust_document(
        &self,
        workspace: WorkspaceIdentity,
    ) -> TrustStoreDocument {
        if self.agents.trust_policy.borrow().is_none() {
            let document = self
                .workspace_trust_store()
                .map(|store| store.effective_at(self.trust_clock.now()))
                .unwrap_or_else(|| self.empty_trust_document(workspace));
            self.agents.trust_policy.replace(Some(document));
        }
        self.agents
            .trust_policy
            .borrow()
            .clone()
            .unwrap_or_else(|| self.empty_trust_document(workspace))
    }

    pub(crate) fn reload_workspace_trust_store(&self) -> Result<(), TrustStoreError> {
        let store = self.workspace_trust_store().ok_or(TrustStoreError::StateDirUnavailable)?;
        let document = store.load_at(self.trust_clock.now())?;
        self.agents.trust_policy.replace(Some(document));
        Ok(())
    }

    /// Canonical workspace identity for the primary (working-directory)
    /// root.
    pub(crate) fn primary_workspace_identity(&self) -> WorkspaceIdentity {
        let root =
            std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone());
        WorkspaceIdentity::from_canonical_root_bytes(root.as_os_str().as_encoded_bytes())
    }

    /// Canonical workspace-relative slash-joined path, or `None` when the
    /// path is outside every canonical workspace root.
    pub(super) fn workspace_relative_segments(&self, canonical: &Path) -> Option<String> {
        let roots = self.canonical_workspace_roots();
        let root = roots.iter().find(|root| canonical.starts_with(root))?;
        let relative = canonical.strip_prefix(root).ok()?;
        if relative.as_os_str().is_empty() {
            return None;
        }
        Some(
            relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
        )
    }

    /// Shared policy evaluation for one normalized operation against the
    /// host-local store, session state, and the usage ledger (Phase 4).
    /// Time comes from the injected policy clock (Phase 6); the ledger
    /// snapshot is keyed by workspace identity, session, and rule id.
    pub(super) fn evaluate_operation(
        &self,
        operation: &TrustOperation,
        session_id: &str,
        fingerprint: &str,
    ) -> TrustDecision {
        self.evaluate_operation_with_safeguard(operation, session_id, fingerprint, None)
    }

    pub(super) fn evaluate_operation_with_safeguard(
        &self,
        operation: &TrustOperation,
        session_id: &str,
        fingerprint: &str,
        built_in_deny: Option<SafeguardMatch>,
    ) -> TrustDecision {
        let now = self.trust_clock.now();
        let effective = self.effective_trust_document(operation.workspace);
        let usage = self.agents.usage_ledger.snapshot(operation.workspace, session_id);
        let tool_key = operation.tool_key();
        let tool_default = effective
            .tool_defaults
            .iter()
            .find(|rule| rule.tool == tool_key)
            .map(|rule| rule.effect);
        let category_default = effective
            .category_defaults
            .iter()
            .find(|rule| rule.category == operation.category)
            .map(|rule| rule.effect);
        evaluate(&PolicyInput {
            session_id,
            fingerprint,
            operation,
            session: &self.agents.approval_policy,
            rules: &effective.rules,
            now,
            usage: &usage,
            workspace_enabled: effective.workspace_enabled,
            built_in_deny,
            tool_default,
            category_default,
            global_default: Some(effective.global_default),
        })
    }

    /// Phase 6 audit: records one redacted automatic-decision event.  The
    /// entry carries the matched rule id, operation category, machine-
    /// readable reason, and remaining use budget only — never raw paths,
    /// command environment, secret values, or MCP arguments.
    pub(super) fn push_trust_audit(
        &mut self,
        operation: &TrustOperation,
        decision: &TrustDecision,
        session_id: &str,
    ) {
        let remaining_uses = self
            .matched_grant_status(operation, decision, session_id)
            .and_then(|status| status.remaining_uses);
        self.agents.action_log.push(ActionLogEntry::TrustDecision {
            rule_id: decision.rule_id.clone(),
            category: operation.category,
            reason: decision.reason,
            remaining_uses,
            session_id: session_id.to_string(),
        });
    }

    /// Redacted metadata about the rule behind a persistent allow: the
    /// remaining use budget and the absolute expiry (Phase 6 lifecycle).
    pub(super) fn matched_grant_status(
        &self,
        operation: &TrustOperation,
        decision: &TrustDecision,
        session_id: &str,
    ) -> Option<TrustGrantStatus> {
        let TrustDecision {
            outcome: TrustOutcome::Allow,
            reason: DecisionReason::PersistentAllow,
            rule_id: Some(rule_id),
        } = decision
        else {
            return None;
        };
        let usage = self.agents.usage_ledger.snapshot(operation.workspace, session_id);
        let scope = self
            .effective_trust_document(operation.workspace)
            .rules
            .into_iter()
            .find(|rule| rule.id() == rule_id)?
            .scope()
            .clone();
        Some(TrustGrantStatus {
            remaining_uses: scope.max_uses.map(|max| max.saturating_sub(usage.used(rule_id))),
            expires_at: scope.expires_at,
        })
    }

    /// Curated profile covering a terminal request. Profiles require the
    /// primary workspace-root cwd. Fixed commands use exact argv; `cat` uses
    /// one validated workspace-relative regular-file operand.
    pub(super) fn profile_id_for_request(
        &self,
        request: &CreateTerminalRequest,
    ) -> Option<&'static str> {
        let primary =
            std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone());
        let raw_cwd = request.cwd.as_deref().unwrap_or(&primary);
        let canonical_cwd = resolve_command_cwd(raw_cwd, &self.canonical_workspace_roots()).ok()?;
        if canonical_cwd != primary {
            return None;
        }
        if let Some((id, _)) = match_profile_entry(&request.command, &request.args) {
            return Some(id);
        }
        self.terminal_readonly_cat_profile(&primary, request)
    }

    /// Matches `cat <one-relative-regular-workspace-file>` for the built-in
    /// terminal-read profile. Shell syntax, flags, traversal, protected paths,
    /// secret-store files, missing files, special files, and escapes fail
    /// closed before a profile rule is considered.
    fn terminal_readonly_cat_profile(
        &self,
        primary: &Path,
        request: &CreateTerminalRequest,
    ) -> Option<&'static str> {
        if request.command != "cat" || request.args.len() != 1 {
            return None;
        }
        validate_command_tokens(&request.command, &request.args).ok()?;
        let operand = &request.args[0];
        let path = Path::new(operand);
        if operand.starts_with('-')
            || path.is_absolute()
            || !path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return None;
        }

        let canonical = std::fs::canonicalize(primary.join(path)).ok()?;
        let relative = canonical.strip_prefix(primary).ok()?;
        if relative.as_os_str().is_empty() || !std::fs::metadata(&canonical).ok()?.is_file() {
            return None;
        }
        let relative = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if is_protected_relative_path(&relative) || self.is_secret_store_path(&canonical) {
            return None;
        }
        Some(TERMINAL_READONLY_PROFILE)
    }

    /// Phase 4: normalized evaluation for one native workspace read.  Reads
    /// stay prompt-free today; the decision feeds the phase 6 audit trail
    /// and guarantees protected, secret-store, and external reads can never
    /// match a persistent rule.
    pub(crate) fn native_read_decision(
        &mut self,
        path: &Path,
        byte_count: Option<u64>,
    ) -> TrustDecision {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let relative = self.workspace_relative_segments(&canonical);
        let eligible = relative.as_deref().is_some_and(|relative| {
            !is_protected_relative_path(relative) && !self.is_secret_store_path(path)
        });
        let operation = if eligible {
            TrustOperation {
                workspace: self.primary_workspace_identity(),
                agent: None,
                transport: TransportKind::Acp,
                category: TrustCategory::Read,
                identity: OperationIdentity::ReadPath {
                    relative_path: relative.expect("eligible implies relative"),
                    byte_count,
                },
            }
        } else {
            TrustOperation {
                workspace: self.primary_workspace_identity(),
                agent: None,
                transport: TransportKind::Acp,
                category: TrustCategory::Read,
                identity: OperationIdentity::Unknown,
            }
        };
        let decision = self.evaluate_operation(&operation, READ_SESSION, READ_FINGERPRINT);
        self.push_trust_audit(&operation, &decision, READ_SESSION);
        decision
    }

    /// Phase 4: normalized evaluation for one MCP read invocation (stdio
    /// route): ee-pinned `read` classification, matching server/transport/
    /// tool/schema, and a bounded canonical workspace-relative path.
    pub(crate) fn mcp_read_decision(
        &mut self,
        request: &ReadTextFileRequest,
        route: crate::app::agents_mcp::ProxyRoute,
    ) -> TrustDecision {
        let tool = "ee_read_text_file";
        let canonical =
            std::fs::canonicalize(&request.path).unwrap_or_else(|_| request.path.clone());
        let relative = self.workspace_relative_segments(&canonical);
        let eligible = ee_mcp::classify::side_effect_class(tool) == ee_mcp::SideEffectClass::Read
            && relative.as_deref().is_some_and(|relative| {
                !is_protected_relative_path(relative) && !self.is_secret_store_path(&request.path)
            });
        let operation = if eligible {
            TrustOperation {
                workspace: self.primary_workspace_identity(),
                agent: None,
                transport: route.transport_kind(),
                category: TrustCategory::Read,
                identity: OperationIdentity::McpRead {
                    server: String::from("ee"),
                    transport_identity: route.transport_identity().to_string(),
                    tool: tool.to_string(),
                    tool_schema_version: crate::policy::EE_MCP_SAFE_READ_TOOL_SCHEMA_VERSION,
                    relative_path: relative.expect("eligible implies relative"),
                    byte_count: request.limit.map(u64::from),
                },
            }
        } else {
            TrustOperation {
                workspace: self.primary_workspace_identity(),
                agent: None,
                transport: route.transport_kind(),
                category: TrustCategory::Read,
                identity: OperationIdentity::Unknown,
            }
        };
        let decision = self.evaluate_operation(&operation, READ_SESSION, READ_FINGERPRINT);
        self.push_trust_audit(&operation, &decision, READ_SESSION);
        decision
    }

    /// Whether a host-local broad MCP read profile exists for this workspace.
    /// Legacy narrow read rules remain audit-only, preserving prompt-free read
    /// behavior unless users explicitly opt into this profile.
    pub(super) fn mcp_read_profile_enforced(&self) -> bool {
        self.workspace_trust_store().is_some_and(|store| {
            store
                .effective_at(self.trust_clock.now())
                .rules
                .iter()
                .any(|rule| matches!(rule, TrustRule::McpReadProfile(_)))
        })
    }

    /// Whether the path is the configured host-local secret-store vault
    /// (never covered by persistent read trust).
    pub(crate) fn is_secret_store_path(&self, path: &Path) -> bool {
        let Ok(vault) = crate::secrets::default_vault_path() else {
            return false;
        };
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let canonical_vault = std::fs::canonicalize(&vault).unwrap_or(vault);
        paths_equivalent(&canonical, &canonical_vault)
    }
}
