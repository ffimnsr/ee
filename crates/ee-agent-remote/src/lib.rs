//! Headless remote-control host for ee agent mode.
//!
//! This crate is the single place remote-control logic lives. It embeds
//! [`ee_agent_host::AgentManager`] without any editor dependency and exposes it
//! through three thin surfaces:
//!
//! - a pinned JSON-RPC registry over stdio or WebSocket
//! - an MCP tool server for agent drivers
//! - a static WASM web client
//!
//! Module map only: each concern lives in exactly one module so the crate
//! stays reviewable and every file remains well below the 1k LOC threshold.
//!
//! Hard boundary: this crate never depends on `ee-cli`, `ratatui`, or
//! `crossterm`, and no crate in `ee-agent-host` may depend on it.

pub mod config;
pub mod dispatch;
pub mod error;
pub mod events;
pub mod headless;
pub mod history;
pub mod host;
pub mod mcp;
pub mod permissions;
pub mod protocol;
pub mod redact;
pub mod transport;

pub use config::{RemoteHostConfig, RemotePermissionMode};
pub use error::{RemoteError, RemoteErrorCode};
pub use headless::{HeadlessHandler, TerminalOptions, TerminalRegistry};
pub use history::{HistoryEntry, SessionLog};
pub use host::RemoteHost;
pub use mcp::{MCP_SERVER_NAME, RemoteMcpServer};

/// Protocol version carried on the remote `initialize` handshake.
///
/// Bumped only for breaking wire changes; additive methods keep this value.
pub const REMOTE_PROTOCOL_VERSION: &str = "1.0";

/// Server implementation identity sent on `initialize`.
pub const SERVER_NAME: &str = "ee-agent-remote";
