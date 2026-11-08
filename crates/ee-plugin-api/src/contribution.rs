//! Declarative contributions.
//!
//! A contribution is the only way a plugin changes the interface. The frontend
//! renders contribution data; it never executes plugin callbacks. Every text
//! field is bounded and control characters are rejected so a plugin cannot emit
//! terminal escapes or unbounded payloads.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{CommandId, ContributionId};

/// Maximum length for a single contribution text field.
pub const MAX_TEXT_LEN: usize = 4096;
/// Maximum length for a keybinding spec.
pub const MAX_KEY_LEN: usize = 64;

/// Horizontal alignment for a status item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusAlignment {
    Left,
    Right,
}

/// Panel placement requested from the frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PanelPlacement {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

/// Declarative renderer for a panel or overlay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RenderSpec {
    List {
        #[serde(default)]
        empty_text: Option<String>,
    },
    Text,
    Markdown,
}

/// Anchor for an overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverlayAnchor {
    Cursor,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
}

/// Picker filtering strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilterMode {
    Substring,
    Fuzzy,
    Regex,
}

/// Lifetime of an agent tool contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolScope {
    Host,
    Session,
    Turn,
}

/// Policy gate class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyGate {
    Observe,
    Gate,
}

/// Trust level attached to contributed agent context.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Untrusted,
    Workspace,
    Trusted,
}

/// Command contributed to the palette and keymap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommandContribution {
    pub id: CommandId,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub args_schema: Option<Value>,
}

/// Ex command alias.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExCommandContribution {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub command: CommandId,
    #[serde(default)]
    pub usage: Option<String>,
}

/// Default key binding contributed beneath user bindings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct KeybindingContribution {
    pub command: CommandId,
    pub key: String,
    #[serde(default)]
    pub when: Option<String>,
}

/// Status bar item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StatusItemContribution {
    pub key: String,
    pub text: String,
    pub alignment: StatusAlignment,
    #[serde(default)]
    pub priority: i32,
}

/// Decoration provider descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DecorationProviderContribution {
    pub id: ContributionId,
    pub kinds: Vec<String>,
    #[serde(default)]
    pub revision_policy: Option<String>,
}

/// Panel descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PanelContribution {
    pub id: ContributionId,
    pub title: String,
    pub placement: PanelPlacement,
    pub render: RenderSpec,
    #[serde(default)]
    pub data_provider: bool,
}

/// Overlay descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OverlayContribution {
    pub id: ContributionId,
    pub anchor: OverlayAnchor,
    pub render: RenderSpec,
    #[serde(default)]
    pub transient: bool,
}

/// One picker column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PickerColumn {
    pub key: String,
    pub title: String,
}

/// Picker descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PickerContribution {
    pub id: ContributionId,
    pub title: String,
    #[serde(default)]
    pub columns: Vec<PickerColumn>,
    #[serde(default = "default_filter_mode")]
    pub filter_mode: FilterMode,
    #[serde(default)]
    pub data_provider: bool,
}

fn default_filter_mode() -> FilterMode {
    FilterMode::Substring
}

/// Menu entry pointing at a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MenuItemContribution {
    pub command: CommandId,
    pub label: String,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub order: i32,
}

/// Language definition contributed by a language plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LanguageContribution {
    pub name: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub filenames: Vec<String>,
    #[serde(default)]
    pub globs: Vec<String>,
    #[serde(default)]
    pub first_line_match: Option<String>,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub default_config: Option<Value>,
}

/// Formatter descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FormatterContribution {
    pub id: ContributionId,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
}

/// LSP server descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LspServerContribution {
    pub id: String,
    pub language: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    #[schemars(skip)]
    pub initialization_options: Option<Value>,
}

/// Agent tool descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentToolContribution {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
    #[serde(default = "default_tool_scope")]
    pub scope: AgentToolScope,
}

fn default_tool_scope() -> AgentToolScope {
    AgentToolScope::Host
}

/// Model provider descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelProviderContribution {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Agent policy rule descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyRuleContribution {
    pub id: String,
    pub gate: PolicyGate,
    #[serde(default)]
    pub precedence: i32,
}

/// Agent context provider descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContextProviderContribution {
    pub id: String,
    pub trust: TrustLevel,
    #[serde(default = "default_context_bytes")]
    pub max_bytes: usize,
}

fn default_context_bytes() -> usize {
    16 * 1024
}

/// Plugin configuration section contributed to the host schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigSchemaContribution {
    pub section: String,
    #[schemars(skip)]
    pub schema: Value,
    #[serde(default)]
    #[schemars(skip)]
    pub defaults: Option<Value>,
}

