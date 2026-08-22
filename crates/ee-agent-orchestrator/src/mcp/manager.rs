//! Per-session MCP client manager (Phase 12).
//!
//! [`McpSessionManager`] connects the session's advertised MCP servers for
//! one prompt turn: ACP-native servers through the framework's
//! [`ClientBridge`] (`mcp/connect`/`mcp/message`/`mcp/disconnect`), stdio
//! servers through an rmcp child-process transport.  Every connect, discover,
//! `tools/list`, and `tools/call` round is bounded by the configured timeout
//! and observes the prompt's cancellation watch; connections close on prompt
//! end (RAII + explicit shutdown).
//!
//! Discovery is fail closed per server: a server that cannot connect, cannot
//! initialize, fails `tools/list`, exposes an invalid schema, or collides on
//! a sanitized name contributes no tools and yields a bounded, secret-free
//! diagnostic.  Tool calls map `CallToolResult` into [`ToolResult`] —
//! including MCP `isError` results, which become failed tool results without
//! crashing the turn.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ee_acp_agent_server::ClientBridge;
use ee_agent_protocol::{DisconnectMcpRequest, McpConnectionId, McpServerAcpId, McpServerStdio};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, Implementation,
    InitializeRequestParams, ProtocolVersion, Tool,
};
use rmcp::service::{ClientLifecycleMode, RoleClient, RunningService};
use rmcp::{ClientHandler, ClientServiceExt};
use serde_json::Value;
use tokio::sync::{Mutex, watch};

use crate::destructive_policy::SideEffectSubclass;
use crate::policy::{PolicyContext, PolicyEngine};
use crate::tools::{SideEffectClass, ToolDefinition, ToolErrorKind, ToolResult};

use super::acp_transport::AcpBridgeTransport;
use super::descriptor::{McpServerDescriptor, McpTransportKind};
use super::names::{
    DisplayNameAllocator, EE_SERVER_NAME, has_disallowed_character, resolve_tool_name,
};
use super::policy::{McpToolPolicy, classify_tool, is_ee_proxy_tool};
use super::schema::convert_input_schema;

/// Minimal rmcp client handler: pins the ee identity and the 2026-07-28
/// protocol version; every other handler behavior stays at rmcp's fail-closed
/// defaults (sampling and elicitation requests are rejected).
///
/// `ee-mcp`'s `EeClientHandler` is deliberately not reused here: it is bound
/// to the CLI host's event channel and host-side elicitation round trips,
/// neither of which exist on the provider side.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OrchestratorClientHandler;

impl ClientHandler for OrchestratorClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("ee-agent-orchestrator", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
    }
}

/// One discovered MCP tool, still unallocated to a display name.
#[derive(Debug)]
struct ResolvedMcpTool {
    display_name: String,
    original_name: String,
    server_id: String,
    description: String,
    input_schema: Value,
    class: SideEffectClass,
    subclass: Option<SideEffectSubclass>,
    host_approval: bool,
}

/// One MCP tool registered with the orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct McpToolInfo {
    /// Provider-compatible name advertised to the model.
    pub display_name: String,
    /// The original MCP tool name used on the wire.
    pub original_name: String,
    /// The server's wire name.
    pub server_id: String,
    /// Human-readable description.
    pub description: String,
    /// The tool's argument JSON schema.
    pub input_schema: Value,
    /// Side-effect class driving policy decisions.
    pub class: SideEffectClass,
    /// Destructive side-effect subclass, when classified.
    pub subclass: Option<SideEffectSubclass>,
    /// Whether trusted editor host approval controls this tool's mutation.
    pub host_approval: bool,
}

impl McpToolInfo {
    fn to_definition(&self) -> ToolDefinition {
        let mut definition =
            ToolDefinition::new(self.display_name.clone(), self.description.clone())
                .input_schema(self.input_schema.clone())
                .side_effect_class(self.class);
        if let Some(subclass) = self.subclass {
            definition = definition.side_effect_subclass(subclass);
        }
        if self.host_approval {
            definition = definition.host_approval();
        }
        definition
    }
}

/// One bounded, secret-free discovery diagnostic surfaced to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpDiscoveryDiagnostic {
    /// The affected server's wire name (never a secret).
    pub server_id: String,
    /// Bounded message; never contains env/header values.
    pub message: String,
}

/// Dispatch target of one model-facing MCP tool name.
#[derive(Debug, Clone)]
struct McpDispatchTarget {
    connection: usize,
    original_name: String,
}

/// One live MCP connection (or a failed/skipped one).
struct McpConnection {
    descriptor: McpServerDescriptor,
    /// The running rmcp client service; `None` before connect, after
    /// shutdown, or when discovery failed.
    running: Arc<Mutex<Option<RunningService<RoleClient, OrchestratorClientHandler>>>>,
    /// The ACP connection id, when the server is ACP-native and
    /// `mcp/connect` succeeded; used for an explicit `mcp/disconnect` when
    /// the handshake fails before a service exists.
    acp: Option<McpConnectionId>,
    tools: Vec<McpToolInfo>,
}

/// Why a server's discovery failed (bounded, secret-free).
enum McpDiscoveryError {
    Bridge(String),
    Timeout(String),
    Protocol(String),
    Invalid(String),
}

impl std::fmt::Display for McpDiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bridge(reason) => write!(f, "connection failed: {reason}"),
            Self::Timeout(what) => write!(f, "{what} timed out"),
            Self::Protocol(reason) => write!(f, "protocol error: {reason}"),
            Self::Invalid(reason) => write!(f, "invalid server: {reason}"),
        }
    }
}

/// Per-session MCP client manager, scoped to one prompt turn.
///
/// The manager owns every MCP connection of the session for the duration of
/// the prompt: [`McpSessionManager::discover_all`] connects and lists tools
/// (fail closed per server), [`McpSessionManager::call_tool`] executes model
/// tool intents, and [`McpSessionManager::shutdown`] (also triggered on
/// drop) closes every connection.
pub(crate) struct McpSessionManager {
    bridge: ClientBridge,
    cancel: watch::Receiver<bool>,
    policy: McpToolPolicy,
    timeout: Duration,
    connections: Vec<McpConnection>,
    dispatch: std::sync::Mutex<HashMap<String, McpDispatchTarget>>,
    shutdown: AtomicBool,
    #[cfg(feature = "test-utils")]
    fake_stdio: Arc<Mutex<HashMap<String, Arc<dyn ee_mcp::fake::FakeMcpTransportFactory>>>>,
}

