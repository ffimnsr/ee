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

use std::fmt;
use std::io;

use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{Error as JsonError, Value, json};

const UNKNOWN_REMOTE_ERROR_CODE: i64 = -32001;

/// The possible error outcomes when attempting to send a message.
#[derive(Debug)]
pub enum Error {
    /// An IO error occurred on the underlying communication channel.
    Io(io::Error),
    /// The peer returned an error.
    RemoteError(RemoteError),
    /// The peer closed the connection.
    PeerExited { exit_status: Option<i32> },
    /// The peer did not answer before the timeout elapsed.
    PeerTimedOut { after_ms: u64 },
    /// The peer sent malformed protocol data.
    PeerProtocolError { reason: String },
    /// The peer sent a response containing the id, but was malformed.
    InvalidResponse,
    /// The request was cancelled by the caller.
    RequestCancelled,
}

/// The possible error outcomes when attempting to read a message.
#[derive(Debug)]
pub enum ReadError {
    /// An error occurred in the underlying stream
    Io(io::Error),
    /// The message was not valid JSON.
    Json(JsonError),
    /// The message was not a JSON object.
    NotObject,
    /// The the method and params were not recognized by the handler.
    UnknownRequest(JsonError),
    /// The peer closed the connection.
    Disconnect,
    /// The reader thread panicked and the run loop could not continue.
    ThreadPanic,
    /// A JSON-RPC batch request (array) was received; batch mode is not supported.
    BatchNotSupported,
}

/// Errors that can be received from the other side of the RPC channel.
///
/// This type is intended to go over the wire. And by convention
/// should `Serialize` as a JSON object with "code", "message",
/// and optionally "data" fields.
///
/// Standard JSON-RPC error codes are represented explicitly; custom
/// application codes use [`RemoteError::Custom`].
///
/// # Examples
///
/// An invalid request:
///
/// ```
/// use xi_rpc::RemoteError;
/// use serde_json::Value;
///
/// let json = r#"{
///     "code": -32600,
///     "message": "Invalid request",
///     "data": "Additional details"
///     }"#;
///
/// let err = serde_json::from_str::<RemoteError>(&json).unwrap();
/// assert_eq!(err,
///            RemoteError::InvalidRequest(
///                Some(Value::String("Additional details".into()))));
/// ```
///
/// A custom error:
///
/// ```
/// use xi_rpc::RemoteError;
/// use serde_json::Value;
///
/// let json = r#"{
///     "code": 404,
///     "message": "Not Found"
///     }"#;
///
/// let err = serde_json::from_str::<RemoteError>(&json).unwrap();
/// assert_eq!(err, RemoteError::custom(404, "Not Found", None));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteError {
    /// Invalid JSON was received by the server.
    ParseError(Option<Value>),
    /// The JSON was valid, but was not a correctly formed request.
    InvalidRequest(Option<Value>),
    /// The requested method does not exist or is not supported.
    MethodNotFound(Option<Value>),
    /// The supplied params were well-formed JSON but not valid for the method.
    InvalidParams(Option<Value>),
    /// Internal JSON-RPC error.
    InternalError(Option<Value>),
    /// A custom error, defined by the client.
    Custom { code: i64, message: String, data: Option<Value> },
    /// An error that cannot be represented by an error object.
    ///
    /// This error is intended to accommodate clients that return arbitrary
    /// error values. When re-serialized it is converted into a JSON-RPC
    /// server error with the original payload attached as `data`.
    Unknown(Value),
}

pub trait RemoteErrorDetails: fmt::Display {
    fn remote_error_code(&self) -> i64;

    fn remote_error_data(&self) -> Option<Value> {
        None
    }
}

pub trait OptionExt<T> {
    fn ok_or_remote<S>(self, code: i64, message: S) -> Result<T, RemoteError>
    where
        S: Into<String>;

    fn ok_or_not_found<S>(self, message: S) -> Result<T, RemoteError>
    where
        Self: Sized,
        S: Into<String>,
    {
        self.ok_or_remote(404, message)
    }
}

pub trait ResultExt<T, E> {
    fn map_err_remote<S, F>(self, code: i64, message: F) -> Result<T, RemoteError>
    where
        S: Into<String>,
        F: FnOnce(&E) -> S;
}

impl RemoteError {
    /// Creates a new custom error.
    pub fn custom<S, V>(code: i64, message: S, data: V) -> Self
    where
        S: AsRef<str>,
        V: Into<Option<Value>>,
    {
        let message = message.as_ref().into();
        let data = data.into();
        RemoteError::Custom { code, message, data }
    }

    pub(crate) fn from_request_parse_error(request: &Value, err: JsonError) -> Self {
        let data = Some(json!(err.to_string()));
        let Some(obj) = request.as_object() else {
            return RemoteError::InvalidRequest(data);
        };

        if obj.keys().any(|key| !matches!(key.as_str(), "id" | "jsonrpc" | "method" | "params")) {
            return RemoteError::InvalidRequest(data);
        }

        if obj.get("jsonrpc").is_some_and(|value| value.as_str() != Some("2.0")) {
            return RemoteError::InvalidRequest(data);
        }

        if obj.get("method").and_then(Value::as_str).is_none() {
            return RemoteError::InvalidRequest(data);
        }

        if err.to_string().contains("unknown variant") {
            return RemoteError::MethodNotFound(data);
        }

        if obj.contains_key("params") {
            RemoteError::InvalidParams(data)
        } else {
            RemoteError::InvalidRequest(data)
        }
    }
}

