//! Manifest v2.
//!
//! Only `manifest_version = 2` and `api = "ee-plugin/2"` are accepted. There is
//! no v1 parser and no synthesized descriptor path.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contribution::{
    AgentToolContribution, CommandContribution, ConfigSchemaContribution,
    ContextProviderContribution, ContributionError, ExCommandContribution, FormatterContribution,
    KeybindingContribution, LanguageContribution, LspServerContribution, MenuItemContribution,
    ModelProviderContribution, OverlayContribution, PanelContribution, PickerContribution,
    PolicyRuleContribution, StatusItemContribution,
};
use crate::ids::{CommandId, ServiceId};

/// Only supported manifest version.
pub const MANIFEST_VERSION: u32 = 2;
/// Only supported plugin API identifier.
pub const API_VERSION: &str = "ee-plugin/2";

/// Permission names accepted in a v2 manifest.
pub const KNOWN_PERMISSIONS: &[&str] = &[
    "agent.context.contribute",
    "agent.models.register",
    "agent.policy.gate",
    "agent.policy.observe",
    "agent.sessions.control",
    "agent.sessions.read",
    "agent.storage.read",
    "agent.storage.write",
    "agent.strategies.register",
    "agent.tools.register",
    "agent.validation.contribute",
    "editor.agent.control",
    "editor.commands.invoke",
    "editor.commands.register",
    "editor.config.read",
    "editor.decorations.write",
    "editor.document.read",
    "editor.document.write",
    "editor.fs.read",
    "editor.fs.write",
    "editor.lsp.read",
    "editor.lsp.write",
    "editor.selection.read",
    "editor.selection.write",
    "editor.syntax.read",
    "editor.tasks.run",
    "editor.ui.write",
    "editor.views.read",
    "editor.views.write",
];

/// Manifest validation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("unsupported manifest_version {found}; expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
    #[error("unsupported api {found:?}; expected {expected:?}")]
    UnsupportedApi { found: String, expected: &'static str },
    #[error("manifest field `{field}` must not be empty")]
    EmptyField { field: &'static str },
    #[error("invalid semantic version {value:?}")]
    InvalidVersion { value: String },
    #[error("duplicate host target {target}")]
    DuplicateTarget { target: String },
    #[error("invalid service identifier {value:?}")]
    InvalidServiceId { value: String },
    #[error("duplicate service identifier {value}")]
    DuplicateServiceId { value: String },
    #[error("duplicate contribution identifier {id}")]
    DuplicateContribution { id: String },
    #[error("unknown permission {name:?}")]
    UnknownPermission { name: String },
    #[error("permission {name:?} must be enabled with `true`")]
    DisabledPermission { name: String },
    #[error("invalid limit `{field}`: must be greater than zero")]
    InvalidLimit { field: &'static str },
    #[error("invalid entry point {entry:?} for plugin kind {kind}")]
    InvalidEntry { entry: String, kind: &'static str },
    #[error("invalid contribution: {0}")]
    Contribution(ContributionError),
}

impl From<ContributionError> for ManifestError {
    fn from(value: ContributionError) -> Self {
        Self::Contribution(value)
    }
}

/// Plugin implementation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Builtin,
    #[default]
    Native,
    Wasm,
}

impl PluginKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Native => "native",
            Self::Wasm => "wasm",
        }
    }
}

/// Host that loads the plugin.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HostTarget {
    Editor,
    Agent,
}

/// Native transport used to speak the v2 wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaunchTransport {
    #[default]
    StdioNewline,
    StdioContentLength,
}

/// Native launch parameters. Wasm plugins ignore these fields.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LaunchConfig {
    #[serde(default)]
    pub transport: LaunchTransport,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_working_dir")]
    pub working_dir: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_working_dir() -> String {
    ".".to_owned()
}

/// Scope lifetime targets.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ScopeTarget {
    Host,
    Workspace,
    Buffer,
    View,
    Session,
    Turn,
    Command,
}

