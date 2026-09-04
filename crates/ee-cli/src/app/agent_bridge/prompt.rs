//! `ApprovalPrompt` construction and approval option helpers.

use std::collections::BTreeSet;
use std::time::SystemTime;

use ee_agent_host::ClientRequestResult;
use ee_agent_protocol::{CreateTerminalRequest, SessionId, WriteTextFileRequest};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::app::agents_mcp::ProxyRoute;
use crate::app::write_leases::{WriteLeaseId, WriteLeaseOwner};

use crate::policy::{BoundedRuleCandidate, BoundedRulePreview, McpInvocation};

use super::approval::{
    ApprovalChoice, ApprovalKind, DenyScopePreview, MandatoryConfirmation,
    PERSISTENT_TERMINAL_OPTION_LABEL, PersistentDenyCandidate, PreparedWrite, ProxyWriteSpec,
    WebApprovalCall, WorkspaceMemoryApprovalOperation, WorkspaceMemoryApprovalTarget,
    WriteExpectation, WriteReplyKind,
};
use super::terminal::redact_env_display;

/// A pending file-write or terminal-create approval.
#[derive(Debug)]
pub(crate) struct ApprovalPrompt {
    pub(crate) thread_index: Option<usize>,
    pub(crate) session_id: String,
    /// Agent id of the requesting session (rule scoping; `None` for the
    /// MCP proxy session).
    pub(super) agent_id: Option<String>,
    pub(super) write_lease: Option<WriteLeaseId>,
    pub(super) write_lease_owner: Option<WriteLeaseOwner>,
    pub(crate) title: String,
    pub(crate) detail: String,
    /// `(label, choice)` option list; the user picks one with Enter.
    pub(crate) options: Vec<(String, ApprovalChoice)>,
    pub(crate) selected: usize,
    pub(super) kind: ApprovalKind,
    /// Phase 3: validated generic MCP invocation behind this prompt, when
    /// the request is an eligible proxy tool call.  Presence gates the
    /// persistent `Allow for 1 hour / 20 uses` option.
    pub(super) mcp: Option<McpInvocation>,
    pub(super) allow_candidates: Vec<(ApprovalChoice, BoundedRuleCandidate)>,
    pub(super) confirming_allow: Option<ApprovalChoice>,
    pub(super) deny_candidate: Option<PersistentDenyCandidate>,
    pub(super) confirming_deny: bool,
    pub(super) mandatory_confirmation: Option<MandatoryConfirmation>,
    pub(crate) reply: oneshot::Sender<ClientRequestResult>,
}

