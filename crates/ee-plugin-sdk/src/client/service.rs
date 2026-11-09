//! Generic typed service call wrapper.

use std::sync::Arc;

use ee_plugin_api::context::ServiceCaller;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::ServiceId;
use serde_json::Value;

/// One versioned service handle.
#[derive(Clone)]
pub struct ServiceClient {
    caller: Arc<dyn ServiceCaller>,
    service: ServiceId,
}

impl ServiceClient {
    pub fn new(caller: Arc<dyn ServiceCaller>, service: &'static str) -> Self {
        Self { caller, service: ServiceId::from_static(service) }
    }

    pub fn service_id(&self) -> &ServiceId {
        &self.service
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, PluginError> {
        self.caller.call_service(self.service.clone(), method, params)
    }

    /// Call a method and deserialize the typed result.
    pub fn call_as<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, PluginError> {
        let value = self.call(method, params)?;
        serde_json::from_value(value)
            .map_err(|error| PluginError::Schema { message: error.to_string() })
    }
}
