//! `impl App` methods: MCP pane state and commands.
use super::*;

impl App {
    pub(in crate::app) fn mcp_servers_configured(&self) -> bool {
        !self.config.mcp.servers.is_empty() || self.config.mcp.proxy.enabled
    }

    /// Creates the MCP host bridge on first use (lazy; starts no process
    /// until the worker's `StartAll` runs).
    pub(super) fn ensure_mcp_host(&mut self) {
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
    pub(in crate::app) fn start_mcp_servers(&mut self) {
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
    pub(in crate::app) fn pump_mcp_events(&mut self) {
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

    pub(super) fn handle_mcp_event(&mut self, event: McpEvent) {
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
    pub(in crate::app) fn pump_mcp_replies(&mut self) {
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
    pub(in crate::app) fn agents_mcp_command(&mut self, tail: &str) {
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
            self.agents.layout = AgentPaneLayout::Full;
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
    pub(super) fn mcp_health_notice(&mut self) {
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
    pub(super) fn mcp_browse_request(&mut self, kind: McpBrowseKind) {
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
    pub(super) fn agents_mcp_insert(&mut self, text: &str) {
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
    pub(in crate::app) fn agents_mcp_select(&mut self, delta: isize) {
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
    pub(in crate::app) fn agents_mcp_confirm(&mut self) {
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
    pub(in crate::app) fn shutdown_mcp(&mut self) {
        if let Some(host) = self.agents.mcp.host.take() {
            drop(host);
        }
        self.agents.mcp.servers.clear();
        self.agents.mcp.browse = None;
        self.agents.mcp.proxy_mode = None;
    }
}
