//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `/etc/ee/config.toml`
//!   3. `$XDG_CONFIG_HOME/ee/config.toml` or `~/.config/ee/config.toml`
//!   4. fallback `~/.ee.toml` when XDG user config is missing
//!   5. every ancestor `.ee.toml` from outermost to innermost
//!   6. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

use super::discovery::ConfigLayerKind;
#[cfg(feature = "agents")]
use super::discovery::{ConfigEnvironment, ConfigScope};
use super::raw::{
    AgentApprovalToml, AgentProcessSandboxToml, AgentServerToml, AgentsToml, RubberDuckToml,
    TerminalSandboxLimitsToml, TerminalSandboxToml, WorkspaceMemoryToml,
};
use super::rubber_duck::{RubberDuckModeSetting, RubberDuckSettings};
use super::secret_value::ConfigSecretValue;
#[cfg(feature = "agents")]
use super::value::{ensure_named_table, mutate_config_at_scope};
#[cfg(any(feature = "agents", test))]
use super::web_context::agent_web_context_settings_to_toml;
use super::workspace_memory::WorkspaceMemorySettings;
use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "agents")]
use std::path::Path;
use std::path::PathBuf;

#[cfg(any(feature = "agents", test))]
use ee_agent_host::AgentWebContextConfig;

const DEFAULT_AGENT_MAX_CONCURRENT_PROMPTS: usize = 4;
pub(super) const MAX_AGENT_MAX_CONCURRENT_PROMPTS: usize = 32;
/// Maximum named read-only reference sources kept from config.
pub(crate) const MAX_AGENT_REFERENCES: usize = 16;
/// Maximum reference-name length.
const MAX_AGENT_REFERENCE_NAME_CHARS: usize = 64;

/// Agent modes that never receive trusted-workspace write auto-approval.
/// Read-only modes still get the read-only command allowlist; they only lose
/// the implicit write consent. Repository config layers may add modes but
/// never remove them.
pub(super) const DEFAULT_READ_ONLY_MODES: [&str; 2] = ["ask", "plan"];

/// Bridge-approval alignment with the agent's own permission prompt.
///
/// `exact` (default) reuses a decision only when the ee-validated operation
/// matches the bridge request exactly.  `heuristic` additionally lets one
/// agent-prompt decision cover the next bridge operation of the same kind
/// when the payload carries no verifiable identity.  `off` disables alignment
/// entirely.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AgentAlignmentMode {
    #[default]
    Exact,
    Heuristic,
    Off,
}

impl AgentAlignmentMode {
    /// Parses the configured value; unknown values fail closed to an error
    /// the caller reports while keeping the previous mode.
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "exact" => Ok(Self::Exact),
            "heuristic" => Ok(Self::Heuristic),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "unsupported alignment {other:?}; expected \"exact\", \"heuristic\", or \"off\""
            )),
        }
    }

    #[must_use]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Heuristic => "heuristic",
            Self::Off => "off",
        }
    }
}

/// Resolved `[agents.sandbox]` policy for the agent subprocess.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AgentProcessSandboxMode {
    /// Run the agent process without an OS sandbox.
    #[default]
    Off,
    /// Confine writes to the workspace and explicit grants while keeping
    /// network access for model calls.
    WorkspaceWrite,
}

impl AgentProcessSandboxMode {
    /// Parses a config value.
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "off" => Ok(Self::Off),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            other => Err(format!(
                "agents.sandbox.mode must be \"off\" or \"workspace-write\", got {other:?}"
            )),
        }
    }

    /// Config label.
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceWrite => "workspace-write",
        }
    }
}

/// Resolved `[agents.sandbox]` policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AgentProcessSandboxSettings {
    pub mode: AgentProcessSandboxMode,
    pub write_paths: Vec<PathBuf>,
}

/// Merges `[agents.sandbox]`; host-global only because it controls containment.
pub(super) fn merge_agent_process_sandbox(
    settings: &mut AgentProcessSandboxSettings,
    patch: &AgentProcessSandboxToml,
    kind: ConfigLayerKind,
) {
    if !kind.is_host_global() {
        eprintln!(
            "ee: warning: {kind} agents.sandbox is host-global only; ignoring layer",
            kind = kind.label()
        );
        return;
    }
    if let Some(mode) = patch.mode.as_deref() {
        match AgentProcessSandboxMode::parse(mode) {
            Ok(mode) => settings.mode = mode,
            Err(error) => eprintln!("ee: warning: invalid agents.sandbox.mode: {error}"),
        }
    }
    if let Some(paths) = &patch.write_paths {
        settings.write_paths =
            paths.iter().filter(|path| !path.trim().is_empty()).map(PathBuf::from).collect();
    }
}

