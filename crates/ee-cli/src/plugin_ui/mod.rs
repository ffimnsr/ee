//! Frontend plugin UI state.
//!
//! `ee-cli` is a renderer, not a plugin host. This module holds only the
//! declarative state the backend sends: contribution descriptors, dynamic
//! content, status/health snapshots, and the local picker selection. It never
//! loads plugin manifests, binaries, Wasm modules, or config.

use std::collections::BTreeMap;

pub(crate) mod contributions;
pub(crate) mod keybindings;
pub(crate) mod picker;
pub(crate) mod protocol;
pub(crate) mod registry;
pub(crate) mod render;
pub(crate) mod sanitize;

pub(crate) use contributions::{
    ContributionBatch, PluginDiagnostic, PluginHealthEvent, PluginStatusSnapshot, PluginUiUpdate,
    ProtocolError,
};
pub(crate) use picker::PluginPickerState;
pub(crate) use protocol::{FrontendCapabilities, PluginUiEventKind};
pub(crate) use registry::{ApplyReport, ContributionRegistry, RegistryError};

use ee_plugin_api::contribution::StatusAlignment;
use ee_plugin_api::ids::{CommandId, ContributionId};
use serde_json::Value;

use crate::keymap::{Action, BindingKey};

/// Maximum diagnostics retained for display.
const MAX_DIAGNOSTICS: usize = 128;

/// Rendered dynamic panel/overlay content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DynamicContent {
    pub(crate) revision: u64,
    pub(crate) lines: Vec<String>,
}

/// One dynamic status value that overrides its static descriptor text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DynamicStatus {
    pub(crate) text: String,
    pub(crate) alignment: StatusAlignment,
    pub(crate) priority: i32,
}

/// Frontend state for all plugin contributions.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PluginUiState {
    pub(crate) registry: ContributionRegistry,
    pub(crate) capabilities: FrontendCapabilities,
    pub(crate) status: PluginStatusSnapshot,
    pub(crate) health: BTreeMap<String, PluginHealthEvent>,
    diagnostics: Vec<PluginDiagnostic>,
    pub(crate) panel_content: BTreeMap<ContributionId, DynamicContent>,
    pub(crate) overlay_content: BTreeMap<ContributionId, DynamicContent>,
    status_overrides: BTreeMap<String, DynamicStatus>,
    pub(crate) picker: Option<PluginPickerState>,
}

