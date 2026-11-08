//! `editor.ui` service.
//!
//! UI is declarative: plugins publish status items, panels, overlays, pickers,
//! and menu entries. Dynamic content is requested through a bounded provider
//! deadline; a slow provider keeps its last good payload.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::effect::{OverlayUpdate, PanelUpdate, PickerUpdate};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::ContributionId;
use ee_plugin_api::model::ViewId;
use ee_plugin_api::service::Service;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default provider deadline.
pub const DEFAULT_RENDER_TIMEOUT: Duration = Duration::from_millis(50);
/// Maximum serialized dynamic UI payload.
pub const MAX_UI_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Request passed to a dynamic UI provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiRenderRequest {
    pub view_id: ViewId,
    pub id: ContributionId,
    pub revision: u64,
}

/// Result of one render attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiRenderResult {
    pub data: Value,
    pub stale: bool,
    pub error: Option<String>,
}

/// Dynamic content provider.
pub trait UiProvider: Send + Sync + 'static {
    fn render(&self, request: &UiRenderRequest) -> Result<Value, PluginError>;
}

struct ProviderEntry {
    provider: Arc<dyn UiProvider>,
}

impl Clone for ProviderEntry {
    fn clone(&self) -> Self {
        Self { provider: Arc::clone(&self.provider) }
    }
}

/// Declarative UI state and dynamic providers.
#[derive(Clone)]
pub struct UiService {
    providers: Arc<RwLock<BTreeMap<ContributionId, ProviderEntry>>>,
    panels: Arc<Mutex<BTreeMap<ContributionId, PanelUpdate>>>,
    overlays: Arc<Mutex<BTreeMap<ContributionId, OverlayUpdate>>>,
    pickers: Arc<Mutex<BTreeMap<ContributionId, PickerUpdate>>>,
    last_good: Arc<Mutex<BTreeMap<ContributionId, Value>>>,
    timeout: Duration,
}

impl Default for UiService {
    fn default() -> Self {
        Self::new()
    }
}

