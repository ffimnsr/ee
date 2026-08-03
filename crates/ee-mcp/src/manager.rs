//! MCP client manager: lazy per-server connections, discovery pinning,
//! namespaced primitive access, notification refresh, and elicitation.
//!
//! Every server is a separate rmcp connection spawned lazily by
//! [`McpClientManager::start`].  The manager never starts anything until
//! asked; `shutdown` stops every connection and kills stdio subprocesses.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, GetPromptRequestParams, GetPromptResult,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
    ReadResourceRequestParams, ReadResourceResult, RequestMetaObject, SubscriptionFilter,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RoleClient, RunningService};
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::McpServerConfig;
use crate::discovery::{DiscoveryCache, DiscoverySnapshot};
use crate::events::{McpEvent, McpServerState};
use crate::handler::EeClientHandler;
use crate::registry::{
    NamespacedPrompt, NamespacedResource, NamespacedResourceTemplate, NamespacedTool,
    PrimitiveRegistry,
};
use crate::transport::{build_http_transport, spawn_stdio};
use crate::{DEFAULT_REQUEST_TIMEOUT_MS, MCP_PROTOCOL_VERSION, McpError};

/// A `subscriptions/listen` stream of notifications from one server.
#[derive(Debug)]
pub struct McpSubscription {
    /// The underlying rmcp subscription.
    pub subscription: rmcp::service::Subscription,
}

/// A namespaced primitive registry entry exposed by the manager.
#[derive(Debug)]
#[non_exhaustive]
pub enum NamespacedPrimitive {
    /// A namespaced tool.
    Tool(NamespacedTool),
    /// A namespaced prompt.
    Prompt(NamespacedPrompt),
    /// A namespaced resource.
    Resource(NamespacedResource),
    /// A namespaced resource template.
    ResourceTemplate(NamespacedResourceTemplate),
}

/// Shared per-server connection state (request side).
#[derive(Debug)]
pub struct McpClientState {
    server_id: String,
    config: McpServerConfig,
    events: mpsc::UnboundedSender<McpEvent>,
    running: RwLock<Option<Arc<RunningService<RoleClient, EeClientHandler>>>>,
    discovery: RwLock<DiscoveryCache>,
    registry: RwLock<PrimitiveRegistry>,
    state: RwLock<McpServerState>,
    /// Test-only transport factories (never present in production builds).
    #[cfg(feature = "test-utils")]
    test_factories: Arc<FakeFactoryRegistry>,
}

impl McpClientState {
    fn emit(&self, event: McpEvent) {
        let _ = self.events.send(event);
    }

    async fn set_state(&self, state: McpServerState) {
        *self.state.write().await = state;
        self.emit(McpEvent::ServerState { server_id: self.server_id.clone(), state });
    }

    /// Current lifecycle state.
    pub async fn state(&self) -> McpServerState {
        *self.state.read().await
    }

    /// The live service, when the connection is ready.  Derefs to the peer
    /// for request/notification plumbing.
    async fn running(&self) -> Option<Arc<RunningService<RoleClient, EeClientHandler>>> {
        self.running.read().await.clone()
    }

    /// Cached discovery snapshot, if any.
    pub async fn discovery(&self) -> Option<DiscoverySnapshot> {
        self.discovery.read().await.get().cloned()
    }

    /// Cached registry snapshot.
    pub async fn registry(&self) -> PrimitiveRegistry {
        self.registry.read().await.clone()
    }
}

/// One running server connection (request handles + supervisor task).
struct McpClientHandle {
    state: Arc<McpClientState>,
    supervisor: JoinHandle<()>,
    shutdown: CancellationToken,
}

impl std::fmt::Debug for McpClientHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClientHandle").field("state", &self.state).finish_non_exhaustive()
    }
}

/// Test-only transport factory registry (avoids `dyn` Debug in derives).
#[cfg(feature = "test-utils")]
#[derive(Default)]
struct FakeFactoryRegistry {
    inner: RwLock<BTreeMap<String, Arc<dyn crate::fake::FakeMcpTransportFactory>>>,
}

#[cfg(feature = "test-utils")]
impl std::fmt::Debug for FakeFactoryRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FakeFactoryRegistry")
    }
}

