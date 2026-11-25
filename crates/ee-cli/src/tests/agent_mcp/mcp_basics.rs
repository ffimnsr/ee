use super::*;

#[test]
fn stdio_proxy_mcp_frames_cover_read_write_and_execute_classes() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let info = app.agents.mcp.proxy.clone().expect("proxy listener available");
    drop(connect_proxy(&app)); // Wait for listener bind before subprocess-side socket connect.
    let target = temp.path().join("stdio-proxy-write.txt");
    let target_text = target.display().to_string();

    let (client_stream, server_stream) = tokio::io::duplex(128 * 1024);
    let server = thread::spawn(move || {
        crate::app::agents_mcp::run_proxy_stdio_with_duplex(
            info.socket_path,
            info.token,
            server_stream,
        )
        .expect("stdio proxy exits cleanly after client EOF");
    });
    let (result_tx, result_rx) = mpsc::channel();
    let client = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let (reader, mut writer) = tokio::io::split(client_stream);
            let mut reader = tokio::io::BufReader::new(reader).lines();
            async fn request(
                writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
                reader: &mut tokio::io::Lines<tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
                id: u64,
                method: &str,
                mut params: Value,
            ) -> Value {
                params.as_object_mut().expect("request params object").insert(
                    String::from("_meta"),
                    json!({
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientInfo": {
                            "name": "stdio-test",
                            "version": "1"
                        },
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }),
                );
                let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
                writer.write_all(request.to_string().as_bytes()).await.unwrap();
                writer.write_all(b"\n").await.unwrap();
                writer.flush().await.unwrap();
                let line = reader.next_line().await.unwrap().expect("stdio response");
                serde_json::from_str(&line).unwrap()
            }

            let discovered =
                request(&mut writer, &mut reader, 1, "server/discover", json!({})).await;
            let list = request(&mut writer, &mut reader, 2, "tools/list", json!({})).await;
            let manifest = request(
                &mut writer,
                &mut reader,
                3,
                "tools/call",
                json!({ "name": "ee_tools_manifest", "arguments": {} }),
            )
            .await;
            let read = request(
                &mut writer,
                &mut reader,
                4,
                "tools/call",
                json!({ "name": "ee_workspace_roots", "arguments": {} }),
            )
            .await;
            let write = request(
                &mut writer,
                &mut reader,
                5,
                "tools/call",
                json!({ "name": "ee_write_text_file", "arguments": { "path": target_text, "content": "stdio" } }),
            )
            .await;
            let execute = request(
                &mut writer,
                &mut reader,
                6,
                "tools/call",
                json!({ "name": "ee_terminal_create", "arguments": { "command": "true" } }),
            )
            .await;
            (discovered, list, manifest, read, write, execute)
        });
        result_tx.send(result).unwrap();
    });

    let deadline = Instant::now() + WAIT;
    let responses = loop {
        if let Ok(responses) = result_rx.try_recv() {
            break responses;
        }
        app.pump_agents();
        let _ = app.backend.drain_events();
        if !app.agents.approvals.is_empty() {
            allow_pending_approval_once(&mut app); // Allow once for write and `true` terminal.
        }
        assert!(Instant::now() < deadline, "stdio MCP proxy did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    client.join().expect("stdio client thread");
    server.join().expect("stdio server thread");

    let (discovered, list, manifest, read, write, execute) = responses;
    assert!(
        discovered["result"]["supportedVersions"]
            .as_array()
            .is_some_and(|versions| versions.contains(&json!("2026-07-28")))
    );
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"ee_workspace_roots"));
    assert!(names.contains(&"ee_write_text_file"));
    assert!(names.contains(&"ee_terminal_create"));
    assert!(!names.contains(&"ee_terminal_output"), "ACP-only tool hidden on stdio");
    assert_eq!(manifest["result"]["isError"], json!(false));
    assert_eq!(read["result"]["isError"], json!(false));
    assert_eq!(write["result"]["isError"], json!(false));
    assert_eq!(execute["result"]["isError"], json!(false));
    assert_eq!(fs::read_to_string(target).unwrap().trim_end(), "stdio");
}