impl ApprovalPrompt {
    pub(super) fn write(
        thread_index: Option<usize>,
        session_id: &SessionId,
        request: &WriteTextFileRequest,
        persistent_label: Option<&'static str>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self::write_with(
            thread_index,
            session_id,
            String::from("fs/write_text_file"),
            format!("{} ({} bytes)", request.path.display(), request.content.len()),
            PreparedWrite {
                path: request.path.clone(),
                content: request.content.clone(),
                tool_call_id: None,
                expectation: WriteExpectation::Blind,
                reply_kind: WriteReplyKind::FsWrite,
                proxy_edit_count: 0,
            },
            None,
            persistent_label,
            reply,
        )
    }

    pub(super) fn workspace_memory(
        operation: ee_agent_host::WorkspaceMemoryMutationOperation,
        key: String,
        target: WorkspaceMemoryApprovalTarget,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let operation = match operation {
            ee_agent_host::WorkspaceMemoryMutationOperation::Remember => {
                WorkspaceMemoryApprovalOperation::Remember
            }
            ee_agent_host::WorkspaceMemoryMutationOperation::Verify => {
                WorkspaceMemoryApprovalOperation::Verify
            }
            ee_agent_host::WorkspaceMemoryMutationOperation::Forget => {
                WorkspaceMemoryApprovalOperation::Forget
            }
        };
        Self::workspace_memory_management(operation, key, target, reply)
    }

    pub(super) fn workspace_memory_management(
        operation: WorkspaceMemoryApprovalOperation,
        key: String,
        target: WorkspaceMemoryApprovalTarget,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let operation_label = operation.label();
        Self {
            thread_index: None,
            session_id: SessionId::new("workspace-memory").0.to_string(),
            agent_id: None,
            write_lease: None,
            write_lease_owner: None,
            title: String::from("workspace memory mutation"),
            detail: format!("operation: {operation_label}\nkey: {key}"),
            options: vec![
                (String::from("Allow once"), ApprovalChoice::AllowOnce),
                (String::from("Deny"), ApprovalChoice::DenyOnce),
            ],
            selected: 0,
            kind: ApprovalKind::WorkspaceMemoryApproval { operation, key, target },
            mcp: None,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    pub(super) fn workspace_memory_proxy(
        operation: WorkspaceMemoryApprovalOperation,
        target: WorkspaceMemoryApprovalTarget,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let mut prompt =
            Self::workspace_memory_management(operation, String::from("[redacted]"), target, reply);
        prompt.detail = format!("operation: {}\npayload: [redacted]", operation.label());
        prompt
    }

    pub(super) fn filesystem(
        operation: crate::app::agent_filesystem::FilesystemOperation,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self {
            thread_index: None,
            session_id: SessionId::new("proxy").0.to_string(),
            agent_id: None,
            write_lease: None,
            write_lease_owner: None,
            title: operation.tool_name().to_string(),
            detail: operation.detail(),
            options: approval_options(None),
            selected: 0,
            kind: ApprovalKind::Filesystem { operation },
            mcp: None,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    pub(super) fn proxy_write(
        spec: ProxyWriteSpec,
        mcp: Option<McpInvocation>,
        persistent_label: Option<&'static str>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self::write_with(
            None,
            &SessionId::new("proxy"),
            spec.title,
            mcp.as_ref().map(mcp_approval_detail).unwrap_or(spec.detail),
            spec.prepared,
            mcp,
            persistent_label,
            reply,
        )
    }

    /// Internal constructor shared by the write prompt builders; the
    /// argument count is inherent to the prompt shape.
    #[allow(clippy::too_many_arguments)]
    fn write_with(
        thread_index: Option<usize>,
        session_id: &SessionId,
        title: String,
        detail: String,
        prepared: PreparedWrite,
        mcp: Option<McpInvocation>,
        persistent_label: Option<&'static str>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self {
            thread_index,
            session_id: session_id.0.to_string(),
            agent_id: None,
            write_lease: None,
            write_lease_owner: None,
            title,
            detail,
            options: approval_options(persistent_label),
            selected: 0,
            kind: ApprovalKind::Write {
                path: prepared.path,
                content: prepared.content,
                tool_call_id: prepared.tool_call_id,
                expectation: prepared.expectation,
                reply_kind: prepared.reply_kind,
                proxy_edit_count: prepared.proxy_edit_count,
            },
            mcp,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    pub(super) fn proxy_write_batch(
        title: String,
        detail: String,
        writes: Vec<PreparedWrite>,
        total_edit_count: u32,
        mcp: Option<McpInvocation>,
        persistent_label: Option<&'static str>,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self {
            thread_index: None,
            session_id: SessionId::new("proxy").0.to_string(),
            agent_id: None,
            write_lease: None,
            write_lease_owner: None,
            title,
            detail: mcp.as_ref().map(mcp_approval_detail).unwrap_or(detail),
            options: approval_options(persistent_label),
            selected: 0,
            kind: ApprovalKind::WriteBatch { writes, total_edit_count },
            mcp,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn web(
        route: ProxyRoute,
        network_session_id: String,
        requested_host: String,
        current_host: String,
        provider_label: Option<&str>,
        call: WebApprovalCall,
        approved_hosts: BTreeSet<String>,
        cancellation: CancellationToken,
        reply: oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let action = match &call {
            WebApprovalCall::Search { .. } => "web search",
            WebApprovalCall::Fetch { .. } => "fetch URL",
            WebApprovalCall::BrowserRun { request } => match request.action {
                ee_mcp::BrowserRunAction::Content => "Browser Run content",
                ee_mcp::BrowserRunAction::Screenshot => "Browser Run screenshot",
                ee_mcp::BrowserRunAction::Markdown => "Browser Run markdown",
                ee_mcp::BrowserRunAction::Scrape => "Browser Run scrape",
                ee_mcp::BrowserRunAction::Json => "Browser Run JSON extraction",
                ee_mcp::BrowserRunAction::Links => "Browser Run links",
            },
        };
        Self {
            thread_index: None,
            // Network grants bind both transport and opaque connection scope.
            // A later stdio or ACP connection cannot reuse this decision.
            session_id: network_session_id,
            agent_id: None,
            write_lease: None,
            write_lease_owner: None,
            title: format!("network/{action}"),
            detail: match provider_label {
                Some(provider) => format!("provider: {provider} · host: {current_host}"),
                None => format!("host: {current_host}"),
            },
            options: approval_options(None),
            selected: 0,
            kind: ApprovalKind::Network {
                route,
                requested_host,
                current_host,
                call,
                approved_hosts,
                cancellation,
            },
            mcp: None,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    pub(super) fn terminal(
        thread_index: Option<usize>,
        agent_id: Option<String>,
        session_id: &SessionId,
        request: &CreateTerminalRequest,
        reply: oneshot::Sender<ClientRequestResult>,
        persistent_allowed: bool,
    ) -> Self {
        let command = if request.args.is_empty() {
            request.command.clone()
        } else {
            format!("{} {}", request.command, request.args.join(" "))
        };
        let cwd = request
            .cwd
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| String::from("(default)"));
        let env = redact_env_display(&request.env);
        let env_text = if env.is_empty() {
            String::from("(inherited, secrets filtered)")
        } else {
            env.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join(" ")
        };
        Self {
            thread_index,
            session_id: session_id.0.to_string(),
            agent_id,
            write_lease: None,
            write_lease_owner: None,
            title: String::from("terminal/create"),
            detail: format!("{command} · cwd: {cwd} · env: {env_text}"),
            options: approval_options(
                persistent_allowed.then_some(PERSISTENT_TERMINAL_OPTION_LABEL),
            ),
            selected: 0,
            kind: ApprovalKind::TerminalCreate { request: request.clone() },
            mcp: None,
            allow_candidates: Vec::new(),
            confirming_allow: None,
            deny_candidate: None,
            confirming_deny: false,
            mandatory_confirmation: None,
            reply,
        }
    }

    pub(crate) fn allow_confirmation_preview(&self) -> Option<&BoundedRulePreview> {
        let choice = self.confirming_allow?;
        self.allow_candidates.iter().find_map(|(candidate_choice, candidate)| {
            (*candidate_choice == choice).then_some(&candidate.preview)
        })
    }

    pub(crate) fn deny_confirmation_preview(&self) -> Option<&DenyScopePreview> {
        self.confirming_deny
            .then(|| self.deny_candidate.as_ref().map(|candidate| &candidate.preview))
            .flatten()
    }

    pub(crate) fn is_confirming_rule(&self) -> bool {
        self.confirming_deny || self.confirming_allow.is_some()
    }

    pub(crate) fn confirming_allow_choice(&self) -> Option<ApprovalChoice> {
        self.confirming_allow
    }

    pub(crate) fn mandatory_confirmation(&self) -> Option<&MandatoryConfirmation> {
        self.mandatory_confirmation.as_ref()
    }
}

/// The approval option list.  Allow-always (unlimited persistence) is
/// intentionally absent; the bounded persistent option exists only for
/// eligible terminal requests (Phase 2 command trust), eligible generic MCP
/// invocations (Phase 3), and eligible bounded native writes (Phase 5).
fn approval_options(persistent_label: Option<&'static str>) -> Vec<(String, ApprovalChoice)> {
    let mut options = [
        ApprovalChoice::AllowOnce,
        ApprovalChoice::AllowSession,
        ApprovalChoice::DenyOnce,
        ApprovalChoice::DenySession,
    ]
    .into_iter()
    .map(|choice| (choice.label().to_string(), choice))
    .collect::<Vec<_>>();
    if let Some(label) = persistent_label {
        options.push((label.to_string(), ApprovalChoice::AllowPersistent));
    }
    options
}

/// Redacted MCP approval text: server, tool, side-effect class, and bounded
/// canonical arguments only (Phase 3); never renders secrets, environment
/// values, or file contents.
fn mcp_approval_detail(invocation: &McpInvocation) -> String {
    format!(
        "server: {} · tool: {} · class: {} · args: {}",
        invocation.server,
        invocation.tool,
        invocation.category.as_str(),
        redact_arguments_display(&invocation.arguments_json),
    )
}

/// Bounded argument display; oversized canonical payloads are truncated.
fn redact_arguments_display(arguments: &str) -> String {
    const MAX_DISPLAY_BYTES: usize = 200;
    if arguments.len() <= MAX_DISPLAY_BYTES {
        arguments.to_string()
    } else {
        format!("{}…", &arguments[..MAX_DISPLAY_BYTES])
    }
}

// ── Action log ───────────────────────────────────────────────────────────────

/// Redacted lifecycle metadata about a matched persistent grant (Phase 6).
pub(super) struct TrustGrantStatus {
    pub(super) remaining_uses: Option<u64>,
    pub(super) expires_at: Option<SystemTime>,
}

/// UTC RFC3339 display for a grant expiry; no paths or secrets.
pub(super) fn format_expiry_utc(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
use super::approval::ApprovalPolicy;
#[cfg(test)]
use super::approval::PERSISTENT_WRITE_OPTION_LABEL;
#[cfg(test)]
use super::approval::SessionChoice;
#[cfg(test)]
use super::approval::approval_fingerprint;
#[cfg(test)]
use super::approval::session_decision;
#[cfg(test)]
use super::bridge_ui::BridgeUiHandler;
#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_approval_exposes_only_route_and_canonical_host() {
        let query = "private search query";
        let url = "https://docs.example/private/path?token=super-secret";
        let (reply, _receiver) = oneshot::channel();
        let prompt = ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:test"),
            String::from("docs.example"),
            String::from("docs.example"),
            None,
            WebApprovalCall::Fetch { url: String::from(url) },
            BTreeSet::new(),
            CancellationToken::new(),
            reply,
        );
        let rendered = format!("{prompt:?}");
        assert_eq!(prompt.title, "network/fetch URL");
        assert_eq!(prompt.detail, "host: docs.example");
        assert!(!rendered.contains(query));
        assert!(!rendered.contains(url));
        assert!(!rendered.contains("super-secret"));
        assert_eq!(prompt.options.len(), 4);
        assert!(
            prompt.options.iter().all(|(_, choice)| *choice != ApprovalChoice::AllowPersistent)
        );
    }

    #[test]
    fn network_redirect_prompt_keeps_requested_host_but_scopes_grant_to_current_host() {
        let (reply, _receiver) = oneshot::channel();
        let prompt = ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:test"),
            String::from("origin.example"),
            String::from("redirect.example"),
            Some("Exa"),
            WebApprovalCall::Search { query: String::from("private") },
            BTreeSet::from([String::from("origin.example")]),
            CancellationToken::new(),
            reply,
        );
        assert_eq!(prompt.detail, "provider: Exa · host: redirect.example");
        let rendered = format!("{prompt:?}");
        assert!(!rendered.contains("private"));
        assert_eq!(
            approval_fingerprint(&prompt.kind),
            "network:stdio:ee --mcp-proxy:search:redirect.example"
        );
        match &prompt.kind {
            ApprovalKind::Network { requested_host, current_host, approved_hosts, .. } => {
                assert_eq!(requested_host, "origin.example");
                assert_eq!(current_host, "redirect.example");
                assert!(approved_hosts.contains("origin.example"));
                assert!(!approved_hosts.contains("redirect.example"));
            }
            _ => panic!("expected network prompt"),
        }
    }

    #[test]
    fn network_search_and_fetch_grants_are_scoped_to_their_actions() {
        let (search_reply, _search_receiver) = oneshot::channel();
        let search = ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:test"),
            String::from("api.exa.ai"),
            String::from("api.exa.ai"),
            Some("Exa"),
            WebApprovalCall::Search { query: String::from("private") },
            BTreeSet::new(),
            CancellationToken::new(),
            search_reply,
        );
        let (fetch_reply, _fetch_receiver) = oneshot::channel();
        let fetch = ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:test"),
            String::from("api.exa.ai"),
            String::from("api.exa.ai"),
            None,
            WebApprovalCall::Fetch { url: String::from("https://api.exa.ai/source") },
            BTreeSet::new(),
            CancellationToken::new(),
            fetch_reply,
        );

        assert_ne!(approval_fingerprint(&search.kind), approval_fingerprint(&fetch.kind));
    }

    #[test]
    fn network_session_fingerprints_are_route_and_connection_scoped() {
        let make_prompt = |route, scope: &str| {
            let (reply, _receiver) = oneshot::channel();
            ApprovalPrompt::web(
                route,
                format!("proxy-network:{}:{scope}", route.transport_identity()),
                String::from("docs.example"),
                String::from("docs.example"),
                Some("Exa"),
                WebApprovalCall::Search { query: String::from("must stay private") },
                BTreeSet::new(),
                CancellationToken::new(),
                reply,
            )
        };
        let stdio = make_prompt(ProxyRoute::Stdio, "connection-a");
        let second_stdio = make_prompt(ProxyRoute::Stdio, "connection-b");
        let acp = make_prompt(ProxyRoute::AcpNative, "connection-a");
        let stdio_fingerprint = approval_fingerprint(&stdio.kind);
        let acp_fingerprint = approval_fingerprint(&acp.kind);
        assert_ne!(stdio_fingerprint, acp_fingerprint);
        assert_ne!(stdio.session_id, second_stdio.session_id);
        assert!(!stdio_fingerprint.contains("must stay private"));
        assert!(!acp_fingerprint.contains("must stay private"));

        let mut policy = ApprovalPolicy::default();
        policy.record(&stdio.session_id, &stdio_fingerprint, SessionChoice::Allow);
        assert_eq!(
            policy.lookup(&acp.session_id, &acp_fingerprint),
            None,
            "stdio host decision must not apply to ACP-native route"
        );
        assert_eq!(
            policy.lookup(&second_stdio.session_id, &stdio_fingerprint),
            None,
            "one stdio connection must not reuse another connection's network grant"
        );
    }

    #[test]
    fn workspace_memory_approval_is_one_time_and_redacts_value() {
        let secret = "do-not-show-this-value";
        let (reply, _receiver) = oneshot::channel();
        let prompt = ApprovalPrompt::workspace_memory(
            ee_agent_host::WorkspaceMemoryMutationOperation::Remember,
            String::from("build.command"),
            WorkspaceMemoryApprovalTarget::Remember { value: String::from(secret) },
            reply,
        );

        assert_eq!(prompt.detail, "operation: remember\nkey: build.command");
        assert_eq!(
            prompt.options,
            vec![
                (String::from("Allow once"), ApprovalChoice::AllowOnce),
                (String::from("Deny"), ApprovalChoice::DenyOnce),
            ]
        );
        let rendered = format!("{prompt:?}");
        assert!(!rendered.contains(secret));
        assert!(!prompt.options.iter().any(|(_, choice)| matches!(
            choice,
            ApprovalChoice::AllowSession
                | ApprovalChoice::AllowPersistent
                | ApprovalChoice::AllowPersistentShort
                | ApprovalChoice::AllowPersistentPrefix(_)
                | ApprovalChoice::AllowPersistentPrefixShort(_)
        )));
        assert!(BridgeUiHandler::editor_capabilities().workspace_memory_mutation_approval);
    }

    #[test]
    fn workspace_memory_management_approvals_are_one_time_and_redact_import_values() {
        let secret = "imported-secret-value";
        let export = ee_agent_host::WorkspaceMemoryExportDto {
            schema_version: 1,
            workspace_id: String::from("workspace"),
            redacted: false,
            facts: vec![ee_agent_host::WorkspaceMemoryExportedFact {
                namespace: String::from("default"),
                key: String::from("build.command"),
                value: Some(secret.to_string()),
                kind: String::from("command"),
                authority: String::from("user_asserted"),
                freshness: String::from("current"),
                provenance: ee_agent_host::WorkspaceMemoryExportProvenance {
                    source_kind: String::from("user"),
                    source_id: String::from("import"),
                    revision: None,
                    fingerprint: None,
                    verified_at: None,
                },
                expires_at: None,
                content_hash: String::from("hash"),
            }],
        };
        let operations = [
            (WorkspaceMemoryApprovalOperation::Clear, WorkspaceMemoryApprovalTarget::Clear),
            (
                WorkspaceMemoryApprovalOperation::DisableDelete,
                WorkspaceMemoryApprovalTarget::DisableDelete {
                    config_path: PathBuf::from(".ee.toml"),
                },
            ),
            (
                WorkspaceMemoryApprovalOperation::Export,
                WorkspaceMemoryApprovalTarget::Export { include_values: true },
            ),
            (
                WorkspaceMemoryApprovalOperation::Import,
                WorkspaceMemoryApprovalTarget::Import { export: Box::new(export) },
            ),
        ];

        for (operation, target) in operations {
            let (reply, _receiver) = oneshot::channel();
            let prompt = ApprovalPrompt::workspace_memory_management(
                operation,
                String::from("explicit scope"),
                target,
                reply,
            );
            assert_eq!(
                prompt.options,
                vec![
                    (String::from("Allow once"), ApprovalChoice::AllowOnce),
                    (String::from("Deny"), ApprovalChoice::DenyOnce),
                ]
            );
            assert!(!format!("{prompt:?}").contains(secret));
        }

        for (operation, target, hidden) in [
            (
                WorkspaceMemoryApprovalOperation::Retract,
                WorkspaceMemoryApprovalTarget::RetractKey { key: secret.to_string() },
                secret,
            ),
            (
                WorkspaceMemoryApprovalOperation::Export,
                WorkspaceMemoryApprovalTarget::ExportValue { include_values: true },
                "true",
            ),
        ] {
            let (reply, _receiver) = oneshot::channel();
            let prompt = ApprovalPrompt::workspace_memory_proxy(operation, target, reply);
            assert_eq!(
                prompt.detail,
                format!("operation: {}\npayload: [redacted]", operation.label())
            );
            assert!(!format!("{prompt:?}").contains(hidden));
            assert_eq!(
                prompt.options,
                vec![
                    (String::from("Allow once"), ApprovalChoice::AllowOnce),
                    (String::from("Deny"), ApprovalChoice::DenyOnce),
                ]
            );
        }
    }

    #[test]
    fn approval_options_offer_persistent_only_when_eligible() {
        // Ineligible prompts never get a persistent option; the option list
        // stays at four choices with no unlimited allow.
        let base = approval_options(None);
        assert_eq!(base.len(), 4);
        for (label, _) in &base {
            assert!(!label.contains("Always"), "allow-always must stay disabled: {label}");
            assert!(!label.contains("1 hour"));
        }
        // Eligible terminal prompts append the bounded persistent option.
        let persistent = approval_options(Some(PERSISTENT_TERMINAL_OPTION_LABEL));
        assert_eq!(persistent.len(), 5);
        assert_eq!(
            persistent.last().unwrap().0,
            PERSISTENT_TERMINAL_OPTION_LABEL,
            "persistent option label"
        );
        assert_eq!(persistent.last().unwrap().1, ApprovalChoice::AllowPersistent);
        assert!(ApprovalChoice::AllowPersistent.allows());
        // Persistent grants are host-local rules, never session decisions.
        assert_eq!(session_decision(ApprovalChoice::AllowPersistent), None);
        // Eligible bounded writes carry the write option label (phase 5).
        let writes = approval_options(Some(PERSISTENT_WRITE_OPTION_LABEL));
        assert_eq!(writes.len(), 5);
        assert_eq!(writes.last().unwrap().0, PERSISTENT_WRITE_OPTION_LABEL);
        assert_eq!(writes.last().unwrap().1, ApprovalChoice::AllowPersistent);
        assert_ne!(PERSISTENT_WRITE_OPTION_LABEL, PERSISTENT_TERMINAL_OPTION_LABEL);
    }
}