/// The MCP client manager (sendable; connections run on the caller's runtime).
#[derive(Debug)]
pub struct McpClientManager {
    configs: BTreeMap<String, McpServerConfig>,
    events: mpsc::UnboundedSender<McpEvent>,
    clients: RwLock<BTreeMap<String, Arc<McpClientHandle>>>,
    /// Test-only transport factories (never present in production builds).
    #[cfg(feature = "test-utils")]
    test_factories: Arc<FakeFactoryRegistry>,
}

/// How long [`McpClientManager::start`] waits for the first handshake.
const START_TIMEOUT: Duration = Duration::from_secs(30);

impl McpClientManager {
    /// Creates a manager for the given resolved server configs.
    ///
    /// No process or connection is started until [`Self::start`] is called.
    #[must_use]
    pub fn new(
        configs: BTreeMap<String, McpServerConfig>,
        events: mpsc::UnboundedSender<McpEvent>,
    ) -> Self {
        Self {
            configs,
            events,
            clients: RwLock::new(BTreeMap::new()),
            #[cfg(feature = "test-utils")]
            test_factories: Arc::new(FakeFactoryRegistry::default()),
        }
    }

    /// Installs an in-memory fake transport for `server_id` (test-utils).
    ///
    /// The factory is used instead of spawning a stdio subprocess or
    /// opening an HTTP connection, for every connect/reconnect.
    #[cfg(feature = "test-utils")]
    pub async fn install_fake_transport(
        &self,
        server_id: &str,
        factory: Arc<dyn crate::fake::FakeMcpTransportFactory>,
    ) {
        self.test_factories.inner.write().await.insert(server_id.to_string(), factory);
    }

    /// The configured server ids.
    #[must_use]
    pub fn server_ids(&self) -> Vec<String> {
        self.configs.keys().cloned().collect()
    }

    /// Lazily starts the connection for `server_id` (spawns the stdio
    /// subprocess or opens the HTTP transport, then runs `server/discover`).
    ///
    /// Returns once the server reached `Ready` (discovery pinned) or failed
    /// its first handshake.  A failed server keeps retrying in the
    /// background; callers observe transitions through [`McpEvent`]s.
    ///
    /// Idempotent: a running server is returned as-is.
    ///
    /// # Errors
    ///
    /// Fails when the server id is unknown, the configuration is invalid, or
    /// the first connection attempt failed (including unsupported protocol
    /// versions).
    pub async fn start(&self, server_id: &str) -> Result<(), McpError> {
        if self.clients.read().await.contains_key(server_id) {
            return Ok(());
        }
        let config = self
            .configs
            .get(server_id)
            .ok_or_else(|| McpError::NotFound(format!("unknown mcp server {server_id:?}")))?
            .clone();
        config.validate()?;
        let state = Arc::new(McpClientState {
            server_id: server_id.to_string(),
            config: config.clone(),
            events: self.events.clone(),
            running: RwLock::new(None),
            discovery: RwLock::new(DiscoveryCache::default()),
            registry: RwLock::new(PrimitiveRegistry::new(server_id)),
            state: RwLock::new(McpServerState::Disabled),
            #[cfg(feature = "test-utils")]
            test_factories: Arc::clone(&self.test_factories),
        });
        state.set_state(McpServerState::Starting).await;
        let shutdown = CancellationToken::new();
        let (first_ready_tx, first_ready_rx) = oneshot::channel();
        let supervisor =
            tokio::spawn(supervise_connection(state.clone(), shutdown.clone(), first_ready_tx));
        self.clients.write().await.insert(
            server_id.to_string(),
            Arc::new(McpClientHandle { state, supervisor, shutdown }),
        );
        match tokio::time::timeout(START_TIMEOUT, first_ready_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err(McpError::Cancelled),
            Err(_) => Err(McpError::Timeout { timeout_ms: START_TIMEOUT.as_millis() as u64 }),
        }
    }

    /// Starts every configured server.
    pub async fn start_all(&self) -> Vec<(String, Result<(), McpError>)> {
        let ids = self.server_ids();
        let mut results = Vec::new();
        for id in ids {
            results.push((id.clone(), self.start(&id).await));
        }
        results
    }

