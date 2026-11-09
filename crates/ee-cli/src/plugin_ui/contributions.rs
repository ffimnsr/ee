//! Typed parsing for the frontend plugin protocol.
//!
//! The frontend never reads manifests, binaries, Wasm modules, or plugin
//! config. It only consumes the declarative wire methods defined by the v2
//! design: `plugin_contributions`, `plugin_ui_update`, `plugin_status`, and
//! `plugin_health`.
//!
//! A contribution batch is parsed and validated completely before any registry
//! mutation happens. Unsupported contribution kinds are reported and skipped;
//! malformed payloads reject the whole batch so the previous registry remains
//! authoritative.

use std::collections::HashSet;
use std::fmt;

use ee_plugin_api::contribution::{Contribution, ContributionError, MAX_KEY_LEN, StatusAlignment};
use ee_plugin_api::ids::ContributionId;
use serde_json::Value;

use super::sanitize::{
    MAX_ITEMS, SanitizeError, sanitize_column_count, sanitize_item_count, sanitize_multiline,
    sanitize_non_empty, sanitize_text,
};

/// Frontend-facing contribution kinds this client can render.
pub const SUPPORTED_CONTRIBUTION_KINDS: &[&str] = &[
    "command",
    "ex_command",
    "keybinding",
    "status_item",
    "panel",
    "overlay",
    "picker",
    "menu_item",
];

/// One diagnostic produced while ingesting plugin protocol data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginDiagnostic {
    UnsupportedKind { kind: String, id: Option<String> },
    InvalidPayload(String),
    Health { plugin: String, state: String, reason: Option<String> },
}

impl PluginDiagnostic {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::UnsupportedKind { kind, id } => match id {
                Some(id) => format!("plugin contribution `{id}` has unsupported kind `{kind}`"),
                None => format!("plugin contribution has unsupported kind `{kind}`"),
            },
            Self::InvalidPayload(message) => format!("plugin payload rejected: {message}"),
            Self::Health { plugin, state, reason } => match reason {
                Some(reason) => format!("plugin {plugin}: {state} ({reason})"),
                None => format!("plugin {plugin}: {state}"),
            },
        }
    }
}

impl fmt::Display for PluginDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