impl McpSessionManager {
    /// Creates a manager for the session's descriptors.
    #[must_use]
    pub(crate) fn new(
        descriptors: Vec<McpServerDescriptor>,
        bridge: ClientBridge,
        cancel: watch::Receiver<bool>,
        policy: McpToolPolicy,
    ) -> Self {
        let timeout = Duration::from_millis(policy.request_timeout_ms.max(1));
        let connections = descriptors
            .into_iter()
            .map(|descriptor| McpConnection {
                descriptor,
                running: Arc::new(Mutex::new(None)),
                acp: None,
                tools: Vec::new(),
            })
            .collect();
        Self {
            bridge,
            cancel,
            policy,
            timeout,
            connections,
            dispatch: std::sync::Mutex::new(HashMap::new()),
            shutdown: AtomicBool::new(false),
            #[cfg(feature = "test-utils")]
            fake_stdio: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Test-only: routes `server_id`'s stdio spawn through an in-process
    /// fake transport (deterministic, network-free).
    #[cfg(feature = "test-utils")]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn install_fake_stdio(
        &mut self,
        server_id: &str,
        factory: Arc<dyn ee_mcp::fake::FakeMcpTransportFactory>,
    ) {
        self.fake_stdio.lock().await.insert(server_id.to_string(), factory);
    }

    /// Whether any MCP server was configured for the session.
    #[must_use]
    pub(crate) fn has_servers(&self) -> bool {
        !self.connections.is_empty()
    }

    /// Whether the turn was cancelled.
    fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Connects every configured server, lists its tools, and resolves
    /// display names.  Fail closed per server: each failure yields a bounded
    /// diagnostic and contributes no tools.
    ///
    /// Cancellation is observed before each server and aborts discovery.
    pub(crate) async fn discover_all(&mut self) -> Vec<McpDiscoveryDiagnostic> {
        let mut diagnostics = Vec::new();
        let mut allocator = DisplayNameAllocator::new();
        for index in 0..self.connections.len() {
            if self.is_cancelled() {
                diagnostics.push(McpDiscoveryDiagnostic {
                    server_id: self.connections[index].descriptor.name.clone(),
                    message: "MCP discovery cancelled before this server connected".into(),
                });
                break;
            }
            let name = self.connections[index].descriptor.name.clone();
            match self.discover_one(index).await {
                Ok(tools) => {
                    for tool in tools {
                        match allocator.try_reserve(&tool.display_name) {
                            Ok(()) => {
                                self.connections[index].tools.push(tool.into_info());
                            }
                            Err(error) => diagnostics.push(McpDiscoveryDiagnostic {
                                server_id: name.clone(),
                                message: error,
                            }),
                        }
                    }
                }
                Err(error) => {
                    diagnostics.push(McpDiscoveryDiagnostic {
                        server_id: name.clone(),
                        message: format!("mcp server {:?} unavailable: {error}", name),
                    });
                }
            }
        }
        self.rebuild_dispatch();
        diagnostics
    }

    /// Registers display names of all discovered tools in the dispatch map.
    fn rebuild_dispatch(&mut self) {
        let mut dispatch = HashMap::new();
        for (index, connection) in self.connections.iter().enumerate() {
            for tool in &connection.tools {
                dispatch.insert(
                    tool.display_name.clone(),
                    McpDispatchTarget {
                        connection: index,
                        original_name: tool.original_name.clone(),
                    },
                );
            }
        }
        *self.dispatch.lock().expect("dispatch poisoned") = dispatch;
    }

    /// Connects one server and lists its tools (raw, unresolved names).
    async fn discover_one(
        &mut self,
        index: usize,
    ) -> Result<Vec<ResolvedMcpTool>, McpDiscoveryError> {
        let descriptor = self.connections[index].descriptor.clone();
        match descriptor.kind {
            McpTransportKind::Acp { server_id } => {
                self.discover_acp(index, &descriptor.name, &server_id).await
            }
            McpTransportKind::Stdio(stdio) => {
                self.discover_stdio(index, &descriptor.name, &stdio).await
            }
        }
    }

    async fn discover_acp(
        &mut self,
        index: usize,
        name: &str,
        server_id: &McpServerAcpId,
    ) -> Result<Vec<ResolvedMcpTool>, McpDiscoveryError> {
        let transport = tokio::time::timeout(
            self.timeout,
            AcpBridgeTransport::connect(&self.bridge, server_id, self.timeout),
        )
        .await
        .map_err(|_| McpDiscoveryError::Timeout("mcp/connect".into()))?
        .map_err(|error| McpDiscoveryError::Bridge(error.to_string()))?;
        let connection_id = transport.connection_id();
        let running = match tokio::time::timeout(self.timeout, serve_client(transport)).await {
            Ok(Ok(running)) => running,
            Ok(Err(error)) => {
                self.disconnect_acp(&connection_id).await;
                return Err(McpDiscoveryError::Protocol(format!(
                    "initialize for mcp server {name:?} failed: {error}"
                )));
            }
            Err(_) => {
                self.disconnect_acp(&connection_id).await;
                return Err(McpDiscoveryError::Timeout(format!(
                    "initialize for mcp server {name:?}"
                )));
            }
        };
        *self.connections[index].running.lock().await = Some(running);
        self.connections[index].acp = Some(connection_id);
        self.list_tools(index, name).await
    }

    /// Sends an explicit `mcp/disconnect` when the handshake failed before a
    /// service existed; best-effort and bounded.
    async fn disconnect_acp(&self, connection_id: &McpConnectionId) {
        let _ = tokio::time::timeout(
            self.timeout,
            self.bridge.mcp_disconnect(DisconnectMcpRequest::new(connection_id.clone())),
        )
        .await;
    }

    async fn discover_stdio(
        &mut self,
        index: usize,
        name: &str,
        stdio: &McpServerStdio,
    ) -> Result<Vec<ResolvedMcpTool>, McpDiscoveryError> {
        #[cfg(feature = "test-utils")]
        {
            let fake = { self.fake_stdio.lock().await.get(name).cloned() };
            if let Some(factory) = fake {
                let transport = factory.build();
                let running = tokio::time::timeout(self.timeout, serve_client(transport))
                    .await
                    .map_err(|_| {
                        McpDiscoveryError::Timeout(format!("initialize for mcp server {name:?}"))
                    })?
                    .map_err(|error| {
                        McpDiscoveryError::Protocol(format!(
                            "initialize for mcp server {name:?} failed: {error}"
                        ))
                    })?;
                *self.connections[index].running.lock().await = Some(running);
                return self.list_tools(index, name).await;
            }
        }
        let transport = spawn_stdio(stdio, name).map_err(McpDiscoveryError::Invalid)?;
        let running = tokio::time::timeout(self.timeout, serve_client(transport))
            .await
            .map_err(|_| McpDiscoveryError::Timeout(format!("initialize for mcp server {name:?}")))?
            .map_err(|error| {
                McpDiscoveryError::Protocol(format!(
                    "initialize for mcp server {name:?} failed: {error}"
                ))
            })?;
        *self.connections[index].running.lock().await = Some(running);
        self.list_tools(index, name).await
    }

    /// Runs `tools/list` (paginated) and converts the tools.
    async fn list_tools(
        &self,
        index: usize,
        name: &str,
    ) -> Result<Vec<ResolvedMcpTool>, McpDiscoveryError> {
        // Discovery is single-threaded per connection, so the guard may be
        // held across the list round.
        let mut guard = self.connections[index].running.lock().await;
        let Some(running) = guard.as_mut() else {
            return Err(McpDiscoveryError::Bridge("connection not established".into()));
        };
        let tools = tokio::time::timeout(self.timeout, running.list_all_tools())
            .await
            .map_err(|_| McpDiscoveryError::Timeout(format!("tools/list for mcp server {name:?}")))?
            .map_err(|error| {
                McpDiscoveryError::Protocol(format!(
                    "tools/list for mcp server {name:?} failed: {error}"
                ))
            })?;
        drop(guard);
        let mut resolved = Vec::new();
        for tool in tools {
            match resolve_mcp_tool(name, &tool, &self.policy) {
                Ok(tool) => resolved.push(tool),
                Err(reason) => {
                    tracing::debug!(server = name, tool = %tool.name, reason, "skipping MCP tool");
                }
            }
        }
        Ok(resolved)
    }

    /// Tool definitions of every successfully discovered MCP tool, sorted by
    /// display name for deterministic model requests.
    #[must_use]
    pub(crate) fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<ToolDefinition> = self
            .connections
            .iter()
            .flat_map(|connection| connection.tools.iter().map(McpToolInfo::to_definition))
            .collect();
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        definitions
    }