/// When the OS sandbox is applied to terminal commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum TerminalSandboxMode {
    /// Never sandbox terminal commands.
    Off,
    /// Sandbox commands auto-allowed by workspace terminal execution trust.
    #[default]
    Granted,
    /// Sandbox every native terminal command.
    All,
}

impl TerminalSandboxMode {
    /// Parses a config value.
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "off" => Ok(Self::Off),
            "granted" => Ok(Self::Granted),
            "all" => Ok(Self::All),
            other => Err(format!(
                "terminal_sandbox.mode must be \"off\", \"granted\", or \"all\", got {other:?}"
            )),
        }
    }

    /// Config label.
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Granted => "granted",
            Self::All => "all",
        }
    }
}

/// Behavior when the sandbox backend is unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum TerminalSandboxUnavailable {
    /// Fall back to the explicit approval prompt.
    #[default]
    Prompt,
    /// Deny the request instead of prompting.
    Deny,
}

impl TerminalSandboxUnavailable {
    /// Parses a config value.
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "prompt" => Ok(Self::Prompt),
            "deny" => Ok(Self::Deny),
            other => Err(format!(
                "terminal_sandbox.on_unavailable must be \"prompt\" or \"deny\", got {other:?}"
            )),
        }
    }

    /// Config label.
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Deny => "deny",
        }
    }
}

/// Resolved `[agents.terminal_sandbox]` policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalSandboxSettings {
    pub mode: TerminalSandboxMode,
    pub read: ee_sandbox::ReadAccess,
    pub network: ee_sandbox::NetworkAccess,
    pub on_unavailable: TerminalSandboxUnavailable,
    pub write_paths: Vec<PathBuf>,
    pub limits: ee_sandbox::ProcessLimits,
}

impl Default for TerminalSandboxSettings {
    fn default() -> Self {
        Self {
            mode: TerminalSandboxMode::Granted,
            read: ee_sandbox::ReadAccess::Broad,
            network: ee_sandbox::NetworkAccess::Deny,
            on_unavailable: TerminalSandboxUnavailable::Prompt,
            write_paths: Vec::new(),
            limits: ee_sandbox::ProcessLimits::default(),
        }
    }
}

/// Merges one `[agents.terminal_sandbox]` patch. Only host-global config may
/// change sandbox policy: a checked-in workspace file must not disable it.
pub(super) fn merge_terminal_sandbox(
    settings: &mut TerminalSandboxSettings,
    patch: &TerminalSandboxToml,
    kind: ConfigLayerKind,
) {
    if !kind.is_host_global() {
        eprintln!(
            "ee: warning: {kind} agents.terminal_sandbox is host-global only; ignoring layer",
            kind = kind.label()
        );
        return;
    }
    if let Some(mode) = patch.mode.as_deref() {
        match TerminalSandboxMode::parse(mode) {
            Ok(mode) => settings.mode = mode,
            Err(error) => eprintln!("ee: warning: invalid agents.terminal_sandbox.mode: {error}"),
        }
    }
    if let Some(read) = patch.read.as_deref() {
        match ee_sandbox::ReadAccess::parse(read) {
            Ok(read) => settings.read = read,
            Err(error) => eprintln!("ee: warning: invalid agents.terminal_sandbox.read: {error}"),
        }
    }
    if let Some(network) = patch.network.as_deref() {
        match ee_sandbox::NetworkAccess::parse(network) {
            Ok(network) => settings.network = network,
            Err(error) => {
                eprintln!("ee: warning: invalid agents.terminal_sandbox.network: {error}")
            }
        }
    }
    if let Some(value) = patch.on_unavailable.as_deref() {
        match TerminalSandboxUnavailable::parse(value) {
            Ok(value) => settings.on_unavailable = value,
            Err(error) => {
                eprintln!("ee: warning: invalid agents.terminal_sandbox.on_unavailable: {error}")
            }
        }
    }
    if let Some(paths) = &patch.write_paths {
        settings.write_paths =
            paths.iter().filter(|path| !path.trim().is_empty()).map(PathBuf::from).collect();
    }
    if let Some(limits) = &patch.limits {
        settings.limits = parse_terminal_limits(limits);
    }
}

