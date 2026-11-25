//! Resolved remote-host configuration.
//!
//! The remote host shares the editor's agent launch configuration shape
//! (`AgentProcessConfig`, `PermissionRules`, workspace memory) so a headless
//! process behaves like the editor for the same config layers. Remote-only
//! policy (permission mode, timeouts, queue bounds, audit path) lives here.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ee_agent_host::permission_rules::PermissionRuleset;
use ee_agent_host::{
    AgentManagerConfig, AgentProcessConfig, MemoryRetention, PermissionRules,
    WorkspaceMemoryHostConfig, WorkspaceMemoryQuotas,
};

use crate::error::RemoteError;

/// Default bounded approval timeout; expiry always ends in deny.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
/// Default per-request deadline for remote registry methods.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Default idle WebSocket timeout that never kills in-flight turns.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Default per-connection outbound queue bound.
pub const DEFAULT_MAX_OUTBOUND_QUEUE: usize = 256;
/// Default per-session notification ring capacity.
pub const DEFAULT_RING_CAPACITY: usize = 2_048;
/// Default durable history page size.
pub const DEFAULT_HISTORY_PAGE_SIZE: usize = 200;
/// Default audit log size cap before one rotation.
pub const DEFAULT_AUDIT_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How remote approvals are answered.
///
/// No mode ever blanket-auto-approves writes or terminals from agent context
/// alone. Parse errors fail closed to [`Self::Ask`] (surface everything).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemotePermissionMode {
    /// Approvals surface to the connected client, then time out to deny.
    #[default]
    Ask,
    /// Only declarative `PermissionRules`/trust profiles answer; anything
    /// unmatched stays pending until the timeout denies it.
    Rules,
    /// Every gated operation is refused immediately.
    Deny,
}

impl RemotePermissionMode {
    /// Parses the config value. Unknown values error; callers fall back to
    /// `Ask` semantics instead of widening authority.
    ///
    /// # Errors
    ///
    /// Returns the rejected value in the message.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "rules" => Ok(Self::Rules),
            "deny" => Ok(Self::Deny),
            other => Err(format!(
                "remote_permission_mode must be \"ask\", \"rules\", or \"deny\", got {other:?}"
            )),
        }
    }

    /// Stable config label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Rules => "rules",
            Self::Deny => "deny",
        }
    }

    /// Whether this mode may emit client-visible approval requests.
    #[must_use]
    pub fn surfaces_approvals(self) -> bool {
        self == Self::Ask
    }
}

/// Resolved configuration for one remote host process.
#[derive(Clone)]
pub struct RemoteHostConfig {
    /// Agent id → launch config. Empty is allowed for tests; `serve` rejects
    /// it with a typed error before starting.
    pub agents: BTreeMap<String, AgentProcessConfig>,
    /// Whether the ee MCP proxy is armed on agent connections.
    pub ee_proxy_enabled: bool,
    /// Resolved workspace-memory settings.
    pub workspace_memory: WorkspaceMemoryHostConfig,
    /// Declarative permission rules evaluated before any approval round-trip.
    pub permission_rules: Arc<PermissionRules>,
    /// Remote approval posture.
    pub permission_mode: RemotePermissionMode,
    /// Roots the headless handler may touch without further grants.
    pub allowed_roots: Vec<PathBuf>,
    /// Known secret values (configured env values) removed from every outbound
    /// payload in addition to the generic credential-shape pass.
    pub secret_values: Vec<String>,
    /// Configured default agent id, when set.
    pub default_agent: Option<String>,
    /// How long an approval stays pending before it is denied.
    pub approval_timeout: Duration,
    /// Per-request deadline for registry methods.
    pub request_timeout: Duration,
    /// Idle connection timeout; in-flight turns are never interrupted.
    pub idle_timeout: Duration,
    /// Per-connection outbound queue bound before `overload` is returned.
    pub max_outbound_queue: usize,
    /// Per-session notification ring capacity.
    pub ring_capacity: usize,
    /// Durable notification/history page size cap.
    pub history_page_size: usize,
    /// Audit log path; `None` disables the audit trail.
    pub audit_path: Option<PathBuf>,
    /// Audit log size cap before one rotation.
    pub audit_max_bytes: u64,
    /// Durable provider-side session log directory, when configured.
    pub session_log_dir: Option<PathBuf>,
    /// Test-only: agent id → fake transport factory.
    #[cfg(feature = "test-utils")]
    pub fake_transports: BTreeMap<String, Arc<dyn ee_agent_host::FakeTransportFactory>>,
}