/// Any declarative contribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Contribution {
    Command(CommandContribution),
    ExCommand(ExCommandContribution),
    Keybinding(KeybindingContribution),
    StatusItem(StatusItemContribution),
    DecorationProvider(DecorationProviderContribution),
    Panel(PanelContribution),
    Overlay(OverlayContribution),
    Picker(PickerContribution),
    MenuItem(MenuItemContribution),
    Language(LanguageContribution),
    Formatter(FormatterContribution),
    LspServer(LspServerContribution),
    AgentTool(AgentToolContribution),
    ModelProvider(ModelProviderContribution),
    PolicyRule(PolicyRuleContribution),
    ContextProvider(ContextProviderContribution),
    ConfigSchema(ConfigSchemaContribution),
}

/// Contribution validation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContributionError {
    #[error("contribution field `{field}` must not be empty")]
    EmptyField { field: &'static str },
    #[error("contribution field `{field}` exceeds {max} bytes")]
    TooLong { field: &'static str, max: usize },
    #[error("contribution field `{field}` contains control characters")]
    ControlCharacters { field: &'static str },
    #[error("invalid keybinding spec: {key:?}")]
    InvalidKeySpec { key: String },
    #[error("contribution field `{field}` must be a JSON object")]
    InvalidSchema { field: &'static str },
}

fn validate_text(field: &'static str, value: &str) -> Result<(), ContributionError> {
    if value.len() > MAX_TEXT_LEN {
        return Err(ContributionError::TooLong { field, max: MAX_TEXT_LEN });
    }
    if value.chars().any(char::is_control) {
        return Err(ContributionError::ControlCharacters { field });
    }
    Ok(())
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), ContributionError> {
    if value.is_empty() {
        return Err(ContributionError::EmptyField { field });
    }
    validate_text(field, value)
}

fn validate_key(key: &str) -> Result<(), ContributionError> {
    if key.is_empty() || key.len() > MAX_KEY_LEN || key.chars().any(char::is_control) {
        return Err(ContributionError::InvalidKeySpec { key: key.to_owned() });
    }
    Ok(())
}

impl Contribution {
    /// Stable identifier used for upsert/remove tracking.
    pub fn id(&self) -> ContributionId {
        let raw = match self {
            Self::Command(c) => c.id.as_str(),
            Self::ExCommand(c) => c.name.as_str(),
            Self::Keybinding(c) => c.command.as_str(),
            Self::StatusItem(c) => c.key.as_str(),
            Self::DecorationProvider(c) => c.id.as_str(),
            Self::Panel(c) => c.id.as_str(),
            Self::Overlay(c) => c.id.as_str(),
            Self::Picker(c) => c.id.as_str(),
            Self::MenuItem(c) => c.command.as_str(),
            Self::Language(c) => c.name.as_str(),
            Self::Formatter(c) => c.id.as_str(),
            Self::LspServer(c) => c.id.as_str(),
            Self::AgentTool(c) => c.id.as_str(),
            Self::ModelProvider(c) => c.id.as_str(),
            Self::PolicyRule(c) => c.id.as_str(),
            Self::ContextProvider(c) => c.id.as_str(),
            Self::ConfigSchema(c) => c.section.as_str(),
        };
        ContributionId::new(raw).expect("validated contribution id")
    }

    /// Validate every bounded field.
    pub fn validate(&self) -> Result<(), ContributionError> {
        match self {
            Self::Command(c) => {
                validate_non_empty("title", &c.title)?;
                validate_text("description", &c.description)?;
                validate_optional("category", c.category.as_deref())?;
                if let Some(schema) = &c.args_schema
                    && !schema.is_object()
                {
                    return Err(ContributionError::InvalidSchema { field: "args_schema" });
                }
            }
            Self::ExCommand(c) => {
                validate_non_empty("name", &c.name)?;
                for alias in &c.aliases {
                    validate_non_empty("aliases", alias)?;
                }
                validate_optional("usage", c.usage.as_deref())?;
            }
            Self::Keybinding(c) => validate_key(&c.key)?,
            Self::StatusItem(c) => {
                validate_non_empty("key", &c.key)?;
                validate_non_empty("text", &c.text)?;
            }
            Self::DecorationProvider(c) => {
                for kind in &c.kinds {
                    validate_non_empty("kinds", kind)?;
                }
            }
            Self::Panel(c) => {
                validate_non_empty("title", &c.title)?;
                validate_render(&c.render)?;
            }
            Self::Overlay(c) => validate_render(&c.render)?,
            Self::Picker(c) => {
                validate_non_empty("title", &c.title)?;
                for column in &c.columns {
                    validate_non_empty("columns.key", &column.key)?;
                    validate_non_empty("columns.title", &column.title)?;
                }
            }
            Self::MenuItem(c) => validate_non_empty("label", &c.label)?,
            Self::Language(c) => {
                validate_non_empty("name", &c.name)?;
                validate_non_empty("scope", &c.scope)?;
                for extension in &c.extensions {
                    validate_non_empty("extensions", extension)?;
                }
            }
            Self::Formatter(c) => {
                validate_non_empty("command", &c.command)?;
                for extension in &c.extensions {
                    validate_non_empty("extensions", extension)?;
                }
            }
            Self::LspServer(c) => {
                validate_non_empty("id", &c.id)?;
                validate_non_empty("language", &c.language)?;
                validate_non_empty("command", &c.command)?;
            }
            Self::AgentTool(c) => {
                validate_non_empty("id", &c.id)?;
                validate_non_empty("name", &c.name)?;
                validate_non_empty("description", &c.description)?;
                if !c.parameters_schema.is_object() {
                    return Err(ContributionError::InvalidSchema { field: "parameters_schema" });
                }
            }
            Self::ModelProvider(c) => {
                validate_non_empty("id", &c.id)?;
                validate_non_empty("name", &c.name)?;
            }
            Self::PolicyRule(c) => validate_non_empty("id", &c.id)?,
            Self::ContextProvider(c) => validate_non_empty("id", &c.id)?,
            Self::ConfigSchema(c) => {
                validate_non_empty("section", &c.section)?;
                if !c.schema.is_object() {
                    return Err(ContributionError::InvalidSchema { field: "schema" });
                }
            }
        }
        Ok(())
    }
}

fn validate_optional(field: &'static str, value: Option<&str>) -> Result<(), ContributionError> {
    match value {
        Some(value) => validate_text(field, value),
        None => Ok(()),
    }
}

fn validate_render(render: &RenderSpec) -> Result<(), ContributionError> {
    if let RenderSpec::List { empty_text: Some(text) } = render {
        validate_text("render.empty_text", text)?;
    }
    Ok(())
}

macro_rules! validate_contribution_variant {
    ($ty:ty, $variant:ident) => {
        impl $ty {
            /// Validate this descriptor using the shared contribution rules.
            pub fn validate(&self) -> Result<(), ContributionError> {
                Contribution::$variant(self.clone()).validate()
            }
        }
    };
}

validate_contribution_variant!(CommandContribution, Command);
validate_contribution_variant!(ExCommandContribution, ExCommand);
validate_contribution_variant!(KeybindingContribution, Keybinding);
validate_contribution_variant!(StatusItemContribution, StatusItem);
validate_contribution_variant!(DecorationProviderContribution, DecorationProvider);
validate_contribution_variant!(PanelContribution, Panel);
validate_contribution_variant!(OverlayContribution, Overlay);
validate_contribution_variant!(PickerContribution, Picker);
validate_contribution_variant!(MenuItemContribution, MenuItem);
validate_contribution_variant!(LanguageContribution, Language);
validate_contribution_variant!(FormatterContribution, Formatter);
validate_contribution_variant!(LspServerContribution, LspServer);
validate_contribution_variant!(AgentToolContribution, AgentTool);
validate_contribution_variant!(ModelProviderContribution, ModelProvider);
validate_contribution_variant!(PolicyRuleContribution, PolicyRule);
validate_contribution_variant!(ContextProviderContribution, ContextProvider);
validate_contribution_variant!(ConfigSchemaContribution, ConfigSchema);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{CommandId, ContributionId};

    fn command() -> CommandId {
        CommandId::new("outline.toggle").unwrap()
    }

    fn contribution() -> ContributionId {
        ContributionId::new("outline.panel").unwrap()
    }

    fn every_variant() -> Vec<Contribution> {
        vec![
            Contribution::Command(CommandContribution {
                id: command(),
                title: "Toggle Outline".to_owned(),
                description: "Show or hide the document outline".to_owned(),
                category: Some("outline".to_owned()),
                args_schema: Some(serde_json::json!({ "type": "object" })),
            }),
            Contribution::ExCommand(ExCommandContribution {
                name: "outline".to_owned(),
                aliases: vec!["out".to_owned()],
                command: command(),
                usage: None,
            }),
            Contribution::Keybinding(KeybindingContribution {
                command: command(),
                key: "ctrl+shift+o".to_owned(),
                when: Some("editor".to_owned()),
            }),
            Contribution::StatusItem(StatusItemContribution {
                key: "outline.status".to_owned(),
                text: "outline: on".to_owned(),
                alignment: StatusAlignment::Right,
                priority: 20,
            }),
            Contribution::DecorationProvider(DecorationProviderContribution {
                id: contribution(),
                kinds: vec!["diagnostic".to_owned()],
                revision_policy: Some("revisioned".to_owned()),
            }),
            Contribution::Panel(PanelContribution {
                id: contribution(),
                title: "Outline".to_owned(),
                placement: PanelPlacement::Right,
                render: RenderSpec::List { empty_text: Some("No symbols".to_owned()) },
                data_provider: true,
            }),
            Contribution::Overlay(OverlayContribution {
                id: contribution(),
                anchor: OverlayAnchor::Cursor,
                render: RenderSpec::Text,
                transient: true,
            }),
            Contribution::Picker(PickerContribution {
                id: contribution(),
                title: "Select symbol".to_owned(),
                columns: vec![PickerColumn { key: "name".to_owned(), title: "Name".to_owned() }],
                filter_mode: FilterMode::Fuzzy,
                data_provider: true,
            }),
            Contribution::MenuItem(MenuItemContribution {
                command: command(),
                label: "Outline".to_owned(),
                group: Some("view".to_owned()),
                order: 10,
            }),
            Contribution::Language(LanguageContribution {
                name: "rust".to_owned(),
                extensions: vec!["rs".to_owned()],
                filenames: vec![],
                globs: vec![],
                first_line_match: None,
                scope: "source.rust".to_owned(),
                default_config: None,
            }),
            Contribution::Formatter(FormatterContribution {
                id: contribution(),
                command: "rustfmt".to_owned(),
                args: vec![],
                extensions: vec!["rs".to_owned()],
            }),
            Contribution::LspServer(LspServerContribution {
                id: "rust-analyzer".to_owned(),
                language: "rust".to_owned(),
                command: "rust-analyzer".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                initialization_options: None,
            }),
            Contribution::AgentTool(AgentToolContribution {
                id: "demo.tool".to_owned(),
                name: "demo".to_owned(),
                description: "Demo tool".to_owned(),
                parameters_schema: serde_json::json!({ "type": "object" }),
                scope: AgentToolScope::Turn,
            }),
            Contribution::ModelProvider(ModelProviderContribution {
                id: "demo.model".to_owned(),
                name: "Demo".to_owned(),
                capabilities: vec!["chat".to_owned()],
            }),
            Contribution::PolicyRule(PolicyRuleContribution {
                id: "demo.rule".to_owned(),
                gate: PolicyGate::Observe,
                precedence: 5,
            }),
            Contribution::ContextProvider(ContextProviderContribution {
                id: "demo.context".to_owned(),
                trust: TrustLevel::Workspace,
                max_bytes: 1024,
            }),
            Contribution::ConfigSchema(ConfigSchemaContribution {
                section: "outline".to_owned(),
                schema: serde_json::json!({ "type": "object" }),
                defaults: None,
            }),
        ]
    }

    #[test]
    fn every_variant_round_trips_json() {
        for contribution in every_variant() {
            contribution.validate().unwrap();
            let value = serde_json::to_value(&contribution).unwrap();
            assert!(value.get("kind").is_some());
            let decoded: Contribution = serde_json::from_value(value).unwrap();
            assert_eq!(decoded, contribution);
        }
    }

    #[test]
    fn ids_are_derived_from_each_variant() {
        for contribution in every_variant() {
            assert!(!contribution.id().as_str().is_empty());
        }
    }

    #[test]
    fn control_characters_are_rejected() {
        let bad = Contribution::StatusItem(StatusItemContribution {
            key: "outline.status".to_owned(),
            text: "\u{1b}[31mred".to_owned(),
            alignment: StatusAlignment::Left,
            priority: 0,
        });
        assert_eq!(bad.validate(), Err(ContributionError::ControlCharacters { field: "text" }));
    }

    #[test]
    fn oversized_text_is_rejected() {
        let bad = Contribution::Panel(PanelContribution {
            id: contribution(),
            title: "x".repeat(MAX_TEXT_LEN + 1),
            placement: PanelPlacement::Left,
            render: RenderSpec::Text,
            data_provider: false,
        });
        assert_eq!(
            bad.validate(),
            Err(ContributionError::TooLong { field: "title", max: MAX_TEXT_LEN })
        );
    }

    #[test]
    fn invalid_keybinding_is_rejected() {
        let bad = Contribution::Keybinding(KeybindingContribution {
            command: command(),
            key: String::new(),
            when: None,
        });
        assert!(matches!(bad.validate(), Err(ContributionError::InvalidKeySpec { .. })));
    }

    #[test]
    fn invalid_agent_schema_is_rejected() {
        let bad = Contribution::AgentTool(AgentToolContribution {
            id: "demo.tool".to_owned(),
            name: "demo".to_owned(),
            description: "Demo".to_owned(),
            parameters_schema: serde_json::json!([]),
            scope: AgentToolScope::Host,
        });
        assert_eq!(
            bad.validate(),
            Err(ContributionError::InvalidSchema { field: "parameters_schema" })
        );
    }
}