fn parse_terminal_limits(limits: &TerminalSandboxLimitsToml) -> ee_sandbox::ProcessLimits {
    let positive = |value: Option<u64>| value.filter(|value| *value > 0);
    ee_sandbox::ProcessLimits {
        wall_seconds: positive(limits.wall_seconds),
        max_rss_bytes: positive(limits.max_rss_bytes),
        max_cpu_seconds: positive(limits.max_cpu_seconds),
        max_open_files: positive(limits.max_open_files),
        max_file_bytes: positive(limits.max_file_bytes),
        max_processes: positive(limits.max_processes),
    }
}

/// Resolved `[agents.approval]` policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AgentApprovalSettings {
    pub alignment: AgentAlignmentMode,
}

/// Merges one `[agents.approval]` patch.  `heuristic` alignment is
/// host-global only: repository (ancestor) config may restrict to `exact` or
/// `off` but can never broaden approval reuse.  Invalid values keep the
/// previous mode and print a bounded warning.
pub(super) fn merge_agent_approval(
    settings: &mut AgentApprovalSettings,
    patch: &AgentApprovalToml,
    kind: ConfigLayerKind,
) {
    if let Some(value) = patch.alignment.as_deref() {
        match AgentAlignmentMode::parse(value.trim()) {
            Ok(AgentAlignmentMode::Heuristic) if !kind.is_host_global() => {
                eprintln!(
                    "ee: warning: agents.approval.alignment = \"heuristic\" requires host-global config; keeping {} for {} config",
                    settings.alignment.label(),
                    kind.label()
                );
            }
            Ok(mode) => settings.alignment = mode,
            Err(error) => eprintln!(
                "ee: warning: invalid agents.approval.alignment: {error}; keeping {}",
                settings.alignment.label()
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentsSettings {
    /// Whether agents mode is enabled. On by default: the pane opens and
    /// reports that no agent is configured until `[agents.servers.<id>]`
    /// exists. Set `enabled = false` to opt out entirely.
    pub enabled: bool,
    pub default_agent: Option<String>,
    /// Maximum provider prompts in flight on each configured agent connection.
    pub max_concurrent_prompts: usize,
    /// Agent modes that never receive trusted-workspace write auto-approval.
    pub read_only_modes: BTreeSet<String>,
    pub servers: BTreeMap<String, AgentServerSettings>,
    /// Named read-only reference sources. Never workspace or write roots.
    pub references: BTreeMap<String, AgentReferenceSettings>,
    /// Bridge-approval alignment with the agent's own permission prompt.
    pub approval: AgentApprovalSettings,
    /// Declarative permission rules for external agent permission requests.
    /// `None` keeps the default ask-every-time behavior.
    pub permissions: Option<serde_json::Value>,
    /// Remote approval posture for headless drivers.
    pub remote_permission_mode: Option<String>,
    /// Frontend-resolved critic policy; translated to backend policy on use.
    pub rubber_duck: RubberDuckSettings,
    /// OS sandbox policy for prompt-free terminal commands.
    pub terminal_sandbox: TerminalSandboxSettings,
    /// OS sandbox policy for the agent subprocess.
    pub sandbox: AgentProcessSandboxSettings,
    /// Durable workspace memory with persistence. Enabled by default when
    /// agents mode is on; set `enabled = false` to opt out.
    pub workspace_memory: WorkspaceMemorySettings,
    /// Trusted web retrieval policy. This exists only in agents-enabled builds;
    /// raw config remains parseable in every build so schema validation is stable.
    #[cfg(any(feature = "agents", test))]
    pub web_context: AgentWebContextConfig,
}

impl Default for AgentsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            default_agent: None,
            max_concurrent_prompts: DEFAULT_AGENT_MAX_CONCURRENT_PROMPTS,
            read_only_modes: DEFAULT_READ_ONLY_MODES
                .iter()
                .map(|mode| (*mode).to_string())
                .collect(),
            servers: BTreeMap::new(),
            references: BTreeMap::new(),
            approval: AgentApprovalSettings::default(),
            permissions: None,
            remote_permission_mode: None,
            rubber_duck: RubberDuckSettings::default(),
            terminal_sandbox: TerminalSandboxSettings::default(),
            sandbox: AgentProcessSandboxSettings::default(),
            workspace_memory: WorkspaceMemorySettings::default(),
            #[cfg(any(feature = "agents", test))]
            web_context: AgentWebContextConfig::default(),
        }
    }
}

/// One agent environment value with its config-layer provenance (phase 5).
///
/// Shared with MCP env/header values through [`ConfigSecretValue`]: an exact
/// `secret://<name>` value stays raw text through parsing, merging, schema
/// generation, and display, and resolves only at use time under the layer
/// rules documented on that type.
pub(crate) type AgentEnvValue = ConfigSecretValue;

/// Resolved ACP agent subprocess definition.
/// One resolved read-only reference source (`[agents.references]`).
///
/// Local entries are read in place; remote entries are cloned into a bounded
/// host cache on first use and refreshed on a daily throttle. Reference roots
/// are never write roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentReferenceSettings {
    /// Absolute or workspace-relative local directory.
    Local(PathBuf),
    /// Remote git repository and optional branch.
    Remote { repository: String, branch: Option<String> },
}

