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
    /// Whether setup must collect a value for this input.
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    /// Optional heading that groups related inputs in setup clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Suggested values; an empty list leaves the input free-form.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<SetupChoice>,
    /// Whether setup clients may hide this input behind an advanced-options gate.
    #[serde(default, skip_serializing_if = "is_false")]
    pub advanced: bool,
    /// Destination configuration mapping.
    pub config: SetupInputConfig,
}

/// One suggested value for a [`SetupInput`].
///
/// Choices with a [`SetupChoiceWhen`] condition are offered only when the
/// referenced sibling input already has the matching value, which lets an
/// agent present surface-specific model lists without asking the user to type
/// a model id by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupChoice {
    /// Exact value written to the destination environment variable.
    pub value: String,
    /// Optional human-readable label; falls back to `value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional sibling-input condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<SetupChoiceWhen>,
}

impl SetupChoice {
    /// Human-readable label shown by setup clients.
    #[must_use]
    pub fn display(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.value)
    }
}

/// Condition that scopes a [`SetupChoice`] to a previously collected input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupChoiceWhen {
    /// Key of the sibling input that must match `value`.
    pub input: String,
    /// Required sibling value.
    pub value: String,
}

fn is_false(value: &bool) -> bool {
    !*value
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
                required: false,
                section: None,
                choices: Vec::new(),
                advanced: false,
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
        assert!(!manifest.inputs[0].required);
        assert!(manifest.inputs[0].section.is_none());
        assert!(manifest.inputs[0].choices.is_empty());
        assert!(!manifest.inputs[0].advanced);
    }

    #[test]
    fn conditional_choices_deserialize_and_render_labels() {
        let manifest: SetupManifest = serde_json::from_value(json!({
            "schema_version": 1,
            "agent": { "id": "example", "display_name": "Example" },
            "env_vars": [],
            "inputs": [{
                "key": "surface",
                "label": "Surface",
                "required": true,
                "section": "Model selection",
                "choices": [
                    { "value": "zen" },
                    { "value": "go", "label": "go subscription" }
                ],
                "config": { "env": "EXAMPLE_SURFACE" }
            }, {
                "key": "model",
                "label": "Model",
                "required": true,
                "choices": [{
                    "value": "kimi-k3",
                    "label": "kimi-k3 (go)",
                    "when": { "input": "surface", "value": "go" }
                }],
                "advanced": true,
                "config": { "env": "EXAMPLE_MODEL" }
            }]
        }))
        .expect("choice manifest parses");

        let surface = &manifest.inputs[0];
        assert!(surface.required);
        assert_eq!(surface.section.as_deref(), Some("Model selection"));
        assert_eq!(surface.choices[0].display(), "zen");
        assert_eq!(surface.choices[1].display(), "go subscription");

        let model = &manifest.inputs[1];
        assert!(model.advanced);
        let when = model.choices[0].when.as_ref().expect("conditional choice");
        assert_eq!(when.input, "surface");
        assert_eq!(when.value, "go");
    }
}
