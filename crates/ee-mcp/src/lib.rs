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
//! # ee proxy contract
//!
//! Editor proxy tool names are stable `ee_` identifiers. Clients should call
//! `ee_tools_manifest` once per MCP session with no arguments and cache its
//! versioned response. Each entry defines its input schema, side-effect class,
//! approval behavior, transport availability, required host capabilities,
//! output caps, redaction rules, typed error classes, deprecation/replacement
//! metadata, and a minimal schema-valid example. Hosts may advertise a partial
//! tool set, and known disabled tools fail closed as tool-level errors. New
//! incompatible schemas require a new tool name; complex arguments should
//! become smaller focused tools. Paths, sensitive values, and diagnostics stay
//! subject to host validation, bounds, and redaction.
//!
//! [`rmcp`]: https://docs.rs/rmcp
//! [`ClientLifecycleMode::Discover`]: rmcp::service::ClientLifecycleMode

pub mod classify;
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
pub mod tool_governance;
pub mod transport;

pub use classify::{SideEffectClass, exact_trust_eligible, side_effect_class};
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
    ChangedFileEntry, ChangedFilesResult, CodeActionEntry, CodeActionsResult, DiagnosticEntry,
    DiagnosticsResult, DirectoryEntry, DirectoryEntryAll, DocumentSymbolEntry,
    DocumentSymbolsResult, EditTextResult, EeMcpProxy, EeProxyBackend, FileDependencyEdge,
    FileDependencyMapResult, FileMatch, GitDiffResult, GitStatusResult, ListDirectoryAllResult,
    ListDirectoryResult, MAX_TOOL_ARGUMENT_BYTES, OpenBufferEntry, OpenBuffersResult,
    PlannedFileEdit, PlannedTextEdit, ProjectInstructionSource, ProjectInstructionsResult,
    ProxyToolError, ReferenceEntry, ReferencesResult, RenamePreviewResult, ReviewContextResult,
    SearchFilesAllResult, SearchFilesResult, SearchTextResult, SessionNoteResult,
    SessionNotesResult, TerminalOutputChunk, TerminalOutputResult, TerminalWaitResult, TextEdit,
    TextMatch, TextRange, ToolManifestEntry, ToolOutputCap, ToolsManifestResult,
    WorkspaceEditResult, WorkspaceRootsResult,
};
pub use registry::{
    NamespacedPrompt, NamespacedResource, NamespacedTool, PrimitiveRegistry, PrimitiveSummary,
    prompt_text,
};
pub use tool_governance::{
    EE_TOOL_SCHEMA_VERSION, STABLE_TOOL_NAMES, ToolGovernance, ToolTransport, governance,
    supports_transport, tool_names_for_transport,
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