/// Parses one `[agents.references]` entry and validates the name/fields.
pub(crate) fn parse_agent_reference(
    name: &str,
    value: &serde_json::Value,
) -> Result<AgentReferenceSettings, String> {
    let valid_name = !name.is_empty()
        && name.chars().count() <= MAX_AGENT_REFERENCE_NAME_CHARS
        && name.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        && name != "."
        && name != "..";
    if !valid_name {
        return Err(format!(
            "reference name {name:?} must be 1-{MAX_AGENT_REFERENCE_NAME_CHARS} ascii letters, digits, '-', '_', or '.'"
        ));
    }
    match value {
        serde_json::Value::String(path) if !path.trim().is_empty() => {
            Ok(AgentReferenceSettings::Local(PathBuf::from(path)))
        }
        serde_json::Value::String(_) => Err(String::from("local reference path must not be empty")),
        serde_json::Value::Object(map) => {
            let repository = map
                .get("repository")
                .and_then(serde_json::Value::as_str)
                .filter(|repository| !repository.trim().is_empty())
                .ok_or_else(|| String::from("remote reference requires a non-empty repository"))?;
            let branch = match map.get("branch") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(branch)) => {
                    if branch.trim().is_empty()
                        || branch.starts_with('-')
                        || branch.chars().any(char::is_whitespace)
                    {
                        return Err(String::from(
                            "remote reference branch must be a non-empty git ref without whitespace or a leading '-'",
                        ));
                    }
                    Some(branch.clone())
                }
                Some(_) => return Err(String::from("remote reference branch must be a string")),
            };
            for key in map.keys() {
                if !matches!(key.as_str(), "repository" | "branch") {
                    return Err(format!("unknown remote reference key {key:?}"));
                }
            }
            Ok(AgentReferenceSettings::Remote { repository: repository.to_string(), branch })
        }
        _ => Err(String::from("reference must be a path string or a repository table")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentServerSettings {
    /// Optional frontend-only label shown in agent pickers.
    pub label: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, AgentEnvValue>,
    pub cwd: Option<PathBuf>,
    /// ACP protocol version for this agent. Defaults to v1; v2 is opt-in
    /// while the spec is Preview.
    pub protocol: AgentProtocol,
}

/// Selected ACP protocol version for one configured agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AgentProtocol {
    /// Stable ACP v1 (default).
    #[default]
    V1,
    /// ACP v2 Preview; needs an MCP route for client tools.
    V2,
}

impl AgentProtocol {
    /// Parses the config value; only exact `v1`/`v2` are accepted.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "v1" => Ok(Self::V1),
            "v2" => Ok(Self::V2),
            other => Err(format!("protocol must be \"v1\" or \"v2\", got {other:?}")),
        }
    }

    /// Stable config label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }
}

