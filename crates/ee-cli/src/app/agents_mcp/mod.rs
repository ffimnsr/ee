//! MCP integration for the agents pane.
//!
//! Three concerns live here, all behind the `agents` feature:
//!
//! 1. **MCP configuration forwarding** — user-configured `McpServerSettings`
//!    are converted into ACP `session/new` `mcpServers` entries so the agent
//!    can connect to them directly.  ee starts its own MCP clients only for
//!    health, discovery, and prompt/resource browsing.
//! 2. **MCP health registry + browsing** — a lazy [`McpClientManager`] hosted
//!    on a dedicated worker thread; per-server states, identity, and
//!    capability summaries feed the pane, and prompt/resource/tool browsing
//!    inserts selections into the prompt draft.
//! 3. **ee MCP proxy** — an optional MCP server surface ([`ee_mcp::EeMcpProxy`])
//!    exposed to ACP agents.  ACP-native MCP-over-ACP (Phase 6b) is the
//!    first-class path: `ee-agent-host` advertises the `ee` server as an ACP
//!    `McpServer::Acp` entry and serves `mcp/connect` / `mcp/message` /
//!    `mcp/disconnect` when the agent advertises `mcp_capabilities.acp`.
//!    The stdio `ee --mcp-proxy` entry (this module's socket listener) is
//!    the fallback for agents without ACP-native support.  Both modes route
//!    tool calls through the same approval and bridge paths as direct ACP
//!    client methods, so approvals never bypass the permission broker.
//!
//! Policy: MCP servers start lazily only when the agents pane opens; MCP
//! health failures are non-fatal for ACP chat startup; secrets are never
//! logged or shown in approval text.

pub(crate) use std::collections::BTreeMap;
pub(crate) use std::path::PathBuf;
pub(crate) use std::sync::Arc;
pub(crate) use std::sync::atomic::{AtomicU64, Ordering};

/// Which transport delivered a proxy tool call (Phase 3 MCP trust).
///
/// Exact MCP rules match the transport identity, so a grant created through
/// one route never applies to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]

pub(crate) enum ProxyRoute {
    /// Stdio `ee --mcp-proxy` socket fallback.
    Stdio,
    /// ACP-native MCP-over-ACP.
    AcpNative,
}

impl ProxyRoute {
    /// Stable transport identity for exact MCP rule matching.
    pub(crate) fn transport_identity(self) -> &'static str {
        match self {
            ProxyRoute::Stdio => "stdio:ee --mcp-proxy",
            ProxyRoute::AcpNative => "acp:ee",
        }
    }

    pub(crate) fn transport_kind(self) -> crate::policy::TransportKind {
        match self {
            ProxyRoute::Stdio => crate::policy::TransportKind::McpStdio,
            ProxyRoute::AcpNative => crate::policy::TransportKind::McpAcp,
        }
    }
}
use std::sync::mpsc as std_mpsc;
use std::time::SystemTime;

use ee_agent_host::{AgentError, ClientRequestResponse, ClientRequestResult};
use ee_agent_protocol::{
    CreateTerminalRequest, EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio,
    ReadTextFileRequest, SessionId, WriteTextFileRequest,
};
use ee_mcp::{McpClientManager, McpEvent, McpServerState};
use tokio::runtime::Builder as TokioBuilder;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use super::agent_bridge::BridgeUiMessage;
use super::*;

// ── Policy constants ─────────────────────────────────────────────────────────

/// Cap on one proxy IPC frame (socket or stdio), in bytes.  Exceeding the cap
/// closes the connection with an error (fail closed, no partial parse).
pub(crate) const PROXY_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Cap on one socket-token line, in bytes.
const PROXY_TOKEN_MAX_BYTES: usize = 256;

// ── Pane-side server state ───────────────────────────────────────────────────

/// Per-server state shown in the agents pane channel column.
#[derive(Debug, Clone)]
pub(crate) struct McpServerUi {
    pub(crate) state: McpServerState,
    /// `name version` from `server/discover`, when provided.
    pub(crate) identity: Option<String>,
    /// Capability summary (e.g. `tools, prompts`).
    pub(crate) capabilities: String,
    /// Latest non-fatal diagnostic/error line.
    pub(crate) error: Option<String>,
    /// Namespaced tool keys (`<server_id>/<name>`) last seen on this server.
    pub(crate) tools: Vec<String>,
}

impl Default for McpServerUi {
    fn default() -> Self {
        Self {
            state: McpServerState::Disabled,
            identity: None,
            capabilities: String::new(),
            error: None,
            tools: Vec::new(),
        }
    }
}

impl McpServerUi {
    fn apply_discovery(&mut self, snapshot: &ee_mcp::DiscoverySnapshot) {
        self.identity = snapshot.server_info.as_ref().map(|info| {
            let mut identity = format!("{} {}", info.name, info.version);
            if let Some(title) = &info.title {
                identity = format!("{title} ({identity})");
            }
            identity
        });
        let mut capabilities = Vec::new();
        if snapshot.capabilities.tools {
            capabilities.push("tools");
        }
        if snapshot.capabilities.resources {
            capabilities.push("resources");
        }
        if snapshot.capabilities.prompts {
            capabilities.push("prompts");
        }
        self.capabilities = capabilities.join(", ");
    }
}