/// Parse or validation failure for one wire payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    MissingField(&'static str),
    InvalidField { field: &'static str, reason: String },
    DuplicateId { kind: &'static str, id: String },
    UnsupportedKind(String),
    UnsupportedHost(String),
    Contribution(ContributionError),
    Sanitize(SanitizeError),
    Json(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingField(field) => write!(f, "missing field `{field}`"),
            Self::InvalidField { field, reason } => write!(f, "invalid `{field}`: {reason}"),
            Self::DuplicateId { kind, id } => {
                write!(f, "duplicate {kind} contribution id `{id}`")
            }
            Self::UnsupportedKind(kind) => write!(f, "unsupported contribution kind `{kind}`"),
            Self::UnsupportedHost(host) => write!(f, "unsupported contribution host `{host}`"),
            Self::Contribution(error) => write!(f, "{error}"),
            Self::Sanitize(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<ContributionError> for ProtocolError {
    fn from(value: ContributionError) -> Self {
        Self::Contribution(value)
    }
}

impl From<SanitizeError> for ProtocolError {
    fn from(value: SanitizeError) -> Self {
        Self::Sanitize(value)
    }
}

fn json_error(error: serde_json::Error) -> ProtocolError {
    ProtocolError::Json(error.to_string())
}

/// One atomic revisioned contribution batch.
#[derive(Clone, Debug, PartialEq)]
pub struct ContributionBatch {
    pub revision: u64,
    pub upsert: Vec<Contribution>,
    pub remove: Vec<ContributionId>,
    /// Non-fatal ingest diagnostics (unsupported kinds) retained for display.
    pub diagnostics: Vec<PluginDiagnostic>,
}

impl ContributionBatch {
    /// Parse `plugin_contributions` params.
    pub fn parse(params: &Value) -> Result<Self, ProtocolError> {
        if let Some(host) = params.get("host")
            && let Some(host) = host.as_str()
            && host != "editor"
        {
            return Err(ProtocolError::UnsupportedHost(host.to_owned()));
        }

        let revision = params
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or(ProtocolError::MissingField("revision"))?;

        let remove_values =
            params.get("remove").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
        let remove =
            serde_json::from_value::<Vec<ContributionId>>(remove_values).map_err(json_error)?;

        let upsert_values =
            params.get("upsert").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
        let upsert_values = upsert_values.as_array().ok_or(ProtocolError::InvalidField {
            field: "upsert",
            reason: String::from("expected an array"),
        })?;

        let mut diagnostics = Vec::new();
        let mut upsert = Vec::with_capacity(upsert_values.len());
        let mut seen = HashSet::new();

        for raw in upsert_values {
            let kind = raw
                .get("kind")
                .and_then(Value::as_str)
                .ok_or(ProtocolError::MissingField("kind"))?;
            let id = contribution_raw_id(raw);
            if !SUPPORTED_CONTRIBUTION_KINDS.contains(&kind) {
                diagnostics.push(PluginDiagnostic::UnsupportedKind { kind: kind.to_owned(), id });
                continue;
            }
            let contribution =
                serde_json::from_value::<Contribution>(raw.clone()).map_err(json_error)?;
            contribution.validate()?;
            validate_frontend_contribution(&contribution)?;
            let key = (kind.to_owned(), contribution.id().into_string());
            if !seen.insert(key) {
                return Err(ProtocolError::DuplicateId {
                    kind: contribution_kind_name(&contribution),
                    id: contribution.id().into_string(),
                });
            }
            upsert.push(contribution);
        }

        if upsert.len() > MAX_ITEMS {
            return Err(ProtocolError::Sanitize(SanitizeError::TooManyItems {
                field: "upsert",
                limit: MAX_ITEMS,
            }));
        }
        if remove.len() > MAX_ITEMS {
            return Err(ProtocolError::Sanitize(SanitizeError::TooManyItems {
                field: "remove",
                limit: MAX_ITEMS,
            }));
        }

        Ok(Self { revision, upsert, remove, diagnostics })
    }
}

fn contribution_raw_id(raw: &Value) -> Option<String> {
    for key in ["id", "key", "name", "command", "section", "label"] {
        if let Some(id) = raw.get(key).and_then(Value::as_str) {
            return Some(id.to_owned());
        }
    }
    None
}

fn contribution_kind_name(contribution: &Contribution) -> &'static str {
    contribution_kind(contribution)
}

/// Stable wire kind for a parsed contribution.
#[must_use]
pub fn contribution_kind(contribution: &Contribution) -> &'static str {
    match contribution {
        Contribution::Command(_) => "command",
        Contribution::ExCommand(_) => "ex_command",
        Contribution::Keybinding(_) => "keybinding",
        Contribution::StatusItem(_) => "status_item",
        Contribution::DecorationProvider(_) => "decoration_provider",
        Contribution::Panel(_) => "panel",
        Contribution::Overlay(_) => "overlay",
        Contribution::Picker(_) => "picker",
        Contribution::MenuItem(_) => "menu_item",
        Contribution::Language(_) => "language",
        Contribution::Formatter(_) => "formatter",
        Contribution::LspServer(_) => "lsp_server",
        Contribution::AgentTool(_) => "agent_tool",
        Contribution::ModelProvider(_) => "model_provider",
        Contribution::PolicyRule(_) => "policy_rule",
        Contribution::ContextProvider(_) => "context_provider",
        Contribution::ConfigSchema(_) => "config_schema",
    }
}

fn validate_frontend_contribution(contribution: &Contribution) -> Result<(), ProtocolError> {
    match contribution {
        Contribution::Command(command) => {
            sanitize_non_empty("command.title", &command.title)?;
            sanitize_text("command.description", &command.description)?;
        }
        Contribution::ExCommand(command) => {
            sanitize_non_empty("ex_command.name", &command.name)?;
            for alias in &command.aliases {
                sanitize_non_empty("ex_command.aliases", alias)?;
            }
        }
        Contribution::Keybinding(binding) => {
            if binding.key.len() > MAX_KEY_LEN {
                return Err(ProtocolError::Sanitize(SanitizeError::TooLong {
                    field: "keybinding.key",
                    limit: MAX_KEY_LEN,
                }));
            }
            sanitize_non_empty("keybinding.key", &binding.key)?;
        }
        Contribution::StatusItem(item) => {
            sanitize_non_empty("status_item.key", &item.key)?;
            sanitize_non_empty("status_item.text", &item.text)?;
        }
        Contribution::Panel(panel) => {
            sanitize_non_empty("panel.title", &panel.title)?;
        }
        Contribution::Picker(picker) => {
            sanitize_non_empty("picker.title", &picker.title)?;
            sanitize_column_count("picker.columns", picker.columns.len())?;
            for column in &picker.columns {
                sanitize_non_empty("picker.columns.key", &column.key)?;
                sanitize_non_empty("picker.columns.title", &column.title)?;
            }
        }
        Contribution::MenuItem(item) => {
            sanitize_non_empty("menu_item.label", &item.label)?;
        }
        _ => {}
    }
    Ok(())
}

/// Dynamic `plugin_ui_update` content.
#[derive(Clone, Debug, PartialEq)]
pub enum PluginUiUpdate {
    Status { key: String, text: String, alignment: StatusAlignment, priority: i32 },
    Panel { id: ContributionId, revision: u64, visible: bool, data: Value },
    Overlay { id: ContributionId, revision: u64, visible: bool, data: Value },
    Picker { id: ContributionId, revision: u64, items: Vec<PickerRow> },
}

impl PluginUiUpdate {
    /// Parse `plugin_ui_update` params. The payload is tagged by `kind`.
    pub fn parse(params: &Value) -> Result<Self, ProtocolError> {
        let kind = params
            .get("kind")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingField("kind"))?;
        match kind {
            "status" | "status_item" => {
                let key = params
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or(ProtocolError::MissingField("key"))?
                    .to_owned();
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(ProtocolError::MissingField("text"))?
                    .to_owned();
                sanitize_non_empty("status.key", &key)?;
                sanitize_non_empty("status.text", &text)?;
                let alignment = serde_json::from_value::<StatusAlignment>(
                    params.get("alignment").cloned().unwrap_or(Value::String(String::from("left"))),
                )
                .map_err(json_error)?;
                let priority = params.get("priority").and_then(Value::as_i64).unwrap_or(0) as i32;
                Ok(Self::Status { key, text, alignment, priority })
            }
            "panel" | "overlay" => {
                let id = parse_contribution_id(params)?;
                let revision = parse_revision(params)?;
                let visible = params.get("visible").and_then(Value::as_bool).unwrap_or(true);
                let data = params.get("data").cloned().unwrap_or(Value::Null);
                validate_dynamic_data(&data, 0)?;
                if kind == "panel" {
                    Ok(Self::Panel { id, revision, visible, data })
                } else {
                    Ok(Self::Overlay { id, revision, visible, data })
                }
            }
            "picker" => {
                let id = parse_contribution_id(params)?;
                let revision = parse_revision(params)?;
                let raw_items = params
                    .get("items")
                    .and_then(Value::as_array)
                    .ok_or(ProtocolError::MissingField("items"))?;
                sanitize_item_count("picker.items", raw_items.len())?;
                let mut items = Vec::with_capacity(raw_items.len());
                for raw in raw_items {
                    validate_dynamic_data(raw, 1)?;
                    items.push(PickerRow::parse(raw));
                }
                Ok(Self::Picker { id, revision, items })
            }
            other => Err(ProtocolError::UnsupportedKind(other.to_owned())),
        }
    }
}

fn parse_contribution_id(params: &Value) -> Result<ContributionId, ProtocolError> {
    let id = params.get("id").and_then(Value::as_str).ok_or(ProtocolError::MissingField("id"))?;
    ContributionId::new(id)
        .map_err(|error| ProtocolError::InvalidField { field: "id", reason: error.to_string() })
}

fn parse_revision(params: &Value) -> Result<u64, ProtocolError> {
    params.get("revision").and_then(Value::as_u64).ok_or(ProtocolError::MissingField("revision"))
}

fn validate_dynamic_data(value: &Value, depth: usize) -> Result<(), ProtocolError> {
    if depth > 8 {
        return Err(ProtocolError::InvalidField {
            field: "data",
            reason: String::from("nesting is too deep"),
        });
    }
    match value {
        Value::String(text) => sanitize_multiline("data", text)?,
        Value::Array(items) => {
            sanitize_item_count("data.items", items.len())?;
            for item in items {
                validate_dynamic_data(item, depth + 1)?;
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                validate_dynamic_data(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// One dynamic picker row. The raw value is retained so column metadata can
/// select cells without the frontend inventing a schema.
#[derive(Clone, Debug, PartialEq)]
pub struct PickerRow {
    pub id: Option<String>,
    pub raw: Value,
}

impl PickerRow {
    #[must_use]
    pub fn parse(raw: &Value) -> Self {
        let id = raw
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| raw.get("value").and_then(Value::as_str))
            .map(str::to_owned);
        Self { id, raw: raw.clone() }
    }

    /// Display text for one column key, falling back to label/text/string.
    #[must_use]
    pub fn cell(&self, key: &str) -> String {
        if let Some(value) = self.raw.get(key) {
            return value_to_text(value);
        }
        self.label()
    }

    #[must_use]
    pub fn label(&self) -> String {
        match &self.raw {
            Value::String(text) => text.clone(),
            other => {
                for key in ["label", "text", "title", "name"] {
                    if let Some(value) = other.get(key) {
                        return value_to_text(value);
                    }
                }
                other.to_string()
            }
        }
    }

    /// Text used by local filtering.
    #[must_use]
    pub fn search_text(&self) -> String {
        match &self.raw {
            Value::String(text) => text.clone(),
            Value::Object(map) => {
                let mut parts = Vec::with_capacity(map.len());
                for value in map.values() {
                    parts.push(value_to_text(value));
                }
                parts.join(" ")
            }
            other => other.to_string(),
        }
    }
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One plugin entry from a `plugin_status` snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginStatusEntry {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub active: bool,
    pub health: String,
    pub reason: Option<String>,
}

/// Full `plugin_status` snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PluginStatusSnapshot {
    pub generation: u64,
    pub plugins: Vec<PluginStatusEntry>,
}

impl PluginStatusSnapshot {
    pub fn parse(params: &Value) -> Result<Self, ProtocolError> {
        let generation = params.get("generation").and_then(Value::as_u64).unwrap_or(0);
        let plugins = params
            .get("plugins")
            .and_then(Value::as_array)
            .ok_or(ProtocolError::MissingField("plugins"))?;
        sanitize_item_count("plugins", plugins.len())?;
        let parsed = plugins.iter().map(parse_status_entry).collect::<Result<Vec<_>, _>>()?;
        Ok(Self { generation, plugins: parsed })
    }

    /// Displayable diagnostics for unusual states.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<PluginDiagnostic> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.health != "loaded" && plugin.health != "reloaded")
            .map(|plugin| PluginDiagnostic::Health {
                plugin: plugin.name.clone(),
                state: plugin.health.clone(),
                reason: plugin.reason.clone(),
            })
            .collect()
    }
}

fn parse_status_entry(value: &Value) -> Result<PluginStatusEntry, ProtocolError> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::MissingField("plugins.name"))?
        .to_owned();
    sanitize_non_empty("plugins.name", &name)?;
    let version = value.get("version").and_then(Value::as_str).unwrap_or_default().to_owned();
    sanitize_text("plugins.version", &version)?;
    let kind = value.get("kind").and_then(Value::as_str).unwrap_or("unknown").to_owned();
    sanitize_non_empty("plugins.kind", &kind)?;
    let active = value.get("active").and_then(Value::as_bool).unwrap_or(false);
    let health = value
        .get("health")
        .and_then(|health| health.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("loaded")
        .to_owned();
    sanitize_non_empty("plugins.health.state", &health)?;
    let reason = value
        .get("health")
        .and_then(|health| health.get("reason"))
        .filter(|reason| !reason.is_null())
        .map(|reason| match reason {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
    if let Some(reason) = &reason {
        sanitize_multiline("plugins.health.reason", reason)?;
    }
    Ok(PluginStatusEntry { name, version, kind, active, health, reason })
}

/// One `plugin_health` transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginHealthEvent {
    pub plugin: String,
    pub state: String,
    pub reason: Option<String>,
}

impl PluginHealthEvent {
    pub fn parse(params: &Value) -> Result<Self, ProtocolError> {
        let plugin = params
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingField("plugin"))?
            .to_owned();
        let state = params
            .get("state")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingField("state"))?
            .to_owned();
        sanitize_non_empty("health.plugin", &plugin)?;
        sanitize_non_empty("health.state", &state)?;
        let reason =
            params.get("reason").filter(|reason| !reason.is_null()).map(|reason| match reason {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            });
        if let Some(reason) = &reason {
            sanitize_multiline("health.reason", reason)?;
        }
        Ok(Self { plugin, state, reason })
    }

    #[must_use]
    pub fn diagnostic(&self) -> PluginDiagnostic {
        PluginDiagnostic::Health {
            plugin: self.plugin.clone(),
            state: self.state.clone(),
            reason: self.reason.clone(),
        }
    }
}
