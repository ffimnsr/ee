use super::*;

// ── lifecycle closure ────────────────────────────────────────────────────────

#[tokio::test]
async fn mcp_over_acp_turn_cancel_closes_logical_connections() {
    let script = connect_and_discover_script()
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202)
        .wait_for("session/prompt")
        // The driver processes `session/cancel` (notification + close of all
        // logical MCP connections) in one command; a short settle after the
        // notification guarantees the serve threads have exited before the
        // next message probes the closed registry.
        .wait_for("session/cancel")
        .delay(100)
        .emit(emit_message(203, "tools/list", None));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread = connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");

    // The first tools/list round trip works (the connection exists).
    let list = await_response(&fake, 202).await;
    assert!(list["result"]["tools"].is_array());

    // Start a turn (the fake consumes session/prompt), then cancel it.
    let blocks =
        vec![ee_agent_protocol::ContentBlock::Text(ee_agent_protocol::TextContent::new("hi"))];
    let prompt = tokio::spawn({
        let thread = thread.clone();
        async move { thread.send_prompt(blocks).await }
    });
    // Wait until the host actually sent session/prompt before cancelling.
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !fake.log_contains("\"method\":\"session/prompt\"") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session/prompt sent");
    thread.cancel().await.expect("turn cancelled");
    let result = prompt.await.expect("prompt task finished");
    assert!(matches!(result, Err(AgentError::Cancelled)));

    // Turn cancel closed every logical MCP connection: the next message is
    // rejected with invalid params.
    let reply = await_response(&fake, 203).await;
    let error = reply["error"].as_object().expect("message after cancel must fail");
    assert_eq!(error["code"], json!(-32602));
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn mcp_over_acp_host_close_closes_logical_connections_deterministically() {
    let script = connect_and_discover_script()
        .emit(emit_message(202, "tools/list", None))
        .wait_for_response(202);
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(vec![PathBuf::from("/work")], Vec::new(), Some(stdio_fallback()))
        .await
        .expect("session starts");
    await_response(&fake, 202).await;

    // App shutdown: the connection closes every logical MCP connection
    // without hanging (the fake's driver reaches its script end and exits).
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}

// ── SDK gap audit (Phase 6b criteria) ────────────────────────────────────────

/// Proves no duplicate incompatible `rmcp` major version can leak into ee
/// public APIs: the lockfile pins exactly one `rmcp` (3.x) and no
/// `agent-client-protocol-rmcp` package (its `rmcp ^2.x` dependency is the
/// documented SDK gap).
#[test]
fn mcp_over_acp_workspace_has_single_rmcp_major_and_no_rmcp_bridge_crate() {
    let lockfile = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lockfile = std::fs::canonicalize(&lockfile).expect("workspace Cargo.lock");
    let lock = std::fs::read_to_string(&lockfile).expect("read Cargo.lock");
    let packages = lock.split("[[package]]").skip(1).collect::<Vec<_>>();

    let rmcp_versions = packages
        .iter()
        .filter(|package| package.lines().any(|line| line.trim() == "name = \"rmcp\""))
        .filter_map(|package| {
            package
                .lines()
                .find_map(|line| line.trim().strip_prefix("version = \""))
                .map(|version| version.trim_end_matches('"').to_string())
        })
        .collect::<Vec<_>>();
    assert_eq!(rmcp_versions.len(), 1, "exactly one rmcp package: {rmcp_versions:?}");
    assert!(rmcp_versions[0].starts_with("3."), "rmcp must stay on 3.x: {}", rmcp_versions[0]);

    let has_bridge = packages.iter().any(|package| {
        package.lines().any(|line| line.trim() == "name = \"agent-client-protocol-rmcp\"")
    });
    assert!(
        !has_bridge,
        "agent-client-protocol-rmcp would pull rmcp 2.x; keep the local adapter until upstream ships rmcp 3.x support"
    );
}

/// The host never advertises the proxy as an MCP root or any other MCP
/// resource: workspace directories stay in plain session metadata.
#[tokio::test]
async fn mcp_over_acp_workspace_directories_stay_in_plain_session_metadata() {
    let (fake, host) = spawn_host(acp_base_script(), Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    connection
        .new_session(
            vec![PathBuf::from("/work"), PathBuf::from("/extra")],
            Vec::new(),
            Some(stdio_fallback()),
        )
        .await
        .expect("session starts");

    let session_new = fake.requests_by_method("session/new");
    let params = session_new[0].get("params").expect("params");
    assert_eq!(params["cwd"], json!("/work"));
    assert_eq!(params["additionalDirectories"], json!(["/extra"]));
    assert!(params.get("roots").is_none(), "workspace dirs must never be forwarded as MCP roots");
    host.connection.close().await;
    fake.join(TEST_TIMEOUT).await;
}
