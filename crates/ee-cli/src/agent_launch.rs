//! Shared agent launch resolution for the editor and headless serve paths.
//!
//! Exactly one place turns merged `[agents.servers]` settings into
//! `AgentProcessConfig` values, resolving `secret://` references through the
//! same store and workspace trust policy in both frontends.

use std::collections::BTreeMap;

use ee_agent_host::AgentProcessConfig;

use crate::config::AgentServerSettings;
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

/// Resolves every configured agent server into a launch config.
///
/// A server whose secret references cannot resolve is skipped and reported;
/// it is never spawned with partial credentials.
#[must_use]
pub(crate) fn resolve_agent_launches(
    servers: &BTreeMap<String, AgentServerSettings>,
    store: Option<&SecretStore>,
    workspace_policy: WorkspaceRefPolicy,
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
        let resolved = resolve_agent_launches(&servers, None, WorkspaceRefPolicy::Deny);
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
        let resolved = resolve_agent_launches(&servers, None, WorkspaceRefPolicy::Deny);
        assert!(resolved.agents.is_empty());
        assert!(resolved.failures.contains_key("a"));
    }
}
