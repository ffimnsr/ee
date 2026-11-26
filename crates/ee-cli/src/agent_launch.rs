//! Shared agent launch resolution for the editor and headless serve paths.
//!
//! Exactly one place turns merged `[agents.servers]` settings into
//! `AgentProcessConfig` values, resolving `secret://` references through the
//! same store and workspace trust policy in both frontends.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ee_agent_host::AgentProcessConfig;
use ee_sandbox::{NetworkAccess, SandboxSpec};

use crate::config::{AgentProcessSandboxSettings, AgentServerSettings};
use crate::secrets::SecretStore;
use crate::secrets::resolve::{WorkspaceRefPolicy, agent_env_has_references, resolve_agent_env};

/// Resolved launch inputs for every configured agent.
#[derive(Debug, Default)]
pub(crate) struct ResolvedAgentLaunches {
    /// Agent id → process config.
    pub(crate) agents: BTreeMap<String, AgentProcessConfig>,
    /// Secret-like final env values for diagnostics redaction.
    pub(crate) secret_values: Vec<String>,
    /// Agent id → bounded launch failure message.
    pub(crate) failures: BTreeMap<String, String>,
}

/// Workspace-scoped sandbox template for agent subprocesses.
#[derive(Debug, Clone)]
pub(crate) struct AgentSandboxTemplate {
    workspace: PathBuf,
    write_paths: Vec<PathBuf>,
}

/// Builds the agent-process sandbox template when `[agents.sandbox]` is on.
pub(crate) fn agent_sandbox_template(
    workspace: &std::path::Path,
    settings: &AgentProcessSandboxSettings,
) -> Option<AgentSandboxTemplate> {
    if settings.mode == crate::config::AgentProcessSandboxMode::Off {
        return None;
    }
    let workspace = std::fs::canonicalize(workspace).ok()?;
    let mut write_paths = settings.write_paths.clone();
    // Agent session logs and caches are outside the workspace by design.
    for path in [
        dirs::data_local_dir().map(|dir| dir.join("ee").join("agent-sessions")),
        dirs::cache_dir().map(|dir| dir.join("ee")),
    ]
    .into_iter()
    .flatten()
    {
        if path.is_dir() && !write_paths.iter().any(|existing| existing == &path) {
            write_paths.push(path);
        }
    }
    Some(AgentSandboxTemplate { workspace, write_paths })
}

/// Resolves every configured agent server into a launch config.
///
/// A server whose secret references cannot resolve is skipped and reported;
/// it is never spawned with partial credentials.
#[must_use]
pub(crate) fn resolve_agent_launches(
    servers: &BTreeMap<String, AgentServerSettings>,
    store: Option<&SecretStore>,
    workspace_policy: WorkspaceRefPolicy,
    sandbox: Option<&AgentSandboxTemplate>,
) -> ResolvedAgentLaunches {
    let mut resolved = ResolvedAgentLaunches::default();
    for (id, server) in servers {
        let env = if agent_env_has_references(&server.env) {
            let Some(store) = store else {
                resolved.failures.insert(id.clone(), String::from("secrets store unavailable"));
                continue;
            };
            match resolve_agent_env(store, server, workspace_policy) {
                Ok(env) => env,
                Err(error) => {
                    resolved.failures.insert(id.clone(), error.to_string());
                    continue;
                }
            }
        } else {
            server.env.iter().map(|(key, value)| (key.clone(), value.raw.clone())).collect()
        };
        for (name, value) in &env {
            if ee_agent_host::redact::is_secret_key(name) {
                resolved.secret_values.push(value.clone());
            }
        }
        resolved.agents.insert(
            id.clone(),
            AgentProcessConfig {
                command: server.command.clone(),
                args: server.args.clone(),
                env,
                cwd: server.cwd.clone(),
                sandbox: sandbox.and_then(|template| {
                    let cwd = server.cwd.clone().unwrap_or_else(|| template.workspace.clone());
                    let mut spec = SandboxSpec::new(&template.workspace, &cwd)
                        .ok()?
                        .with_network(NetworkAccess::Allow);
                    for path in &template.write_paths {
                        if let Ok(with_path) = spec.clone().with_write_path(path) {
                            spec = with_path;
                        }
                    }
                    Some(spec)
                }),
            },
        );
    }
    resolved
}

#[cfg(test)]
mod tests {
    use crate::config::ConfigLayerKind;
    use crate::config::ConfigSecretValue;

    use super::*;

    fn server(env: BTreeMap<String, ConfigSecretValue>) -> AgentServerSettings {
        AgentServerSettings {
            label: None,
            command: String::from("agent"),
            args: Vec::new(),
            env,
            cwd: None,
            protocol: crate::config::AgentProtocol::V1,
        }
    }

    #[test]
    fn agent_sandbox_template_populates_process_config() {
        let temp = tempfile::tempdir().unwrap();
        let settings = crate::config::AgentProcessSandboxSettings {
            mode: crate::config::AgentProcessSandboxMode::WorkspaceWrite,
            write_paths: Vec::new(),
        };
        let template = agent_sandbox_template(temp.path(), &settings).expect("sandbox template");
        let servers = BTreeMap::from([(String::from("a"), server(BTreeMap::new()))]);
        let resolved =
            resolve_agent_launches(&servers, None, WorkspaceRefPolicy::Deny, Some(&template));
        let config = resolved.agents.get("a").unwrap();
        let spec = config.sandbox.as_ref().expect("agent sandbox spec");
        assert_eq!(spec.workspace, std::fs::canonicalize(temp.path()).unwrap());
        assert_eq!(spec.network, ee_sandbox::NetworkAccess::Allow);
    }

    #[test]
    fn plain_env_is_passed_through_and_secret_like_values_are_collected() {
        let mut env = BTreeMap::new();
        env.insert(
            "PLAIN".to_string(),
            ConfigSecretValue { layer: ConfigLayerKind::UserXdg, raw: String::from("value") },
        );
        env.insert(
            "API_TOKEN".to_string(),
            ConfigSecretValue { layer: ConfigLayerKind::UserXdg, raw: String::from("tok") },
        );
        let servers = BTreeMap::from([(String::from("a"), server(env))]);
        let resolved = resolve_agent_launches(&servers, None, WorkspaceRefPolicy::Deny, None);
        let agent = resolved.agents.get("a").expect("agent");
        assert_eq!(agent.env.get("PLAIN"), Some(&String::from("value")));
        assert_eq!(resolved.secret_values, vec![String::from("tok")]);
        assert!(resolved.failures.is_empty());
    }

    #[test]
    fn unresolved_references_skip_the_agent() {
        let mut env = BTreeMap::new();
        env.insert(
            "API_TOKEN".to_string(),
            ConfigSecretValue {
                layer: ConfigLayerKind::UserXdg,
                raw: String::from("secret://missing"),
            },
        );
        let servers = BTreeMap::from([(String::from("a"), server(env))]);
        let resolved = resolve_agent_launches(&servers, None, WorkspaceRefPolicy::Deny, None);
        assert!(resolved.agents.is_empty());
        assert!(resolved.failures.contains_key("a"));
    }
}