// ── Browsing state ──────────────────────────────────────────────────────────

/// What the pane browse list is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpBrowseKind {
    Tools,
    Prompts,
    Resources,
}

impl McpBrowseKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            McpBrowseKind::Tools => "tools",
            McpBrowseKind::Prompts => "prompts",
            McpBrowseKind::Resources => "resources",
        }
    }
}

/// One selectable browse entry.
#[derive(Debug, Clone)]
pub(crate) struct McpBrowseItem {
    /// Primary label shown in the list.
    pub(crate) label: String,
    /// Text inserted into the prompt draft on Enter.
    pub(crate) insert: String,
    /// Secondary detail line (description).
    pub(crate) detail: Option<String>,
}

/// Open browse list state (composer area shows the picker).
#[derive(Debug)]
pub(crate) struct McpBrowseState {
    pub(crate) kind: McpBrowseKind,
    pub(crate) items: Vec<McpBrowseItem>,
    pub(crate) selected: usize,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
    /// Pending list reply (prompts/resources).
    pub(crate) pending_list: Option<std_mpsc::Receiver<Result<Vec<serde_json::Value>, String>>>,
    /// Pending prompt content fetch (prompt browsing only).
    pub(crate) pending_get: Option<std_mpsc::Receiver<Result<String, String>>>,
}

/// Cached code action details for `ee_apply_code_action`.
#[derive(Debug, Clone)]
pub(crate) struct CachedProxyCodeAction {
    pub(crate) path: String,
    pub(crate) has_command: bool,
    pub(crate) edits: Vec<ee_mcp::PlannedTextEdit>,
}

// ── Proxy info ───────────────────────────────────────────────────────────────

/// Runtime identity of the ee MCP proxy listener.
#[derive(Debug, Clone)]
pub(crate) struct ProxyInfo {
    pub(crate) socket_path: PathBuf,
    pub(crate) token: String,
}

// ── Pane MCP state ───────────────────────────────────────────────────────────

/// All MCP pane state; `Default` is the inert startup state (no manager, no
/// processes, no servers).
#[derive(Default)]
pub(crate) struct McpPaneState {
    pub(crate) servers: BTreeMap<String, McpServerUi>,
    pub(crate) browse: Option<McpBrowseState>,
    pub(crate) error: Option<String>,
    /// Lazy MCP client host (None until the pane opens with servers).
    pub(crate) host: Option<McpHostBridge>,
    /// Pending per-server tool refreshes (server id → reply).
    pub(crate) pending_tools: BTreeMap<String, std_mpsc::Receiver<Result<Vec<String>, String>>>,
    /// Pending tools browse list (tools kind only).
    pub(crate) pending_browse_tools: Option<std_mpsc::Receiver<Result<Vec<String>, String>>>,
    /// Proxy listener info when proxy mode is active.
    pub(crate) proxy: Option<ProxyInfo>,
    /// How the ee proxy was exposed to the latest session: `acp-native`,
    /// `stdio fallback`, or `disabled` (Phase 6b diagnostics).
    pub(crate) proxy_mode: Option<String>,
    /// Cached code actions listed for proxy apply calls.
    pub(crate) proxy_code_actions: BTreeMap<String, CachedProxyCodeAction>,
    /// Monotone id source for cached proxy code actions.
    pub(crate) next_proxy_action_id: u64,
    /// Test-only: server id → fake transport factory (see `tests/agent_mcp.rs`).
    #[cfg(test)]
    pub(crate) test_fake_transports:
        BTreeMap<String, Arc<dyn ee_mcp::fake::FakeMcpTransportFactory>>,
}

impl std::fmt::Debug for McpPaneState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpPaneState")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .field("browse", &self.browse.as_ref().map(|b| b.kind))
            .field("proxy_code_actions", &self.proxy_code_actions.len())
            .finish_non_exhaustive()
    }
}

// ── Host worker ──────────────────────────────────────────────────────────────

