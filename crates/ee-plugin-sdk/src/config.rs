//! Plugin configuration loaded from the host environment.

use std::collections::BTreeMap;

use ee_plugin_api::error::PluginError;
use ee_plugin_api::host::HostKind;
use ee_plugin_api::ids::{PluginId, ServiceId};
use ee_plugin_api::manifest::{API_VERSION, Limits};
use serde_json::Value;

/// Environment variable carrying the plugin package identifier.
pub const ENV_PLUGIN_ID: &str = "EE_PLUGIN_ID";
/// Environment variable carrying `editor` or `agent`.
pub const ENV_HOST: &str = "EE_PLUGIN_HOST";
/// Environment variable carrying a JSON config object.
pub const ENV_CONFIG: &str = "EE_PLUGIN_CONFIG";
/// Environment variable carrying a comma-separated service list.
pub const ENV_SERVICES: &str = "EE_PLUGIN_SERVICES";

/// Configuration handed to a native plugin at startup.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginConfig {
    pub plugin_id: PluginId,
    pub host: HostKind,
    pub api: String,
    pub config: Value,
    pub limits: Limits,
    pub services: Vec<ServiceId>,
}

impl PluginConfig {
    /// Load configuration from process environment.
    pub fn from_env() -> Result<Self, PluginError> {
        Self::from_env_map(std::env::vars())
    }

    /// Load configuration from an explicit key/value map.
    ///
    /// Split from [`PluginConfig::from_env`] so tests never mutate process
    /// environment globals.
    pub fn from_env_map<I, K, V>(vars: I) -> Result<Self, PluginError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let vars: BTreeMap<String, String> =
            vars.into_iter().map(|(key, value)| (key.into(), value.into())).collect();

        let plugin_id = vars.get(ENV_PLUGIN_ID).ok_or_else(|| PluginError::Config {
            message: format!("missing required environment variable {ENV_PLUGIN_ID}"),
        })?;
        let plugin_id = PluginId::new(plugin_id.clone()).map_err(PluginError::from)?;

        let host = match vars.get(ENV_HOST).map(String::as_str) {
            None | Some("editor") => HostKind::Editor,
            Some("agent") => HostKind::Agent,
            Some(other) => {
                return Err(PluginError::Config {
                    message: format!("unknown {ENV_HOST} value {other:?}"),
                });
            }
        };

        let config = match vars.get(ENV_CONFIG) {
            None => Value::Null,
            Some(raw) => serde_json::from_str(raw).map_err(|error| PluginError::Config {
                message: format!("invalid {ENV_CONFIG}: {error}"),
            })?,
        };

        let services = match vars.get(ENV_SERVICES) {
            None => Vec::new(),
            Some(raw) => raw
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.parse::<ServiceId>().map_err(PluginError::from))
                .collect::<Result<Vec<_>, _>>()?,
        };

        Ok(Self {
            plugin_id,
            host,
            api: API_VERSION.to_owned(),
            config,
            limits: Limits::default(),
            services,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_and_optional_values() {
        let config = PluginConfig::from_env_map([
            (ENV_PLUGIN_ID, "demo"),
            (ENV_HOST, "agent"),
            (ENV_CONFIG, "{\"enabled\":true}"),
            (ENV_SERVICES, "editor.document@1, agent.tools@1"),
        ])
        .unwrap();

        assert_eq!(config.plugin_id.as_str(), "demo");
        assert_eq!(config.host, HostKind::Agent);
        assert_eq!(config.config, serde_json::json!({"enabled": true}));
        assert_eq!(config.services.len(), 2);
        assert_eq!(config.services[1].as_str(), "agent.tools@1");
    }

    #[test]
    fn defaults_host_and_config() {
        let config = PluginConfig::from_env_map([(ENV_PLUGIN_ID, "demo")]).unwrap();
        assert_eq!(config.host, HostKind::Editor);
        assert_eq!(config.config, Value::Null);
        assert!(config.services.is_empty());
    }

    #[test]
    fn rejects_missing_plugin_id() {
        assert!(matches!(
            PluginConfig::from_env_map(std::iter::empty::<(&str, &str)>()),
            Err(PluginError::Config { .. })
        ));
    }

    #[test]
    fn rejects_invalid_host() {
        assert!(matches!(
            PluginConfig::from_env_map([(ENV_PLUGIN_ID, "demo"), (ENV_HOST, "nope")]),
            Err(PluginError::Config { .. })
        ));
    }

    #[test]
    fn rejects_invalid_config_json() {
        assert!(matches!(
            PluginConfig::from_env_map([(ENV_PLUGIN_ID, "demo"), (ENV_CONFIG, "{")]),
            Err(PluginError::Config { .. })
        ));
    }

    #[test]
    fn rejects_unversioned_service() {
        assert!(matches!(
            PluginConfig::from_env_map([
                (ENV_PLUGIN_ID, "demo"),
                (ENV_SERVICES, "editor.document"),
            ]),
            Err(PluginError::Id(_))
        ));
    }
}