impl Default for PluginUiState {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginUiState {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            registry: ContributionRegistry::default(),
            capabilities: FrontendCapabilities::ee_cli(),
            status: PluginStatusSnapshot::default(),
            health: BTreeMap::new(),
            diagnostics: Vec::new(),
            panel_content: BTreeMap::new(),
            overlay_content: BTreeMap::new(),
            status_overrides: BTreeMap::new(),
            picker: None,
        }
    }

    /// Apply one atomic batch. On failure the previous registry is retained and
    /// the failure is recorded as a diagnostic.
    pub(crate) fn apply_batch(
        &mut self,
        batch: &ContributionBatch,
    ) -> Result<ApplyReport, RegistryError> {
        match self.registry.apply(batch) {
            Ok(report) => {
                for diagnostic in &report.diagnostics {
                    self.push_diagnostic(diagnostic.clone());
                }
                for id in &report.removed {
                    self.panel_content.remove(id);
                    self.overlay_content.remove(id);
                    self.status_overrides.remove(id.as_str());
                    if self.picker.as_ref().is_some_and(|picker| picker.id == *id) {
                        self.picker = None;
                    }
                }
                Ok(report)
            }
            Err(error) => {
                self.push_diagnostic(PluginDiagnostic::InvalidPayload(error.to_string()));
                Err(error)
            }
        }
    }

    /// Apply one dynamic `plugin_ui_update`.
    pub(crate) fn apply_ui_update(&mut self, update: &PluginUiUpdate) -> Result<(), ProtocolError> {
        match update {
            PluginUiUpdate::Status { key, text, alignment, priority } => {
                if self.registry.status_item(key).is_none() {
                    return Err(ProtocolError::InvalidField {
                        field: "status",
                        reason: format!("unknown status item `{key}`"),
                    });
                }
                self.status_overrides.insert(
                    key.clone(),
                    DynamicStatus {
                        text: text.clone(),
                        alignment: *alignment,
                        priority: *priority,
                    },
                );
            }
            PluginUiUpdate::Panel { id, revision, visible, data } => {
                let Some(panel) = self.registry.panel(id) else {
                    return Err(ProtocolError::InvalidField {
                        field: "panel",
                        reason: format!("unknown panel `{id}`"),
                    });
                };
                if !self.capabilities.supports_renderer(render_name(&panel.render)) {
                    return Err(ProtocolError::InvalidField {
                        field: "panel",
                        reason: format!("unsupported renderer for `{id}`"),
                    });
                }
                if *visible {
                    self.panel_content.insert(
                        id.clone(),
                        DynamicContent { revision: *revision, lines: content_lines(data) },
                    );
                } else {
                    self.panel_content.remove(id);
                }
            }
            PluginUiUpdate::Overlay { id, revision, visible, data } => {
                let Some(overlay) = self.registry.overlay(id) else {
                    return Err(ProtocolError::InvalidField {
                        field: "overlay",
                        reason: format!("unknown overlay `{id}`"),
                    });
                };
                if !self.capabilities.supports_renderer(render_name(&overlay.render)) {
                    return Err(ProtocolError::InvalidField {
                        field: "overlay",
                        reason: format!("unsupported renderer for `{id}`"),
                    });
                }
                if *visible {
                    self.overlay_content.insert(
                        id.clone(),
                        DynamicContent { revision: *revision, lines: content_lines(data) },
                    );
                } else {
                    self.overlay_content.remove(id);
                }
            }
            PluginUiUpdate::Picker { id, revision, items } => {
                let Some(descriptor) = self.registry.picker(id) else {
                    return Err(ProtocolError::InvalidField {
                        field: "picker",
                        reason: format!("unknown picker `{id}`"),
                    });
                };
                if let Some(active) = self.picker.as_mut()
                    && active.id == *id
                {
                    if !active.apply_update(*revision, items.clone()) {
                        return Err(ProtocolError::InvalidField {
                            field: "picker.revision",
                            reason: format!("stale picker update {revision}"),
                        });
                    }
                    return Ok(());
                }
                self.picker = Some(PluginPickerState::new(
                    id.clone(),
                    descriptor.title.clone(),
                    descriptor.columns.clone(),
                    descriptor.filter_mode,
                    *revision,
                    items.clone(),
                ));
            }
        }
        Ok(())
    }

    /// Replace the full plugin status snapshot.
    pub(crate) fn apply_status(&mut self, status: PluginStatusSnapshot) {
        for diagnostic in status.diagnostics() {
            self.push_diagnostic(diagnostic);
        }
        self.health.clear();
        for plugin in &status.plugins {
            self.health.insert(
                plugin.name.clone(),
                PluginHealthEvent {
                    plugin: plugin.name.clone(),
                    state: plugin.health.clone(),
                    reason: plugin.reason.clone(),
                },
            );
        }
        self.status = status;
    }

    /// Record one health transition.
    pub(crate) fn apply_health(&mut self, event: PluginHealthEvent) {
        self.push_diagnostic(event.diagnostic());
        self.health.insert(event.plugin.clone(), event);
    }

    pub(crate) fn push_diagnostic(&mut self, diagnostic: PluginDiagnostic) {
        if self.diagnostics.contains(&diagnostic) {
            return;
        }
        if self.diagnostics.len() >= MAX_DIAGNOSTICS {
            self.diagnostics.remove(0);
        }
        self.diagnostics.push(diagnostic);
    }

    #[must_use]
    pub(crate) fn diagnostics(&self) -> &[PluginDiagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub(crate) fn status(&self) -> &PluginStatusSnapshot {
        &self.status
    }

    /// Active plugin commands for palette rows.
    pub(crate) fn commands(
        &self,
    ) -> impl Iterator<Item = &ee_plugin_api::contribution::CommandContribution> {
        self.registry.commands()
    }

    #[must_use]
    pub(crate) fn has_command(&self, id: &str) -> bool {
        self.registry.has_command(id)
    }

    #[must_use]
    pub(crate) fn resolve_ex_alias(&self, alias: &str) -> Option<&CommandId> {
        self.registry.resolve_ex_alias(alias)
    }

    /// Action for a plugin default binding.
    #[must_use]
    pub(crate) fn keybinding_action(&self, binding: &BindingKey) -> Option<Action> {
        self.registry.keybinding_action(binding)
    }

    /// Status items merged with dynamic overrides, ordered by priority and key.
    #[must_use]
    pub(crate) fn status_items(&self) -> Vec<(String, String, StatusAlignment, i32)> {
        let mut merged: BTreeMap<String, (String, StatusAlignment, i32)> = BTreeMap::new();
        for item in self.registry.status_items() {
            merged.insert(item.key.clone(), (item.text.clone(), item.alignment, item.priority));
        }
        for (key, dynamic) in &self.status_overrides {
            merged.insert(key.clone(), (dynamic.text.clone(), dynamic.alignment, dynamic.priority));
        }
        let mut items = merged
            .into_iter()
            .map(|(key, (text, alignment, priority))| (key, text, alignment, priority))
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            left.3
                .cmp(&right.3)
                .then_with(|| left.0.cmp(&right.0))
                .then_with(|| left.1.cmp(&right.1))
        });
        items
    }

    /// Panels that currently have dynamic content or static renderers.
    #[must_use]
    pub(crate) fn visible_panels(&self) -> Vec<VisiblePanel<'_>> {
        self.registry
            .panels()
            .map(|panel| VisiblePanel { panel, content: self.panel_content.get(&panel.id) })
            .collect()
    }

    #[must_use]
    pub(crate) fn visible_overlays(&self) -> Vec<VisibleOverlay<'_>> {
        self.registry
            .overlays()
            .map(|overlay| VisibleOverlay {
                overlay,
                content: self.overlay_content.get(&overlay.id),
            })
            .collect()
    }

    /// Remove a dynamic picker and return its id for ui-event routing.
    pub(crate) fn close_picker(&mut self) -> Option<ContributionId> {
        self.picker.take().map(|picker| picker.id)
    }
}