    /// Executes one model tool intent through MCP `tools/call`.
    ///
    /// Maps MCP `isError` results to failed [`ToolResult`]s without crashing
    /// the turn, and bounds the round trip by the configured timeout and the
    /// prompt's cancellation watch.
    pub(crate) async fn call_tool(&self, display_name: &str, arguments: Value) -> ToolResult {
        let target = {
            let dispatch = self.dispatch.lock().expect("dispatch poisoned");
            dispatch.get(display_name).cloned()
        };
        let Some(target) = target else {
            return ToolResult::failure(
                ToolErrorKind::Backend,
                format!("unknown MCP tool {display_name:?}"),
            );
        };
        if self.shutdown.load(Ordering::SeqCst) {
            return ToolResult::failure(
                ToolErrorKind::Backend,
                format!("MCP tool {display_name:?} called after session close"),
            );
        }
        if self.is_cancelled() {
            return ToolResult::failure(ToolErrorKind::Cancelled, "turn cancelled before MCP call");
        }
        let Ok(arguments) = serde_json::from_value::<serde_json::Map<String, Value>>(arguments)
        else {
            return ToolResult::failure(
                ToolErrorKind::InvalidArguments,
                "MCP tool arguments must be a JSON object",
            );
        };
        let original_name = target.original_name.clone();
        let params = CallToolRequestParams::new(original_name).with_arguments(arguments);
        // The per-connection mutex is held across the call so shutdown cannot
        // race a running tool call; read-only parallelism across servers is
        // unaffected (one mutex per connection).
        let mut guard = self.connections[target.connection].running.lock().await;
        let Some(running) = guard.as_mut() else {
            return ToolResult::failure(
                ToolErrorKind::Backend,
                format!("MCP connection for {display_name:?} is not ready"),
            );
        };
        let outcome = tokio::select! {
            result = running.call_tool(params) => result,
            () = cancelled_watch(&self.cancel) => {
                return ToolResult::failure(ToolErrorKind::Cancelled, "turn cancelled during MCP call");
            }
            () = tokio::time::sleep(self.timeout) => {
                return ToolResult::failure(
                    ToolErrorKind::Timeout,
                    format!("MCP call {display_name:?} timed out after {:?}", self.timeout),
                );
            }
        };
        drop(guard);
        match outcome {
            Ok(result) => normalize_call_tool_result(&target.original_name, result),
            Err(error) => ToolResult::failure(
                ToolErrorKind::Backend,
                format!("MCP call {display_name:?} failed: {error}"),
            ),
        }
    }

    /// Closes every connection: cancels the client services (whose cleanup
    /// disconnects ACP servers and kills stdio children) and sends explicit
    /// `mcp/disconnect` for ACP servers whose handshake failed.  Idempotent;
    /// safe to call from any thread (interior mutability), so a session guard
    /// can also run it from [`Drop`].
    pub(crate) async fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        for connection in &self.connections {
            let running = connection.running.lock().await.take();
            match running {
                Some(mut running) => {
                    // Service cleanup calls transport.close(), which sends
                    // `mcp/disconnect` for ACP servers.
                    let _ = tokio::time::timeout(
                        self.timeout,
                        running.close_with_timeout(self.timeout),
                    )
                    .await;
                }
                None => {
                    if let Some(connection_id) = &connection.acp {
                        self.disconnect_acp(connection_id).await;
                    }
                }
            }
        }
        self.dispatch.lock().expect("dispatch poisoned").clear();
    }
}

