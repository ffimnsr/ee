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
use crate::policy::{
    CommandInvocation, is_bare_executable, is_shell_wrapper, terminal_operand_stays_in_workspace,
};
use crate::workspace_trust::WorkspaceTrustStore;
use ee_sandbox::{SandboxError, SandboxSpec};

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
        let mode = self.config.agents.terminal_sandbox.mode;
        if mode != crate::config::TerminalSandboxMode::Off {
            if !ee_sandbox::cached_probe().available {
                return false;
            }
            if self.terminal_sandbox_spec(request).is_err() {
                return false;
            }
        }
        self.terminal_trust_invocation(request).is_some()
    }

    /// Builds the sandbox spec for one native terminal request.
    pub(crate) fn terminal_sandbox_spec(
        &self,
        request: &CreateTerminalRequest,
    ) -> Result<SandboxSpec, SandboxError> {
        let policy = &self.config.agents.terminal_sandbox;
        let workspace = self.primary_workspace_root().ok_or_else(|| {
            SandboxError::Invalid(String::from("primary workspace root is unavailable"))
        })?;
        let cwd = request.cwd.clone().unwrap_or_else(|| self.working_dir.clone());
        let mut spec = SandboxSpec::new(&workspace, &cwd)?
            .with_network(policy.network)
            .with_read(policy.read)
            .with_limits(policy.limits);
        for path in &policy.write_paths {
            spec = spec.with_write_path(path)?;
        }
        Ok(spec)
    }

    /// Validated invocation eligible for the terminal execution grant: simple
    /// command text, no shell wrapper or assignment, a bare PATH-resolved
    /// executable, a canonical cwd inside the primary workspace root, no
    /// execution-affecting environment overrides, and path-like argv operands
    /// that resolve inside the primary workspace root. Ambiguous or external
    /// paths fall back to approval.
    pub(crate) fn terminal_trust_invocation(
        &self,
        request: &CreateTerminalRequest,
    ) -> Option<CommandInvocation> {
        let invocation = self.command_invocation_for_request(request).ok()?;
        let primary = self.primary_workspace_root()?;
        if !invocation.canonical_cwd.starts_with(&primary) {
            return None;
        }
        if !is_bare_executable(&invocation.executable) {
            return None;
        }
        if !terminal_env_allows_trust(request) {
            return None;
        }
        if invocation.argv.iter().any(|token| is_shell_wrapper(token)) {
            return None;
        }
        if !invocation.argv.iter().all(|token| {
            terminal_operand_stays_in_workspace(token, &invocation.canonical_cwd, &primary)
        }) {
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
