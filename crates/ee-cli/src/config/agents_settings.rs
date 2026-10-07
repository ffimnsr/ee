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
    AgentApprovalToml, AgentServerToml, AgentsToml, RubberDuckToml, WorkspaceMemoryToml,
};
use super::rubber_duck::{RubberDuckModeSetting, RubberDuckSettings};
use super::secret_value::ConfigSecretValue;
#[cfg(feature = "agents")]
use super::value::{ensure_named_table, mutate_config_at_scope};
#[cfg(any(feature = "agents", test))]
use super::web_context::agent_web_context_settings_to_toml;
use super::workspace_memory::WorkspaceMemorySettings;
use std::collections::BTreeMap;
#[cfg(feature = "agents")]
use std::path::Path;
use std::path::PathBuf;

#[cfg(any(feature = "agents", test))]
use ee_agent_host::AgentWebContextConfig;

const DEFAULT_AGENT_MAX_CONCURRENT_PROMPTS: usize = 4;
pub(super) const MAX_AGENT_MAX_CONCURRENT_PROMPTS: usize = 32;

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
    pub enabled: bool,
    pub default_agent: Option<String>,
    /// Maximum provider prompts in flight on each configured agent connection.
    pub max_concurrent_prompts: usize,
    pub servers: BTreeMap<String, AgentServerSettings>,
    /// Bridge-approval alignment with the agent's own permission prompt.
    pub approval: AgentApprovalSettings,
    /// Frontend-resolved critic policy; translated to backend policy on use.
    pub rubber_duck: RubberDuckSettings,
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
            enabled: false,
            default_agent: None,
            max_concurrent_prompts: DEFAULT_AGENT_MAX_CONCURRENT_PROMPTS,
            servers: BTreeMap::new(),
            approval: AgentApprovalSettings::default(),
            rubber_duck: RubberDuckSettings::default(),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentServerSettings {
    /// Optional frontend-only label shown in agent pickers.
    pub label: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, AgentEnvValue>,
    pub cwd: Option<PathBuf>,
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
    })
}

pub(super) fn agents_settings_to_toml(agents: &AgentsSettings) -> Option<AgentsToml> {
    Some(AgentsToml {
        enabled: Some(agents.enabled),
        default_agent: agents.default_agent.clone(),
        max_concurrent_prompts: Some(agents.max_concurrent_prompts),
        approval: Some(AgentApprovalToml {
            alignment: Some(agents.approval.alignment.label().to_string()),
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
                .into(),
            ),
            internal_model_id: agents.rubber_duck.internal_model_id.clone(),
            external_agent_id: agents.rubber_duck.external_agent_id.clone(),
            max_calls: Some(agents.rubber_duck.max_calls),
            max_context_bytes: Some(agents.rubber_duck.max_context_bytes),
            max_output_bytes: Some(agents.rubber_duck.max_output_bytes),
            timeout_ms: Some(agents.rubber_duck.timeout_ms),
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
) -> Result<PathBuf, String> {
    configure_global_agent_server_with_env(
        agent_id,
        command,
        args,
        env_values,
        &ConfigEnvironment::from_process(),
    )
}

#[cfg(feature = "agents")]
pub(super) fn configure_global_agent_server_with_env(
    agent_id: &str,
    command: &Path,
    args: &[String],
    env_values: &BTreeMap<String, String>,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    configure_agent_server_with_env(ConfigScope::Global, agent_id, command, args, env_values, env)
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
) -> Result<PathBuf, String> {
    configure_agent_server_with_env(
        ConfigScope::Local,
        agent_id,
        command,
        args,
        env_values,
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
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    let command =
        command.to_str().ok_or_else(|| String::from("agent executable path is not valid UTF-8"))?;
    mutate_config_at_scope(scope, env, |root| {
        let agents = ensure_named_table(root, "agents", "agents")?;
        agents.insert(String::from("enabled"), toml::Value::Boolean(true));
        agents.insert(String::from("default_agent"), toml::Value::String(agent_id.to_owned()));
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
