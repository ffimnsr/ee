// Copyright 2017 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Parsing of raw JSON messages into RPC objects.

use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::trace_span;

use crate::error::{ReadError, RemoteError};
use crate::transport::ReadTransport;

/// A JSON-RPC 2.0 request identifier, which may be either a number or a string.
///
/// Per the JSON-RPC 2.0 specification, `null` identifiers are deliberately
/// not supported; a `null` id in an incoming message is treated as a
/// notification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric identifier (most common; xi-rpc always generates these).
    Number(u64),
    /// A string identifier (accepted for compatibility with other peers).
    Str(String),
}

/// An RPC response, received from the peer.
pub type Response = Result<Value, RemoteError>;

/// Reads and parses RPC messages from a stream, maintaining an
/// internal buffer.
#[derive(Debug, Default)]
pub struct MessageReader(String);

/// An internal type used during initial JSON parsing.
///
/// Wraps an arbitrary JSON object, which may be any valid or invalid
/// RPC message. This allows initial parsing and response handling to
/// occur on the read thread. If the message looks like a request, it
/// is passed to the main thread for handling.
#[derive(Debug, Clone)]
pub struct RpcObject(pub Value);

#[derive(Debug, Clone, PartialEq)]
/// An RPC call, which may be either a notification or a request.
pub enum Call<N, R> {
    /// An id and an RPC Request
    Request(RequestId, R),
    /// An RPC Notification
    Notification(N),
    /// A malformed request: the request contained an id, but could
    /// not be parsed. The client will receive an error.
    InvalidRequest(RequestId, RemoteError),
    /// An incoming notification that could not be deserialized.
    ///
    /// The run loop should log this and continue rather than disconnecting.
    UnknownNotification(String),
}

impl MessageReader {
    /// Attempts to read the next framed message from the transport and
    /// parse it as an RPC object.
    ///
    /// # Errors
    ///
    /// This function will return an error if there is an underlying
    /// I/O error, if the stream is closed, or if the message is not
    /// a valid JSON object.
    pub fn next<R: ReadTransport>(&mut self, reader: &mut R) -> Result<RpcObject, ReadError> {
        self.0.clear();
        let n = reader.read_message(&mut self.0)?;
        if n == 0 {
            Err(ReadError::Disconnect)
        } else {
            self.parse(&self.0)
        }
    }

    /// Attempts to parse a &str as an RPC Object.
    ///
    /// This should not be called directly unless you are writing tests.
    #[doc(hidden)]
    pub fn parse(&self, s: &str) -> Result<RpcObject, ReadError> {
        let span = trace_span!(target: "xi_rpc", "parse_message");
        let _entered = span.enter();
        let val = serde_json::from_str::<Value>(s)?;
        if val.is_array() {
            // JSON-RPC batch requests are explicitly rejected.
            Err(ReadError::BatchNotSupported)
        } else if !val.is_object() {
            Err(ReadError::NotObject)
        } else {
            Ok(val.into())
        }
    }
}

impl RpcObject {
    /// Returns the `id` of the underlying object, if present.
    ///
    /// Accepts both numeric and string ids per JSON-RPC 2.0.
    pub fn get_id(&self) -> Option<RequestId> {
        match self.0.get("id")? {
            Value::Number(n) => n.as_u64().map(RequestId::Number),
            Value::String(s) => Some(RequestId::Str(s.clone())),
            _ => None,
        }
    }

    /// Returns the 'method' field of the underlying object, if present.
    pub fn get_method(&self) -> Option<&str> {
        self.0.get("method").and_then(Value::as_str)
    }

    /// Returns `true` if this object looks like an RPC response;
    /// that is, if it has an 'id' field and does _not_ have a 'method'
    /// field.
    pub fn is_response(&self) -> bool {
        self.0.get("id").is_some() && self.0.get("method").is_none()
    }

    /// Attempts to convert the underlying `Value` into an RPC response
    /// object, and returns the result.
    ///
    /// The caller is expected to verify that the object is a response
    /// before calling this method.
    ///
    /// # Errors
    ///
    /// If the `Value` is not a well formed response object, this will
    /// return a `String` containing an error message. The caller should
    /// print this message and exit.
    pub fn into_response(mut self) -> Result<Response, String> {
        let _ = self.get_id().ok_or("Response requires 'id' field.".to_string())?;

        let has_result = self.0.get("result").is_some();
        let has_error = self.0.get("error").is_some();
        if has_result == has_error {
            return Err("RPC response must contain exactly one of \
                        'error' or 'result' fields."
                .into());
        }

        // Reject any unexpected fields beyond the standard response members.
        if let Some(obj) = self.0.as_object() {
            for key in obj.keys() {
                match key.as_str() {
                    "id" | "result" | "error" | "jsonrpc" => {}
                    other => {
                        return Err(format!("Unexpected field in RPC response: '{}'", other));
                    }
                }
            }
        }

        let result = self.0.as_object_mut().and_then(|obj| obj.remove("result"));

        match result {
            Some(r) => Ok(Ok(r)),
            None => {
                let error = self.0.as_object_mut().and_then(|obj| obj.remove("error")).unwrap();
                match serde_json::from_value::<RemoteError>(error) {
                    Ok(e) => Ok(Err(e)),
                    Err(e) => Err(format!("Error handling response: {:?}", e)),
                }
            }
        }
    }