/// Activation conditions.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Activation {
    #[serde(default)]
    pub on_startup: bool,
    #[serde(default)]
    pub on_language: Vec<String>,
    #[serde(default)]
    pub on_command: Vec<String>,
    #[serde(default)]
    pub on_event: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<ScopeTarget>,
}

/// Required and optional service dependencies.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Requires {
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub optional_services: Vec<String>,
}

/// Services provided by the plugin.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Provides {
    #[serde(default)]
    pub services: Vec<String>,
}

/// Declared permissions. Every value must be `true`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Permissions(pub BTreeMap<String, bool>);

impl Permissions {
    pub fn grants(&self, permission: &str) -> bool {
        self.0.get(permission).copied().unwrap_or(false)
    }
}

/// Runtime resource limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Limits {
    #[serde(default = "default_memory")]
    pub max_memory_bytes: u64,
    #[serde(default = "default_cpu")]
    pub max_cpu_seconds: u64,
    #[serde(default = "default_rpc_timeout")]
    pub rpc_timeout_ms: u64,
    #[serde(default = "default_contributions")]
    pub max_contributions: usize,
}

fn default_memory() -> u64 {
    64 * 1024 * 1024
}

fn default_cpu() -> u64 {
    10
}

fn default_rpc_timeout() -> u64 {
    2_000
}

fn default_contributions() -> usize {
    128
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_memory_bytes: default_memory(),
            max_cpu_seconds: default_cpu(),
            rpc_timeout_ms: default_rpc_timeout(),
            max_contributions: default_contributions(),
        }
    }
}

/// Plugin configuration schema and defaults.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConfigSchema {
    #[serde(default)]
    pub schema: Option<Value>,
    #[serde(default)]
    pub defaults: Option<Value>,
}

/// UI contributions.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiContributions {
    #[serde(default)]
    pub status_items: Vec<StatusItemContribution>,
    #[serde(default)]
    pub panels: Vec<PanelContribution>,
    #[serde(default)]
    pub overlays: Vec<OverlayContribution>,
    #[serde(default)]
    pub pickers: Vec<PickerContribution>,
}

/// Agent contributions.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct AgentContributions {
    #[serde(default)]
    pub tools: Vec<AgentToolContribution>,
    #[serde(default)]
    pub model_providers: Vec<ModelProviderContribution>,
    #[serde(default)]
    pub policy_rules: Vec<PolicyRuleContribution>,
    #[serde(default)]
    pub context_providers: Vec<ContextProviderContribution>,
}

/// Declarative contribution tables.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Contributions {
    #[serde(default)]
    pub commands: Vec<CommandContribution>,
    #[serde(default)]
    pub ex_commands: Vec<ExCommandContribution>,
    #[serde(default)]
    pub keybindings: Vec<KeybindingContribution>,
    #[serde(default)]
    pub menu_items: Vec<MenuItemContribution>,
    #[serde(default)]
    pub ui: UiContributions,
    #[serde(default)]
    pub languages: Vec<LanguageContribution>,
    #[serde(default)]
    pub formatters: Vec<FormatterContribution>,
    #[serde(default)]
    pub lsp_servers: Vec<LspServerContribution>,
    #[serde(default)]
    pub agent: AgentContributions,
    #[serde(default)]
    pub config_schema: Vec<ConfigSchemaContribution>,
}

/// Complete v2 manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ManifestV2 {
    pub manifest_version: u32,
    pub name: String,
    pub version: String,
    pub api: String,
    pub targets: Vec<HostTarget>,
    pub kind: PluginKind,
    pub entry: String,
    #[serde(default)]
    pub launch: LaunchConfig,
    #[serde(default)]
    pub activation: Activation,
    #[serde(default)]
    pub requires: Requires,
    #[serde(default)]
    pub provides: Provides,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub contributes: Contributions,
    #[serde(default)]
    pub config: ConfigSchema,
}

