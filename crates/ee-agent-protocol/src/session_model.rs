//! Mid-session model switch surface.
//!
//! ACP has no model-selection request.  ee adds `session/set_model`, a
//! host-level switch that resolves the target against the provider's model
//! registry, rebuilds the provider profile, and keeps the normalized
//! transcript.  The switch is only valid while the session is idle.

use serde::{Deserialize, Serialize};

use crate::Meta;
use crate::SessionId;

/// Wire method name for the model switch.
pub const SESSION_SET_MODEL_METHOD_NAME: &str = "session/set_model";

/// Request parameters for `session/set_model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SetSessionModelRequest {
    /// The session whose model changes.
    pub session_id: SessionId,
    /// Registry id of the target model.
    pub model_id: String,
    /// Reserved ACP metadata.
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

impl SetSessionModelRequest {
    /// Builds a request with the required fields set.
    #[must_use]
    pub fn new(session_id: impl Into<SessionId>, model_id: impl Into<String>) -> Self {
        Self { session_id: session_id.into(), model_id: model_id.into(), meta: None }
    }
}

/// Response for `session/set_model`; empty on success.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SetSessionModelResponse {}

impl SetSessionModelResponse {
    /// Builds the empty success response.
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }
}

impl agent_client_protocol::JsonRpcMessage for SetSessionModelRequest {
    fn matches_method(method: &str) -> bool {
        method == SESSION_SET_MODEL_METHOD_NAME
    }

    fn method(&self) -> &str {
        SESSION_SET_MODEL_METHOD_NAME
    }

    fn to_untyped_message(
        &self,
    ) -> std::result::Result<agent_client_protocol::UntypedMessage, crate::Error> {
        agent_client_protocol::UntypedMessage::new(SESSION_SET_MODEL_METHOD_NAME, self)
    }

    fn parse_message(
        method: &str,
        params: &impl serde::Serialize,
    ) -> std::result::Result<Self, crate::Error> {
        if method != SESSION_SET_MODEL_METHOD_NAME {
            return Err(crate::Error::method_not_found());
        }
        agent_client_protocol::util::json_cast_params(params)
    }
}

impl agent_client_protocol::JsonRpcRequest for SetSessionModelRequest {
    type Response = SetSessionModelResponse;
}

impl agent_client_protocol::JsonRpcResponse for SetSessionModelResponse {
    fn into_json(self, _method: &str) -> std::result::Result<serde_json::Value, crate::Error> {
        serde_json::to_value(self).map_err(|error| {
            crate::Error::internal_error().data(serde_json::json!({ "reason": error.to_string() }))
        })
    }

    fn from_value(
        _method: &str,
        value: serde_json::Value,
    ) -> std::result::Result<Self, crate::Error> {
        serde_json::from_value(value).map_err(|error| {
            crate::Error::invalid_params().data(serde_json::json!({ "reason": error.to_string() }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_roundtrips_with_camel_case_fields() {
        let request = SetSessionModelRequest::new("s-1", "model-b");
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["sessionId"], "s-1");
        assert_eq!(wire["modelId"], "model-b");
        let restored: SetSessionModelRequest = serde_json::from_value(wire).expect("parses");
        assert_eq!(restored, request);
    }

    #[test]
    fn empty_response_is_an_object() {
        let value = serde_json::to_value(SetSessionModelResponse::new()).expect("serializes");
        assert_eq!(value, json!({}));
    }
}
