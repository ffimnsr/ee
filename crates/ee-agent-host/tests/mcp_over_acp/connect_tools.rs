use super::*;

#[tokio::test]
async fn mcp_over_acp_filesystem_write_routes_through_the_handler() {
    let script = connect_and_discover_script()
        .emit(emit_message(
            202,
            "tools/call",
            Some(json!({
                "name": "ee_move_path",
                "arguments": {
                    "source_path": "/work/old",
                    "destination_path": "/work/new"
                }
            })),
        ))
        .wait_for_response(202);
    let handler = Arc::new(ScriptedHandler::default());
    let (fake, host) = spawn_host(script, handler.clone()).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let response = await_response(&fake, 202).await;
    assert_eq!(response["result"]["structuredContent"]["path"], json!("/work/old"));
    assert_eq!(response["result"]["structuredContent"]["destinationPath"], json!("/work/new"));
    assert!(handler.seen().iter().any(|request| matches!(
        request,
        ClientRequest::ProxyMovePath { source_path, destination_path }
            if source_path == "/work/old" && destination_path == "/work/new"
    )));

    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_open_buffers_tool_round_trips_through_the_handler() {
    let script = connect_and_discover_script()
        .emit(emit_message(202, "tools/call", Some(json!({ "name": "ee_open_buffers" }))))
        .wait_for_response(202);
    let handler = Arc::new(ScriptedHandler::default());
    let (fake, host) = spawn_host(script, handler.clone()).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let response = await_response(&fake, 202).await;
    assert_eq!(response["result"]["structuredContent"]["buffers"][0]["languageId"], json!("rust"));
    assert!(
        handler.seen().iter().any(|request| matches!(request, ClientRequest::ProxyOpenBuffers))
    );

    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_connect_and_tools_list_round_trip() {
    let script = connect_and_discover_script()
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202);
    let (fake, host) =
        spawn_host(script, Arc::new(RecordingHandler::new(HandlerCapabilities::all()))).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let connect = await_response(&fake, 200).await;
    let connection_id = connect["result"]["connectionId"].as_str().expect("connection id");
    assert!(connection_id.starts_with("ee-mcp:"), "ee-owned connection id: {connection_id}");

    let list = await_response(&fake, 202).await;
    let tools = list["result"]["tools"].as_array().expect("tools list in result");
    let names: Vec<&str> =
        tools.iter().filter_map(|tool| tool.get("name").and_then(Value::as_str)).collect();
    assert_eq!(
        names,
        vec![
            "ee_workspace_roots",
            "ee_list_directory",
            "ee_list_directory_all",
            "ee_search_files",
            "ee_search_files_all",
            "ee_search_text",
            "ee_search_text_regex",
            "ee_search_text_in_files",
            "ee_web_search",
            "ee_fetch_url",
            "ee_browser_run_content",
            "ee_browser_run_screenshot",
            "ee_browser_run_markdown",
            "ee_browser_run_scrape",
            "ee_browser_run_json",
            "ee_browser_run_links",
            "ee_replace_text",
            "ee_apply_patch",
            "ee_create_text_file",
            "ee_overwrite_text_file",
            "ee_create_directory",
            "ee_delete_path",
            "ee_copy_path",
            "ee_move_path",
            "ee_read_buffer",
            "ee_read_buffer_lines",
            "ee_open_buffers",
            "ee_get_diagnostics",
            "ee_get_file_diagnostics",
            "ee_document_symbols",
            "ee_references",
            "ee_list_code_actions",
            "ee_apply_code_action",
            "ee_format_file",
            "ee_preview_rename_symbol",
            "ee_rename_symbol",
            "ee_read_text_file",
            "ee_write_text_file",
            "ee_terminal_create",
            "ee_terminal_output",
            "ee_terminal_output_since",
            "ee_terminal_wait",
            "ee_terminal_wait_long",
            "ee_terminal_kill",
            "ee_terminal_release",
            "ee_git_status",
            "ee_git_diff",
            "ee_git_diff_staged",
            "ee_git_diff_file",
            "ee_changed_files",
            "ee_review_context",
            "ee_project_instructions",
            "ee_save_note",
            "ee_read_notes",
            "ee_read_note",
            "ee_update_note",
            "ee_delete_note",
            "ee_remember_workspace_fact",
            "ee_replace_workspace_fact",
            "ee_recall_workspace_facts",
            "ee_read_workspace_fact",
            "ee_forget_workspace_fact",
            "ee_list_workspace_facts",
            "ee_retract_workspace_fact",
            "ee_export_workspace_memory",
            "ee_import_workspace_memory",
            "ee_clear_workspace_memory",
            "ee_file_dependency_map",
            "ee_symbol_dependency_map",
            "ee_tools_manifest",
            "ee_diagnostics",
        ]
    );
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_manifest_round_trips_complete_governance_metadata() {
    let script = connect_and_discover_script()
        .emit(emit_message(202, "tools/call", Some(json!({ "name": "ee_tools_manifest" }))))
        .wait_for_response(202);
    let (fake, host) =
        spawn_host(script, Arc::new(RecordingHandler::new(HandlerCapabilities::all()))).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let manifest = await_response(&fake, 202).await;
    assert_eq!(
        manifest["result"]["structuredContent"]["manifestVersion"],
        json!(ee_mcp::EE_TOOL_SCHEMA_VERSION)
    );
    let tools =
        manifest["result"]["structuredContent"]["tools"].as_array().expect("manifest tool list");
    assert!(!tools.is_empty());
    for tool in tools {
        assert!(tool["inputSchema"].is_object(), "input schema");
        assert!(tool["transportAvailability"].as_array().is_some_and(|routes| !routes.is_empty()));
        assert!(tool["outputCaps"].as_array().is_some_and(|caps| !caps.is_empty()));
        assert!(tool["redactionRules"].as_array().is_some_and(|rules| !rules.is_empty()));
        assert!(tool["errorClasses"].as_array().is_some_and(|classes| !classes.is_empty()));
    }

    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_message_before_connect_is_rejected_with_invalid_params() {
    let script = acp_base_script().emit(json!({
        "jsonrpc": "2.0",
        "id": 200,
        "method": "mcp/message",
        "params": { "connectionId": "nope", "method": "tools/list" }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 200).await;
    let error = reply["error"].as_object().expect("invalid params error");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_connect_unknown_server_id_is_rejected() {
    let script = acp_base_script().emit(json!({
        "jsonrpc": "2.0",
        "id": 200,
        "method": "mcp/connect",
        "params": { "serverId": "some-other-server" }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 200).await;
    let error = reply["error"].as_object().expect("invalid params error");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_connect_without_acp_capability_fails_closed() {
    // Proxy configured (armed) but the agent never advertised acp support:
    // the host must reject mcp/connect even though the server id is valid.
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({ "protocolVersion": 1, "agentCapabilities": {} }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 200,
            "method": "mcp/connect",
            "params": { "serverId": "ee-mcp-proxy:fake" }
        }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 200).await;
    let error = reply["error"].as_object().expect("invalid params error");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_disconnect_unknown_connection_is_rejected() {
    let script = acp_base_script().emit(json!({
        "jsonrpc": "2.0",
        "id": 200,
        "method": "mcp/disconnect",
        "params": { "connectionId": "ee-mcp:fake:99" }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let reply = await_response(&fake, 200).await;
    let error = reply["error"].as_object().expect("invalid params error");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_repeated_connects_yield_distinct_connection_ids() {
    let script = acp_base_script()
        .emit(emit_connect(200))
        .capture(CaptureSource::Response { id: 200 }, "result.connectionId", "conn_id")
        .emit(emit_connect(201))
        .capture(CaptureSource::Response { id: 201 }, "result.connectionId", "conn_id2");
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let first = await_response(&fake, 200).await;
    let second = await_response(&fake, 201).await;
    let first_id = first["result"]["connectionId"].as_str().expect("first id");
    let second_id = second["result"]["connectionId"].as_str().expect("second id");
    assert_ne!(first_id, second_id, "connection ids must be unique");
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_disconnect_closes_the_logical_connection() {
    let script = connect_and_discover_script()
        .emit(json!({
            "jsonrpc": "2.0",
            "id": 202,
            "method": "mcp/disconnect",
            "params": { "connectionId": { "$capture": "conn_id" } }
        }))
        .wait_for_response(202)
        .emit(emit_message(203, "tools/list", None));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    await_response(&fake, 202).await; // disconnect ok
    let reply = await_response(&fake, 203).await;
    let error = reply["error"].as_object().expect("message after disconnect must fail");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_oversized_message_frame_fails_closed() {
    let huge = "x".repeat(MCP_OVER_ACP_MAX_FRAME_BYTES + 1);
    let script = connect_and_discover_script()
        .emit(emit_message(202, "tools/list", Some(json!({ "pad": huge }))))
        .emit(emit_message(203, "tools/list", None));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    // The oversized frame is rejected and closes the logical connection...
    let reply = await_response(&fake, 202).await;
    let error = reply["error"].as_object().expect("oversized frame fails closed");
    assert_eq!(error["code"], json!(-32602));
    // ...so the next message on the same connection id fails too.
    let reply = await_response(&fake, 203).await;
    let error = reply["error"].as_object().expect("closed connection rejects messages");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_inner_notification_does_not_break_the_connection() {
    let script = connect_and_discover_script()
        .emit(emit_notification("notifications/cancelled", json!({ "requestId": 999 })))
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    let list = await_response(&fake, 202).await;
    let Some(tools) = list["result"]["tools"].as_array() else {
        panic!("tools still listed: list={list}\nhost log: {:?}", fake.log());
    };
    assert!(!tools.is_empty());
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_notification_before_the_lifecycle_probe_does_not_break_the_connection() {
    // rmcp's pre-initialize negotiation answers `server/discover` as a
    // bootstrap probe without selecting the lifecycle, so a notification sent
    // right after it is read by the still-negotiating loop.  The host must hold
    // notifications back instead of letting rmcp fail the whole connection.
    let script = acp_base_script()
        .emit(emit_connect(200))
        .capture(CaptureSource::Response { id: 200 }, "result.connectionId", "conn_id")
        .emit(emit_notification("notifications/cancelled", json!({ "requestId": 999 })))
        .emit(emit_message(201, "server/discover", Some(json!({}))))
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    await_response(&fake, 201).await;
    let list = await_response(&fake, 202).await;
    let Some(tools) = list["result"]["tools"].as_array() else {
        panic!("tools still listed: list={list}\nhost log: {:?}", fake.log());
    };
    assert!(!tools.is_empty());
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}