impl ManifestV2 {
    /// Validate every field and every contribution.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.manifest_version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion {
                found: self.manifest_version,
                expected: MANIFEST_VERSION,
            });
        }
        if self.api != API_VERSION {
            return Err(ManifestError::UnsupportedApi {
                found: self.api.clone(),
                expected: API_VERSION,
            });
        }
        if self.name.is_empty() {
            return Err(ManifestError::EmptyField { field: "name" });
        }
        if self.version.is_empty() {
            return Err(ManifestError::EmptyField { field: "version" });
        }
        semver::Version::parse(&self.version)
            .map_err(|_| ManifestError::InvalidVersion { value: self.version.clone() })?;
        if self.targets.is_empty() {
            return Err(ManifestError::EmptyField { field: "targets" });
        }
        let mut seen_targets = BTreeSet::new();
        for target in &self.targets {
            if !seen_targets.insert(*target) {
                return Err(ManifestError::DuplicateTarget { target: format!("{target:?}") });
            }
        }
        if self.entry.is_empty() {
            return Err(ManifestError::EmptyField { field: "entry" });
        }
        self.validate_entry()?;
        self.validate_services()?;
        self.validate_permissions()?;
        self.validate_limits()?;
        self.validate_activation()?;
        self.validate_contributions()
    }

    fn validate_entry(&self) -> Result<(), ManifestError> {
        let ok = match self.kind {
            PluginKind::Builtin => self.entry.starts_with("builtin:") && self.entry.len() > 8,
            PluginKind::Native => true,
            PluginKind::Wasm => self.entry.ends_with(".wasm"),
        };
        if ok {
            Ok(())
        } else {
            Err(ManifestError::InvalidEntry { entry: self.entry.clone(), kind: self.kind.as_str() })
        }
    }

    fn validate_services(&self) -> Result<(), ManifestError> {
        let mut seen = BTreeSet::new();
        for value in self.requires.services.iter().chain(&self.requires.optional_services) {
            let id: ServiceId = value
                .parse()
                .map_err(|_| ManifestError::InvalidServiceId { value: value.clone() })?;
            if !seen.insert(id.to_string()) {
                return Err(ManifestError::DuplicateServiceId { value: id.to_string() });
            }
        }
        for value in &self.provides.services {
            let id: ServiceId = value
                .parse()
                .map_err(|_| ManifestError::InvalidServiceId { value: value.clone() })?;
            if !seen.insert(id.to_string()) {
                return Err(ManifestError::DuplicateServiceId { value: id.to_string() });
            }
        }
        Ok(())
    }

    fn validate_permissions(&self) -> Result<(), ManifestError> {
        for (name, enabled) in &self.permissions.0 {
            if !KNOWN_PERMISSIONS.contains(&name.as_str()) {
                return Err(ManifestError::UnknownPermission { name: name.clone() });
            }
            if !enabled {
                return Err(ManifestError::DisabledPermission { name: name.clone() });
            }
        }
        Ok(())
    }

    fn validate_limits(&self) -> Result<(), ManifestError> {
        let checks = [
            ("max_memory_bytes", self.limits.max_memory_bytes == 0),
            ("max_cpu_seconds", self.limits.max_cpu_seconds == 0),
            ("rpc_timeout_ms", self.limits.rpc_timeout_ms == 0),
            ("max_contributions", self.limits.max_contributions == 0),
        ];
        for (field, invalid) in checks {
            if invalid {
                return Err(ManifestError::InvalidLimit { field });
            }
        }
        Ok(())
    }

    fn validate_activation(&self) -> Result<(), ManifestError> {
        for language in &self.activation.on_language {
            if language.is_empty() {
                return Err(ManifestError::EmptyField { field: "activation.on_language" });
            }
        }
        for command in &self.activation.on_command {
            command
                .parse::<CommandId>()
                .map_err(|_| ManifestError::EmptyField { field: "activation.on_command" })?;
        }
        for event in &self.activation.on_event {
            if event.is_empty() {
                return Err(ManifestError::EmptyField { field: "activation.on_event" });
            }
        }
        Ok(())
    }

    fn validate_contributions(&self) -> Result<(), ManifestError> {
        let contributions = &self.contributes;
        let mut ids = BTreeSet::new();

        for command in &contributions.commands {
            command.validate()?;
            check_duplicate(&mut ids, command.id.as_str())?;
        }
        for ex_command in &contributions.ex_commands {
            ex_command.validate()?;
            check_duplicate(&mut ids, &ex_command.name)?;
        }
        for item in &contributions.menu_items {
            item.validate()?;
        }
        for item in &contributions.keybindings {
            item.validate()?;
        }
        for item in &contributions.ui.status_items {
            item.validate()?;
            check_duplicate(&mut ids, &item.key)?;
        }
        for panel in &contributions.ui.panels {
            panel.validate()?;
            check_duplicate(&mut ids, panel.id.as_str())?;
        }
        for overlay in &contributions.ui.overlays {
            overlay.validate()?;
            check_duplicate(&mut ids, overlay.id.as_str())?;
        }
        for picker in &contributions.ui.pickers {
            picker.validate()?;
            check_duplicate(&mut ids, picker.id.as_str())?;
        }
        for language in &contributions.languages {
            language.validate()?;
            check_duplicate(&mut ids, &language.name)?;
        }
        for formatter in &contributions.formatters {
            formatter.validate()?;
            check_duplicate(&mut ids, formatter.id.as_str())?;
        }
        for server in &contributions.lsp_servers {
            server.validate()?;
            check_duplicate(&mut ids, &server.id)?;
        }
        for tool in &contributions.agent.tools {
            tool.validate()?;
            check_duplicate(&mut ids, &tool.id)?;
        }
        for provider in &contributions.agent.model_providers {
            provider.validate()?;
            check_duplicate(&mut ids, &provider.id)?;
        }
        for rule in &contributions.agent.policy_rules {
            rule.validate()?;
            check_duplicate(&mut ids, &rule.id)?;
        }
        for provider in &contributions.agent.context_providers {
            provider.validate()?;
            check_duplicate(&mut ids, &provider.id)?;
        }
        for schema in &contributions.config_schema {
            schema.validate()?;
            check_duplicate(&mut ids, &schema.section)?;
        }
        Ok(())
    }
}

