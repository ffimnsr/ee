//! Pinned remote JSON-RPC registry: method names, typed params/results, and
//! documented error codes.
//!
//! Every dispatcher route resolves through [`registry::Method`]; call sites
//! never carry method-name string literals. Typed payloads live in [`types`],
//! and the JSON-RPC envelope handling lives in [`jsonrpc`].

pub mod jsonrpc;
pub mod registry;
pub mod types;

pub use jsonrpc::{
    ErrorObject, Incoming, IncomingNotification, IncomingRequest, IncomingResponse, RequestId,
    is_parse_error, notification, parse_incoming, response_error, response_error_null_id,
    response_ok, server_request,
};
pub use registry::{
    ERROR_CODES, ErrorCodeDoc, HISTORY_TAIL, INITIALIZE, INITIALIZED, Method, MethodKind,
    PERMISSION_REQUEST_APPROVAL, PERMISSIONS_PENDING, PERMISSIONS_RESPOND, SESSION_CLOSE,
    SESSION_LIST, SESSION_NEW, SESSION_RESUME, SESSION_SET_MODE, SESSION_SET_MODEL, SESSION_UPDATE,
    STATUS_CHANGED, TURN_COMPLETED, TURN_INTERRUPT, TURN_START,
};
pub use types::*;