pub(super) fn validate_agent_server(id: &str, server: &AgentServerToml) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err(String::from("agent server id must not be empty"));
    }
    if server.command.as_deref().is_some_and(|command| command.trim().is_empty()) {
        return Err(String::from("agent server command must not be empty"));
    }
    if server.label.as_deref().is_some_and(|label| label.trim().is_empty()) {
        return Err(String::from("agent server label must not be empty"));
    }
    for (key, value) in &server.env {
        crate::secrets::resolve::validate_secret_value(value, false).map_err(|err| {
            format!("invalid secret reference in agents.servers.{id}.env.{key}: {err}")
        })?;
    }
    if let Some(protocol) = &server.protocol {
        AgentProtocol::parse(protocol)
            .map_err(|error| format!("invalid agents.servers.{id}.protocol: {error}"))?;
    }
    Ok(())
}

pub(super) fn merge_agent_server(
    id: &str,
    server: &AgentServerToml,
    existing: Option<&AgentServerSettings>,
    kind: ConfigLayerKind,
) -> Result<AgentServerSettings, String> {
    validate_agent_server(id, server)?;

    let command = server
        .command
        .as_deref()
        .map(str::trim)
        .map(str::to_owned)
        .or_else(|| existing.map(|server| server.command.clone()))
        .unwrap_or_default();
    let args = server
        .args
        .clone()
        .or_else(|| existing.map(|server| server.args.clone()))
        .unwrap_or_default();
    let mut env = existing.map(|server| server.env.clone()).unwrap_or_default();
    for (key, value) in &server.env {
        if crate::secrets::is_secret_reference_text(value) && kind == ConfigLayerKind::System {
            return Err(format!(
                "secret references are not allowed in system config layers, \
                 but agents.servers.{id}.env.{key} comes from {} config",
                kind.label()
            ));
        }
        env.insert(key.clone(), AgentEnvValue { layer: kind, raw: value.clone() });
    }

    Ok(AgentServerSettings {
        label: server
            .label
            .as_deref()
            .map(str::trim)
            .map(str::to_owned)
            .or_else(|| existing.and_then(|server| server.label.clone())),
        command,
        args,
        env,
        cwd: server.cwd.clone().or_else(|| existing.and_then(|server| server.cwd.clone())),
        protocol: match &server.protocol {
            Some(protocol) => AgentProtocol::parse(protocol)?,
            None => existing.map_or(AgentProtocol::V1, |server| server.protocol),
        },
    })
}