/// One panel plus any validated dynamic content.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VisiblePanel<'a> {
    pub(crate) panel: &'a ee_plugin_api::contribution::PanelContribution,
    pub(crate) content: Option<&'a DynamicContent>,
}

/// One overlay plus any validated dynamic content.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VisibleOverlay<'a> {
    pub(crate) overlay: &'a ee_plugin_api::contribution::OverlayContribution,
    pub(crate) content: Option<&'a DynamicContent>,
}

fn render_name(render: &ee_plugin_api::contribution::RenderSpec) -> &'static str {
    match render {
        ee_plugin_api::contribution::RenderSpec::List { .. } => "list",
        ee_plugin_api::contribution::RenderSpec::Text => "text",
        ee_plugin_api::contribution::RenderSpec::Markdown => "markdown",
    }
}

/// Convert bounded dynamic data into display lines.
#[must_use]
pub(crate) fn content_lines(data: &Value) -> Vec<String> {
    let mut lines = content_lines_unbounded(data);
    lines.truncate(sanitize::MAX_LINES);
    lines
}

fn content_lines_unbounded(data: &Value) -> Vec<String> {
    match data {
        Value::Null => Vec::new(),
        Value::String(text) => text.lines().map(str::to_owned).collect(),
        Value::Array(items) => items.iter().map(item_label).collect(),
        Value::Object(map) => {
            for key in ["markdown", "text", "content"] {
                if let Some(Value::String(text)) = map.get(key) {
                    return text.lines().map(str::to_owned).collect();
                }
            }
            for key in ["lines", "items", "rows"] {
                if let Some(Value::Array(items)) = map.get(key) {
                    return items.iter().map(item_label).collect();
                }
            }
            map.iter().map(|(key, value)| format!("{key}: {}", item_label(value))).collect()
        }
        other => vec![other.to_string()],
    }
}

fn item_label(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => {
            for key in ["label", "text", "title", "name"] {
                if let Some(value) = map.get(key) {
                    return value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
                }
            }
            map.iter()
                .map(|(key, value)| {
                    format!(
                        "{key}: {}",
                        value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string())
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_ui::contributions::ContributionBatch;

    #[test]
    fn dynamic_content_understands_text_list_and_object_shapes() {
        assert_eq!(content_lines(&serde_json::json!("one\ntwo")), vec!["one", "two"]);
        assert_eq!(content_lines(&serde_json::json!(["a", "b"])), vec!["a", "b"]);
        assert_eq!(
            content_lines(&serde_json::json!({"items": ["a", {"label": "b"}]})),
            vec!["a", "b"]
        );
    }

    #[test]
    fn unsupported_kind_is_reported_not_registered() {
        let batch = ContributionBatch::parse(&serde_json::json!({
            "revision": 1,
            "upsert": [{"kind": "widget", "id": "demo.widget"}]
        }))
        .unwrap();
        let mut state = PluginUiState::new();
        state.apply_batch(&batch).unwrap();
        assert_eq!(state.registry.revision(), 1);
        assert_eq!(state.diagnostics().len(), 1);
    }

    #[test]
    fn removed_contribution_drops_dynamic_content() {
        let mut state = PluginUiState::new();
        let add = ContributionBatch::parse(&serde_json::json!({
            "revision": 1,
            "upsert": [{
                "kind": "panel",
                "id": "demo.panel",
                "title": "Demo",
                "placement": "right",
                "render": {"type": "text"}
            }]
        }))
        .unwrap();
        state.apply_batch(&add).unwrap();
        let update = PluginUiUpdate::parse(&serde_json::json!({
            "kind": "panel",
            "id": "demo.panel",
            "revision": 1,
            "visible": true,
            "data": {"text": "hello"}
        }))
        .unwrap();
        state.apply_ui_update(&update).unwrap();
        assert_eq!(state.panel_content.len(), 1);

        let remove = ContributionBatch::parse(&serde_json::json!({
            "revision": 2,
            "remove": ["demo.panel"]
        }))
        .unwrap();
        state.apply_batch(&remove).unwrap();
        assert!(state.panel_content.is_empty());
    }
}