/// Commands executed sequentially on the MCP host worker thread.
enum McpHostCommand {
    StartAll,
    ListPrompts {
        reply: std_mpsc::Sender<Result<Vec<serde_json::Value>, String>>,
    },
    GetPrompt {
        key: String,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    ListResources {
        reply: std_mpsc::Sender<Result<Vec<serde_json::Value>, String>>,
    },
    ListTools {
        reply: std_mpsc::Sender<Result<Vec<String>, String>>,
    },
    RefreshRegistry {
        server_id: String,
    },
    #[cfg(test)]
    InstallFake {
        server_id: String,
        factory: Arc<dyn ee_mcp::fake::FakeMcpTransportFactory>,
    },
    Shutdown,
}
pub(crate) fn mcp_forward_entries(settings: &crate::config::McpSettings) -> Vec<McpServer> {
    let mut entries = Vec::new();
    for (id, server) in &settings.servers {
        match server {
            crate::config::McpServerSettings::Stdio { command, args, env, cwd } => {
                let mut stdio = McpServerStdio::new(id.clone(), command.clone()).args(args.clone());
                let variables = env
                    .iter()
                    .map(|(name, value)| EnvVariable::new(name.clone(), value.clone()))
                    .collect();
                stdio = stdio.env(variables);
                if let Some(cwd) = cwd {
                    let mut meta = serde_json::Map::new();
                    meta.insert(
                        String::from("ee"),
                        serde_json::json!({ "cwd": cwd.display().to_string() }),
                    );
                    stdio = stdio.meta(meta);
                }
                entries.push(McpServer::Stdio(stdio));
            }
            crate::config::McpServerSettings::StreamableHttp { url, headers, .. } => {
                let header_values = headers
                    .iter()
                    .map(|(name, value)| HttpHeader::new(name.clone(), value.clone()))
                    .collect();
                entries.push(McpServer::Http(
                    McpServerHttp::new(id.clone(), url.clone()).headers(header_values),
                ));
            }
        }
    }
    entries
}

/// The stdio `ee --mcp-proxy` fallback entry for the ee proxy.
///
/// Used only when the selected agent does not advertise ACP-native
/// MCP-over-ACP support; `ee-agent-host` swaps it for an `McpServer::Acp`
/// entry when the agent supports MCP-over-ACP, so the two modes are never
/// both advertised for server id `ee`.
pub(crate) fn proxy_stdio_fallback_entry(info: &ProxyInfo) -> McpServerStdio {
    let command = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ee"));
    McpServerStdio::new("ee", command)
        .args(vec![String::from("--mcp-proxy")])
        .env(vec![
            EnvVariable::new("EE_MCP_PROXY_SOCKET", info.socket_path.display().to_string()),
            EnvVariable::new("EE_MCP_PROXY_TOKEN", info.token.clone()),
        ])
        .meta({
            let mut meta = serde_json::Map::new();
            meta.insert(String::from("ee"), serde_json::json!({ "proxy": true }));
            meta
        })
}
fn browse_item_from_value(kind: &McpBrowseKind, value: serde_json::Value) -> Option<McpBrowseItem> {
    let key = value.get("key")?.as_str()?.to_string();
    let title = value.get("title").and_then(serde_json::Value::as_str).unwrap_or(&key).to_string();
    let detail =
        value.get("description").and_then(serde_json::Value::as_str).map(ToOwned::to_owned);
    let insert = match kind {
        McpBrowseKind::Tools | McpBrowseKind::Prompts => key,
        McpBrowseKind::Resources => {
            value.get("uri").and_then(serde_json::Value::as_str)?.to_string()
        }
    };
    Some(McpBrowseItem { label: title, insert, detail })
}

/// Builds raw MCP settings from resolved ee-cli settings.
fn raw_server_settings(
    settings: &crate::config::McpServerSettings,
) -> ee_mcp::RawMcpServerSettings {
    match settings {
        crate::config::McpServerSettings::Stdio { command, args, env, cwd } => {
            ee_mcp::RawMcpServerSettings {
                stdio: Some(ee_mcp::RawStdioSettings {
                    command: command.clone(),
                    args: args.clone(),
                    env: env.clone(),
                    cwd: cwd.clone(),
                    stderr_cap: None,
                }),
                streamable_http: None,
                timeout_ms: None,
            }
        }
        crate::config::McpServerSettings::StreamableHttp { url, headers, timeout_ms } => {
            ee_mcp::RawMcpServerSettings {
                stdio: None,
                streamable_http: Some(ee_mcp::RawStreamableHttpSettings {
                    url: url.clone(),
                    headers: headers.clone(),
                }),
                timeout_ms: Some(*timeout_ms),
            }
        }
    }
}

/// A per-run proxy socket path under the temp directory.
fn proxy_socket_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = format!("ee-mcp-proxy-{}-{nonce}-{sequence}.sock", std::process::id());
    let socket_path = std::env::temp_dir().join(&file_name);
    if socket_path.as_os_str().as_encoded_bytes().len() < 100 {
        socket_path
    } else {
        PathBuf::from("/tmp").join(file_name)
    }
}

/// A per-run proxy auth token (never logged).
fn proxy_token() -> String {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("ee-proxy-{}-{nonce:x}", std::process::id())
}

mod app_impl;
mod host_bridge;
mod proxy_calls;
mod proxy_server;
mod socket_backend;
mod stdio;

pub(crate) use host_bridge::McpHostBridge;
pub(crate) use proxy_calls::ProxyToolCall;
#[allow(unused_imports)]
pub(crate) use stdio::run_proxy_stdio;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use stdio::run_proxy_stdio_with_duplex;

#[cfg(test)]
mod tests;
