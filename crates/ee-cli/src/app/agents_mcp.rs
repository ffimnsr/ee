//! Phase 6 + 6b: MCP integration for the agents pane.
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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Runs the MCP manager on a dedicated worker thread (its own single-threaded
/// runtime).  The manager supervises reconnects in the background; commands
/// execute sequentially and every operation is internally bounded by the
/// server's request timeout, so a hung server can never wedge the worker.
fn mcp_host_worker(
    runtime: tokio::runtime::Runtime,
    manager: McpClientManager,
    proxy: Option<ProxyInfo>,
    bridge_tx: std_mpsc::Sender<BridgeUiMessage>,
    mut rx: tokio_mpsc::UnboundedReceiver<McpHostCommand>,
) {
    runtime.block_on(async move {
        let shutdown = tokio_util::sync::CancellationToken::new();
        // The proxy listener accepts connections from agent-spawned
        // `ee --mcp-proxy` processes and routes tool calls through the same
        // approval bridge as direct ACP client methods.
        if let Some(info) = proxy.clone() {
            let bridge = bridge_tx.clone();
            tokio::spawn(serve_proxy_listener(info, bridge, shutdown.clone()));
        }
        while let Some(command) = rx.recv().await {
            match command {
                McpHostCommand::StartAll => {
                    let _ = manager.start_all().await;
                }
                McpHostCommand::ListPrompts { reply } => {
                    let result = list_prompts_values(&manager).await;
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                McpHostCommand::GetPrompt { key, reply } => {
                    let result: Result<String, ee_mcp::McpError> = async {
                        let server = namespaced_server(&key)?;
                        let result = manager.get_prompt(&server, &key, None).await?;
                        Ok(ee_mcp::prompt_text(&result))
                    }
                    .await;
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                McpHostCommand::ListResources { reply } => {
                    let result = list_resources_values(&manager).await;
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                McpHostCommand::ListTools { reply } => {
                    let result = list_tool_keys(&manager).await;
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                McpHostCommand::RefreshRegistry { server_id } => {
                    let _ = manager.refresh_registry(&server_id).await;
                }
                #[cfg(test)]
                McpHostCommand::InstallFake { server_id, factory } => {
                    manager.install_fake_transport(&server_id, factory).await;
                }
                McpHostCommand::Shutdown => break,
            }
        }
        shutdown.cancel();
        manager.shutdown().await;
        if let Some(info) = &proxy {
            let _ = std::fs::remove_file(&info.socket_path);
        }
    });
}

/// Lists prompts across every ready server as browse values
/// (`{key, title, description}`).  Per-server failures become visible
/// `<error>` entries; browsing never fails the whole pane.
async fn list_prompts_values(
    manager: &McpClientManager,
) -> Result<Vec<serde_json::Value>, ee_mcp::McpError> {
    let mut values = Vec::new();
    for server_id in manager.server_ids() {
        if manager.state(&server_id).await != Some(McpServerState::Ready) {
            continue;
        }
        match manager.list_prompts(&server_id).await {
            Ok(entries) => {
                for entry in entries {
                    values.push(serde_json::json!({
                        "key": entry.key,
                        "title": entry.prompt.name,
                        "description": entry.prompt.description,
                    }));
                }
            }
            Err(error) => {
                values.push(serde_json::json!({
                    "key": format!("{server_id}/<error>"),
                    "title": "<error>",
                    "description": error.to_string(),
                }));
            }
        }
    }
    values.sort_by(|a, b| {
        a.get("key")
            .and_then(serde_json::Value::as_str)
            .cmp(&b.get("key").and_then(serde_json::Value::as_str))
    });
    Ok(values)
}

/// Lists resources across every ready server as browse values
/// (`{key, title, uri, description}`).
async fn list_resources_values(
    manager: &McpClientManager,
) -> Result<Vec<serde_json::Value>, ee_mcp::McpError> {
    let mut values = Vec::new();
    for server_id in manager.server_ids() {
        if manager.state(&server_id).await != Some(McpServerState::Ready) {
            continue;
        }
        if let Ok(entries) = manager.list_resources(&server_id).await {
            for entry in entries {
                let uri = entry.resource.uri.to_string();
                let title = if entry.resource.name.is_empty() {
                    uri.clone()
                } else {
                    entry.resource.name.clone()
                };
                values.push(serde_json::json!({
                    "key": format!("{server_id}/{uri}"),
                    "title": title,
                    "uri": uri,
                    "description": entry.resource.description,
                }));
            }
        }
    }
    values.sort_by(|a, b| {
        a.get("key")
            .and_then(serde_json::Value::as_str)
            .cmp(&b.get("key").and_then(serde_json::Value::as_str))
    });
    Ok(values)
}

/// Lists namespaced tool keys across every ready server.
async fn list_tool_keys(manager: &McpClientManager) -> Result<Vec<String>, ee_mcp::McpError> {
    let mut tools = Vec::new();
    for server_id in manager.server_ids() {
        if manager.state(&server_id).await != Some(McpServerState::Ready) {
            continue;
        }
        if let Ok(entries) = manager.list_tools(&server_id).await {
            tools.extend(entries.into_iter().map(|entry| entry.key));
        }
    }
    tools.sort();
    Ok(tools)
}

/// The server id embedded in a `<server_id>/<name>` key.
fn namespaced_server(key: &str) -> Result<String, ee_mcp::McpError> {
    match key.split_once('/') {
        Some((server, name)) if !name.is_empty() => Ok(server.to_string()),
        _ => Err(ee_mcp::McpError::NotFound(format!("invalid namespaced key {key:?}"))),
    }
}

/// Owns the MCP host: manager worker, event receiver, and command channel.
pub(crate) struct McpHostBridge {
    pub(crate) events: tokio_mpsc::UnboundedReceiver<McpEvent>,
    /// Configured server ids (identity for the health registry).
    pub(crate) server_ids: Vec<String>,
    commands: tokio_mpsc::UnboundedSender<McpHostCommand>,
}

impl McpHostBridge {
    fn new(
        manager: McpClientManager,
        events: tokio_mpsc::UnboundedReceiver<McpEvent>,
        proxy: Option<ProxyInfo>,
        bridge_tx: std_mpsc::Sender<BridgeUiMessage>,
    ) -> Self {
        let server_ids = manager.server_ids();
        let (commands_tx, commands_rx) = tokio_mpsc::unbounded_channel();
        let runtime =
            TokioBuilder::new_current_thread().enable_all().build().expect("mcp host runtime");
        std::thread::Builder::new()
            .name(String::from("ee-mcp-host"))
            .spawn(move || mcp_host_worker(runtime, manager, proxy, bridge_tx, commands_rx))
            .expect("spawn mcp host worker");
        Self { events, server_ids, commands: commands_tx }
    }

    /// Starts every configured server (results arrive as state events).
    fn start_all(&self) {
        let _ = self.commands.send(McpHostCommand::StartAll);
    }

    /// Lists prompts from every ready server as browse values.
    fn list_prompts(&self) -> std_mpsc::Receiver<Result<Vec<serde_json::Value>, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(McpHostCommand::ListPrompts { reply: reply_tx });
        reply_rx
    }

    /// Fetches one prompt's content (namespaced key).
    fn get_prompt(&self, key: &str) -> std_mpsc::Receiver<Result<String, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ =
            self.commands.send(McpHostCommand::GetPrompt { key: key.to_string(), reply: reply_tx });
        reply_rx
    }

    /// Lists resources from every ready server as browse values.
    fn list_resources(&self) -> std_mpsc::Receiver<Result<Vec<serde_json::Value>, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(McpHostCommand::ListResources { reply: reply_tx });
        reply_rx
    }

    /// Lists namespaced tool keys from every ready server.
    fn list_tools(&self) -> std_mpsc::Receiver<Result<Vec<String>, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(McpHostCommand::ListTools { reply: reply_tx });
        reply_rx
    }

    /// Invalidates a server's primitive registries (list-changed notification).
    fn refresh_registry(&self, server_id: &str) {
        let _ = self
            .commands
            .send(McpHostCommand::RefreshRegistry { server_id: server_id.to_string() });
    }

    /// Installs a fake transport factory (test-utils only; sent before the
    /// worker's `StartAll` so the first connection uses the fake).
    #[cfg(test)]
    fn install_fake(
        &self,
        server_id: &str,
        factory: Arc<dyn ee_mcp::fake::FakeMcpTransportFactory>,
    ) {
        let _ = self
            .commands
            .send(McpHostCommand::InstallFake { server_id: server_id.to_string(), factory });
    }
}

impl Drop for McpHostBridge {
    fn drop(&mut self) {
        // The worker shuts the manager down and removes the proxy socket.
        let _ = self.commands.send(McpHostCommand::Shutdown);
    }
}

// ── MCP config forwarding (ACP `session/new` mcpServers) ────────────────────

/// Converts resolved `McpSettings` into ACP `mcpServers` entries.
///
/// Stdio cwd (absent from the ACP v1 `McpServerStdio` shape) travels in the
/// `_meta` extensibility field.  Header/env values are forwarded verbatim to
/// the agent (it needs them to connect); they are never logged here.
///
/// The ee proxy entry is *not* built here: `ee-agent-host` appends it to
/// `session/new` after capability negotiation (ACP-native `McpServer::Acp`
/// when the agent advertises `mcp_capabilities.acp`, the stdio fallback
/// entry from [`proxy_stdio_fallback_entry`] otherwise).
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

// ── Proxy IPC ────────────────────────────────────────────────────────────────

/// One proxy tool call (socket + stdio subprocess share this shape).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub(crate) enum ProxyCall {
    ReadTextFile {
        path: String,
        line: Option<u32>,
        limit: Option<u32>,
    },
    WriteTextFile {
        path: String,
        content: String,
    },
    TerminalCreate {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        cwd: Option<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    Diagnostics,
}

/// The reply to one proxy tool call.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum ProxyReply {
    Ok { text: String },
    Err { error: ProxyErrorBody },
}