    /// Stops the connection for `server_id` (kills the stdio subprocess).
    pub async fn stop(&self, server_id: &str) -> Result<(), McpError> {
        let Some(handle) = self.clients.write().await.remove(server_id) else {
            return Ok(());
        };
        handle.shutdown.cancel();
        if let Ok(handle) = Arc::try_unwrap(handle) {
            let _ = handle.supervisor.await;
        }
        Ok(())
    }

    /// Stops every connection.
    pub async fn shutdown(&self) {
        let ids: Vec<String> = self.clients.write().await.keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id).await;
        }
    }

    /// Current state of `server_id`.
    pub async fn state(&self, server_id: &str) -> Option<McpServerState> {
        let client = self.clients.read().await.get(server_id)?.state.clone();
        Some(client.state().await)
    }

    /// Cached discovery snapshot for `server_id`.
    pub async fn discovery(&self, server_id: &str) -> Option<DiscoverySnapshot> {
        let client = self.clients.read().await.get(server_id)?.state.clone();
        client.discovery().await
    }

    /// Lists tools from `server_id`, namespaced as `<server_id>/<name>`.
    ///
    /// Uses the cached registry while the server-provided `ttlMs` window is
    /// fresh; otherwise re-lists (paginating through rmcp).
    ///
    /// # Errors
    ///
    /// Fails when the server is unknown, not ready, or the response is
    /// invalid or times out.
    pub async fn list_tools(&self, server_id: &str) -> Result<Vec<NamespacedTool>, McpError> {
        let client = self.require_client(server_id).await?;
        if client.state.registry.read().await.tools_fresh() {
            return Ok(client.state.registry.read().await.tools().to_vec());
        }
        let tools = {
            let c = client.clone();
            self.with_timeout(client.state.config.request_timeout(), async move {
                c.state
                    .running()
                    .await
                    .ok_or_else(|| McpError::ConnectionClosed("connection not ready".into()))?
                    .list_all_tools()
                    .await
                    .map_err(McpError::from)
            })
            .await?
        };
        {
            let mut registry = client.state.registry.write().await;
            let result = ListToolsResult::with_all_items(tools);
            registry.store_tools(&result)?;
        }
        Ok(client.state.registry.read().await.tools().to_vec())
    }

    /// Calls a namespaced tool (`<server_id>/<name>`).
    ///
    /// # Errors
    ///
    /// Fails for unknown servers/tools, invalid arguments, protocol errors,
    /// or when an elicitation round is declined/cancelled by the host.
    pub async fn call_tool(
        &self,
        server_id: &str,
        namespaced_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        let client = self.require_client(server_id).await?;
        let name = unnamespace(server_id, namespaced_name)?;
        let params = CallToolRequestParams::new(name).with_arguments(
            serde_json::from_value(arguments).map_err(|error| {
                McpError::InvalidPrimitiveResult(format!("tool arguments: {error}"))
            })?,
        );
        self.with_timeout(client.state.config.request_timeout(), async move {
            client
                .state
                .running()
                .await
                .ok_or_else(|| McpError::ConnectionClosed("connection not ready".into()))?
                .call_tool(params)
                .await
                .map_err(McpError::from)
        })
        .await
    }

    /// Lists prompts from `server_id`.
    ///
    /// # Errors
    ///
    /// Fails when the server is unknown, not ready, or the response is
    /// invalid or times out.
    pub async fn list_prompts(&self, server_id: &str) -> Result<Vec<NamespacedPrompt>, McpError> {
        let client = self.require_client(server_id).await?;
        if client.state.registry.read().await.prompts_fresh() {
            return Ok(client.state.registry.read().await.prompts().to_vec());
        }
        let prompts = {
            let c = client.clone();
            self.with_timeout(client.state.config.request_timeout(), async move {
                c.state
                    .running()
                    .await
                    .ok_or_else(|| McpError::ConnectionClosed("connection not ready".into()))?
                    .list_all_prompts()
                    .await
                    .map_err(McpError::from)
            })
            .await?
        };
        {
            let mut registry = client.state.registry.write().await;
            let result = ListPromptsResult::with_all_items(prompts);
            registry.store_prompts(&result)?;
        }
        Ok(client.state.registry.read().await.prompts().to_vec())
    }

    /// Gets a namespaced prompt (`<server_id>/<name>`) with arguments.
    ///
    /// # Errors
    ///
    /// Fails for unknown servers/prompts or protocol errors; MRTR
    /// elicitation rounds flow through the host.
    pub async fn get_prompt(
        &self,
        server_id: &str,
        namespaced_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<GetPromptResult, McpError> {
        let client = self.require_client(server_id).await?;
        let name = unnamespace(server_id, namespaced_name)?;
        let mut params = GetPromptRequestParams::new(name);
        if let Some(arguments) = arguments {
            let arguments: serde_json::Map<String, serde_json::Value> =
                serde_json::from_value(arguments).map_err(|error| {
                    McpError::InvalidPrimitiveResult(format!("prompt arguments: {error}"))
                })?;
            params.arguments = Some(arguments);
        }
        self.with_timeout(client.state.config.request_timeout(), async move {
            client
                .state
                .running()
                .await
                .ok_or_else(|| McpError::ConnectionClosed("connection not ready".into()))?
                .get_prompt(params)
                .await
                .map_err(McpError::from)
        })
        .await
    }

    /// Lists resources from `server_id`.
    ///
    /// # Errors
    ///
    /// Fails when the server is unknown, not ready, or the response is
    /// invalid or times out.
    pub async fn list_resources(
        &self,
        server_id: &str,
    ) -> Result<Vec<NamespacedResource>, McpError> {
        let client = self.require_client(server_id).await?;
        if client.state.registry.read().await.resources_fresh() {
            return Ok(client.state.registry.read().await.resources().to_vec());
        }
        let resources = {
            let c = client.clone();
            self.with_timeout(client.state.config.request_timeout(), async move {
                c.state
                    .running()
                    .await
                    .ok_or_else(|| McpError::ConnectionClosed("connection not ready".into()))?
                    .list_all_resources()
                    .await
                    .map_err(McpError::from)
            })
            .await?
        };
        {
            let mut registry = client.state.registry.write().await;
            let result = ListResourcesResult::with_all_items(resources);
            registry.store_resources(&result)?;
        }
        Ok(client.state.registry.read().await.resources().to_vec())
    }

    /// Lists resource templates from `server_id`.
    ///
    /// # Errors
    ///
    /// Fails when the server is unknown, not ready, or the response is
    /// invalid or times out.
    pub async fn list_resource_templates(
        &self,
        server_id: &str,
    ) -> Result<Vec<NamespacedResourceTemplate>, McpError> {
        let client = self.require_client(server_id).await?;
        if client.state.registry.read().await.resource_templates_fresh() {
            return Ok(client.state.registry.read().await.resource_templates().to_vec());
        }
        let templates = {
            let c = client.clone();
            self.with_timeout(client.state.config.request_timeout(), async move {
                c.state
                    .running()
                    .await
                    .ok_or_else(|| McpError::ConnectionClosed("connection not ready".into()))?
                    .list_all_resource_templates()
                    .await
                    .map_err(McpError::from)
            })
            .await?
        };
        {
            let mut registry = client.state.registry.write().await;
            let result = ListResourceTemplatesResult::with_all_items(templates);
            registry.store_resource_templates(&result)?;
        }
        Ok(client.state.registry.read().await.resource_templates().to_vec())
    }

    /// Reads a resource by URI from `server_id`.
    ///
    /// # Errors
    ///
    /// Fails for unknown servers or protocol errors; MRTR elicitation rounds
    /// flow through the host.
    pub async fn read_resource(
        &self,
        server_id: &str,
        uri: &str,
    ) -> Result<ReadResourceResult, McpError> {
        let client = self.require_client(server_id).await?;
        let params = ReadResourceRequestParams::new(uri);
        self.with_timeout(client.state.config.request_timeout(), async move {
            client
                .state
                .running()
                .await
                .ok_or_else(|| McpError::ConnectionClosed("connection not ready".into()))?
                .read_resource(params)
                .await
                .map_err(McpError::from)
        })
        .await
    }

    /// Opens a `subscriptions/listen` stream for the given filter.
    ///
    /// Notifications routed through the returned subscription are not also
    /// delivered through handler callbacks (rmcp semantics).
    ///
    /// # Errors
    ///
    /// Fails when the server is unknown or the listen handshake fails.
    pub async fn subscribe(
        &self,
        server_id: &str,
        filter: SubscriptionFilter,
    ) -> Result<McpSubscription, McpError> {
        let client = self.require_client(server_id).await?;
        let subscription = self
            .with_timeout(client.state.config.request_timeout(), async move {
                client
                    .state
                    .running()
                    .await
                    .ok_or_else(|| McpError::ConnectionClosed("connection not ready".into()))?
                    .listen(filter)
                    .await
                    .map_err(McpError::from)
            })
            .await?;
        Ok(McpSubscription { subscription })
    }

    /// Invalidates every registry category for `server_id` after a
    /// list-changed notification; categories re-list on next access.
    pub async fn refresh_registry(&self, server_id: &str) -> Result<(), McpError> {
        let client = self.require_client(server_id).await?;
        client.state.registry.write().await.invalidate_all();
        Ok(())
    }

    async fn require_client(&self, server_id: &str) -> Result<Arc<McpClientHandle>, McpError> {
        self.clients
            .read()
            .await
            .get(server_id)
            .cloned()
            .ok_or_else(|| McpError::NotFound(format!("unknown mcp server {server_id:?}")))
    }

    /// Bounds a peer call by the server's configured request timeout.
    async fn with_timeout<T>(
        &self,
        timeout: Duration,
        call: impl std::future::Future<Output = Result<T, McpError>>,
    ) -> Result<T, McpError> {
        tokio::time::timeout(timeout, call)
            .await
            .map_err(|_| McpError::Timeout { timeout_ms: timeout.as_millis() as u64 })?
    }
}

