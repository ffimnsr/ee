//! Setup-manifest contract advertised by installed ee agent executables.
//!
//! Hosts invoke an agent with `--ee-config`, parse the JSON emitted on stdout,
//! then collect the declared environment variables and inputs.

use serde::{Deserialize, Serialize};

/// Current version of the agent setup-manifest JSON contract.
pub const SETUP_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Versioned setup requirements advertised by an ee agent executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupManifest {
    /// Version of this manifest contract.
    pub schema_version: u32,
    /// Agent identity shown by setup clients.
    pub agent: SetupAgent,
    /// Environment variables supplied directly by the user.
    pub env_vars: Vec<SetupEnvVar>,
    /// Configurable inputs mapped to environment variables.
    pub inputs: Vec<SetupInput>,
}

/// Stable identity and display metadata for an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupAgent {
    /// Stable machine-readable agent identifier.
    pub id: String,
    /// Human-readable agent name.
    pub display_name: String,
}

/// One environment variable required or accepted during setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupEnvVar {
    /// Environment variable name.
    pub name: String,
    /// Whether setup must collect a value for this variable.
    pub required: bool,
    /// Whether setup must treat this value as a secret.
    pub secret: bool,
    /// User-facing explanation of the value.
    pub description: String,
}

/// One non-secret setup input mapped to agent configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupInput {
    /// Stable input identifier.
    pub key: String,
    /// User-facing input label.
    pub label: String,
    /// Optional default value shown by setup clients.
    pub default: Option<String>,
    /// Destination configuration mapping.
    pub config: SetupInputConfig,
}

/// Configuration destination for a setup input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupInputConfig {
    /// Environment variable written from this input.
    pub env: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn manifest_serializes_to_stable_json_shape() {
        let manifest = SetupManifest {
            schema_version: SETUP_MANIFEST_SCHEMA_VERSION,
            agent: SetupAgent {
                id: String::from("example"),
                display_name: String::from("Example"),
            },
            env_vars: vec![SetupEnvVar {
                name: String::from("EXAMPLE_API_KEY"),
                required: true,
                secret: true,
                description: String::from("API key."),
            }],
            inputs: vec![SetupInput {
                key: String::from("model"),
                label: String::from("Model"),
                default: None,
                config: SetupInputConfig { env: String::from("EXAMPLE_MODEL") },
            }],
        };

        let value = serde_json::to_value(manifest).expect("manifest serializes");

        assert_eq!(
            value,
            json!({
                "schema_version": 1,
                "agent": { "id": "example", "display_name": "Example" },
                "env_vars": [{
                    "name": "EXAMPLE_API_KEY",
                    "required": true,
                    "secret": true,
                    "description": "API key."
                }],
                "inputs": [{
                    "key": "model",
                    "label": "Model",
                    "default": null,
                    "config": { "env": "EXAMPLE_MODEL" }
                }]
            })
        );
    }

    #[test]
    fn manifest_deserializes_from_agent_output() {
        let manifest: SetupManifest = serde_json::from_value(json!({
            "schema_version": 1,
            "agent": { "id": "example", "display_name": "Example" },
            "env_vars": [],
            "inputs": [{
                "key": "model",
                "label": "Model",
                "default": "example/default",
                "config": { "env": "EXAMPLE_MODEL" }
            }]
        }))
        .expect("agent manifest parses");

        assert_eq!(manifest.schema_version, SETUP_MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.agent.id, "example");
        assert_eq!(manifest.inputs[0].default.as_deref(), Some("example/default"));
        assert_eq!(manifest.inputs[0].config.env, "EXAMPLE_MODEL");
    }
}