/// Error body of a denied/failed proxy tool call.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct ProxyErrorBody {
    pub(crate) message: String,
    pub(crate) denied: bool,
}

impl ProxyReply {
    fn from_client_result(result: ClientRequestResult) -> Self {
        match result {
            Ok(ClientRequestResponse::ReadTextFile(response)) => {
                Self::Ok { text: response.content }
            }
            Ok(ClientRequestResponse::WriteTextFile(_)) => Self::Ok { text: String::from("ok") },
            Ok(ClientRequestResponse::CreateTerminal(response)) => {
                Self::Ok { text: response.terminal_id.0.to_string() }
            }
            // Diagnostics are carried as terminal-output text internally
            // (transport-only mapping; never crosses the ACP wire).
            Ok(ClientRequestResponse::TerminalOutput(response)) => {
                Self::Ok { text: response.output }
            }
            Ok(_) => Self::Ok { text: String::from("ok") },
            Err(error) => Self::Err {
                error: ProxyErrorBody {
                    message: error.to_string(),
                    denied: matches!(error, AgentError::PermissionDenied { .. }),
                },
            },
        }
    }
}

/// Reads one bounded line from a buffered stream.
async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let read = tokio::io::AsyncBufReadExt::read_line(reader, &mut line).await?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame exceeds the {cap}-byte cap"),
        ));
    }
    Ok(Some(line.trim_end().to_string()))
}

