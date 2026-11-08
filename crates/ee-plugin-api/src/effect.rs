//! Effects returned by plugin commands and listeners.
//!
//! Plugins never mutate editor state directly. They return effects, and the host
//! applies them on the core thread in deterministic order. This keeps the core
//! borrow model intact and makes edits revision-checked and undo-group aware.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::PluginError;
use crate::ids::{CommandId, ContributionId};
use crate::model::{EditRequest, ViewId};

/// Semantic style role, for example `ui.status.text` or `syntax.keyword`.
///
/// Contributions never carry RGB values or ANSI escapes; the frontend owns the
/// concrete theme mapping.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct StyleRole(String);

impl StyleRole {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Key for a mutable status item.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct StatusItemKey(String);

impl StatusItemKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Selection update request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SelectionRequest {
    pub view_id: ViewId,
    pub rev: u64,
    pub selections: Vec<crate::model::SelectionRange>,
}

/// Scroll request; the frontend owns final placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScrollRequest {
    pub view_id: ViewId,
    pub line: usize,
    pub column: usize,
}

/// Status item supplied by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusItem {
    pub key: StatusItemKey,
    pub text: String,
    pub alignment: crate::contribution::StatusAlignment,
    pub priority: i32,
}

/// One decorated range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DecorationSpan {
    pub start: usize,
    pub end: usize,
    pub style: StyleRole,
    #[serde(default)]
    pub text: Option<String>,
}

/// Revisioned decoration patch for one view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DecorationPatch {
    pub view_id: ViewId,
    pub rev: u64,
    pub decoration_kind: String,
    pub spans: Vec<DecorationSpan>,
}

/// Dynamic panel content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PanelUpdate {
    pub id: ContributionId,
    pub revision: u64,
    pub visible: bool,
    #[schemars(skip)]
    pub data: Value,
}

/// Dynamic overlay content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OverlayUpdate {
    pub id: ContributionId,
    pub revision: u64,
    pub visible: bool,
    #[schemars(skip)]
    pub data: Value,
}

/// Dynamic picker content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PickerUpdate {
    pub id: ContributionId,
    pub revision: u64,
    #[schemars(skip)]
    pub items: Vec<Value>,
}

/// Background task request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct TaskRequest {
    pub task_kind: String,
    #[schemars(skip)]
    pub payload: Value,
    pub timeout_ms: u64,
}

/// Result emitted by an agent tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentToolResult {
    pub call_id: String,
    pub is_error: bool,
    #[schemars(skip)]
    pub content: Value,
}

/// A state change requested by a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    ApplyEdit(EditRequest),
    SetSelection(SelectionRequest),
    ScrollTo(ScrollRequest),
    SetStatus(StatusItem),
    RemoveStatus { key: StatusItemKey },
    SetDecorations(DecorationPatch),
    ShowPanel(PanelUpdate),
    ShowOverlay(OverlayUpdate),
    ShowPicker(PickerUpdate),
    RunTask(TaskRequest),
    EmitAgentToolResult(AgentToolResult),
    Noop,
}

/// Declarative command registered by a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommandSpec {
    pub id: CommandId,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: Option<String>,
    /// Whether the command may mutate document state. The host rejects such a
    /// command before dispatch when the target view is a VLF buffer.
    #[serde(default)]
    pub mutates_document: bool,
    #[serde(default)]
    #[schemars(skip)]
    pub args_schema: Option<Value>,
}

/// Invocation delivered to a command handler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommandRequest {
    pub id: CommandId,
    pub view_id: ViewId,
    pub request_id: u64,
    #[schemars(skip)]
    pub args: Value,
}

/// Handler installed for one command id.
pub trait CommandHandler: Send + Sync + 'static {
    fn invoke(&self, request: CommandRequest) -> Result<Vec<Effect>, PluginError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit_request() -> EditRequest {
        EditRequest {
            view_id: ViewId(1),
            rev: 1,
            edits: Vec::new(),
            priority: 0,
            after_cursor: false,
            undo_group: None,
            author: "plugin:demo".to_owned(),
        }
    }

    fn panel_id() -> ContributionId {
        ContributionId::new("demo.panel").unwrap()
    }

    fn every_variant() -> Vec<Effect> {
        vec![
            Effect::ApplyEdit(edit_request()),
            Effect::SetSelection(SelectionRequest {
                view_id: ViewId(1),
                rev: 1,
                selections: Vec::new(),
            }),
            Effect::ScrollTo(ScrollRequest { view_id: ViewId(1), line: 2, column: 0 }),
            Effect::SetStatus(StatusItem {
                key: StatusItemKey::new("demo.status"),
                text: "ok".to_owned(),
                alignment: crate::contribution::StatusAlignment::Right,
                priority: 1,
            }),
            Effect::RemoveStatus { key: StatusItemKey::new("demo.status") },
            Effect::SetDecorations(DecorationPatch {
                view_id: ViewId(1),
                rev: 2,
                decoration_kind: "diagnostic".to_owned(),
                spans: vec![DecorationSpan {
                    start: 0,
                    end: 1,
                    style: StyleRole::new("syntax.keyword"),
                    text: None,
                }],
            }),
            Effect::ShowPanel(PanelUpdate {
                id: panel_id(),
                revision: 1,
                visible: true,
                data: serde_json::json!({}),
            }),
            Effect::ShowOverlay(OverlayUpdate {
                id: panel_id(),
                revision: 1,
                visible: false,
                data: serde_json::json!({}),
            }),
            Effect::ShowPicker(PickerUpdate { id: panel_id(), revision: 1, items: Vec::new() }),
            Effect::RunTask(TaskRequest {
                task_kind: "demo".to_owned(),
                payload: serde_json::json!({}),
                timeout_ms: 1000,
            }),
            Effect::EmitAgentToolResult(AgentToolResult {
                call_id: "call-1".to_owned(),
                is_error: false,
                content: serde_json::json!({}),
            }),
            Effect::Noop,
        ]
    }

    #[test]
    fn every_effect_variant_round_trips_json() {
        for effect in every_variant() {
            let value = serde_json::to_value(&effect).unwrap();
            assert!(value.get("kind").is_some());
            let decoded: Effect = serde_json::from_value(value).unwrap();
            assert_eq!(decoded, effect);
        }
    }

    #[test]
    fn noop_serializes_to_tag_only() {
        assert_eq!(
            serde_json::to_value(Effect::Noop).unwrap(),
            serde_json::json!({"kind": "noop"})
        );
    }

    #[test]
    fn style_roles_are_transparent_strings() {
        let role = StyleRole::new("ui.status.text");
        assert_eq!(serde_json::to_value(&role).unwrap(), serde_json::json!("ui.status.text"));
    }

    #[test]
    fn command_request_round_trips() {
        let request = CommandRequest {
            id: CommandId::new("demo.run").unwrap(),
            view_id: ViewId(2),
            request_id: 9,
            args: serde_json::json!({"value": 1}),
        };
        let value = serde_json::to_value(&request).unwrap();
        let decoded: CommandRequest = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, request);
    }
}
