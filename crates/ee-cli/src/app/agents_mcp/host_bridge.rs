//! MCP host worker and bridge.
use super::proxy_server::serve_proxy_listener;
use super::*;

pub(super) fn mcp_host_worker(
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
pub(super) async fn list_prompts_values(
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
pub(super) async fn list_resources_values(
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
pub(super) async fn list_tool_keys(
    manager: &McpClientManager,
) -> Result<Vec<String>, ee_mcp::McpError> {
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
pub(super) fn namespaced_server(key: &str) -> Result<String, ee_mcp::McpError> {
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
    pub(super) fn new(
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
    pub(super) fn start_all(&self) {
        let _ = self.commands.send(McpHostCommand::StartAll);
    }

    /// Lists prompts from every ready server as browse values.
    pub(super) fn list_prompts(
        &self,
    ) -> std_mpsc::Receiver<Result<Vec<serde_json::Value>, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(McpHostCommand::ListPrompts { reply: reply_tx });
        reply_rx
    }

    /// Fetches one prompt's content (namespaced key).
    pub(super) fn get_prompt(&self, key: &str) -> std_mpsc::Receiver<Result<String, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ =
            self.commands.send(McpHostCommand::GetPrompt { key: key.to_string(), reply: reply_tx });
        reply_rx
    }

    /// Lists resources from every ready server as browse values.
    pub(super) fn list_resources(
        &self,
    ) -> std_mpsc::Receiver<Result<Vec<serde_json::Value>, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(McpHostCommand::ListResources { reply: reply_tx });
        reply_rx
    }

    /// Lists namespaced tool keys from every ready server.
    pub(super) fn list_tools(&self) -> std_mpsc::Receiver<Result<Vec<String>, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(McpHostCommand::ListTools { reply: reply_tx });
        reply_rx
    }

    /// Invalidates a server's primitive registries (list-changed notification).
    pub(super) fn refresh_registry(&self, server_id: &str) {
        let _ = self
            .commands
            .send(McpHostCommand::RefreshRegistry { server_id: server_id.to_string() });
    }

    /// Installs a fake transport factory (test-utils only; sent before the
    /// worker's `StartAll` so the first connection uses the fake).
    #[cfg(test)]
    pub(super) fn install_fake(
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
