use super::*;

// ── tool calls through the approval/bridge path ──────────────────────────────

#[tokio::test]
async fn mcp_over_acp_write_tool_denial_leaves_buffer_and_disk_unchanged() {
    let handler = ScriptedHandler::default();
    let script = connect_and_discover_script()
        .emit(emit_message(
            202,
            "tools/call",
            Some(json!({
                "name": "ee_write_text_file",
                "arguments": { "path": "/tmp/ee-mcp-over-acp-write.txt", "content": "boom" }
            })),
        ))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(true));
    let text = result["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("denial text");
    assert!(text.contains("denied"), "denial surfaced to the agent: {text}");

    // The denial came from the shared handler (approval path), not a crafted
    // ACP-side result: the request reached the handler exactly once.
    let seen = handler.seen();
    assert_eq!(seen.len(), 1);
    assert!(matches!(&seen[0], ClientRequest::WriteTextFile(WriteTextFileRequest { .. })));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_terminal_create_denial_does_not_spawn_terminal() {
    let handler = ScriptedHandler::default();
    let script = connect_and_discover_script()
        .emit(emit_message(
            202,
            "tools/call",
            Some(json!({
                "name": "ee_terminal_create",
                "arguments": { "command": "touch", "args": ["/tmp/ee-denied-terminal"] }
            })),
        ))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(true));
    // The handler was asked, so the terminal was denied before any spawn.
    let seen = handler.seen();
    assert_eq!(seen.len(), 1);
    assert!(matches!(&seen[0], ClientRequest::CreateTerminal(CreateTerminalRequest { .. })));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_read_tool_round_trips_through_the_handler() {
    let handler = ScriptedHandler::default();
    let script = connect_and_discover_script()
        .emit(emit_message(
            202,
            "tools/call",
            Some(json!({
                "name": "ee_read_text_file",
                "arguments": { "path": "/tmp/ee-mcp-over-acp-read.txt" }
            })),
        ))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(false));
    let text = result["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("read text");
    assert_eq!(text, "file contents");
    let seen = handler.seen();
    assert!(matches!(&seen[0], ClientRequest::ReadTextFile(ReadTextFileRequest { .. })));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn critic_profile_filters_discovery_and_rejects_cached_mutation_calls() {
    let handler = RecordingHandler::new(HandlerCapabilities::all());
    let script = connect_and_discover_script()
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202)
        .emit(emit_message(
            203,
            "tools/call",
            Some(json!({
                "name": "ee_write_text_file",
                "arguments": { "path": "/tmp/x", "content": "boom" }
            })),
        ))
        .wait_for_response(203);
    let (fake, host) = spawn_host_with_profile(
        script,
        Arc::new(handler.clone()),
        true,
        EeProxyToolProfile::CriticReadOnly,
    )
    .await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let list = await_response(&fake, 202).await;
    let names = list["result"]["tools"]
        .as_array()
        .expect("tools list")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(names.contains(&"ee_read_text_file"));
    assert!(names.contains(&"ee_read_workspace_fact"));
    assert!(names.contains(&"ee_recall_workspace_facts"));
    assert!(names.contains(&"ee_tools_manifest"));
    assert!(!names.contains(&"ee_remember_workspace_fact"));
    assert!(!names.contains(&"ee_forget_workspace_fact"));
    assert!(!names.contains(&"ee_write_text_file"));
    assert!(!names.contains(&"ee_terminal_output"));
    assert!(!names.contains(&"ee_web_search"));

    let rejected = await_response(&fake, 203).await;
    assert_eq!(rejected["result"]["isError"], json!(true));
    assert!(handler.seen().is_empty(), "cached mutation must not reach handler");
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_unadvertised_capabilities_are_rejected_before_the_handler() {
    // The capability gate lives in `proxy_executor`: a handler without
    // capabilities must fail closed before it is invoked.
    let handler = RecordingHandler::new(HandlerCapabilities::none());
    let script = connect_and_discover_script()
        .emit(emit_message(
            202,
            "tools/call",
            Some(json!({
                "name": "ee_write_text_file",
                "arguments": { "path": "/tmp/x", "content": "boom" }
            })),
        ))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(handler.clone())).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(true));
    assert!(handler.seen().is_empty(), "handler must not run for unadvertised capabilities");
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_diagnostics_tool_returns_bounded_redacted_stderr() {
    let script = connect_and_discover_script()
        .emit(emit_message(202, "tools/call", Some(json!({ "name": "ee_diagnostics" }))))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 202).await;
    let result = reply["result"].as_object().expect("tool result payload");
    assert_eq!(result["isError"], json!(true));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}
