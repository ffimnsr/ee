//! Transport surfaces over the remote registry.
//!
//! R6 adds newline-delimited JSON-RPC over stdio. R10 adds the WebSocket and
//! HTTP health surface.

pub(crate) mod handler;
pub mod stdio;
pub mod ws;

pub use stdio::{MAX_FRAME_BYTES, serve_stdio, serve_stream};
pub use ws::{DEFAULT_BIND, WsServeOptions, WsServer, serve_ws};
