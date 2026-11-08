//! `editor.lsp` service.
//!
//! Core only routes typed calls. Protocol handling and server lifecycle stay in
//! the LSP plugin, which installs an [`LspProvider`] during activation.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use ee_plugin_api::disposer::Disposer;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::model::ViewId;
use ee_plugin_api::service::Service;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default provider deadline.
pub const DEFAULT_LSP_TIMEOUT: Duration = Duration::from_millis(750);

/// One typed LSP request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspRequest {
    pub view_id: ViewId,
    pub line: usize,
    pub column: usize,
    #[serde(default)]
    pub new_name: Option<String>,
}

/// Provider installed by the LSP plugin.
pub trait LspProvider: Send + Sync + 'static {
    fn call(&self, method: &str, params: Value) -> Result<Value, PluginError>;
}

/// Routed LSP queries.
#[derive(Clone)]
pub struct LspService {
    provider: Arc<RwLock<Option<Arc<dyn LspProvider>>>>,
    timeout: Duration,
}

impl Default for LspService {
    fn default() -> Self {
        Self::new()
    }
}

impl LspService {
    pub fn new() -> Self {
        Self { provider: Arc::new(RwLock::new(None)), timeout: DEFAULT_LSP_TIMEOUT }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Install the provider. Returns a disposer that removes only this value.
    pub fn register_provider(&self, provider: Arc<dyn LspProvider>) -> Disposer {
        let slot = Arc::clone(&self.provider);
        let stored = Arc::clone(&provider);
        *slot.write().expect("lsp lock poisoned") = Some(stored);
        Disposer::new(move || {
            if let Ok(mut slot) = slot.write()
                && slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, &provider))
            {
                *slot = None;
            }
        })
    }

    pub fn has_provider(&self) -> bool {
        self.provider.read().expect("lsp lock poisoned").is_some()
    }

    pub fn hover(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("hover", request)
    }

    pub fn completion(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("completion", request)
    }

    pub fn definition(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("definition", request)
    }

    pub fn references(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("references", request)
    }

    pub fn formatting(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("formatting", request)
    }

    pub fn code_actions(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("code_actions", request)
    }

    pub fn rename_preview(&self, request: &LspRequest) -> Result<Value, PluginError> {
        self.call("rename_preview", request)
    }

    /// Route a method with a bounded deadline.
    pub fn call(&self, method: &str, request: &LspRequest) -> Result<Value, PluginError> {
        let provider = self
            .provider
            .read()
            .expect("lsp lock poisoned")
            .clone()
            .ok_or(PluginError::Unsupported { feature: "editor.lsp provider" })?;
        let method = method.to_owned();
        let thread_method = method.clone();
        let params = serde_json::to_value(request).map_err(|error| PluginError::Schema {
            message: format!("lsp request serialization: {error}"),
        })?;

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name(format!("ee-lsp-{thread_method}"))
            .spawn(move || {
                let _ = sender.send(provider.call(&thread_method, params));
            })
            .map_err(|error| PluginError::Runtime {
                message: format!("cannot start LSP worker: {error}"),
            })?;

        match receiver.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(PluginError::Runtime { message: format!("LSP request `{method}` timed out") })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(PluginError::Runtime {
                message: format!("LSP request `{method}` worker exited"),
            }),
        }
    }
}

impl Service for LspService {
    const ID: &'static str = "editor.lsp@1";
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    impl LspProvider for Fake {
        fn call(&self, method: &str, params: Value) -> Result<Value, PluginError> {
            Ok(serde_json::json!({"method": method, "params": params}))
        }
    }

    struct Slow;

    impl LspProvider for Slow {
        fn call(&self, _method: &str, _params: Value) -> Result<Value, PluginError> {
            std::thread::sleep(Duration::from_millis(50));
            Ok(Value::Null)
        }
    }

    fn request() -> LspRequest {
        LspRequest { view_id: ViewId(1), line: 2, column: 3, new_name: None }
    }

    #[test]
    fn provider_round_trips() {
        let service = LspService::new();
        let _disposer = service.register_provider(Arc::new(Fake));
        let value = service.hover(&request()).unwrap();
        assert_eq!(value["method"], "hover");
        assert_eq!(value["params"]["line"], 2);
    }

    #[test]
    fn missing_provider_fails_closed() {
        let service = LspService::new();
        let error = service.definition(&request()).unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Runtime);
    }

    #[test]
    fn timeout_is_reported() {
        let service = LspService::new().with_timeout(Duration::from_millis(5));
        let _disposer = service.register_provider(Arc::new(Slow));
        let error = service.completion(&request()).unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn stale_disposer_does_not_remove_replacement() {
        let service = LspService::new();
        let first = service.register_provider(Arc::new(Fake));
        let second = service.register_provider(Arc::new(Fake));
        first.dispose();
        assert!(service.has_provider());
        second.dispose();
        assert!(!service.has_provider());
    }
}
