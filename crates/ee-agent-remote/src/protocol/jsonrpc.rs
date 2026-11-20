//! JSON-RPC 2.0 envelope types for the remote surface.
//!
//! The remote protocol is JSON-RPC 2.0 over newline-delimited stdio or
//! WebSocket text frames. Batches are rejected in v1 with a typed
//! `invalid_request` error. Request ids are echoed exactly, including string
//! ids, so drivers using opaque ids keep working.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{RemoteError, RemoteErrorCode};

/// JSON-RPC protocol version literal.
pub const JSONRPC_VERSION: &str = "2.0";

/// A request id, preserved exactly as received.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id.
    Number(i64),
    /// String id.
    String(String),
}

impl RequestId {
    /// Stable display form for logs and error data.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Number(number) => number.to_string(),
            Self::String(text) => text.clone(),
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// JSON-RPC error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// Builds the error object for one typed remote failure.
    #[must_use]
    pub fn from_remote(error: &RemoteError) -> Self {
        Self {
            code: error.code().jsonrpc_code(),
            message: error.message().to_string(),
            data: error.data().cloned(),
        }
    }
}

/// One incoming request.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IncomingRequest {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Client-assigned id, echoed on the response.
    pub id: RequestId,
    /// Registry method name.
    pub method: String,
    /// Optional params object.
    #[serde(default)]
    pub params: Option<Value>,
}

/// One incoming notification.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IncomingNotification {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Registry notification name.
    pub method: String,
    /// Optional params object.
    #[serde(default)]
    pub params: Option<Value>,
}

/// One incoming response to a server-initiated request.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IncomingResponse {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Id of the server request being answered.
    pub id: RequestId,
    /// Successful result, when present.
    #[serde(default)]
    pub result: Option<Value>,
    /// Failure object, when present.
    #[serde(default)]
    pub error: Option<ErrorObject>,
}

/// Any decoded client frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A request expecting a response.
    Request(IncomingRequest),
    /// A notification with no response.
    Notification(IncomingNotification),
    /// A response to a server request (approval answers).
    Response(IncomingResponse),
}

fn check_version(version: &str) -> Result<(), RemoteError> {
    if version == JSONRPC_VERSION {
        Ok(())
    } else {
        Err(RemoteError::invalid_request(format!("jsonrpc must be \"2.0\", got {version:?}")))
    }
}

/// Parses one client frame, rejecting batches with a typed error.
///
/// # Errors
///
/// Returns `parse_error` for malformed JSON, `invalid_request` for batches or
/// envelopes missing required members.
pub fn parse_incoming(text: &str) -> Result<Incoming, RemoteError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| RemoteError::parse_error(format!("invalid JSON: {error}")))?;
    if value.is_array() {
        return Err(RemoteError::invalid_request(
            "JSON-RPC batches are not supported on the remote surface",
        ));
    }
    if !value.is_object() {
        return Err(RemoteError::invalid_request("frames must be JSON objects"));
    }
    let has_method = value.get("method").is_some();
    if has_method {
        let has_id = value.get("id").is_some();
        if has_id {
            let request: IncomingRequest = serde_json::from_value(value)
                .map_err(|error| RemoteError::invalid_request(error.to_string()))?;
            check_version(&request.jsonrpc)?;
            Ok(Incoming::Request(request))
        } else {
            let notification: IncomingNotification = serde_json::from_value(value)
                .map_err(|error| RemoteError::invalid_request(error.to_string()))?;
            check_version(&notification.jsonrpc)?;
            Ok(Incoming::Notification(notification))
        }
    } else {
        let response: IncomingResponse = serde_json::from_value(value)
            .map_err(|error| RemoteError::invalid_request(error.to_string()))?;
        check_version(&response.jsonrpc)?;
        Ok(Incoming::Response(response))
    }
}

/// Builds a success response value.
#[must_use]
pub fn response_ok(id: &RequestId, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    })
}

/// Builds an error response value.
#[must_use]
pub fn response_error(id: &RequestId, error: &RemoteError) -> Value {
    serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": ErrorObject::from_remote(error),
    })
}

/// Builds an error response for a request whose id could not be recovered.
#[must_use]
pub fn response_error_null_id(error: &RemoteError) -> Value {
    serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": Value::Null,
        "error": ErrorObject::from_remote(error),
    })
}

/// Builds one notification value.
#[must_use]
pub fn notification(method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": method,
        "params": params,
    })
}

/// Builds one server→client request value.
#[must_use]
pub fn server_request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Maps a parse failure to the envelope JSON-RPC error code.
#[must_use]
pub fn is_parse_error(error: &RemoteError) -> bool {
    error.code() == RemoteErrorCode::ParseError
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_and_number_ids_round_trip_exactly() {
        let number =
            parse_incoming(r#"{"jsonrpc":"2.0","id":7,"method":"initialize"}"#).expect("number id");
        let Incoming::Request(number) = number else { panic!("request") };
        assert_eq!(number.id, RequestId::Number(7));
        assert_eq!(
            response_ok(&number.id, Value::Null),
            serde_json::json!({"jsonrpc":"2.0","id":7,"result":null})
        );

        let text = parse_incoming(r#"{"jsonrpc":"2.0","id":"req-9","method":"initialize"}"#)
            .expect("string id");
        let Incoming::Request(text) = text else { panic!("request") };
        assert_eq!(text.id, RequestId::String("req-9".into()));
        assert_eq!(
            response_ok(&text.id, Value::Null),
            serde_json::json!({"jsonrpc":"2.0","id":"req-9","result":null})
        );
    }

    #[test]
    fn notifications_are_distinguished_from_requests() {
        let parsed = parse_incoming(r#"{"jsonrpc":"2.0","method":"initialized"}"#).expect("frame");
        assert!(matches!(parsed, Incoming::Notification(_)));
    }

    #[test]
    fn responses_are_distinguished_from_requests() {
        let parsed = parse_incoming(r#"{"jsonrpc":"2.0","id":3,"result":{"decision":"allow"}}"#)
            .expect("frame");
        let Incoming::Response(response) = parsed else { panic!("response") };
        assert_eq!(response.id, RequestId::Number(3));
        assert!(response.error.is_none());
    }

    #[test]
    fn batches_are_rejected_typed() {
        let error = parse_incoming(r#"[{"jsonrpc":"2.0","id":1,"method":"initialize"}]"#)
            .expect_err("batch");
        assert_eq!(error.code(), RemoteErrorCode::InvalidRequest);
        assert!(error.message().contains("batches"));
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let error = parse_incoming("{").expect_err("bad json");
        assert!(is_parse_error(&error));
    }

    #[test]
    fn wrong_protocol_version_is_invalid_request() {
        let error = parse_incoming(r#"{"jsonrpc":"1.0","id":1,"method":"initialize"}"#)
            .expect_err("version");
        assert_eq!(error.code(), RemoteErrorCode::InvalidRequest);
    }
}