impl<T> OptionExt<T> for Option<T> {
    fn ok_or_remote<S>(self, code: i64, message: S) -> Result<T, RemoteError>
    where
        S: Into<String>,
    {
        self.ok_or_else(|| RemoteError::custom(code, message.into(), None))
    }
}

impl<T, E> ResultExt<T, E> for Result<T, E> {
    fn map_err_remote<S, F>(self, code: i64, message: F) -> Result<T, RemoteError>
    where
        S: Into<String>,
        F: FnOnce(&E) -> S,
    {
        self.map_err(|err| RemoteError::custom(code, message(&err).into(), None))
    }
}

impl ReadError {
    /// Returns `true` iff this is the `ReadError::Disconnect` variant.
    pub fn is_disconnect(&self) -> bool {
        matches!(*self, ReadError::Disconnect)
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ReadError::Io(ref err) => write!(f, "I/O Error: {:?}", err),
            ReadError::Json(ref err) => write!(f, "JSON Error: {:?}", err),
            ReadError::NotObject => write!(f, "JSON message was not an object."),
            ReadError::UnknownRequest(ref err) => write!(f, "Unknown request: {:?}", err),
            ReadError::Disconnect => write!(f, "Peer closed the connection."),
            ReadError::ThreadPanic => write!(f, "Reader thread panicked unexpectedly."),
            ReadError::BatchNotSupported => {
                write!(f, "JSON-RPC batch requests are not supported.")
            }
        }
    }
}

impl From<JsonError> for ReadError {
    fn from(err: JsonError) -> ReadError {
        ReadError::Json(err)
    }
}

impl From<io::Error> for ReadError {
    fn from(err: io::Error) -> ReadError {
        ReadError::Io(err)
    }
}

impl From<JsonError> for RemoteError {
    fn from(err: JsonError) -> RemoteError {
        RemoteError::ParseError(Some(json!(err.to_string())))
    }
}

impl<T> From<T> for RemoteError
where
    T: RemoteErrorDetails,
{
    fn from(err: T) -> RemoteError {
        RemoteError::custom(err.remote_error_code(), err.to_string(), err.remote_error_data())
    }
}

impl From<RemoteError> for Error {
    fn from(err: RemoteError) -> Error {
        Error::RemoteError(err)
    }
}

#[derive(Deserialize, Serialize)]
struct ErrorHelper {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl<'de> Deserialize<'de> for RemoteError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v = Value::deserialize(deserializer)?;
        let resp = match ErrorHelper::deserialize(&v) {
            Ok(resp) => resp,
            Err(_) => return Ok(RemoteError::Unknown(v)),
        };

        Ok(match resp.code {
            -32700 => RemoteError::ParseError(resp.data),
            -32600 => RemoteError::InvalidRequest(resp.data),
            -32601 => RemoteError::MethodNotFound(resp.data),
            -32602 => RemoteError::InvalidParams(resp.data),
            -32603 => RemoteError::InternalError(resp.data),
            UNKNOWN_REMOTE_ERROR_CODE => RemoteError::Unknown(resp.data.unwrap_or(Value::Null)),
            _ => RemoteError::Custom { code: resp.code, message: resp.message, data: resp.data },
        })
    }
}

impl Serialize for RemoteError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (code, message, data) = match *self {
            RemoteError::ParseError(ref d) => (-32700, "Parse error", d),
            RemoteError::InvalidRequest(ref d) => (-32600, "Invalid request", d),
            RemoteError::MethodNotFound(ref d) => (-32601, "Method not found", d),
            RemoteError::InvalidParams(ref d) => (-32602, "Invalid params", d),
            RemoteError::InternalError(ref d) => (-32603, "Internal error", d),
            RemoteError::Custom { code, ref message, ref data } => (code, message.as_ref(), data),
            RemoteError::Unknown(ref value) => {
                (UNKNOWN_REMOTE_ERROR_CODE, "Unknown remote error", &Some(value.clone()))
            }
        };
        let message = message.to_owned();
        let data = data.to_owned();
        let err = ErrorHelper { code, message, data };
        err.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::{OptionExt, RemoteError, RemoteErrorDetails, ResultExt};
    use serde_json::{Value, json};

    #[derive(Debug)]
    struct SampleError;

    impl std::fmt::Display for SampleError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "sample failure")
        }
    }

    impl RemoteErrorDetails for SampleError {
        fn remote_error_code(&self) -> i64 {
            42
        }

        fn remote_error_data(&self) -> Option<Value> {
            Some(json!({ "kind": "sample" }))
        }
    }

    #[test]
    fn remote_error_details_convert_into_remote_error() {
        let err: RemoteError = SampleError.into();

        assert_eq!(
            err,
            RemoteError::custom(42, "sample failure", Some(json!({ "kind": "sample" })))
        );
    }

    #[test]
    fn option_ext_maps_missing_values_to_not_found_errors() {
        let err = None::<usize>.ok_or_not_found("missing value").unwrap_err();

        assert_eq!(err, RemoteError::custom(404, "missing value", None));
    }

    #[test]
    fn result_ext_maps_errors_to_remote_error_messages() {
        let err = Err::<(), _>("bad regex")
            .map_err_remote(400, |inner| format!("substitute: {inner}"))
            .unwrap_err();

        assert_eq!(err, RemoteError::custom(400, "substitute: bad regex", None));
    }
}