/// Cancellation helper: resolves when the watch flips to `true`.  A dropped
/// sender means no further cancellation signals can arrive; the helper then
/// waits forever (the call's own timeout still bounds the wait).
async fn cancelled_watch(cancel: &watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    let mut cancel = cancel.clone();
    loop {
        match cancel.changed().await {
            Ok(()) => {
                if *cancel.borrow() {
                    return;
                }
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }
}

/// Serves the rmcp client lifecycle (Discover, 2026-07-28 only).
async fn serve_client<T, E, A>(
    transport: T,
) -> Result<
    RunningService<RoleClient, OrchestratorClientHandler>,
    Box<rmcp::service::ClientInitializeError>,
>
where
    T: rmcp::transport::IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    OrchestratorClientHandler
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .map_err(Box::new)
}

/// Spawns the stdio MCP server child process (rmcp child-process transport).
fn spawn_stdio(
    stdio: &McpServerStdio,
    name: &str,
) -> Result<rmcp::transport::child_process::TokioChildProcess, String> {
    use rmcp::transport::child_process::TokioChildProcess;
    use std::process::Stdio;

    let mut command = tokio::process::Command::new(&stdio.command);
    command.args(&stdio.args);
    for variable in &stdio.env {
        command.env(&variable.name, &variable.value);
    }
    if let Some(cwd) = stdio_cwd(stdio) {
        command.current_dir(cwd);
    }
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot spawn mcp server {name:?}: {error}"))?;
    Ok(transport)
}

/// The stdio working directory carried in the `_meta.ee.cwd` extension.
fn stdio_cwd(stdio: &McpServerStdio) -> Option<std::path::PathBuf> {
    let meta = stdio.meta.as_ref()?;
    let cwd = meta.get("ee").and_then(Value::as_object)?.get("cwd")?;
    let cwd = cwd.as_str()?;
    Some(std::path::PathBuf::from(cwd))
}

/// Converts one rmcp `Tool` into a resolved, classified tool entry.
///
/// Fails closed (with a diagnostic) when the name sanitizes to empty or the
/// input schema is invalid; unclassified tools get the conservative default
/// class (see [`super::policy::conservative_default`]).
fn resolve_mcp_tool(
    server_name: &str,
    tool: &Tool,
    policy: &McpToolPolicy,
) -> Result<ResolvedMcpTool, String> {
    let resolved = resolve_tool_name(server_name, &tool.name).ok_or_else(|| {
        format!("MCP tool name {:?} sanitizes to an empty provider-compatible name", tool.name)
    })?;
    if has_disallowed_character(&resolved.display_name) {
        return Err(format!(
            "MCP tool name {:?} is not provider-compatible",
            resolved.display_name
        ));
    }
    let input_schema = convert_input_schema(&Value::Object((*tool.input_schema).clone()))?;
    let spec = classify_tool(server_name, &tool.name, policy);
    Ok(ResolvedMcpTool {
        display_name: resolved.display_name,
        original_name: resolved.original_name,
        server_id: resolved.server_id,
        description: tool.description.as_deref().unwrap_or_default().to_string(),
        input_schema,
        class: spec.class,
        subclass: spec.subclass,
        host_approval: server_name == EE_SERVER_NAME && is_ee_proxy_tool(&tool.name),
    })
}

impl ResolvedMcpTool {
    fn into_info(self) -> McpToolInfo {
        McpToolInfo {
            display_name: self.display_name,
            original_name: self.original_name,
            server_id: self.server_id,
            description: self.description,
            input_schema: self.input_schema,
            class: self.class,
            subclass: self.subclass,
            host_approval: self.host_approval,
        }
    }
}

/// Renders a `CallToolResult`'s content blocks as text (bounded join).
fn content_text(content: &[rmcp::model::ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in content {
        if let rmcp::model::ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    parts.join("\n")
}

/// Normalizes an MCP `CallToolResult` into an orchestrator [`ToolResult`]:
/// `isError` results become failed results, never panics.
fn normalize_call_tool_result(name: &str, result: CallToolResult) -> ToolResult {
    let text = content_text(&result.content);
    if result.is_error == Some(true) {
        let text = if text.is_empty() {
            format!("MCP tool {name:?} returned an error")
        } else {
            format!("MCP tool {name:?} error: {text}")
        };
        return ToolResult::failure(ToolErrorKind::Backend, text);
    }
    match result.structured_content {
        Some(structured) if !text.is_empty() => ToolResult::success_structured(text, structured),
        Some(structured) => ToolResult::success_structured(structured.to_string(), structured),
        None if !text.is_empty() => ToolResult::success(text),
        None => ToolResult::success(format!("MCP tool {name:?} completed")),
    }
}

/// Whether the active policy would deny every listed MCP tool (used for the
/// "policy filtered all tools" diagnostic).
#[must_use]
pub(crate) fn policy_filters_all(policy: &PolicyEngine, definitions: &[ToolDefinition]) -> bool {
    !definitions.is_empty()
        && definitions
            .iter()
            .all(|definition| !policy.check(definition, PolicyContext::default()).allow)
}

/// A [`ServerTool`](crate::tools::ServerTool) executing one MCP tool through
/// the session manager.
///
/// The manager outlives the tool: it is owned by the per-prompt session
/// guard, so a call racing prompt shutdown resolves as a failed result
/// instead of touching a dropped connection.
pub(crate) struct McpBackedTool {
    definition: ToolDefinition,
    manager: Arc<McpSessionManager>,
}

impl McpBackedTool {
    /// Creates a tool bound to the display name and its session manager.
    #[must_use]
    pub(crate) fn new(definition: ToolDefinition, manager: Arc<McpSessionManager>) -> Self {
        Self { definition, manager }
    }
}

impl crate::tools::ServerTool for McpBackedTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _client: ee_acp_agent_server::ClientBridge,
        _cancel: watch::Receiver<bool>,
        _context: crate::tools::ToolCallContext,
    ) -> crate::tools::ToolFuture<crate::tools::ToolResult> {
        let manager = self.manager.clone();
        let name = self.definition.name.clone();
        Box::pin(async move { manager.call_tool(&name, arguments).await })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn acp_descriptor() -> McpServerDescriptor {
        McpServerDescriptor::from_wire(ee_agent_protocol::ee_proxy_acp_entry(McpServerAcpId::new(
            "ee-mcp-proxy:test",
        )))
        .expect("descriptor validates")
    }

    fn test_bridge() -> ClientBridge {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ClientBridge::new_for_test(Duration::from_secs(60), tx)
    }

    fn manager_with(
        descriptors: Vec<McpServerDescriptor>,
    ) -> (McpSessionManager, watch::Receiver<bool>) {
        let (_cancel_tx, cancel) = watch::channel(false);
        let manager = McpSessionManager::new(
            descriptors,
            test_bridge(),
            cancel.clone(),
            McpToolPolicy::default(),
        );
        (manager, cancel)
    }

    fn tool(name: &str, schema: Value) -> Tool {
        serde_json::from_value(json!({
            "name": name,
            "description": format!("tool {name}"),
            "inputSchema": schema,
        }))
        .expect("tool deserializes")
    }

    #[test]
    fn tool_definitions_carry_classification_and_schema() {
        let policy = McpToolPolicy::default();
        let resolved =
            resolve_mcp_tool("ee", &tool("ee_workspace_roots", json!({"type": "object"})), &policy)
                .expect("resolves");
        let info = resolved.into_info();
        let definition = info.to_definition();
        assert_eq!(definition.name, "ee_workspace_roots");
        assert_eq!(definition.side_effect_class, SideEffectClass::Read);
        assert_eq!(definition.side_effect_subclass, None);
    }

    #[test]
    fn unknown_external_tools_get_conservative_definition() {
        let policy = McpToolPolicy::default();
        let resolved =
            resolve_mcp_tool("external", &tool("write.file", json!({"type": "object"})), &policy)
                .expect("resolves");
        let definition = resolved.into_info().to_definition();
        assert_eq!(definition.name, "mcp_external_write_file");
        assert_eq!(definition.side_effect_class, SideEffectClass::Write);
        assert_eq!(
            definition.side_effect_subclass,
            Some(SideEffectSubclass::Overwrite),
            "unknown tools keep the conservative destructive subclass"
        );
    }

    #[test]
    fn invalid_schemas_fail_closed_per_tool() {
        let policy = McpToolPolicy::default();
        let error =
            resolve_mcp_tool("ee", &tool("ee_list_directory", json!({ "type": "array" })), &policy)
                .expect_err("array schema rejected");
        assert!(error.contains("must be \"object\""), "{error}");
    }

    #[test]
    fn normalize_call_tool_result_maps_is_error_to_failure() {
        let mut result = CallToolResult::default();
        result.content = vec![rmcp::model::ContentBlock::text("boom")];
        result.is_error = Some(true);
        let normalized = normalize_call_tool_result("ee_x", result);
        assert!(!normalized.success);
        assert_eq!(normalized.error_kind, Some(ToolErrorKind::Backend));
        assert!(normalized.text_output.contains("boom"));
    }

    #[test]
    fn normalize_call_tool_result_keeps_text_and_structured_output() {
        let mut result = CallToolResult::default();
        result.content = vec![rmcp::model::ContentBlock::text("text")];
        result.structured_content = Some(json!({ "roots": [] }));
        let normalized = normalize_call_tool_result("ee_x", result);
        assert!(normalized.success);
        assert_eq!(normalized.text_output, "text");
        assert_eq!(normalized.structured_output, Some(json!({ "roots": [] })));
    }

    #[test]
    fn normalize_call_tool_result_empty_success_stays_successful() {
        let normalized = normalize_call_tool_result("ee_x", CallToolResult::default());
        assert!(normalized.success);
        assert!(normalized.text_output.contains("completed"));
    }

    #[test]
    fn policy_filters_all_detects_full_policy_denial() {
        let engine = PolicyEngine::default(); // reads only
        let policy = McpToolPolicy::default();
        let write =
            resolve_mcp_tool("external", &tool("write", json!({"type": "object"})), &policy)
                .expect("resolves")
                .into_info()
                .to_definition();
        let read =
            resolve_mcp_tool("ee", &tool("ee_workspace_roots", json!({"type": "object"})), &policy)
                .expect("resolves")
                .into_info()
                .to_definition();
        assert!(policy_filters_all(&engine, std::slice::from_ref(&write)));
        assert!(!policy_filters_all(&engine, std::slice::from_ref(&read)));
        assert!(!policy_filters_all(&engine, &[read, write]));
        assert!(!policy_filters_all(&engine, &[]), "no tools is not a policy filter");
    }

    #[test]
    fn stdio_cwd_comes_from_meta_extension() {
        let mut stdio = McpServerStdio::new("srv", PathBuf::from("/bin/server"));
        stdio.meta = Some(
            serde_json::from_value(json!({
                "ee": { "cwd": "/work" },
            }))
            .expect("meta parses"),
        );
        assert_eq!(stdio_cwd(&stdio), Some(PathBuf::from("/work")));
    }

    #[tokio::test]
    async fn unknown_tool_call_fails_closed_without_transport() {
        let (manager, _cancel) = manager_with(vec![acp_descriptor()]);
        let result = manager.call_tool("nope", json!({})).await;
        assert!(!result.success);
        assert_eq!(result.error_kind, Some(ToolErrorKind::Backend));
    }

    #[tokio::test]
    async fn cancelled_turn_aborts_discovery_with_diagnostic() {
        let (cancel_tx, cancel) = watch::channel(true);
        let _ = cancel_tx;
        let mut manager = McpSessionManager::new(
            vec![acp_descriptor()],
            test_bridge(),
            cancel,
            McpToolPolicy::default(),
        );
        let diagnostics = manager.discover_all().await;
        assert!(diagnostics.iter().any(|d| d.message.contains("cancelled")), "{diagnostics:?}");
        assert!(manager.tool_definitions().is_empty(), "no tools after cancellation");
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let (manager, _cancel) = manager_with(vec![acp_descriptor()]);
        manager.shutdown().await;
        manager.shutdown().await;
        assert!(manager.tool_definitions().is_empty());
    }
}

/// Deterministic fake-server tests: fake ACP MCP host (scripted `mcp/*`
/// responder) and fake stdio server (`ee_mcp::fake` line actor).  No network,
/// no subprocesses.
#[cfg(all(feature = "test-utils", test))]
mod fake_server_tests {
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    use ee_acp_agent_server::server::OutboundEvent;
    use ee_agent_protocol::{Error as RpcError, RawJsonRpcMessage, RawJsonRpcParams, Response};
    use ee_mcp::fake::{FakeMcpScript, FakeMcpServer, FakeMcpTransport, FakeMcpTransportFactory};
    use serde_json::{Value, json};
    use tokio::sync::{mpsc, oneshot, watch};

    use super::*;

    fn discover_result() -> Value {
        json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": { "tools": {} },
            "ttlMs": 0,
            "cacheScope": "private",
        })
    }

    fn tools_result(tools: Value) -> Value {
        json!({
            "tools": tools,
            "resultType": "complete",
            "ttlMs": 0,
            "cacheScope": "private",
        })
    }

    fn tool(name: &str) -> Value {
        json!({ "name": name, "description": format!("{name} tool"), "inputSchema": { "type": "object", "properties": {} } })
    }

    /// A scripted fake of the host's ACP MCP side (`mcp/connect`,
    /// `mcp/message`, `mcp/disconnect`), answering inner MCP requests from a
    /// canned map.  Unanswered methods simulate a hanging host.
    struct ScriptedAcpHost {
        bridge: ClientBridge,
        rx: mpsc::UnboundedReceiver<OutboundEvent>,
        inner_results: HashMap<String, Value>,
        call_results: HashMap<String, Value>,
        fail_connect: bool,
        hang_methods: HashSet<String>,
        log: Arc<StdMutex<Vec<String>>>,
    }

    impl ScriptedAcpHost {
        fn new(bridge: ClientBridge, rx: mpsc::UnboundedReceiver<OutboundEvent>) -> Self {
            Self {
                bridge,
                rx,
                inner_results: HashMap::new(),
                call_results: HashMap::new(),
                fail_connect: false,
                hang_methods: HashSet::new(),
                log: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn answer(&mut self, method: &str, result: Value) {
            self.inner_results.insert(method.to_string(), result);
        }

        fn answer_call(&mut self, tool_name: &str, result: Value) {
            self.call_results.insert(tool_name.to_string(), result);
        }

        /// Marks an outer method as never-answered (hanging host).
        fn hang(&mut self, method: &str) {
            self.hang_methods.insert(method.to_string());
        }

        fn handle(&mut self, frame: OutboundEvent) {
            let OutboundEvent::ClientRequest { frame } = frame else {
                return;
            };
            match frame {
                RawJsonRpcMessage::Request(request) => {
                    let params = match request.params {
                        None => Value::Null,
                        Some(RawJsonRpcParams::Object(map)) => Value::Object(map),
                        Some(RawJsonRpcParams::Array(array)) => Value::Array(array),
                    };
                    let method = request.method.to_string();
                    let outcome = self.response_for(&method, &params);
                    self.log.lock().expect("log poisoned").push(format!("{method}: {params}"));
                    match outcome {
                        Some(Ok(result)) => {
                            self.bridge
                                .handle_response(Response::Result { id: request.id, result });
                        }
                        Some(Err((code, message))) => {
                            self.bridge.handle_response(Response::Error {
                                id: request.id,
                                error: RpcError::new(code, message),
                            });
                        }
                        None => {
                            // Unanswered: the host hangs on this request.
                            self.log.lock().expect("log poisoned").push("unanswered".into());
                        }
                    }
                }
                RawJsonRpcMessage::Notification(notification) => {
                    self.log
                        .lock()
                        .expect("log poisoned")
                        .push(format!("notification: {}", notification.method));
                }
                RawJsonRpcMessage::Response(_) => {}
            }
        }

        fn response_for(
            &self,
            method: &str,
            params: &Value,
        ) -> Option<Result<Value, (i32, String)>> {
            let inner_method =
                params.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
            let hang_key = if method == "mcp/message" && !inner_method.is_empty() {
                inner_method.as_str()
            } else {
                method
            };
            if self.hang_methods.contains(hang_key) {
                return None;
            }
            match method {
                "mcp/connect" => {
                    if self.fail_connect {
                        Some(Err((-32602, "unknown MCP server id".to_string())))
                    } else {
                        Some(Ok(json!({ "connectionId": "conn-1" })))
                    }
                }
                "mcp/disconnect" => Some(Ok(json!({}))),
                "mcp/message" => {
                    let inner_method =
                        params.get("method").and_then(Value::as_str).unwrap_or_default();
                    if inner_method == "tools/call" {
                        let tool_name = params
                            .pointer("/params/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        return self.call_results.get(tool_name).cloned().map(Ok);
                    }
                    self.inner_results.get(inner_method).cloned().map(Ok)
                }
                _ => Some(Ok(json!({}))),
            }
        }

        /// Serves frames until `stop` fires or the outbound channel closes.
        async fn run(&mut self, mut stop: oneshot::Receiver<()>) {
            loop {
                tokio::select! {
                    _ = &mut stop => break,
                    frame = self.rx.recv() => {
                        let Some(frame) = frame else { break };
                        self.handle(frame);
                    }
                }
            }
        }
    }

    /// Fake stdio server factory whose handle is retrievable after connect.
    #[derive(Clone)]
    struct ScriptedFake {
        script: FakeMcpScript,
        handle: Arc<StdMutex<Option<FakeMcpServer>>>,
    }

    impl ScriptedFake {
        fn new(script: FakeMcpScript) -> Self {
            Self { script, handle: Arc::new(StdMutex::new(None)) }
        }

        fn server(&self) -> FakeMcpServer {
            self.handle
                .lock()
                .expect("fake handle poisoned")
                .clone()
                .expect("fake server not spawned yet")
        }
    }

    impl FakeMcpTransportFactory for ScriptedFake {
        fn build(&self) -> FakeMcpTransport {
            let (server, transport) = FakeMcpServer::spawn(self.script.clone());
            *self.handle.lock().expect("fake handle poisoned") = Some(server);
            transport
        }
    }

    fn ee_entries() -> (Vec<McpServerDescriptor>, watch::Receiver<bool>) {
        let (_cancel_tx, cancel) = watch::channel(false);
        (
            vec![
                McpServerDescriptor::from_wire(ee_agent_protocol::ee_proxy_acp_entry(
                    McpServerAcpId::new("ee-mcp-proxy:test"),
                ))
                .expect("descriptor"),
            ],
            cancel,
        )
    }

    fn acp_host(
        answers: &[(&str, Value)],
        calls: &[(&str, Value)],
    ) -> (ScriptedAcpHost, ClientBridge) {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let bridge = ClientBridge::new_for_test(Duration::from_secs(60), outbound_tx);
        let mut host = ScriptedAcpHost::new(bridge.clone(), outbound_rx);
        for (method, result) in answers {
            host.answer(method, result.clone());
        }
        for (name, result) in calls {
            host.answer_call(name, result.clone());
        }
        (host, bridge)
    }

    /// Standard ee-proxy ACP host: connect + discover + a tools/list.
    fn standard_acp_host(tools: Value) -> (ScriptedAcpHost, ClientBridge) {
        acp_host(
            &[("server/discover", discover_result()), ("tools/list", tools_result(tools))],
            &[],
        )
    }

    async fn spawn_host(
        host: ScriptedAcpHost,
    ) -> (tokio::task::JoinHandle<()>, oneshot::Sender<()>) {
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut host = host;
            host.run(stop_rx).await;
        });
        (task, stop_tx)
    }

    async fn stop_host(
        task: tokio::task::JoinHandle<()>,
        stop: oneshot::Sender<()>,
        host_log: Arc<StdMutex<Vec<String>>>,
    ) -> Vec<String> {
        let _ = stop.send(());
        task.await.expect("host joins");
        host_log.lock().expect("log poisoned").clone()
    }

    #[tokio::test]
    async fn acp_host_discover_list_and_call_round_trip() {
        let (descriptors, cancel) = ee_entries();
        let (mut host, bridge) = standard_acp_host(json!([tool("ee_workspace_roots")]));
        host.answer_call(
            "ee_workspace_roots",
            json!({ "resultType": "complete", "content": [ { "type": "text", "text": "/work" } ], "structuredContent": { "roots": ["/work"] } }),
        );
        let log = host.log.clone();
        let (host_task, stop) = spawn_host(host).await;
        let mut manager =
            McpSessionManager::new(descriptors, bridge, cancel, McpToolPolicy::default());

        let diagnostics = manager.discover_all().await;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let definitions = manager.tool_definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "ee_workspace_roots");
        assert_eq!(definitions[0].side_effect_class, SideEffectClass::Read);
        assert!(!definitions[0].name.contains('.'));

        let result = manager.call_tool("ee_workspace_roots", json!({})).await;
        assert!(result.success, "{result:?}");
        assert_eq!(result.text_output, "/work");
        assert_eq!(result.structured_output, Some(json!({ "roots": ["/work"] })));

        manager.shutdown().await;
        let log = stop_host(host_task, stop, log).await;
        assert!(
            log.iter().any(|line| line.starts_with("mcp/disconnect:")),
            "shutdown must disconnect: {log:?}"
        );
        assert!(
            log.iter()
                .any(|line| line.contains("tools/call") && line.contains("ee_workspace_roots")),
            "{log:?}"
        );
    }

    #[tokio::test]
    async fn stdio_fake_discover_list_and_call_round_trip() {
        let script = FakeMcpScript::new()
            .discover_2026_07_28(json!({ "tools": {} }))
            .respond("tools/list", tools_result(json!([tool("ee_workspace_roots")])))
            .respond(
                "tools/call",
                json!({ "resultType": "complete", "content": [ { "type": "text", "text": "/work" } ] }),
            );
        let fake = ScriptedFake::new(script);
        let descriptor = McpServerDescriptor::from_wire(ee_agent_protocol::McpServer::Stdio(
            McpServerStdio::new("ee", PathBuf::from("/usr/bin/ee"))
                .args(vec!["--mcp-proxy".into()]),
        ))
        .expect("descriptor");
        let (_cancel_tx, cancel) = watch::channel(false);
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
        let bridge = ClientBridge::new_for_test(Duration::from_secs(60), outbound_tx);
        let mut manager =
            McpSessionManager::new(vec![descriptor], bridge, cancel, McpToolPolicy::default());
        manager.install_fake_stdio("ee", Arc::new(fake.clone())).await;

        let diagnostics = manager.discover_all().await;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(manager.tool_definitions().len(), 1);

        let result = manager.call_tool("ee_workspace_roots", json!({})).await;
        assert!(result.success, "{result:?}");
        assert_eq!(result.text_output, "/work");
        assert!(fake.server().log_contains("tools/call"));

        manager.shutdown().await;
        fake.server().join(Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn external_tool_dispatch_uses_the_reversible_original_name() {
        // The model calls the sanitized display name; the wire call must use
        // the original MCP tool name (reversible display → original map).
        let script = FakeMcpScript::new()
            .discover_2026_07_28(json!({ "tools": {} }))
            .respond("tools/list", tools_result(json!([tool("read.file")])))
            .respond(
                "tools/call",
                json!({ "resultType": "complete", "content": [ { "type": "text", "text": "ok" } ] }),
            );
        let fake = ScriptedFake::new(script);
        let descriptor = McpServerDescriptor::from_wire(ee_agent_protocol::McpServer::Stdio(
            McpServerStdio::new("ext.server", PathBuf::from("/bin/ext")),
        ))
        .expect("descriptor");
        let (_cancel_tx, cancel) = watch::channel(false);
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
        let bridge = ClientBridge::new_for_test(Duration::from_secs(60), outbound_tx);
        let mut manager =
            McpSessionManager::new(vec![descriptor], bridge, cancel, McpToolPolicy::default());
        manager.install_fake_stdio("ext.server", Arc::new(fake.clone())).await;

        let diagnostics = manager.discover_all().await;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let definitions = manager.tool_definitions();
        assert_eq!(definitions[0].name, "mcp_ext_server_read_file");
        assert!(!definitions[0].name.contains('.'));

        let result = manager.call_tool("mcp_ext_server_read_file", json!({})).await;
        assert!(result.success, "{result:?}");

        // The wire tools/call carried the original name, not the display name.
        let calls = fake.server().requests_by_method("tools/call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["params"]["name"], "read.file");

        manager.shutdown().await;
        fake.server().join(Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn acp_connect_rejected_fails_closed() {
        let (descriptors, cancel) = ee_entries();
        let (mut host, bridge) = acp_host(&[], &[]);
        host.fail_connect = true;
        let log = host.log.clone();
        let (host_task, stop) = spawn_host(host).await;
        let mut manager =
            McpSessionManager::new(descriptors, bridge, cancel, McpToolPolicy::default());

        let diagnostics = manager.discover_all().await;
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("unavailable"), "{diagnostics:?}");
        assert!(manager.tool_definitions().is_empty());

        manager.shutdown().await;
        let _ = stop_host(host_task, stop, log).await;
    }

    #[tokio::test]
    async fn acp_tools_list_failure_fails_closed_and_disconnects() {
        let (descriptors, cancel) = ee_entries();
        let (host, bridge) = acp_host(
            &[("server/discover", discover_result()), ("tools/list", json!({ "error": "boom" }))],
            &[],
        );
        // tools/list answer that is not a valid ListToolsResult fails closed.
        let log = host.log.clone();
        let (host_task, stop) = spawn_host(host).await;
        let mut manager =
            McpSessionManager::new(descriptors, bridge, cancel, McpToolPolicy::default());

        let diagnostics = manager.discover_all().await;
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("tools/list"), "{diagnostics:?}");
        assert!(manager.tool_definitions().is_empty());

        manager.shutdown().await;
        let log = stop_host(host_task, stop, log).await;
        assert!(
            log.iter().any(|line| line.starts_with("mcp/disconnect:")),
            "shutdown must disconnect a connected server: {log:?}"
        );
    }

    #[tokio::test]
    async fn acp_connect_timeout_fails_closed() {
        let (descriptors, cancel) = ee_entries();
        // The host never answers mcp/connect: the connect round must time out.
        let (mut host, bridge) = acp_host(&[], &[]);
        host.hang("mcp/connect");
        let log = host.log.clone();
        let (host_task, stop) = spawn_host(host).await;
        let policy = McpToolPolicy { request_timeout_ms: 100, ..McpToolPolicy::default() };
        let mut manager = McpSessionManager::new(descriptors, bridge, cancel, policy);

        let diagnostics = manager.discover_all().await;
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("timed out"), "{diagnostics:?}");

        manager.shutdown().await;
        let _ = stop_host(host_task, stop, log).await;
    }

    #[tokio::test]
    async fn call_tool_is_error_maps_to_failed_result_without_crashing() {
        let (descriptors, cancel) = ee_entries();
        let (mut host, bridge) = standard_acp_host(json!([tool("ee_workspace_roots")]));
        host.answer_call(
            "ee_workspace_roots",
            json!({ "resultType": "complete", "content": [ { "type": "text", "text": "boom" } ], "isError": true }),
        );
        let log = host.log.clone();
        let (host_task, stop) = spawn_host(host).await;
        let mut manager =
            McpSessionManager::new(descriptors, bridge, cancel, McpToolPolicy::default());
        manager.discover_all().await;

        let result = manager.call_tool("ee_workspace_roots", json!({})).await;
        assert!(!result.success, "{result:?}");
        assert_eq!(result.error_kind, Some(ToolErrorKind::Backend));
        assert!(result.text_output.contains("boom"));

        manager.shutdown().await;
        let _ = stop_host(host_task, stop, log).await;
    }

    #[tokio::test]
    async fn call_tool_protocol_error_fails_closed_without_crashing() {
        let (descriptors, cancel) = ee_entries();
        let (mut host, bridge) = standard_acp_host(json!([tool("ee_workspace_roots")]));
        // tools/call answers with a JSON-RPC error (protocol-level failure).
        host.answer_call(
            "ee_workspace_roots",
            json!({ "error": { "code": -32000, "message": "boom" } }),
        );
        let log = host.log.clone();
        let (host_task, stop) = spawn_host(host).await;
        let mut manager =
            McpSessionManager::new(descriptors, bridge, cancel, McpToolPolicy::default());
        manager.discover_all().await;

        let result = manager.call_tool("ee_workspace_roots", json!({})).await;
        assert!(!result.success, "{result:?}");
        assert_eq!(result.error_kind, Some(ToolErrorKind::Backend));

        manager.shutdown().await;
        let _ = stop_host(host_task, stop, log).await;
    }

    #[tokio::test]
    async fn call_tool_timeout_fails_closed() {
        let (descriptors, cancel) = ee_entries();
        let (mut host, bridge) = standard_acp_host(json!([tool("ee_workspace_roots")]));
        // The host never answers tools/call: the call round must time out.
        host.hang("tools/call");
        let log = host.log.clone();
        let (host_task, stop) = spawn_host(host).await;
        let policy = McpToolPolicy { request_timeout_ms: 100, ..McpToolPolicy::default() };
        let mut manager = McpSessionManager::new(descriptors, bridge, cancel, policy);
        manager.discover_all().await;

        let result = manager.call_tool("ee_workspace_roots", json!({})).await;
        assert!(!result.success, "{result:?}");
        assert_eq!(result.error_kind, Some(ToolErrorKind::Timeout));

        manager.shutdown().await;
        let _ = stop_host(host_task, stop, log).await;
    }

    #[tokio::test]
    async fn call_tool_cancellation_returns_cancelled_result() {
        let (descriptors, _cancel) = ee_entries();
        let (host, bridge) = standard_acp_host(json!([tool("ee_workspace_roots")]));
        let log = host.log.clone();
        let (host_task, stop) = spawn_host(host).await;
        let (cancel_tx, cancel) = watch::channel(false);
        let mut manager =
            McpSessionManager::new(descriptors, bridge, cancel, McpToolPolicy::default());
        manager.discover_all().await;
        // Flip the cancellation watch before the call: it must fail closed.
        cancel_tx.send(true).expect("watch open");

        let result = manager.call_tool("ee_workspace_roots", json!({})).await;
        assert!(!result.success, "{result:?}");
        assert_eq!(result.error_kind, Some(ToolErrorKind::Cancelled));

        manager.shutdown().await;
        let _ = stop_host(host_task, stop, log).await;
    }

    #[tokio::test]
    async fn sanitized_name_collision_skips_second_tool_fail_closed() {
        let script = FakeMcpScript::new()
            .discover_2026_07_28(json!({ "tools": {} }))
            .respond("tools/list", tools_result(json!([tool("x")])));
        let fake_a = ScriptedFake::new(script.clone());
        let fake_b = ScriptedFake::new(script);
        let (_cancel_tx, cancel) = watch::channel(false);
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
        let bridge = ClientBridge::new_for_test(Duration::from_secs(60), outbound_tx);
        let mut manager = McpSessionManager::new(
            vec![
                McpServerDescriptor::from_wire(ee_agent_protocol::McpServer::Stdio(
                    McpServerStdio::new("a.b", PathBuf::from("/bin/a")),
                ))
                .expect("descriptor"),
                McpServerDescriptor::from_wire(ee_agent_protocol::McpServer::Stdio(
                    McpServerStdio::new("a_b", PathBuf::from("/bin/b")),
                ))
                .expect("descriptor"),
            ],
            bridge,
            cancel,
            McpToolPolicy::default(),
        );
        manager.install_fake_stdio("a.b", Arc::new(fake_a.clone())).await;
        manager.install_fake_stdio("a_b", Arc::new(fake_b.clone())).await;

        let diagnostics = manager.discover_all().await;
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("collision"), "{diagnostics:?}");
        assert_eq!(manager.tool_definitions().len(), 1);
        assert_eq!(manager.tool_definitions()[0].name, "mcp_a_b_x");

        manager.shutdown().await;
        fake_a.server().join(Duration::from_secs(5)).await;
        fake_b.server().join(Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn discovery_diagnostics_never_contain_stdio_secrets() {
        // A stdio descriptor with a secret env value whose server fails to
        // initialize: the diagnostic must not leak the value.
        let script = FakeMcpScript::new().close();
        let fake = ScriptedFake::new(script);
        let (_cancel_tx, cancel) = watch::channel(false);
        let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
        let bridge = ClientBridge::new_for_test(Duration::from_secs(60), outbound_tx);
        let mut manager = McpSessionManager::new(
            vec![
                McpServerDescriptor::from_wire(ee_agent_protocol::McpServer::Stdio(
                    McpServerStdio::new("filesystem", PathBuf::from("/bin/server")).env(vec![
                        ee_agent_protocol::EnvVariable::new("API_TOKEN", "sekrit-value"),
                    ]),
                ))
                .expect("descriptor"),
            ],
            bridge,
            cancel,
            McpToolPolicy::default(),
        );
        manager.install_fake_stdio("filesystem", Arc::new(fake.clone())).await;

        let diagnostics = manager.discover_all().await;
        assert!(!diagnostics.is_empty());
        let debug = format!("{diagnostics:?}");
        assert!(!debug.contains("sekrit-value"), "secret leaked into diagnostics: {debug}");
        manager.shutdown().await;
        fake.server().join(Duration::from_secs(5)).await;
    }
}
