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
        if self.workspace_terminal_unrestricted_granted() {
            // Unrestricted grant: any command text, but keep the workspace cwd
            // scope so the grant never turns an explicit outside cwd into
            // prompt-free execution.
            let Some(primary) = self.primary_workspace_root() else {
                return false;
            };
            let raw_cwd = request.cwd.as_deref().unwrap_or(&self.working_dir);
            let Ok(canonical_cwd) =
                crate::policy::resolve_command_cwd(raw_cwd, &self.canonical_workspace_roots())
            else {
                return false;
            };
            return canonical_cwd.starts_with(&primary);
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
        let invocation = self
            .command_invocation_for_request(request)
            .ok()
            .or_else(|| self.cd_prefixed_trust_invocation(request))?;
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

    /// Accepts the common `cd <workspace-dir> && <simple command>` shape the
    /// agent uses to pick a subdirectory. The prefix is normalized only for
    /// eligibility: the host still executes the original command line, and the
    /// remainder must pass the same strict simple-command parser. A leading
    /// `cd` may not leave the primary workspace.
    fn cd_prefixed_trust_invocation(
        &self,
        request: &CreateTerminalRequest,
    ) -> Option<CommandInvocation> {
        if !request.args.is_empty() {
            return None;
        }
        let command = request.command.trim();
        let after_cd = command.strip_prefix("cd ")?.trim_start();
        let (directory, rest) = after_cd.split_once(" && ")?;
        let directory = std::path::Path::new(directory.trim());
        if !directory.is_absolute() {
            return None;
        }
        let canonical = std::fs::canonicalize(directory).ok()?;
        if !canonical.is_dir() {
            return None;
        }
        let primary = self.primary_workspace_root()?;
        if !canonical.starts_with(&primary) {
            return None;
        }
        let (executable, argv) = crate::policy::parse_terminal_command(rest.trim(), &[]).ok()?;
        Some(CommandInvocation {
            workspace: crate::policy::WorkspaceIdentity::from_canonical_root_bytes(
                primary.as_os_str().as_encoded_bytes(),
            ),
            executable,
            argv,
            canonical_cwd: canonical,
        })
    }

    /// Whether an auto-approved terminal can run under the configured sandbox.
    /// Non-terminal operations have no sandbox requirement.
    pub(super) fn auto_sandbox_ready(&self, prompt: &ApprovalPrompt) -> bool {
        let ApprovalKind::TerminalCreate { request } = &prompt.kind else {
            return true;
        };
        let mode = self.config.agents.terminal_sandbox.mode;
        mode == crate::config::TerminalSandboxMode::Off
            || (ee_sandbox::cached_probe().available && self.terminal_sandbox_spec(request).is_ok())
    }

    /// Host-local terminal execution opt-in for the primary workspace. Any
    /// missing, malformed, or untrusted decision fails closed.
    pub(crate) fn workspace_terminal_execution_granted(&self) -> bool {
        self.workspace_terminal_trust_store()
            .and_then(|store| store.terminal_execution().ok())
            .unwrap_or(false)
    }

    /// Whether the terminal grant covers unrestricted command text.
    pub(crate) fn workspace_terminal_unrestricted_granted(&self) -> bool {
        self.workspace_terminal_trust_store()
            .and_then(|store| store.terminal_execution_unrestricted().ok())
            .unwrap_or(false)
    }

    fn workspace_terminal_trust_store(&self) -> Option<WorkspaceTrustStore> {
        let root = self.current_workspace_root();
        #[cfg(test)]
        if let Some(base) = self.agents.test_trust_store_base.as_deref() {
            return WorkspaceTrustStore::at(base, &root).ok();
        }
        WorkspaceTrustStore::default_for(&root).ok()
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