pub(super) fn agents_settings_to_toml(agents: &AgentsSettings) -> Option<AgentsToml> {
    Some(AgentsToml {
        enabled: Some(agents.enabled),
        default_agent: agents.default_agent.clone(),
        max_concurrent_prompts: Some(agents.max_concurrent_prompts),
        read_only_modes: Some(agents.read_only_modes.iter().cloned().collect()),
        approval: Some(AgentApprovalToml {
            alignment: Some(agents.approval.alignment.label().to_string()),
        }),
        permissions: agents.permissions.clone(),
        remote_permission_mode: agents.remote_permission_mode.clone(),
        references: (!agents.references.is_empty()).then(|| {
            agents
                .references
                .iter()
                .map(|(name, reference)| {
                    let value = match reference {
                        AgentReferenceSettings::Local(path) => {
                            serde_json::Value::String(path.display().to_string())
                        }
                        AgentReferenceSettings::Remote { repository, branch } => {
                            let mut map = serde_json::Map::new();
                            map.insert(
                                String::from("repository"),
                                serde_json::Value::String(repository.clone()),
                            );
                            if let Some(branch) = branch {
                                map.insert(
                                    String::from("branch"),
                                    serde_json::Value::String(branch.clone()),
                                );
                            }
                            serde_json::Value::Object(map)
                        }
                    };
                    (name.clone(), value)
                })
                .collect()
        }),
        workspace_memory: Some(WorkspaceMemoryToml {
            enabled: Some(agents.workspace_memory.enabled),
            persist_notes: Some(agents.workspace_memory.persist_notes),
            max_value_bytes: Some(agents.workspace_memory.max_value_bytes),
            max_active_facts: Some(agents.workspace_memory.max_active_facts),
            max_active_bytes: Some(agents.workspace_memory.max_active_bytes),
            max_total_facts: Some(agents.workspace_memory.max_total_facts),
            max_total_bytes: Some(agents.workspace_memory.max_total_bytes),
            max_recall_results: Some(agents.workspace_memory.max_recall_results),
            busy_timeout_ms: Some(agents.workspace_memory.busy_timeout_ms),
            default_expiry_days: Some(agents.workspace_memory.default_expiry_days),
            candidate_retention_days: Some(agents.workspace_memory.candidate_retention_days),
            stale_retention_days: Some(agents.workspace_memory.stale_retention_days),
            superseded_retention_days: Some(agents.workspace_memory.superseded_retention_days),
        }),
        rubber_duck: Some(RubberDuckToml {
            mode: Some(
                match agents.rubber_duck.mode {
                    RubberDuckModeSetting::Off => "off",
                    RubberDuckModeSetting::Manual => "manual",
                    RubberDuckModeSetting::Automatic => "automatic",
                }
                .to_string(),
            ),
            internal_model_id: agents.rubber_duck.internal_model_id.clone(),
            external_agent_id: agents.rubber_duck.external_agent_id.clone(),
            max_calls: Some(agents.rubber_duck.max_calls),
            max_context_bytes: Some(agents.rubber_duck.max_context_bytes),
            max_output_bytes: Some(agents.rubber_duck.max_output_bytes),
            timeout_ms: Some(agents.rubber_duck.timeout_ms),
        }),
        terminal_sandbox: Some(TerminalSandboxToml {
            mode: Some(agents.terminal_sandbox.mode.label().to_string()),
            read: Some(agents.terminal_sandbox.read.label().to_string()),
            network: Some(agents.terminal_sandbox.network.label().to_string()),
            on_unavailable: Some(agents.terminal_sandbox.on_unavailable.label().to_string()),
            write_paths: Some(
                agents
                    .terminal_sandbox
                    .write_paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect(),
            ),
            limits: Some(TerminalSandboxLimitsToml {
                wall_seconds: agents.terminal_sandbox.limits.wall_seconds,
                max_rss_bytes: agents.terminal_sandbox.limits.max_rss_bytes,
                max_cpu_seconds: agents.terminal_sandbox.limits.max_cpu_seconds,
                max_open_files: agents.terminal_sandbox.limits.max_open_files,
                max_file_bytes: agents.terminal_sandbox.limits.max_file_bytes,
                max_processes: agents.terminal_sandbox.limits.max_processes,
            }),
        }),
        sandbox: Some(AgentProcessSandboxToml {
            mode: Some(agents.sandbox.mode.label().to_string()),
            write_paths: Some(
                agents.sandbox.write_paths.iter().map(|path| path.display().to_string()).collect(),
            ),
        }),
        web_context: {
            #[cfg(any(feature = "agents", test))]
            {
                agent_web_context_settings_to_toml(&agents.web_context)
            }
            #[cfg(not(any(feature = "agents", test)))]
            {
                None
            }
        },
        servers: agents
            .servers
            .iter()
            .map(|(id, server)| {
                (
                    id.clone(),
                    AgentServerToml {
                        label: server.label.clone(),
                        command: Some(server.command.clone()),
                        args: Some(server.args.clone()),
                        // Display paths keep the raw text: literals and
                        // `secret://` references exactly as configured.
                        env: server
                            .env
                            .iter()
                            .map(|(key, value)| (key.clone(), value.raw.clone()))
                            .collect(),
                        cwd: server.cwd.clone(),
                        protocol: (server.protocol == AgentProtocol::V2)
                            .then(|| server.protocol.label().to_string()),
                    },
                )
            })
            .collect(),
    })
}

#[cfg(feature = "agents")]
/// Writes one complete agent-server definition to the user config layer.
///
/// Agent setup is deliberately global when the user asks for `--user`:
/// workspace config must never receive machine-local executable paths or
/// encrypted secret references.
pub(crate) fn configure_global_agent_server(
    agent_id: &str,
    command: &Path,
    args: &[String],
    env_values: &BTreeMap<String, String>,
    make_default: bool,
) -> Result<PathBuf, String> {
    configure_global_agent_server_with_env(
        agent_id,
        command,
        args,
        env_values,
        make_default,
        &ConfigEnvironment::from_process(),
    )
}

#[cfg(feature = "agents")]
pub(super) fn configure_global_agent_server_with_env(
    agent_id: &str,
    command: &Path,
    args: &[String],
    env_values: &BTreeMap<String, String>,
    make_default: bool,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    configure_agent_server_with_env(
        ConfigScope::Global,
        agent_id,
        command,
        args,
        env_values,
        make_default,
        env,
    )
}