    /// Attempts to convert the underlying `Value` into either an RPC
    /// notification or request.
    ///
    /// # Errors
    ///
    /// Returns a `serde_json::Error` if the `Value` cannot be converted
    /// to one of the expected types.
    /// Attempts to convert the underlying `Value` into either an RPC
    /// notification or request.
    ///
    /// For requests (objects with an `id`) a deserialization failure produces
    /// `Call::InvalidRequest` so the peer receives a structured error response.
    ///
    /// For notifications (objects without an `id`) a deserialization failure
    /// returns `Call::UnknownNotification` so the caller can log and continue
    /// rather than terminating the run loop.
    pub fn into_rpc<N, R>(self) -> Call<N, R>
    where
        N: DeserializeOwned,
        R: DeserializeOwned,
    {
        let id = self.get_id();
        let raw = self.0;
        match id {
            Some(id) => match serde_json::from_value::<R>(raw.clone()) {
                Ok(resp) => Call::Request(id, resp),
                Err(err) => {
                    let remote_error = RemoteError::from_request_parse_error(&raw, err);
                    Call::InvalidRequest(id, remote_error)
                }
            },
            None => match serde_json::from_value::<N>(raw) {
                Ok(notif) => Call::Notification(notif),
                Err(err) => Call::UnknownNotification(err.to_string()),
            },
        }
    }
}

impl From<Value> for RpcObject {
    fn from(v: Value) -> RpcObject {
        RpcObject(v)
    }
}

#[cfg(test)]
mod tests {

    use serde_json::json;
    use super::*;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    #[serde(rename_all = "snake_case")]
    #[serde(tag = "method", content = "params")]
    enum TestR {
        NewView { file_path: Option<String> },
        OldView { file_path: usize },
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    #[serde(rename_all = "snake_case")]
    #[serde(tag = "method", content = "params")]
    enum TestN {
        CloseView { view_id: String },
        Save { view_id: String, file_path: String },
    }

    #[test]
    fn request_success() {
        let json = r#"{"id":0,"method":"new_view","params":{}}"#;
        let p: RpcObject = serde_json::from_str::<Value>(json).unwrap().into();
        assert!(!p.is_response());
        let req = p.into_rpc::<TestN, TestR>();
        assert_eq!(req, Call::Request(RequestId::Number(0), TestR::NewView { file_path: None }));
    }

    #[test]
    fn request_failure() {
        // method does not exist
        let json = r#"{"id":0,"method":"new_truth","params":{}}"#;
        let p: RpcObject = serde_json::from_str::<Value>(json).unwrap().into();
        let req = p.into_rpc::<TestN, TestR>();
        let is_ok = matches!(
            req,
            Call::InvalidRequest(RequestId::Number(0), RemoteError::MethodNotFound(_))
        );
        if !is_ok {
            panic!("{:?}", req);
        }
    }

    #[test]
    fn notif_with_id() {
        // method is a notification, should not have ID
        let json = r#"{"id":0,"method":"close_view","params":{"view_id": "view-id-1"}}"#;
        let p: RpcObject = serde_json::from_str::<Value>(json).unwrap().into();
        let req = p.into_rpc::<TestN, TestR>();
        let is_ok = matches!(req, Call::InvalidRequest(RequestId::Number(0), _));
        if !is_ok {
            panic!("{:?}", req);
        }
    }

    #[test]
    fn request_invalid_params() {
        let json = r#"{"id":0,"method":"new_view","params":{"file_path":9}}"#;
        let p: RpcObject = serde_json::from_str::<Value>(json).unwrap().into();
        let req = p.into_rpc::<TestN, TestR>();
        let is_ok = matches!(
            req,
            Call::InvalidRequest(RequestId::Number(0), RemoteError::InvalidParams(_))
        );
        if !is_ok {
            panic!("{:?}", req);
        }
    }

    #[test]
    fn test_resp_err() {
        let json = r#"{"id":5,"error":{"code":420, "message":"chill out"}}"#;
        let p: RpcObject = serde_json::from_str::<Value>(json).unwrap().into();
        assert!(p.is_response());
        let resp = p.into_response().unwrap();
        assert_eq!(resp, Err(RemoteError::custom(420, "chill out", None)));
    }

    #[test]
    fn test_resp_result() {
        let json = r#"{"id":5,"result":"success!"}"#;
        let p: RpcObject = serde_json::from_str::<Value>(json).unwrap().into();
        assert!(p.is_response());
        let resp = p.into_response().unwrap();
        assert_eq!(resp, Ok(json!("success!")));
    }

    #[test]
    fn test_err() {
        let json = r#"{"code": -32600, "message": "Invalid Request"}"#;
        let e = serde_json::from_str::<RemoteError>(json).unwrap();
        assert_eq!(e, RemoteError::InvalidRequest(None));
    }

    #[test]
    fn test_unknown_error_value_round_trips_as_unknown_remote_error() {
        let e = serde_json::from_value::<RemoteError>(json!("boom")).unwrap();
        assert_eq!(e, RemoteError::Unknown(json!("boom")));

        let serialized = serde_json::to_value(&e).unwrap();
        assert_eq!(serialized["code"], json!(-32001));
        assert_eq!(serialized["message"], json!("Unknown remote error"));
        assert_eq!(serialized["data"], json!("boom"));
    }
}