/// Un-namespaces `<server_id>/<name>` back to the raw tool/prompt name.
fn unnamespace(server_id: &str, namespaced: &str) -> Result<String, McpError> {
    let prefix = format!("{server_id}/");
    namespaced
        .strip_prefix(&prefix)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            McpError::NotFound(format!("unknown primitive {namespaced:?} on server {server_id:?}"))
        })
}

/// Builds the transport for one connection attempt: a test factory when
/// installed, otherwise the configured stdio/HTTP transport.
#[cfg(feature = "test-utils")]
async fn test_transport(state: &McpClientState) -> Option<tokio::io::DuplexStream> {
    let factories = state.test_factories.inner.read().await;
    factories.get(&state.server_id).map(|factory| factory.build())
}

/// Builds the request metadata every request carries (protocol version, ee
/// client info, capabilities).  rmcp attaches this automatically in the
/// Discover lifecycle; the helper exists for tests and future manual calls.
#[must_use]
pub fn request_meta() -> RequestMetaObject {
    let mut capabilities = rmcp::model::ClientCapabilities::builder().enable_elicitation().build();
    // Field mutation is allowed for non-exhaustive structs; advertise both
    // form and URL elicitation support.
    capabilities.elicitation = Some(
        rmcp::model::ElicitationCapability::new()
            .with_form(rmcp::model::FormElicitationCapability::new())
            .with_url(rmcp::model::UrlElicitationCapability::new()),
    );
    RequestMetaObject::with_client_context(
        rmcp::model::ProtocolVersion::V_2026_07_28,
        rmcp::model::Implementation::new(crate::CLIENT_NAME, crate::CLIENT_VERSION)
            .with_title("ee agent editor"),
        capabilities,
    )
}