// ── MCP config forwarding ────────────────────────────────────────────────────

#[test]
fn session_new_receives_mcp_config_when_configured() {
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), true, false);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let params = session_new[0].get("params").expect("params");
    let servers = params.get("mcpServers").expect("mcpServers present");
    let server = servers
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("tools"));
    let server = server.expect("tools server forwarded");
    assert_eq!(server.get("command").and_then(Value::as_str), Some("mcp-tools"));
    assert_eq!(server.get("args").and_then(Value::as_array).unwrap(), &vec![json!("serve")]);
}

#[test]
fn session_new_omits_mcp_config_when_none_configured() {
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), false, false);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let params = session_new[0].get("params").expect("params");
    let servers = params.get("mcpServers").expect("mcpServers field present");
    assert!(servers.as_array().unwrap().is_empty(), "no servers must be forwarded");
}

#[test]
fn session_new_forwards_proxy_stdio_entry_when_proxy_enabled() {
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let servers = session_new[0]
        .get("params")
        .and_then(|params| params.get("mcpServers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let proxy = servers
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("ee"))
        .expect("ee proxy entry forwarded");
    let args = proxy.get("args").and_then(Value::as_array).unwrap();
    assert!(args.contains(&json!("--mcp-proxy")));
    let env = proxy.get("env").and_then(Value::as_array).unwrap();
    assert!(env.iter().any(|variable| {
        variable.get("name").and_then(Value::as_str) == Some("EE_MCP_PROXY_SOCKET")
    }));
}

// ── MCP health registry ──────────────────────────────────────────────────────

#[test]
fn mcp_health_shows_ready_identity_and_capabilities() {
    let script = FakeMcpScript::new().respond(
        "server/discover",
        json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": { "tools": {}, "prompts": {} },
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": "fake-mcp", "version": "9.9", "title": "Fake Tools"
                }
            },
        }),
    );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "mcp server ready", |app| {
        app.agents
            .mcp
            .servers
            .get("tools")
            .is_some_and(|server| server.state == ee_mcp::McpServerState::Ready)
    });
    let server = &app.agents.mcp.servers["tools"];
    let identity = server.identity.as_deref().unwrap_or_default();
    assert!(identity.contains("Fake Tools"), "identity: {identity}");
    assert!(server.capabilities.contains("tools"), "capabilities: {}", server.capabilities);
    assert!(server.capabilities.contains("prompts"));

    // Health lines are deterministic and non-fatal for the ACP chat.
    let lines = app.mcp_health_lines();
    assert!(lines.iter().any(|line| line.contains("tools") && line.contains("ready")));
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Ready);
}

#[test]
fn mcp_health_failure_is_non_fatal_and_shows_failed_state() {
    let script = FakeMcpScript::new().respond(
        "server/discover",
        json!({
            "resultType": "complete",
            "supportedVersions": ["2025-11-25"],
            "capabilities": {},
            "ttlMs": 0,
            "cacheScope": "private",
        }),
    );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "mcp server failed", |app| {
        app.agents
            .mcp
            .servers
            .get("tools")
            .is_some_and(|server| server.state == ee_mcp::McpServerState::Failed)
    });
    // The ACP chat still works despite the failed MCP server.
    assert_eq!(app.agents.threads[0].state, ThreadUiState::Ready);
}

#[test]
fn mcp_servers_start_only_when_pane_opens() {
    let script = FakeMcpScript::new().discover_2026_07_28(json!({ "tools": {} }));
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);

    // Pane closed: no MCP host, no fake server spawned.
    assert!(app.agents.mcp.host.is_none());
    assert!(app.agents.mcp.servers.is_empty());

    open_pane_and_wait_ready(&mut app);
    wait_until(&mut app, "mcp server ready", |app| {
        app.agents
            .mcp
            .servers
            .get("tools")
            .is_some_and(|server| server.state == ee_mcp::McpServerState::Ready)
    });
    let _ = fake.agent();
}
