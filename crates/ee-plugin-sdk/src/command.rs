//! Command and service-call helper types.

use ee_plugin_api::effect::Effect;
use ee_plugin_api::ids::ServiceId;
use ee_plugin_api::wire::WireError;
use serde_json::Value;

/// Effects and optional typed error returned by a command handler.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandResponse {
    pub effects: Vec<Effect>,
    pub error: Option<WireError>,
}

impl CommandResponse {
    /// Successful response with effects.
    pub fn new(effects: Vec<Effect>) -> Self {
        Self { effects, error: None }
    }
}

/// Plugin-initiated service call.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceCall {
    pub service: ServiceId,
    pub method: String,
    pub params: Value,
}

/// Result of a service call.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceCallResult {
    pub result: Option<Value>,
    pub error: Option<WireError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_defaults_to_no_error() {
        let response = CommandResponse::new(vec![Effect::Noop]);
        assert!(response.error.is_none());
        assert_eq!(response.effects.len(), 1);
    }

    #[test]
    fn service_call_names_are_stable() {
        let call = ServiceCall {
            service: ServiceId::from_static("editor.document@1"),
            method: "snapshot".to_owned(),
            params: serde_json::json!({}),
        };
        assert_eq!(call.service.as_str(), "editor.document@1");
    }
}