/// A proxy tool call forwarded to the pane (bridge message payload).  The
/// requests use the same ACP wire types as direct client methods so the
/// approval and bridge paths are shared verbatim.
#[derive(Debug)]
pub(crate) enum ProxyToolCall {
    Read(ReadTextFileRequest),
    Write(WriteTextFileRequest),
    Terminal(CreateTerminalRequest),
    Diagnostics,
}

/// Editor side of the proxy: verifies the token, then serves tool calls until
/// EOF or a frame-cap violation (fail closed).
async fn serve_proxy_connection(
    mut stream: tokio::net::UnixStream,
    token: String,
    bridge_tx: std_mpsc::Sender<BridgeUiMessage>,
) {
    use tokio::io::AsyncWriteExt;
    let (read_half, mut write_half) = stream.split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let Ok(Some(first)) = read_bounded_line(&mut reader, PROXY_TOKEN_MAX_BYTES).await else {
        return;
    };
    if first != token {
        let _ = write_half.write_all(b"{\"error\":\"bad token\"}\n").await;
        return;
    }
    loop {
        let Ok(Some(line)) = read_bounded_line(&mut reader, PROXY_MAX_FRAME_BYTES).await else {
            return;
        };
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            let _ = write_half.write_all(b"{\"error\":\"invalid frame\"}\n").await;
            return;
        };
        let Some(id) = request.get("id").cloned() else {
            continue; // notification frames are ignored
        };
        let params = request.get("params").cloned().unwrap_or_else(|| serde_json::json!({}));
        let reply = match serde_json::from_value::<ProxyCall>(params) {
            Ok(call) => proxy_call_to_bridge(call, &bridge_tx).await,
            Err(_) => ProxyReply::Err {
                error: ProxyErrorBody {
                    message: String::from("invalid proxy call"),
                    denied: false,
                },
            },
        };
        let frame = serde_json::json!({ "id": id, "result": reply });
        let _ = write_half.write_all(frame.to_string().as_bytes()).await;
        let _ = write_half.write_all(b"\n").await;
        let _ = write_half.flush().await;
    }
}

