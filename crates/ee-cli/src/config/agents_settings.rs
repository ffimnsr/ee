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
use super::discovery::{ConfigEnvironment, ConfigScope, config_path_for_scope_with_env};
#[cfg(feature = "agents")]
use super::init::validate_config_contents;
#[cfg(feature = "agents")]
use super::raw::parse_config_document;
use super::raw::{AgentServerToml, AgentsToml, RubberDuckToml, WorkspaceMemoryToml};
use super::rubber_duck::{RubberDuckModeSetting, RubberDuckSettings};
#[cfg(feature = "agents")]
use super::value::ensure_table;
#[cfg(any(feature = "agents", test))]
use super::web_context::agent_web_context_settings_to_toml;
use super::workspace_memory::WorkspaceMemorySettings;
use std::collections::BTreeMap;
#[cfg(feature = "agents")]
use std::fs;
#[cfg(feature = "agents")]
use std::path::Path;
use std::path::PathBuf;

#[cfg(any(feature = "agents", test))]
use ee_agent_host::AgentWebContextConfig;

const DEFAULT_AGENT_MAX_CONCURRENT_PROMPTS: usize = 4;
pub(super) const MAX_AGENT_MAX_CONCURRENT_PROMPTS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentsSettings {
    pub enabled: bool,
    pub default_agent: Option<String>,
    /// Maximum provider prompts in flight on each configured agent connection.
    pub max_concurrent_prompts: usize,
    pub servers: BTreeMap<String, AgentServerSettings>,
    /// Frontend-resolved critic policy; translated to backend policy on use.
    pub rubber_duck: RubberDuckSettings,
    /// Durable workspace memory. Explicitly disabled unless configured.
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
            rubber_duck: RubberDuckSettings::default(),
            workspace_memory: WorkspaceMemorySettings::default(),
            #[cfg(any(feature = "agents", test))]
            web_context: AgentWebContextConfig::default(),
        }
    }
}

/// One agent environment value with its config-layer provenance (phase 5).
///
/// An exact `secret://<name>` value is kept as raw text through parsing,
/// merging, schema generation, and display; it is resolved from the
/// host-bound secrets store only at agent launch, and only when the source
/// layer is user-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentEnvValue {
    /// The config layer this value came from.
    pub layer: ConfigLayerKind,
    /// Raw text exactly as written in config: a literal or an exact
    /// `secret://<name>` reference. Never resolved at merge time.
    pub raw: String,
}

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
        if crate::secrets::is_secret_reference_text(value) {
            crate::secrets::SecretReference::parse(value).map_err(|err| {
                format!("invalid secret reference in agents.servers.{id}.env.{key}: {err}")
            })?;
        }
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
        if crate::secrets::is_secret_reference_text(value)
            && !matches!(kind, ConfigLayerKind::UserXdg | ConfigLayerKind::UserLegacy)
        {
            return Err(format!(
                "secret references are only allowed in user config layers, \
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
        workspace_memory: Some(WorkspaceMemoryToml {
            enabled: Some(agents.workspace_memory.enabled),
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
/// Agent setup is deliberately global: workspace config must never receive
/// machine-local executable paths or encrypted secret references.
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
    let command =
        command.to_str().ok_or_else(|| String::from("agent executable path is not valid UTF-8"))?;
    let path = config_path_for_scope_with_env(ConfigScope::Global, env)?;
    let mut document = parse_config_document(&path)?;
    let root = ensure_table(&mut document)?;
    let agents = match root
        .entry(String::from("agents"))
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
    {
        toml::Value::Table(table) => table,
        _ => return Err(String::from("config key `agents` already exists and is not table")),
    };
    agents.insert(String::from("enabled"), toml::Value::Boolean(true));
    agents.insert(String::from("default_agent"), toml::Value::String(agent_id.to_owned()));
    let servers = match agents
        .entry(String::from("servers"))
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
    {
        toml::Value::Table(table) => table,
        _ => {
            return Err(String::from(
                "config key `agents.servers` already exists and is not table",
            ));
        }
    };
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

    let text = toml::to_string_pretty(&document)
        .map_err(|error| format!("cannot serialize config {}: {error}", path.display()))?;
    validate_config_contents(&path, &text)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create {}: {error}", parent.display()))?;
    }
    fs::write(&path, text).map_err(|error| format!("Cannot write {}: {error}", path.display()))?;
    Ok(path)
}
