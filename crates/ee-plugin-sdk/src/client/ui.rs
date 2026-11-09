//! `editor.ui` effect helpers.

use std::sync::Arc;

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::effect::{
    Effect, OverlayUpdate, PanelUpdate, PickerUpdate, StatusItem, StatusItemKey,
};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::ContributionId;
use ee_plugin_api::model::ViewId;
use serde_json::Value;

/// Declarative UI effect client.
#[derive(Clone)]
pub struct UiClient {
    caller: Arc<dyn ServiceCaller>,
}

impl UiClient {
    pub fn new(caller: Arc<dyn ServiceCaller>) -> Self {
        Self { caller }
    }

    pub fn submit(&self, effects: Vec<Effect>) -> Result<(), PluginError> {
        self.caller.submit_effects(effects)
    }

    pub fn set_status(&self, item: StatusItem) -> Result<(), PluginError> {
        self.submit(vec![Effect::SetStatus(item)])
    }

    pub fn remove_status(&self, key: impl Into<String>) -> Result<(), PluginError> {
        self.submit(vec![Effect::RemoveStatus { key: StatusItemKey::new(key) }])
    }

    pub fn show_panel(
        &self,
        id: ContributionId,
        revision: u64,
        visible: bool,
        data: Value,
    ) -> Result<(), PluginError> {
        self.submit(vec![Effect::ShowPanel(PanelUpdate { id, revision, visible, data })])
    }

    pub fn show_overlay(
        &self,
        id: ContributionId,
        revision: u64,
        visible: bool,
        data: Value,
    ) -> Result<(), PluginError> {
        self.submit(vec![Effect::ShowOverlay(OverlayUpdate { id, revision, visible, data })])
    }

    pub fn show_picker(
        &self,
        id: ContributionId,
        revision: u64,
        items: Vec<Value>,
    ) -> Result<(), PluginError> {
        self.submit(vec![Effect::ShowPicker(PickerUpdate { id, revision, items })])
    }

    /// Convenience for a per-view status key.
    pub fn status_key(prefix: &str, view: ViewId) -> String {
        format!("{prefix}:{}", view.0)
    }
}