/// Routes one proxy tool call through the same bridge/approval paths as ACP
/// client methods.  File reads are served directly; writes and terminal
/// creates queue an approval prompt.
async fn proxy_call_to_bridge(
    call: ProxyCall,
    bridge_tx: &std_mpsc::Sender<BridgeUiMessage>,
) -> ProxyReply {
    let session_id = SessionId::new("proxy");
    let call = match call {
        ProxyCall::ReadTextFile { path, line, limit } => {
            let mut request = ReadTextFileRequest::new(session_id, path);
            request.line = line;
            request.limit = limit;
            ProxyToolCall::Read(request)
        }
        ProxyCall::WriteTextFile { path, content } => {
            ProxyToolCall::Write(WriteTextFileRequest::new(session_id, path, content))
        }
        ProxyCall::TerminalCreate { command, args, cwd, env } => {
            let mut request = CreateTerminalRequest::new(session_id, command);
            request.args = args;
            if let Some(cwd) = cwd {
                request.cwd = Some(PathBuf::from(cwd));
            }
            request.env =
                env.into_iter().map(|(name, value)| EnvVariable::new(name, value)).collect();
            ProxyToolCall::Terminal(request)
        }
        ProxyCall::Diagnostics => ProxyToolCall::Diagnostics,
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if bridge_tx.send(BridgeUiMessage::ProxyTool { call, reply: reply_tx }).is_err() {
        return ProxyReply::Err {
            error: ProxyErrorBody {
                message: String::from("editor is shutting down"),
                denied: false,
            },
        };
    }
    match reply_rx.await {
        Ok(result) => ProxyReply::from_client_result(result),
        Err(_) => ProxyReply::Err {
            error: ProxyErrorBody {
                message: String::from("approval channel closed"),
                denied: false,
            },
        },
    }
}

/// Binds the proxy Unix socket and serves connections until shutdown.
async fn serve_proxy_listener(
    info: ProxyInfo,
    bridge_tx: std_mpsc::Sender<BridgeUiMessage>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let listener = match tokio::net::UnixListener::bind(&info.socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("ee: warning: mcp proxy listener bind failed: {error}");
            return;
        }
    };
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let token = info.token.clone();
                        let bridge = bridge_tx.clone();
                        connections.spawn(async move {
                            serve_proxy_connection(stream, token, bridge).await;
                        });
                    }
                    Err(error) => {
                        eprintln!("ee: warning: mcp proxy accept failed: {error}");
                    }
                }
            }
        }
    }
    connections.abort_all();
}

// ── Proxy subprocess mode (`ee --mcp-proxy`) ────────────────────────────────

/// A sync socket backend forwarding tool calls to the editor's proxy
/// listener.  The backend blocks while the editor approves/executes the
/// call; in the proxy subprocess there is nothing else to do meanwhile.
struct SocketProxyBackend {
    inner: std::sync::Mutex<SocketProxyState>,
}

/// The socket state (writer + reader share the same connection).
struct SocketProxyState {
    writer: std::io::BufWriter<std::os::unix::net::UnixStream>,
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
}