/// Drives one server connection: spawn/connect, discover, then supervise
/// reconnects while the manager keeps the request handle.
async fn supervise_connection(
    state: Arc<McpClientState>,
    shutdown: CancellationToken,
    first_ready_tx: oneshot::Sender<Result<(), McpError>>,
) {
    const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
    let mut first = Some(first_ready_tx);
    loop {
        let result = connect_once(&state, shutdown.clone()).await;
        let running = match result {
            Ok(running) => running,
            Err(error) => {
                if let Some(first) = first.take() {
                    let _ = first.send(Err(error.clone()));
                }
                if shutdown.is_cancelled() {
                    return;
                }
                state.emit(McpEvent::Diagnostics {
                    server_id: state.server_id.clone(),
                    message: format!("mcp connection failed: {error}"),
                });
                state.set_state(McpServerState::Failed).await;
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(RECONNECT_BACKOFF) => continue,
                }
            }
        };
        if let Some(first) = first.take() {
            let _ = first.send(Ok(()));
        }
        // Run the connection until the service terminates or shutdown is
        // requested; shutdown cancels the service and waits for closure.
        loop {
            if running.is_closed() {
                break;
            }
            tokio::select! {
                _ = shutdown.cancelled() => {
                    running.cancellation_token().cancel();
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
        if shutdown.is_cancelled() {
            // Give the service a moment to wind down (transport close, child
            // kill) before returning.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !running.is_closed() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            return;
        }
        state.emit(McpEvent::Diagnostics {
            server_id: state.server_id.clone(),
            message: "mcp connection closed; reconnecting".to_string(),
        });
        state.set_state(McpServerState::Failed).await;
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(RECONNECT_BACKOFF) => {}
        }
    }
}

/// Establishes one connection: transport + `server/discover` handshake.
async fn connect_once(
    state: &McpClientState,
    shutdown: CancellationToken,
) -> Result<Arc<RunningService<RoleClient, EeClientHandler>>, McpError> {
    let handler = EeClientHandler::new(state.server_id.clone(), state.events.clone());
    #[cfg(feature = "test-utils")]
    if let Some(transport) = test_transport(state).await {
        let running = Arc::new(establish(handler, transport, shutdown.clone()).await?);
        finish_connect(state, &running, shutdown).await?;
        return Ok(running);
    }
    match &state.config.kind {
        crate::config::McpServerKind::Stdio(_) => {
            let process = spawn_stdio(&state.config)?;
            state.emit(McpEvent::Diagnostics {
                server_id: state.server_id.clone(),
                message: format!("mcp server {} spawned", state.server_id),
            });
            let running = Arc::new(establish(handler, process.transport, shutdown.clone()).await?);
            finish_connect(state, &running, shutdown).await?;
            Ok(running)
        }
        crate::config::McpServerKind::StreamableHttp(_) => {
            let http = build_http_transport(&state.config)?;
            let running = Arc::new(establish(handler, http, shutdown.clone()).await?);
            finish_connect(state, &running, shutdown).await?;
            Ok(running)
        }
    }
}

/// Runs the Discover handshake against a transport.
async fn establish<T, E, A>(
    handler: EeClientHandler,
    transport: T,
    shutdown: CancellationToken,
) -> Result<RunningService<RoleClient, EeClientHandler>, McpError>
where
    T: rmcp::transport::IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    tokio::select! {
        result = handler.serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![rmcp::model::ProtocolVersion::V_2026_07_28],
            },
        ) => result.map_err(McpError::from),
        _ = shutdown.cancelled() => Err(McpError::Cancelled),
    }
}

/// Publishes the fresh service, discovery snapshot, and Ready state.
async fn finish_connect(
    state: &McpClientState,
    running: &Arc<RunningService<RoleClient, EeClientHandler>>,
    shutdown: CancellationToken,
) -> Result<(), McpError> {
    let peer = running.peer().clone();
    // Fresh discovery snapshot (the handshake already pinned the version).
    let result = tokio::select! {
        result = peer.discover(request_meta()) => result.map_err(McpError::from)?,
        _ = shutdown.cancelled() => return Err(McpError::Cancelled),
    };
    let snapshot = DiscoverySnapshot::parse(result)?;
    *state.running.write().await = Some(Arc::clone(running));
    state.discovery.write().await.store(snapshot.clone());
    state.registry.write().await.invalidate_all();
    state.emit(McpEvent::Discovery { server_id: state.server_id.clone(), snapshot });
    state.set_state(McpServerState::Ready).await;
    Ok(())
}

#[allow(dead_code)]
fn _protocol_const_note() {
    // The pinned version constant is exported for callers and tests.
    let _ = MCP_PROTOCOL_VERSION;
    let _ = DEFAULT_REQUEST_TIMEOUT_MS;
}