impl UiService {
    pub fn new() -> Self {
        Self {
            providers: Arc::new(RwLock::new(BTreeMap::new())),
            panels: Arc::new(Mutex::new(BTreeMap::new())),
            overlays: Arc::new(Mutex::new(BTreeMap::new())),
            pickers: Arc::new(Mutex::new(BTreeMap::new())),
            last_good: Arc::new(Mutex::new(BTreeMap::new())),
            timeout: DEFAULT_RENDER_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn register_provider(&self, id: ContributionId, provider: Arc<dyn UiProvider>) -> Disposer {
        let providers = Arc::clone(&self.providers);
        {
            let mut providers = providers.write().expect("ui provider lock poisoned");
            providers.insert(id.clone(), ProviderEntry { provider: Arc::clone(&provider) });
        }
        Disposer::new(move || {
            if let Ok(mut providers) = providers.write()
                && providers.get(&id).is_some_and(|entry| Arc::ptr_eq(&entry.provider, &provider))
            {
                providers.remove(&id);
            }
        })
    }

    pub fn has_provider(&self, id: &ContributionId) -> bool {
        self.providers.read().expect("ui provider lock poisoned").contains_key(id)
    }

    /// Ask a provider for bounded content. A failure keeps the last good value.
    pub fn render(
        &self,
        id: &ContributionId,
        view: ViewId,
        revision: u64,
    ) -> Result<UiRenderResult, PluginError> {
        let provider = self
            .providers
            .read()
            .expect("ui provider lock poisoned")
            .get(id)
            .cloned()
            .ok_or(PluginError::Unsupported { feature: "ui provider" })?;
        let request = UiRenderRequest { view_id: view, id: id.clone(), revision };

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name(format!("ee-ui-render-{id}"))
            .spawn(move || {
                let _ = sender.send(provider.provider.render(&request));
            })
            .map_err(|error| PluginError::Runtime {
                message: format!("cannot start UI render worker: {error}"),
            })?;

        let started = Instant::now();
        match receiver.recv_timeout(self.timeout) {
            Ok(Ok(data)) => {
                let bytes = serde_json::to_vec(&data).map(|bytes| bytes.len()).unwrap_or(0);
                if bytes > MAX_UI_PAYLOAD_BYTES {
                    return self.stale_result(
                        id,
                        PluginError::Resource {
                            message: format!("UI payload exceeds {MAX_UI_PAYLOAD_BYTES} bytes"),
                        },
                    );
                }
                self.last_good
                    .lock()
                    .expect("ui last-good lock poisoned")
                    .insert(id.clone(), data.clone());
                Ok(UiRenderResult { data, stale: false, error: None })
            }
            Ok(Err(error)) => self.stale_result(id, error),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => self.stale_result(
                id,
                PluginError::Runtime {
                    message: format!(
                        "UI provider timed out after {} ms",
                        started.elapsed().as_millis()
                    ),
                },
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => self.stale_result(
                id,
                PluginError::Runtime { message: "UI provider worker exited".to_owned() },
            ),
        }
    }

    fn stale_result(
        &self,
        id: &ContributionId,
        error: PluginError,
    ) -> Result<UiRenderResult, PluginError> {
        let data = self
            .last_good
            .lock()
            .expect("ui last-good lock poisoned")
            .get(id)
            .cloned()
            .unwrap_or(Value::Null);
        Ok(UiRenderResult { data, stale: true, error: Some(error.to_string()) })
    }

    pub fn last_good(&self, id: &ContributionId) -> Option<Value> {
        self.last_good.lock().expect("ui last-good lock poisoned").get(id).cloned()
    }

    pub fn apply_panel(&self, update: PanelUpdate) -> Result<(), PluginError> {
        validate_id(&update.id)?;
        let mut panels = self.panels.lock().expect("ui panel lock poisoned");
        if let Some(current) = panels.get(&update.id)
            && update.revision < current.revision
        {
            return Err(PluginError::Runtime {
                message: format!("stale panel revision {}", update.revision),
            });
        }
        panels.insert(update.id.clone(), update);
        Ok(())
    }

    pub fn apply_overlay(&self, update: OverlayUpdate) -> Result<(), PluginError> {
        validate_id(&update.id)?;
        let mut overlays = self.overlays.lock().expect("ui overlay lock poisoned");
        if let Some(current) = overlays.get(&update.id)
            && update.revision < current.revision
        {
            return Err(PluginError::Runtime {
                message: format!("stale overlay revision {}", update.revision),
            });
        }
        overlays.insert(update.id.clone(), update);
        Ok(())
    }

    pub fn apply_picker(&self, update: PickerUpdate) -> Result<(), PluginError> {
        validate_id(&update.id)?;
        let mut pickers = self.pickers.lock().expect("ui picker lock poisoned");
        if let Some(current) = pickers.get(&update.id)
            && update.revision < current.revision
        {
            return Err(PluginError::Runtime {
                message: format!("stale picker revision {}", update.revision),
            });
        }
        pickers.insert(update.id.clone(), update);
        Ok(())
    }

    pub fn panel(&self, id: &ContributionId) -> Option<PanelUpdate> {
        self.panels.lock().expect("ui panel lock poisoned").get(id).cloned()
    }

    pub fn overlay(&self, id: &ContributionId) -> Option<OverlayUpdate> {
        self.overlays.lock().expect("ui overlay lock poisoned").get(id).cloned()
    }

    pub fn picker(&self, id: &ContributionId) -> Option<PickerUpdate> {
        self.pickers.lock().expect("ui picker lock poisoned").get(id).cloned()
    }
}

fn validate_id(id: &ContributionId) -> Result<(), PluginError> {
    if id.as_str().trim().is_empty() {
        return Err(PluginError::Schema { message: "UI id must not be empty".to_owned() });
    }
    Ok(())
}

impl Service for UiService {
    const ID: &'static str = "editor.ui@1";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Good;

    impl UiProvider for Good {
        fn render(&self, request: &UiRenderRequest) -> Result<Value, PluginError> {
            Ok(json!({"revision": request.revision}))
        }
    }

    struct Slow;

    impl UiProvider for Slow {
        fn render(&self, _request: &UiRenderRequest) -> Result<Value, PluginError> {
            std::thread::sleep(Duration::from_millis(100));
            Ok(json!({"late": true}))
        }
    }

    fn id() -> ContributionId {
        ContributionId::new("demo.panel").unwrap()
    }

    #[test]
    fn dynamic_update_replaces_content() {
        let ui = UiService::new();
        ui.apply_panel(PanelUpdate { id: id(), revision: 1, visible: true, data: json!({"a": 1}) })
            .unwrap();
        ui.apply_panel(PanelUpdate {
            id: id(),
            revision: 2,
            visible: false,
            data: json!({"a": 2}),
        })
        .unwrap();
        let panel = ui.panel(&id()).unwrap();
        assert_eq!(panel.revision, 2);
        assert!(!panel.visible);
    }

    #[test]
    fn stale_dynamic_revision_is_rejected() {
        let ui = UiService::new();
        ui.apply_overlay(OverlayUpdate { id: id(), revision: 4, visible: true, data: Value::Null })
            .unwrap();
        let error = ui
            .apply_overlay(OverlayUpdate {
                id: id(),
                revision: 3,
                visible: true,
                data: Value::Null,
            })
            .unwrap_err();
        assert!(error.to_string().contains("stale"));
    }

    #[test]
    fn missing_provider_is_unsupported() {
        let ui = UiService::new();
        let error = ui.render(&id(), ViewId(1), 1).unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Runtime);
    }

    #[test]
    fn provider_timeout_keeps_last_good() {
        let ui = UiService::new().with_timeout(Duration::from_millis(5));
        let _first = ui.register_provider(id(), Arc::new(Good));
        ui.render(&id(), ViewId(1), 1).unwrap();
        let _slow = ui.register_provider(id(), Arc::new(Slow));
        let result = ui.render(&id(), ViewId(1), 2).unwrap();
        assert!(result.stale);
        assert_eq!(result.data, json!({"revision": 1}));
        assert!(result.error.unwrap().contains("timed out"));
    }

    #[test]
    fn provider_disposer_is_identity_checked() {
        let ui = UiService::new();
        let first = ui.register_provider(id(), Arc::new(Good));
        let second = ui.register_provider(id(), Arc::new(Good));
        first.dispose();
        assert!(ui.has_provider(&id()));
        second.dispose();
        assert!(!ui.has_provider(&id()));
    }
}