impl Default for RemoteHostConfig {
    fn default() -> Self {
        Self {
            agents: BTreeMap::new(),
            ee_proxy_enabled: true,
            workspace_memory: WorkspaceMemoryHostConfig::default(),
            permission_rules: Arc::new(PermissionRules::new(PermissionRuleset::empty())),
            permission_mode: RemotePermissionMode::Ask,
            allowed_roots: Vec::new(),
            secret_values: Vec::new(),
            default_agent: None,
            approval_timeout: DEFAULT_APPROVAL_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            max_outbound_queue: DEFAULT_MAX_OUTBOUND_QUEUE,
            ring_capacity: DEFAULT_RING_CAPACITY,
            history_page_size: DEFAULT_HISTORY_PAGE_SIZE,
            audit_path: None,
            audit_max_bytes: DEFAULT_AUDIT_MAX_BYTES,
            session_log_dir: None,
            #[cfg(feature = "test-utils")]
            fake_transports: BTreeMap::new(),
        }
    }
}

impl std::fmt::Debug for RemoteHostConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteHostConfig")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .field("permission_mode", &self.permission_mode)
            .field("allowed_roots", &self.allowed_roots)
            .finish_non_exhaustive()
    }
}

impl RemoteHostConfig {
    /// Validates launch invariants before any process or socket is created.
    ///
    /// # Errors
    ///
    /// Returns `invalid_params` for an empty server map or an invalid agent
    /// process config (empty command, relative cwd).
    pub fn validate(&self) -> Result<(), RemoteError> {
        if self.agents.is_empty() {
            return Err(RemoteError::invalid_params(
                "no agents configured; add [agents.servers.<id>] to ee config",
            ));
        }
        for (agent_id, process) in &self.agents {
            process.validate().map_err(|error| {
                RemoteError::invalid_params(format!("agents.servers.{agent_id}: {error}"))
            })?;
        }
        if self.ring_capacity == 0 {
            return Err(RemoteError::invalid_params("ring capacity must be greater than zero"));
        }
        Ok(())
    }

    /// Builds the manager config, applying the remote permission mode.
    ///
    /// `deny` replaces the configured ruleset with deny-all so the host broker
    /// refuses gated operations before any remote round-trip.
    #[must_use]
    pub fn manager_config(&self) -> AgentManagerConfig {
        let permission_rules = match self.permission_mode {
            RemotePermissionMode::Deny => {
                Arc::new(PermissionRules::new(PermissionRuleset::deny_all()))
            }
            RemotePermissionMode::Ask | RemotePermissionMode::Rules => {
                self.permission_rules.clone()
            }
        };
        // Struct-update syntax keeps the host's own `test-utils` field set
        // intact under cross-crate feature unification; this crate fills only
        // the fields it owns.
        #[cfg(feature = "test-utils")]
        let manager = AgentManagerConfig {
            agents: self.agents.clone(),
            protocols: std::collections::BTreeMap::new(),
            ee_proxy_enabled: self.ee_proxy_enabled,
            workspace_memory: self.workspace_memory.clone(),
            permission_rules,
            fake_transports: self.fake_transports.clone(),
        };
        #[cfg(not(feature = "test-utils"))]
        let manager = AgentManagerConfig {
            agents: self.agents.clone(),
            ee_proxy_enabled: self.ee_proxy_enabled,
            workspace_memory: self.workspace_memory.clone(),
            permission_rules,
            ..AgentManagerConfig::default()
        };
        manager
    }

    /// Bounded workspace-memory settings for a fixed set of trusted roots.
    #[must_use]
    pub fn workspace_memory_for_roots(roots: Vec<PathBuf>) -> WorkspaceMemoryHostConfig {
        WorkspaceMemoryHostConfig {
            enabled: true,
            trusted_roots: roots,
            database_path: None,
            quotas: WorkspaceMemoryQuotas::default(),
            busy_timeout: Duration::from_millis(5_000),
            retention: MemoryRetention::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_permission_mode_is_rejected() {
        assert!(RemotePermissionMode::parse("ask").is_ok());
        assert!(RemotePermissionMode::parse("RULES").is_ok());
        assert!(RemotePermissionMode::parse("deny ").is_ok());
        assert!(RemotePermissionMode::parse("allow-all").is_err());
    }

    #[test]
    fn deny_mode_replaces_rules_with_deny_all() {
        let mut config = RemoteHostConfig {
            permission_mode: RemotePermissionMode::Deny,
            ..RemoteHostConfig::default()
        };
        config.agents.insert("a".into(), AgentProcessConfig::new("agent"));
        let manager = config.manager_config();
        assert_eq!(
            manager.permission_rules.decide(&ee_agent_host::PermissionTarget::from_tool_call(
                &ee_agent_protocol::ToolCallUpdate::new(
                    "c",
                    ee_agent_protocol::ToolCallUpdateFields::new()
                        .name("bash")
                        .kind(ee_agent_protocol::ToolKind::Execute),
                )
            )),
            ee_agent_host::PermissionEffect::Deny
        );
    }

    #[test]
    fn empty_agent_map_fails_with_typed_error() {
        let error = RemoteHostConfig::default().validate().expect_err("no agents");
        assert_eq!(error.code(), crate::error::RemoteErrorCode::InvalidParams);
        assert!(error.message().contains("[agents.servers"));
    }
}