fn check_duplicate(ids: &mut BTreeSet<String>, id: &str) -> Result<(), ManifestError> {
    if !ids.insert(id.to_owned()) {
        return Err(ManifestError::DuplicateContribution { id: id.to_owned() });
    }
    Ok(())
}

/// JSON schema for [`ManifestV2`].
pub fn manifest_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(ManifestV2)).expect("manifest schema serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contribution::RenderSpec;

    fn base() -> ManifestV2 {
        ManifestV2 {
            manifest_version: MANIFEST_VERSION,
            name: "demo".to_owned(),
            version: "1.0.0".to_owned(),
            api: API_VERSION.to_owned(),
            targets: vec![HostTarget::Editor],
            kind: PluginKind::Native,
            entry: "bin/demo".to_owned(),
            launch: LaunchConfig::default(),
            activation: Activation { on_startup: true, ..Activation::default() },
            requires: Requires::default(),
            provides: Provides::default(),
            permissions: Permissions::default(),
            limits: Limits::default(),
            contributes: Contributions::default(),
            config: ConfigSchema::default(),
        }
    }

    #[test]
    fn valid_native_manifest_passes() {
        base().validate().unwrap();
    }

    #[test]
    fn valid_wasm_manifest_passes() {
        let mut manifest = base();
        manifest.kind = PluginKind::Wasm;
        manifest.entry = "plugin.wasm".to_owned();
        manifest.validate().unwrap();
    }

    #[test]
    fn valid_builtin_manifest_passes() {
        let mut manifest = base();
        manifest.kind = PluginKind::Builtin;
        manifest.entry = "builtin:lsp".to_owned();
        manifest.targets = vec![HostTarget::Editor, HostTarget::Agent];
        manifest.validate().unwrap();
    }

    #[test]
    fn rejects_manifest_v1() {
        let mut manifest = base();
        manifest.manifest_version = 1;
        assert_eq!(
            manifest.validate(),
            Err(ManifestError::UnsupportedVersion { found: 1, expected: 2 })
        );
    }

    #[test]
    fn rejects_old_api() {
        let mut manifest = base();
        manifest.api = "ee-plugin/1".to_owned();
        assert!(matches!(manifest.validate(), Err(ManifestError::UnsupportedApi { .. })));
    }

    #[test]
    fn rejects_invalid_semver() {
        let mut manifest = base();
        manifest.version = "not-a-version".to_owned();
        assert!(matches!(manifest.validate(), Err(ManifestError::InvalidVersion { .. })));
    }

    #[test]
    fn rejects_duplicate_targets() {
        let mut manifest = base();
        manifest.targets = vec![HostTarget::Editor, HostTarget::Editor];
        assert!(matches!(manifest.validate(), Err(ManifestError::DuplicateTarget { .. })));
    }

    #[test]
    fn rejects_bad_wasm_entry() {
        let mut manifest = base();
        manifest.kind = PluginKind::Wasm;
        assert!(matches!(manifest.validate(), Err(ManifestError::InvalidEntry { .. })));
    }

    #[test]
    fn rejects_invalid_service_identifier() {
        let mut manifest = base();
        manifest.requires.services = vec!["editor.document".to_owned()];
        assert!(matches!(manifest.validate(), Err(ManifestError::InvalidServiceId { .. })));
    }

    #[test]
    fn accepts_versioned_service_identifiers() {
        let mut manifest = base();
        manifest.requires.services = vec!["editor.document@1".to_owned()];
        manifest.provides.services = vec!["demo.service@1".to_owned()];
        manifest.validate().unwrap();
    }

    #[test]
    fn rejects_duplicate_contribution_id() {
        let mut manifest = base();
        let command = CommandContribution {
            id: CommandId::new("demo.run").unwrap(),
            title: "Run".to_owned(),
            description: String::new(),
            category: None,
            args_schema: None,
        };
        manifest.contributes.commands = vec![command.clone(), command];
        assert_eq!(
            manifest.validate(),
            Err(ManifestError::DuplicateContribution { id: "demo.run".to_owned() })
        );
    }

    #[test]
    fn rejects_unknown_permission() {
        let mut manifest = base();
        manifest.permissions.0.insert("editor.root".to_owned(), true);
        assert_eq!(
            manifest.validate(),
            Err(ManifestError::UnknownPermission { name: "editor.root".to_owned() })
        );
    }

    #[test]
    fn rejects_disabled_permission() {
        let mut manifest = base();
        manifest.permissions.0.insert("editor.document.read".to_owned(), false);
        assert_eq!(
            manifest.validate(),
            Err(ManifestError::DisabledPermission { name: "editor.document.read".to_owned() })
        );
    }

    #[test]
    fn accepts_contribution_tables() {
        let mut manifest = base();
        manifest.contributes.commands.push(CommandContribution {
            id: CommandId::new("outline.toggle").unwrap(),
            title: "Toggle Outline".to_owned(),
            description: "Show or hide the document outline".to_owned(),
            category: Some("outline".to_owned()),
            args_schema: Some(serde_json::json!({ "type": "object" })),
        });
        manifest.contributes.ui.panels.push(PanelContribution {
            id: crate::ids::ContributionId::new("outline.panel").unwrap(),
            title: "Outline".to_owned(),
            placement: crate::contribution::PanelPlacement::Right,
            render: RenderSpec::List { empty_text: None },
            data_provider: false,
        });
        manifest.validate().unwrap();
    }

    #[test]
    fn manifest_round_trips_json() {
        let manifest = base();
        let value = serde_json::to_value(&manifest).unwrap();
        let decoded: ManifestV2 = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, manifest);
    }
}