#[cfg(feature = "agents")]
/// Writes one complete agent-server definition to the workspace config layer.
/// Secret `secret://` references are written as-is; they resolve only when
/// the workspace trust decision recorded for this repository allows it.
pub(crate) fn configure_local_agent_server(
    agent_id: &str,
    command: &Path,
    args: &[String],
    env_values: &BTreeMap<String, String>,
    make_default: bool,
) -> Result<PathBuf, String> {
    configure_agent_server_with_env(
        ConfigScope::Local,
        agent_id,
        command,
        args,
        env_values,
        make_default,
        &ConfigEnvironment::from_process(),
    )
}

#[cfg(feature = "agents")]
pub(super) fn configure_agent_server_with_env(
    scope: ConfigScope,
    agent_id: &str,
    command: &Path,
    args: &[String],
    env_values: &BTreeMap<String, String>,
    make_default: bool,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    let command =
        command.to_str().ok_or_else(|| String::from("agent executable path is not valid UTF-8"))?;
    mutate_config_at_scope(scope, env, |root| {
        let agents = ensure_named_table(root, "agents", "agents")?;
        agents.insert(String::from("enabled"), toml::Value::Boolean(true));
        if make_default {
            agents.insert(String::from("default_agent"), toml::Value::String(agent_id.to_owned()));
        }
        let servers = ensure_named_table(agents, "servers", "agents.servers")?;
        let mut server = toml::map::Map::new();
        server.insert(String::from("command"), toml::Value::String(command.to_owned()));
        server.insert(
            String::from("args"),
            toml::Value::Array(args.iter().cloned().map(toml::Value::String).collect()),
        );
        server.insert(
            String::from("env"),
            toml::Value::Table(
                env_values
                    .iter()
                    .map(|(name, value)| (name.clone(), toml::Value::String(value.clone())))
                    .collect(),
            ),
        );
        servers.insert(agent_id.to_owned(), toml::Value::Table(server));
        Ok(())
    })
}

#[cfg(feature = "agents")]
/// Writes `[agents.rubber_duck]` mode and one backend id into the chosen
/// config layer. Limits stay at defaults unless already configured; the other
/// backend key is removed so a re-run cannot leave an ambiguous pair.
pub(crate) fn configure_rubber_duck(
    scope: ConfigScope,
    mode: RubberDuckModeSetting,
    internal_model_id: Option<&str>,
    external_agent_id: Option<&str>,
) -> Result<PathBuf, String> {
    configure_rubber_duck_with_env(
        scope,
        mode,
        internal_model_id,
        external_agent_id,
        &ConfigEnvironment::from_process(),
    )
}

#[cfg(feature = "agents")]
pub(super) fn configure_rubber_duck_with_env(
    scope: ConfigScope,
    mode: RubberDuckModeSetting,
    internal_model_id: Option<&str>,
    external_agent_id: Option<&str>,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    if internal_model_id.is_some() && external_agent_id.is_some() {
        return Err(String::from(
            "rubber duck internal model id and external agent id are mutually exclusive",
        ));
    }
    if matches!(mode, RubberDuckModeSetting::Off) {
        return Err(String::from("rubber duck setup cannot write mode `off`"));
    }
    if internal_model_id.is_none() && external_agent_id.is_none() {
        return Err(String::from("rubber duck backend is required"));
    }
    mutate_config_at_scope(scope, env, |root| {
        let agents = ensure_named_table(root, "agents", "agents")?;
        let duck = ensure_named_table(agents, "rubber_duck", "agents.rubber_duck")?;
        duck.insert(
            String::from("mode"),
            toml::Value::String(
                match mode {
                    RubberDuckModeSetting::Manual => "manual",
                    RubberDuckModeSetting::Automatic => "automatic",
                    RubberDuckModeSetting::Off => unreachable!("rejected above"),
                }
                .to_owned(),
            ),
        );
        match (internal_model_id, external_agent_id) {
            (Some(model_id), None) => {
                duck.insert(
                    String::from("internal_model_id"),
                    toml::Value::String(model_id.to_owned()),
                );
                duck.remove("external_agent_id");
            }
            (None, Some(agent_id)) => {
                duck.insert(
                    String::from("external_agent_id"),
                    toml::Value::String(agent_id.to_owned()),
                );
                duck.remove("internal_model_id");
            }
            _ => unreachable!("backend shape validated above"),
        }
        Ok(())
    })
}
