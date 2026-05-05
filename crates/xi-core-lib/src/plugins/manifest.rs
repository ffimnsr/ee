// Copyright 2017 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Structured representation of a plugin's features and capabilities.

use std::path::PathBuf;
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{self, Value, json};

use crate::syntax::{LanguageDefinition, LanguageId};

/// Declared permissions and editor features a plugin may use.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginCapability {
    Edit,
    Hover,
    Annotations,
    StatusItems,
    Filesystem,
    Network,
}

/// Describes attributes and capabilities of a plugin.
///
/// Note: - these will eventually be loaded from manifest files.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct PluginDescription {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub scope: PluginScope,
    #[serde(default)]
    pub runtime: PluginRuntime,
    #[serde(default)]
    pub capabilities: Vec<PluginCapability>,
    #[serde(default)]
    pub launch: PluginLaunchConfig,
    #[serde(default)]
    pub max_rss_bytes: Option<u64>,
    #[serde(default)]
    pub max_cpu_seconds: Option<u64>,
    #[serde(default)]
    pub rpc_timeout_ms: Option<u64>,
    // more metadata ...
    /// path to plugin executable
    #[serde(deserialize_with = "platform_exec_path")]
    pub exec_path: PathBuf,
    /// Events that cause this plugin to run
    #[serde(default)]
    pub activations: Vec<PluginActivation>,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub languages: Vec<LanguageDefinition>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginRuntime {
    #[default]
    Native,
    Wasm,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct PluginLaunchConfig {
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub transport: PluginTransport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginTransport {
    #[default]
    StdioNewline,
    StdioContentLength,
}

fn platform_exec_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    let exec_path = PathBuf::deserialize(deserializer)?;
    if cfg!(windows) { Ok(exec_path.with_extension("exe")) } else { Ok(exec_path) }
}

/// `PluginActivation`s represent events that trigger running a plugin.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginActivation {
    /// Always run this plugin, when available.
    Autorun,
    /// Run this plugin if the provided SyntaxDefinition is active.
    #[allow(dead_code)]
    OnSyntax(LanguageId),
    /// Run this plugin in response to a given command.
    #[allow(dead_code)]
    OnCommand,
}

impl PluginActivation {
    fn matches_language(&self, language: &LanguageId) -> bool {
        match self {
            PluginActivation::OnSyntax(active_language) => active_language == language,
            _ => false,
        }
    }
}

/// Describes the scope of events a plugin receives.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PluginScope {
    /// The plugin receives events from multiple buffers.
    Global,
    /// The plugin receives events for a single buffer.
    #[default]
    BufferLocal,
    /// The plugin is launched in response to a command, and receives no
    /// further updates.
    SingleInvocation,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
