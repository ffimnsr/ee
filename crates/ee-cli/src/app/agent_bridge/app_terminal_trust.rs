//! `impl App`: workspace terminal execution trust for native ACP v1
//! `terminal/create`.
//!
//! This is a separate explicit opt-in on top of the host-local workspace
//! trust decision. It never applies to MCP-originated terminal tools (those
//! keep their own `ee do mcp trust grant --execute` grant), to ACP v2
//! agent-owned command permissions (the agent runs those itself), or to
//! commands whose validated identity leaves the primary workspace.

use ee_agent_protocol::CreateTerminalRequest;

use super::super::*;

use super::approval::ApprovalKind;
use super::prompt::ApprovalPrompt;
use crate::policy::CommandInvocation;
use crate::workspace_trust::WorkspaceTrustStore;

/// Environment names that would let the agent change which program a
/// validated command line actually executes. The grant authorizes the visible
/// command, not a loader/path hijack.
const DENIED_ENV_NAMES: &[&str] = &[
    "PATH",
    "BASH_ENV",
    "ENV",
    "NODE_OPTIONS",
    "PYTHONHOME",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "RUBYOPT",
    "RUSTC_WRAPPER",
    "RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
];

impl App {
    /// Whether one pending native terminal request may skip its prompt because
    /// the workspace is trusted, carries the explicit terminal execution
    /// grant, the agent mode is not read-only, and the command normalizes to a
    /// simple in-workspace invocation.
    pub(super) fn terminal_execution_trust_eligible(&self, prompt: &ApprovalPrompt) -> bool {
        let ApprovalKind::TerminalCreate { request } = &prompt.kind else {
            return false;
        };
        // MCP terminal tools have a separate pinned server grant. Never let
        // one route's authority silently cover the other.
        if prompt.mcp.is_some() {
            return false;
        }
        if !self.workspace_trusted()
            || !self.workspace_terminal_execution_granted()
            || !self.trusted_mode_allows(&prompt.session_id)
        {
            return false;
        }
        self.terminal_trust_invocation(request).is_some()
    }

    /// Validated invocation eligible for the terminal execution grant:
    /// simple command text, no shell wrapper or assignment, canonical cwd
    /// inside the primary workspace root, and no execution-affecting
    /// environment overrides.
    pub(crate) fn terminal_trust_invocation(
        &self,
        request: &CreateTerminalRequest,
    ) -> Option<CommandInvocation> {
        let invocation = self.command_invocation_for_request(request).ok()?;
        let primary = self.primary_workspace_root()?;
        if !invocation.canonical_cwd.starts_with(&primary) {
            return None;
        }
        if !terminal_env_allows_trust(request) {
            return None;
        }
        Some(invocation)
    }

    /// Host-local terminal execution opt-in for the primary workspace. Any
    /// missing, malformed, or untrusted decision fails closed.
    pub(crate) fn workspace_terminal_execution_granted(&self) -> bool {
        let root = self.current_workspace_root();
        #[cfg(test)]
        if let Some(base) = self.agents.test_trust_store_base.as_deref() {
            return WorkspaceTrustStore::at(base, &root)
                .ok()
                .and_then(|store| store.terminal_execution().ok())
                .unwrap_or(false);
        }
        WorkspaceTrustStore::default_for(&root)
            .ok()
            .and_then(|store| store.terminal_execution().ok())
            .unwrap_or(false)
    }
}

fn terminal_env_allows_trust(request: &CreateTerminalRequest) -> bool {
    request.env.iter().all(|variable| {
        let name = variable.name.trim();
        if name.is_empty() || variable.name.chars().any(char::is_control) {
            return false;
        }
        let upper = variable.name.to_ascii_uppercase();
        !DENIED_ENV_NAMES.contains(&upper.as_str())
            && !upper.starts_with("LD_")
            && !upper.starts_with("DYLD_")
    })
}