impl SocketProxyBackend {
    fn connect(socket: &PathBuf, token: &str) -> std::io::Result<Self> {
        use std::io::Write;
        let stream = std::os::unix::net::UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(120)))?;
        let mut writer = std::io::BufWriter::new(stream.try_clone()?);
        writeln!(writer, "{token}")?;
        writer.flush()?;
        Ok(Self {
            inner: std::sync::Mutex::new(SocketProxyState {
                writer,
                reader: std::io::BufReader::new(stream),
            }),
        })
    }

    fn call(&self, call: ProxyCall) -> Result<String, String> {
        use std::io::{BufRead, Write};
        let mut state = self.inner.lock().expect("proxy socket poisoned");
        let frame = serde_json::json!({ "id": 1, "params": call });
        writeln!(state.writer, "{frame}").map_err(|error| error.to_string())?;
        state.writer.flush().map_err(|error| error.to_string())?;
        let mut response = String::new();
        let read = state.reader.read_line(&mut response).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err(String::from("editor closed the proxy connection"));
        }
        if response.len() > PROXY_MAX_FRAME_BYTES {
            return Err(String::from("proxy reply exceeds the frame cap"));
        }
        let value: serde_json::Value =
            serde_json::from_str(response.trim_end()).map_err(|error| error.to_string())?;
        let result =
            value.get("result").ok_or_else(|| String::from("proxy reply missing result"))?;
        if let Some(error) = result.get("error") {
            return Err(error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("proxy error")
                .to_string());
        }
        result
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| String::from("proxy reply missing text"))
    }
}