/// Represents a custom command provided by a plugin.
pub struct Command {
    /// Human readable title, for display in (for example) a menu.
    pub title: String,
    /// A short description of the command.
    pub description: String,
    /// Template of the command RPC as it should be sent to the plugin.
    pub rpc_cmd: PlaceholderRpc,
    /// A list of `CommandArgument`s, which the client should use to build the RPC.
    pub args: Vec<CommandArgument>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
/// A user provided argument to a plugin command.
pub struct CommandArgument {
    /// A human readable name for this argument, for use as placeholder
    /// text or equivelant.
    pub title: String,
    /// A short (single sentence) description of this argument's use.
    pub description: String,
    pub key: String,
    pub arg_type: ArgumentType,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// If `arg_type` is `Choice`, `options` must contain a list of options.
    pub options: Option<Vec<ArgumentOption>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub enum ArgumentType {
    Number,
    Int,
    PosInt,
    Bool,
    String,
    Choice,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
/// Represents an option for a user-selectable argument.
pub struct ArgumentOption {
    pub title: String,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// A placeholder type which can represent a generic RPC.
///
/// This is the type used for custom plugin commands, which may have arbitrary
/// method names and parameters.
pub struct PlaceholderRpc {
    pub method: String,
    pub params: Value,
    pub rpc_type: RpcType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RpcType {
    Notification,
    Request,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestValidationError {
    DuplicateCommandArgument { command: String, key: String },
    MissingCommandArgumentTemplate { command: String, key: String },
    NonObjectCommandParams { command: String },
    UndeclaredPlaceholder { command: String, key: String },
}

impl fmt::Display for ManifestValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestValidationError::DuplicateCommandArgument { command, key } => {
                write!(f, "command {command:?} declares duplicate argument key {key:?}")
            }
            ManifestValidationError::MissingCommandArgumentTemplate { command, key } => {
                write!(f, "command {command:?} is missing template param for argument {key:?}")
            }
            ManifestValidationError::NonObjectCommandParams { command } => {
                write!(f, "command {command:?} must use an object for rpc_cmd.params")
            }
            ManifestValidationError::UndeclaredPlaceholder { command, key } => {
                write!(
                    f,
                    "command {command:?} declares placeholder {key:?} without matching args entry"
                )
            }
        }
    }
}

impl Command {
    pub fn new<S, V>(title: S, description: S, rpc_cmd: PlaceholderRpc, args: V) -> Self
    where
        S: AsRef<str>,
        V: Into<Option<Vec<CommandArgument>>>,
    {
        let title = title.as_ref().to_owned();
        let description = description.as_ref().to_owned();
        let args = args.into().unwrap_or_default();
        Command { title, description, rpc_cmd, args }
    }

    pub fn validate(&self) -> Result<(), ManifestValidationError> {
        let params = self.rpc_cmd.params.as_object().ok_or_else(|| {
            ManifestValidationError::NonObjectCommandParams { command: self.title.clone() }
        })?;

        let mut arg_keys = HashSet::new();
        for arg in &self.args {
            if !arg_keys.insert(arg.key.as_str()) {
                return Err(ManifestValidationError::DuplicateCommandArgument {
                    command: self.title.clone(),
                    key: arg.key.clone(),
                });
            }

            if !params.contains_key(&arg.key) {
                return Err(ManifestValidationError::MissingCommandArgumentTemplate {
                    command: self.title.clone(),
                    key: arg.key.clone(),
                });
            }
        }

        for (key, value) in params {
            let is_placeholder = value.as_str().is_some_and(str::is_empty);
            let is_builtin_placeholder = matches!(key.as_str(), "view");
            if is_placeholder && !arg_keys.contains(key.as_str()) && !is_builtin_placeholder {
                return Err(ManifestValidationError::UndeclaredPlaceholder {
                    command: self.title.clone(),
                    key: key.clone(),
                });
            }
        }

        Ok(())
    }
}

impl CommandArgument {
    pub fn new<S: AsRef<str>>(
        title: S,
        description: S,
        key: S,
        arg_type: ArgumentType,
        options: Option<Vec<ArgumentOption>>,
    ) -> Self {
        let key = key.as_ref().to_owned();
        let title = title.as_ref().to_owned();
        let description = description.as_ref().to_owned();
        if arg_type == ArgumentType::Choice {
            assert!(options.is_some())
        }
        CommandArgument { title, description, key, arg_type, options }
    }
}

impl ArgumentOption {
    pub fn new<S: AsRef<str>, V: Serialize>(title: S, value: V) -> Self {
        let title = title.as_ref().to_owned();
        let value =
            serde_json::to_value(value).expect("ArgumentOption value must be JSON-serializable");
        ArgumentOption { title, value }
    }
}

impl PlaceholderRpc {
    pub fn new<S, V>(method: S, params: V, request: bool) -> Self
    where
        S: AsRef<str>,
        V: Into<Option<Value>>,
    {
        let method = method.as_ref().to_owned();
        let params = params.into().unwrap_or(json!({}));
        let rpc_type = if request { RpcType::Request } else { RpcType::Notification };

        PlaceholderRpc { method, params, rpc_type }
    }

    pub fn is_request(&self) -> bool {
        self.rpc_type == RpcType::Request
    }

    /// Returns a reference to the placeholder's params.
    pub fn params_ref(&self) -> &Value {
        &self.params
    }

    /// Returns a mutable reference to the placeholder's params.
    pub fn params_ref_mut(&mut self) -> &mut Value {
        &mut self.params
    }

    /// Returns a reference to the placeholder's method.
    pub fn method_ref(&self) -> &str {
        &self.method
    }
}

impl PluginDescription {
    pub(crate) fn json_schema() -> Value {
        serde_json::to_value(schema_for!(PluginDescription))
            .expect("plugin description schema should serialize")
    }

    /// Returns `true` if this plugin is globally scoped, else `false`.
    pub fn is_global(&self) -> bool {
        matches!(self.scope, PluginScope::Global)
    }

    /// Returns `true` if this plugin declares support for `capability`.
    pub fn has_capability(&self, capability: PluginCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn validates(&self) -> Result<(), ManifestValidationError> {
        for command in &self.commands {
            command.validate()?;
        }
        Ok(())
    }

    pub fn activates_on_command(&self) -> bool {
        matches!(self.scope, PluginScope::SingleInvocation)
            || self
                .activations
                .iter()
                .any(|activation| matches!(activation, PluginActivation::OnCommand))
    }

    pub fn activates_on_startup(&self) -> bool {
        !matches!(self.scope, PluginScope::SingleInvocation)
            && (self.activations.is_empty()
                || self
                    .activations
                    .iter()
                    .any(|activation| matches!(activation, PluginActivation::Autorun)))
    }

    pub fn activates_for_language(&self, language: &LanguageId) -> bool {
        self.activations.iter().any(|activation| activation.matches_language(language))
    }

    pub fn supports_command(&self, method: &str) -> bool {
        self.commands.iter().any(|command| command.rpc_cmd.method_ref() == method)
    }

    pub fn receives_updates_for(&self, language: &LanguageId) -> bool {
        !matches!(self.scope, PluginScope::SingleInvocation)
            && (self.activates_on_startup() || self.activates_for_language(language))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn platform_exec_path() {
        let json = r#"
        {
            "name": "test_plugin",
            "version": "0.0.0",
            "scope": "global",
            "capabilities": ["hover", "status_items"],
            "launch": {
                "working_dir": "plugin-data",
                "env": { "RUST_LOG": "debug" },
                "transport": "stdio_content_length"
            },
            "exec_path": "path/to/binary",
            "activations": [],
            "commands": [],
            "languages": []
        }
        "#;

        let plugin_desc: PluginDescription = serde_json::from_str(json).unwrap();
        if cfg!(windows) {
            assert!(plugin_desc.exec_path.ends_with("binary.exe"));
        } else {
            assert!(plugin_desc.exec_path.ends_with("binary"));
        }
        assert_eq!(plugin_desc.runtime, PluginRuntime::Native);
        assert!(plugin_desc.has_capability(PluginCapability::Hover));
        assert!(plugin_desc.has_capability(PluginCapability::StatusItems));
        assert_eq!(plugin_desc.launch.working_dir, Some(PathBuf::from("plugin-data")));
        assert_eq!(plugin_desc.launch.env.get("RUST_LOG"), Some(&"debug".to_string()));
        assert_eq!(plugin_desc.launch.transport, PluginTransport::StdioContentLength);
        assert_eq!(plugin_desc.max_rss_bytes, None);
        assert_eq!(plugin_desc.max_cpu_seconds, None);
        assert_eq!(plugin_desc.rpc_timeout_ms, None);
    }

    #[test]
    fn plugin_description_defaults_capabilities_to_empty() {
        let json = r#"
        {
            "name": "test_plugin",
            "version": "0.0.0",
            "exec_path": "path/to/binary"
        }
        "#;

        let plugin_desc: PluginDescription = serde_json::from_str(json).unwrap();

        assert!(plugin_desc.capabilities.is_empty());
        assert!(!plugin_desc.has_capability(PluginCapability::Edit));
        assert_eq!(plugin_desc.runtime, PluginRuntime::Native);
        assert_eq!(plugin_desc.launch, PluginLaunchConfig::default());
        assert_eq!(plugin_desc.max_rss_bytes, None);
        assert_eq!(plugin_desc.max_cpu_seconds, None);
        assert_eq!(plugin_desc.rpc_timeout_ms, None);
    }

    #[test]
    fn plugin_description_deserializes_wasm_runtime() {
        let json = r#"
        {
            "name": "test_plugin",
            "version": "0.0.0",
            "runtime": "wasm",
            "exec_path": "path/to/plugin.wasm"
        }
        "#;

        let plugin_desc: PluginDescription = serde_json::from_str(json).unwrap();

        assert_eq!(plugin_desc.runtime, PluginRuntime::Wasm);
    }

    #[test]
    fn plugin_description_deserializes_resource_limits() {
        let json = r#"
        {
            "name": "test_plugin",
            "version": "0.0.0",
            "max_rss_bytes": 4096,
            "max_cpu_seconds": 12,
            "rpc_timeout_ms": 250,
            "exec_path": "path/to/plugin"
        }
        "#;

        let plugin_desc: PluginDescription = serde_json::from_str(json).unwrap();

        assert_eq!(plugin_desc.max_rss_bytes, Some(4096));
        assert_eq!(plugin_desc.max_cpu_seconds, Some(12));
        assert_eq!(plugin_desc.rpc_timeout_ms, Some(250));
    }

    #[test]
    fn test_serde_command() {
        let json = r#"
    {
        "title": "Test Command",
        "description": "Passes the current test",
        "rpc_cmd": {
            "rpc_type": "notification",
            "method": "test.cmd",
            "params": {
                "view": "",
                "non_arg": "plugin supplied value",
                "arg_one": "",
                "arg_two": ""
            }
        },
        "args": [
            {
                "title": "First argument",
                "description": "Indicates something",
                "key": "arg_one",
                "arg_type": "Bool"
            },
            {
                "title": "Favourite Number",
                "description": "A number used in a test.",
                "key": "arg_two",
                "arg_type": "Choice",
                "options": [
                    {"title": "Five", "value": 5},
                    {"title": "Ten", "value": 10}
                ]
            }
        ]
    }
        "#;

        let command: Command = serde_json::from_str(json).unwrap();
        assert_eq!(command.title, "Test Command");
        assert_eq!(command.args[0].arg_type, ArgumentType::Bool);
        assert_eq!(command.rpc_cmd.params_ref()["non_arg"], "plugin supplied value");
        assert_eq!(command.args[1].options.clone().unwrap()[1].value, json!(10));
        assert!(command.validate().is_ok());
    }

    #[test]
    fn command_validation_rejects_undeclared_placeholders() {
        let command = Command::new(
            "Test Command",
            "desc",
            PlaceholderRpc::new(
                "test.cmd",
                Some(json!({
                    "view": "",
                    "arg_one": "",
                    "mystery": ""
                })),
                false,
            ),
            Some(vec![CommandArgument::new(
                "First argument",
                "desc",
                "arg_one",
                ArgumentType::String,
                None,
            )]),
        );

        assert_eq!(
            command.validate(),
            Err(ManifestValidationError::UndeclaredPlaceholder {
                command: "Test Command".to_string(),
                key: "mystery".to_string(),
            })
        );
    }

    #[test]
    fn command_validation_requires_all_declared_arguments() {
        let command = Command::new(
            "Test Command",
            "desc",
            PlaceholderRpc::new("test.cmd", Some(json!({ "view": "" })), false),
            Some(vec![CommandArgument::new(
                "First argument",
                "desc",
                "arg_one",
                ArgumentType::String,
                None,
            )]),
        );

        assert_eq!(
            command.validate(),
            Err(ManifestValidationError::MissingCommandArgumentTemplate {
                command: "Test Command".to_string(),
                key: "arg_one".to_string(),
            })
        );
    }

    #[test]
    fn plugin_activation_helpers_respect_manifest_activation_modes() {
        let syntax_plugin = PluginDescription {
            name: "syntax-plugin".into(),
            version: "0.1.0".into(),
            requires: Vec::new(),
            scope: PluginScope::BufferLocal,
            runtime: PluginRuntime::Native,
            capabilities: Vec::new(),
            launch: PluginLaunchConfig::default(),
            max_rss_bytes: None,
            max_cpu_seconds: None,
            rpc_timeout_ms: None,
            exec_path: PathBuf::from("plugin"),
            activations: vec![PluginActivation::OnSyntax("rust".into())],
            commands: Vec::new(),
            languages: Vec::new(),
        };
        let command_plugin = PluginDescription {
            name: "command-plugin".into(),
            version: "0.1.0".into(),
            requires: Vec::new(),
            scope: PluginScope::BufferLocal,
            runtime: PluginRuntime::Native,
            capabilities: Vec::new(),
            launch: PluginLaunchConfig::default(),
            max_rss_bytes: None,
            max_cpu_seconds: None,
            rpc_timeout_ms: None,
            exec_path: PathBuf::from("plugin"),
            activations: vec![PluginActivation::OnCommand],
            commands: Vec::new(),
            languages: Vec::new(),
        };
        let single_invocation = PluginDescription {
            name: "single-plugin".into(),
            version: "0.1.0".into(),
            requires: Vec::new(),
            scope: PluginScope::SingleInvocation,
            runtime: PluginRuntime::Native,
            capabilities: Vec::new(),
            launch: PluginLaunchConfig::default(),
            max_rss_bytes: None,
            max_cpu_seconds: None,
            rpc_timeout_ms: None,
            exec_path: PathBuf::from("plugin"),
            activations: Vec::new(),
            commands: Vec::new(),
            languages: Vec::new(),
        };

        assert!(syntax_plugin.receives_updates_for(&"rust".into()));
        assert!(!syntax_plugin.receives_updates_for(&"toml".into()));
        assert!(command_plugin.activates_on_command());
        assert!(!command_plugin.receives_updates_for(&"rust".into()));
        assert!(single_invocation.activates_on_command());
        assert!(!single_invocation.activates_on_startup());
        assert!(!single_invocation.receives_updates_for(&"rust".into()));
    }

    #[test]
    fn plugin_launch_config_defaults_to_newline_stdio() {
        let launch = PluginLaunchConfig::default();

        assert_eq!(launch.transport, PluginTransport::StdioNewline);
        assert!(launch.working_dir.is_none());
        assert!(launch.env.is_empty());
    }

    #[test]
    fn plugin_description_supports_manifest_command_method() {
        let plugin_desc = PluginDescription {
            name: "test_plugin".into(),
            version: "0.0.0".into(),
            requires: Vec::new(),
            scope: PluginScope::Global,
            runtime: PluginRuntime::Native,
            capabilities: Vec::new(),
            launch: PluginLaunchConfig::default(),
            max_rss_bytes: None,
            max_cpu_seconds: None,
            rpc_timeout_ms: None,
            exec_path: PathBuf::from("path/to/binary"),
            activations: Vec::new(),
            commands: vec![Command::new(
                "Reindent",
                "Reindent current selection",
                PlaceholderRpc::new("reindent", Some(json!({})), false),
                None,
            )],
            languages: Vec::new(),
        };

        assert!(plugin_desc.supports_command("reindent"));
        assert!(!plugin_desc.supports_command("toggle_comment"));
    }
}
