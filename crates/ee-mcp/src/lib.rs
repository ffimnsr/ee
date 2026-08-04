//! MCP (Model Context Protocol) client integration for ee agents mode.
//!
//! Repository policy: prefer the official [`rmcp`] SDK crate over handrolled
//! wire/protocol/transport code.  This crate is a thin adapter around `rmcp`
//! for MCP `2026-07-28`; custom MCP code is allowed only where `rmcp` lacks
//! required 2026-07-28 behavior, isolated behind thin adapters with tests
//! proving why SDK coverage was insufficient.
//!
//! Protocol boundary: `2026-07-28` is the single supported server protocol
//! version; anything else fails closed at the handshake.  Deprecated MCP
//! features are intentionally not implemented as client features: `roots`,
//! `sampling`, protocol `logging`, dynamic client registration, sampling
//! `includeContext: "thisServer"` / `"allServers"`, and HTTP+SSE transport.
//! Transports are stdio and Streamable HTTP only.  Migration paths:
//! workspace files/directories pass through tool parameters, resource URIs,
//! or server config; direct LLM provider APIs replace sampling; stderr or
//! OpenTelemetry replace MCP logging; Client ID Metadata Documents replace
//! dynamic registration.
//!
//! # SDK coverage
//!
//! `rmcp` 3.1 models the full 2026-07-28 protocol: `server/discover`
//! ([`ClientLifecycleMode::Discover`]), per-request `_meta` client context,
//! `ttlMs`/`cacheScope` response caching, MRTR `input_required` rounds with
//! `elicitation/create`, `subscriptions/listen`, and Streamable HTTP.  This
//! crate therefore contains no handrolled JSON-RPC plumbing; ee-owned code is
//! limited to configuration, version pinning, deprecated-capability policy,
//! secret rejection, primitive namespacing/validation, host events, and
//! lifecycle orchestration.
//!
//! [`rmcp`]: https://docs.rs/rmcp
//! [`ClientLifecycleMode::Discover`]: rmcp::service::ClientLifecycleMode

pub mod config;
pub mod discovery;
pub mod error;
pub mod events;
#[cfg(feature = "test-utils")]
pub mod fake;
pub mod handler;
pub mod manager;
pub mod proxy;
pub mod registry;
pub mod transport;

pub use config::{
    McpServerConfig, McpServerKind, RawMcpServerSettings, RawStdioSettings,
    RawStreamableHttpSettings, StdioMcpConfig, StreamableHttpConfig,
};
pub use discovery::{CapabilitySnapshot, DiscoveryCache, DiscoverySnapshot};
pub use error::McpError;
pub use events::{McpEvent, McpServerState};
pub use handler::{EeClientHandler, ElicitationBroker};
pub use manager::{McpClientManager, McpSubscription, NamespacedPrimitive};
pub use proxy::{
    CodeActionEntry, CodeActionsResult, DiagnosticEntry, DiagnosticsResult, DirectoryEntry,
    DirectoryEntryAll, DocumentSymbolEntry, DocumentSymbolsResult, EditTextResult, EeMcpProxy,
    EeProxyBackend, FileMatch, ListDirectoryAllResult, ListDirectoryResult, OpenBufferEntry,
    OpenBuffersResult, PlannedFileEdit, PlannedTextEdit, ProxyToolError, ReferenceEntry,
    ReferencesResult, RenamePreviewResult, SearchFilesAllResult, SearchFilesResult,
    SearchTextResult, TextEdit, TextMatch, TextRange, WorkspaceEditResult, WorkspaceRootsResult,
};
pub use registry::{
    NamespacedPrompt, NamespacedResource, NamespacedTool, PrimitiveRegistry, PrimitiveSummary,
    prompt_text,
};

/// The MCP protocol version implemented by the ee MCP client.
///
/// Servers advertising any other protocol version are rejected at the
/// handshake boundary (fail closed).
pub const MCP_PROTOCOL_VERSION: &str = "2026-07-28";

/// Client implementation identity sent in `initialize` and `_meta`.
pub const CLIENT_NAME: &str = "ee";

/// Client implementation version sent in `initialize` and `_meta`.
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default per-request timeout when the server config does not set one.
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// Default cap on retained stderr diagnostics bytes per server process.
pub const DEFAULT_STDERR_DIAGNOSTICS_CAP: usize = 64 * 1024;