impl ee_mcp::EeProxyBackend for SocketProxyBackend {
    fn read_text_file(
        &self,
        path: String,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, ee_mcp::ProxyToolError> {
        let call = ProxyCall::ReadTextFile { path, line, limit };
        self.call(call)
            .map_err(|message| ee_mcp::ProxyToolError { message, is_permission_denied: false })
    }

    fn write_text_file(&self, path: String, content: String) -> Result<(), ee_mcp::ProxyToolError> {
        let call = ProxyCall::WriteTextFile { path, content };
        let _ = self
            .call(call)
            .map_err(|message| ee_mcp::ProxyToolError { message, is_permission_denied: false })?;
        Ok(())
    }

    fn terminal_create(
        &self,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<String, ee_mcp::ProxyToolError> {
        let call = ProxyCall::TerminalCreate { command, args, cwd, env };
        self.call(call)
            .map_err(|message| ee_mcp::ProxyToolError { message, is_permission_denied: false })
    }

    fn diagnostics(&self) -> Vec<String> {
        self.call(ProxyCall::Diagnostics)
            .map(|text| text.lines().map(ToOwned::to_owned).collect())
            .unwrap_or_default()
    }
}

/// rmcp server transport over the proxy subprocess's own stdin/stdout.
///
/// Handrolled because rmcp ships no stdio transport for the server role
/// (SDK gap; the client-side `TokioChildProcess` covers only spawners).  The
/// frame cap is enforced on receive (fail closed).
struct StdioProxyTransport {
    reader: tokio::io::BufReader<tokio::io::Stdin>,
}

impl StdioProxyTransport {
    fn new() -> Self {
        Self { reader: tokio::io::BufReader::new(tokio::io::stdin()) }
    }
}

impl rmcp::transport::Transport<rmcp::service::RoleServer> for StdioProxyTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::service::RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        use tokio::io::AsyncWriteExt;
        let line = serde_json::to_string(&item).unwrap_or_else(|_| "{}".to_string());
        async move {
            let mut stdout = tokio::io::stdout();
            stdout.write_all(line.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await
        }
    }

    async fn receive(
        &mut self,
    ) -> Option<rmcp::service::RxJsonRpcMessage<rmcp::service::RoleServer>> {
        let line = read_bounded_line(&mut self.reader, PROXY_MAX_FRAME_BYTES).await.ok()?;
        let line = line?;
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(&line).ok()
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Runs the proxy subprocess: connects to the editor's proxy socket, then
/// serves the [`ee_mcp::EeMcpProxy`] surface over this process's stdio.
///
/// The editor's listener verifies the token; the agent (which spawned this
/// process) speaks MCP 2026-07-28 over stdin/stdout.
pub(crate) fn run_proxy_stdio(socket: PathBuf, token: String) -> std::io::Result<()> {
    let backend = SocketProxyBackend::connect(&socket, &token)?;
    let proxy = ee_mcp::EeMcpProxy::new(Arc::new(backend));
    let runtime = TokioBuilder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    runtime.block_on(async move {
        let _ = rmcp::serve_server(proxy, StdioProxyTransport::new()).await;
    });
    Ok(())
}

// ── App integration ──────────────────────────────────────────────────────────

impl App {
    /// Whether any MCP server is configured (host starts lazily on open).
    pub(super) fn mcp_servers_configured(&self) -> bool {
        !self.config.mcp.servers.is_empty() || self.config.mcp.proxy.enabled
    }

    /// Creates the MCP host bridge on first use (lazy; starts no process
    /// until the worker's `StartAll` runs).
    fn ensure_mcp_host(&mut self) {
        if self.agents.mcp.host.is_some() {
            return;
        }
        let raw: BTreeMap<String, ee_mcp::RawMcpServerSettings> = self
            .config
            .mcp
            .servers
            .iter()
            .map(|(id, settings)| (id.clone(), raw_server_settings(settings)))
            .collect();
        let configs = match ee_mcp::config::resolve_server_configs(raw) {
            Ok(configs) => configs,
            Err(error) => {
                self.agents.mcp.error = Some(format!("invalid mcp config: {error}"));
                return;
            }
        };
        let proxy = if self.config.mcp.proxy.enabled {
            Some(ProxyInfo { socket_path: proxy_socket_path(), token: proxy_token() })
        } else {
            None
        };
        self.agents.mcp.proxy = proxy.clone();
        let (events_tx, events_rx) = tokio_mpsc::unbounded_channel();
        let manager = McpClientManager::new(configs, events_tx);
        let bridge = McpHostBridge::new(manager, events_rx, proxy, self.agents.bridge_tx.clone());
        #[cfg(test)]
        for (id, factory) in &self.agents.mcp.test_fake_transports {
            bridge.install_fake(id, factory.clone());
        }
        self.agents.mcp.host = Some(bridge);
    }

    /// Starts the MCP host lazily when the pane opens and servers exist.
    pub(super) fn start_mcp_servers(&mut self) {
        if !self.mcp_servers_configured() {
            return;
        }
        self.ensure_mcp_host();
        if let Some(host) = &self.agents.mcp.host {
            for id in &host.server_ids {
                self.agents.mcp.servers.entry(id.clone()).or_default();
            }
            host.start_all();
        }
    }

    /// Drains MCP host events into the pane state.
    pub(super) fn pump_mcp_events(&mut self) {
        let events = {
            let Some(host) = &mut self.agents.mcp.host else {
                return;
            };
            let mut events = Vec::new();
            while let Ok(event) = host.events.try_recv() {
                events.push(event);
            }
            events
        };
        for event in events {
            self.handle_mcp_event(event);
        }
    }

    fn handle_mcp_event(&mut self, event: McpEvent) {
        match event {
            McpEvent::ServerState { server_id, state } => {
                let server = self.agents.mcp.servers.entry(server_id).or_default();
                server.state = state;
                if state == McpServerState::Failed {
                    server.error = Some(String::from("connection failed; retrying in background"));
                }
                // Non-fatal: MCP health never blocks the ACP chat.
            }
            McpEvent::Discovery { server_id, snapshot } => {
                let server = self.agents.mcp.servers.entry(server_id.clone()).or_default();
                server.apply_discovery(&snapshot);
                server.error = None;
                // Prime the tool metadata for the browse picker.
                if let Some(host) = &self.agents.mcp.host {
                    let reply = host.list_tools();
                    self.agents.mcp.pending_tools.insert(server_id, reply);
                }
            }
            McpEvent::Elicitation(_) => {
                // MCP elicitation requires host UI; dropping the handle
                // declines it (the manager resolves the request as declined).
            }
            McpEvent::Diagnostics { server_id, message } => {
                if let Some(server) = self.agents.mcp.servers.get_mut(&server_id) {
                    server.error = Some(message);
                }
            }
            McpEvent::ToolListChanged { server_id } => {
                if let Some(host) = &self.agents.mcp.host {
                    host.refresh_registry(&server_id);
                    let reply = host.list_tools();
                    self.agents.mcp.pending_tools.insert(server_id, reply);
                }
            }
            McpEvent::ResourceListChanged { server_id } => {
                if let Some(host) = &self.agents.mcp.host {
                    host.refresh_registry(&server_id);
                }
            }
            McpEvent::PromptListChanged { server_id } => {
                if let Some(host) = &self.agents.mcp.host {
                    host.refresh_registry(&server_id);
                }
            }
            _ => {}
        }
    }

    /// Polls pending MCP replies (browse lists, prompt fetches, tool lists).
    pub(super) fn pump_mcp_replies(&mut self) {
        // Per-server tool metadata refreshes.
        let tools = std::mem::take(&mut self.agents.mcp.pending_tools);
        for (server_id, reply) in tools {
            if let Ok(Ok(keys)) = reply.try_recv() {
                if let Some(server) = self.agents.mcp.servers.get_mut(&server_id) {
                    server.tools = keys;
                }
            } else {
                self.agents.mcp.pending_tools.insert(server_id, reply);
            }
        }

        // Tools browse list.
        if let Some(reply) = self.agents.mcp.pending_browse_tools.take() {
            match reply.try_recv() {
                Ok(Ok(keys)) => {
                    if let Some(browse) = &mut self.agents.mcp.browse
                        && browse.kind == McpBrowseKind::Tools
                    {
                        browse.items = keys
                            .into_iter()
                            .map(|key| McpBrowseItem {
                                label: key.clone(),
                                insert: key,
                                detail: None,
                            })
                            .collect();
                        browse.loading = false;
                        browse.selected = 0;
                    }
                }
                Ok(Err(error)) => {
                    if let Some(browse) = &mut self.agents.mcp.browse {
                        browse.loading = false;
                        browse.error = Some(error);
                    }
                }
                Err(_) => self.agents.mcp.pending_browse_tools = Some(reply),
            }
        }

        let Some(browse) = &mut self.agents.mcp.browse else {
            return;
        };

        // Prompt/resource browse lists.
        if browse.loading
            && let Some(receiver) = browse.pending_list.as_ref()
        {
            match receiver.try_recv() {
                Ok(Ok(values)) => {
                    browse.pending_list = None;
                    browse.items = values
                        .into_iter()
                        .filter_map(|value| browse_item_from_value(&browse.kind, value))
                        .collect();
                    browse.loading = false;
                    browse.selected = 0;
                }
                Ok(Err(error)) => {
                    browse.pending_list = None;
                    browse.loading = false;
                    browse.error = Some(error);
                }
                Err(_) => {
                    // Still in flight; poll again next pump.
                }
            }
        }

        // Prompt content fetches: insert on success, close the browse state.
        if let Some(pending) = browse.pending_get.as_ref()
            && let Ok(Ok(text)) = pending.try_recv()
        {
            browse.pending_get = None;
            self.agents_mcp_insert(&text);
        }
    }

    /// `:agents_mcp [tools|prompts|resources|close]` — health detail and
    /// browse pickers.
    pub(super) fn agents_mcp_command(&mut self, tail: &str) {
        if !self.config.agents.enabled {
            self.backend.status_message = Some(self.agents_status_message());
            return;
        }
        if !self.mcp_servers_configured() {
            self.backend.status_message = Some(String::from(
                "no MCP servers configured (add `[mcp.servers.<id>]` to `.ee.toml`)",
            ));
            return;
        }
        if self.agents.layout == AgentPaneLayout::Closed {
            self.agents.layout = AgentPaneLayout::Right;
        }
        self.enter_agent_focus();
        self.start_mcp_servers();
        let kind = match tail.trim() {
            "" => {
                self.mcp_health_notice();
                return;
            }
            "tools" => Some(McpBrowseKind::Tools),
            "prompts" => Some(McpBrowseKind::Prompts),
            "resources" => Some(McpBrowseKind::Resources),
            "close" => {
                self.agents.mcp.browse = None;
                self.backend.status_message = Some(String::from("mcp browse closed"));
                return;
            }
            _ => {
                self.backend.status_message =
                    Some(String::from("usage: :agents_mcp [tools|prompts|resources|close]"));
                return;
            }
        };
        let Some(kind) = kind else {
            return;
        };
        self.agents.mcp.browse = Some(McpBrowseState {
            kind,
            items: Vec::new(),
            selected: 0,
            loading: true,
            error: None,
            pending_list: None,
            pending_get: None,
        });
        self.mcp_browse_request(kind);
        self.backend.status_message =
            Some(format!("mcp {} browse… (Enter insert, Esc close)", kind.label()));
    }

    /// Pushes a health notice into the active thread transcript.
    fn mcp_health_notice(&mut self) {
        let lines = self.mcp_health_lines();
        if let Some(active) = self.agents.active_thread_index() {
            for line in lines {
                self.agents.threads[active].push_system(line);
            }
        }
    }

    /// Deterministic health lines: per-server state, identity, capabilities.
    pub(crate) fn mcp_health_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(mode) = &self.agents.mcp.proxy_mode {
            lines.push(format!("mcp proxy ee: {mode}"));
        } else if self.config.mcp.proxy.enabled {
            lines.push(String::from("mcp proxy ee: pending (session not started)"));
        }
        if self.agents.mcp.servers.is_empty() {
            lines.push(String::from("mcp: no servers started"));
            return lines;
        }
        for (id, server) in &self.agents.mcp.servers {
            let mut line = format!("mcp {id} [{}]", server.state);
            if let Some(identity) = &server.identity {
                line.push_str(&format!(" · {identity}"));
            }
            if !server.capabilities.is_empty() {
                line.push_str(&format!(" · {}", server.capabilities));
            }
            if let Some(error) = &server.error {
                line.push_str(&format!(" · {error}"));
            }
            lines.push(line);
        }
        lines
    }

    /// Requests the browse list for `kind` from the host.
    fn mcp_browse_request(&mut self, kind: McpBrowseKind) {
        let Some(host) = &self.agents.mcp.host else {
            if let Some(browse) = &mut self.agents.mcp.browse {
                browse.loading = false;
                browse.error = Some(String::from("mcp host not started"));
            }
            return;
        };
        match kind {
            McpBrowseKind::Tools => {
                self.agents.mcp.pending_browse_tools = Some(host.list_tools());
            }
            McpBrowseKind::Prompts => {
                if let Some(browse) = &mut self.agents.mcp.browse {
                    browse.pending_list = Some(host.list_prompts());
                }
            }
            McpBrowseKind::Resources => {
                if let Some(browse) = &mut self.agents.mcp.browse {
                    browse.pending_list = Some(host.list_resources());
                }
            }
        }
    }

    /// Inserts browse text into the active thread's prompt draft.
    fn agents_mcp_insert(&mut self, text: &str) {
        if let Some(active) = self.agents.active_thread_index() {
            let draft = &mut self.agents.threads[active].draft;
            if !draft.is_empty() && !draft.ends_with(' ') {
                draft.push(' ');
            }
            draft.push_str(text);
        }
        self.agents.mcp.browse = None;
        self.backend.status_message = Some(String::from("mcp item inserted into prompt draft"));
    }

    /// Moves the browse selection (wraps like IRC channel switching).
    pub(super) fn agents_mcp_select(&mut self, delta: isize) {
        let Some(browse) = &mut self.agents.mcp.browse else {
            return;
        };
        if browse.items.is_empty() {
            return;
        }
        let len = browse.items.len();
        browse.selected = (browse.selected as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// Confirms the selected browse item (Enter).
    pub(super) fn agents_mcp_confirm(&mut self) {
        let Some(browse) = &self.agents.mcp.browse else {
            return;
        };
        if browse.loading || browse.items.is_empty() {
            return;
        }
        let Some(item) = browse.items.get(browse.selected).cloned() else {
            return;
        };
        match browse.kind {
            McpBrowseKind::Tools | McpBrowseKind::Resources => {
                self.agents_mcp_insert(&item.insert);
            }
            McpBrowseKind::Prompts => {
                // Prompt content is fetched before insertion; the namespaced
                // key is the insert text.
                if let Some(host) = &self.agents.mcp.host {
                    let reply = host.get_prompt(&item.insert);
                    if let Some(browse) = &mut self.agents.mcp.browse {
                        browse.pending_get = Some(reply);
                    }
                }
            }
        }
    }

    /// Shuts the MCP host down (app quit): stops servers and proxy listener.
    pub(super) fn shutdown_mcp(&mut self) {
        if let Some(host) = self.agents.mcp.host.take() {
            drop(host);
        }
        self.agents.mcp.servers.clear();
        self.agents.mcp.browse = None;
        self.agents.mcp.proxy_mode = None;
    }
}

/// Builds a browse item from a list reply value.
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
    std::env::temp_dir()
        .join(format!("ee-mcp-proxy-{}-{nonce}-{sequence}.sock", std::process::id()))
}

/// A per-run proxy auth token (never logged).
fn proxy_token() -> String {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("ee-proxy-{}-{nonce:x}", std::process::id())
}
