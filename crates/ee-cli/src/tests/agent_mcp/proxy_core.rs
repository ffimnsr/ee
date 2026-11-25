use super::*;

// ── Prompt / resource browsing ───────────────────────────────────────────────

#[test]
fn prompt_browse_inserts_prompt_content_into_draft() {
    let script = mcp_standard_script(FakeMcpScript::new())
        .respond(
            "prompts/list",
            json!({
                "prompts": [
                    { "name": "summarize", "description": "Summarize the file",
                      "arguments": [] }
                ],
                "resultType": "complete", "ttlMs": 0, "cacheScope": "private",
            }),
        )
        .respond(
            "prompts/get",
            json!({
                "messages": [
                    { "role": "user",
                      "content": { "type": "text", "text": "Please summarize the file" } }
                ],
                "resultType": "complete",
            }),
        );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);
    wait_mcp_ready(&mut app, "tools");

    run_ex(&mut app, "agents_mcp prompts");
    wait_until(&mut app, "prompt list loaded", |app| {
        app.agents
            .mcp
            .browse
            .as_ref()
            .is_some_and(|browse| !browse.loading && !browse.items.is_empty())
    });
    let browse = app.agents.mcp.browse.as_ref().unwrap();
    assert_eq!(browse.items[0].label, "summarize");

    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "prompt content inserted", |app| {
        app.agents.threads[0].draft.contains("Please summarize the file")
    });
    assert!(app.agents.mcp.browse.is_none(), "browse closes after insertion");
}

#[test]
fn resource_browse_inserts_uri_mention() {
    let script = mcp_standard_script(FakeMcpScript::new()).respond(
        "resources/list",
        json!({
            "resources": [
                { "uri": "file:///tmp/notes.txt", "name": "notes" }
            ],
            "resultType": "complete", "ttlMs": 0, "cacheScope": "private",
        }),
    );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);
    wait_mcp_ready(&mut app, "tools");

    run_ex(&mut app, "agents_mcp resources");
    wait_until(&mut app, "resource list loaded", |app| {
        app.agents
            .mcp
            .browse
            .as_ref()
            .is_some_and(|browse| !browse.loading && !browse.items.is_empty())
    });
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "uri inserted", |app| {
        app.agents.threads[0].draft.contains("file:///tmp/notes.txt")
    });
}

#[test]
fn tool_list_changed_refreshes_pane_tool_metadata() {
    let script = FakeMcpScript::new()
        .discover_2026_07_28(json!({ "tools": {} }))
        .respond_once(
            "tools/list",
            json!({
                "tools": [{ "name": "toolA", "description": "a", "inputSchema": {} }],
                "resultType": "complete", "ttlMs": 0, "cacheScope": "private",
            }),
        )
        .emit(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }))
        .respond_once(
            "tools/list",
            json!({
                "tools": [{ "name": "toolB", "description": "b", "inputSchema": {} }],
                "resultType": "complete", "ttlMs": 0, "cacheScope": "private",
            }),
        );
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    install_mcp_fake(&mut app, script);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "tool metadata refreshed", |app| {
        app.agents
            .mcp
            .servers
            .get("tools")
            .is_some_and(|server| server.tools.contains(&String::from("tools/toolB")))
    });
    let server = &app.agents.mcp.servers["tools"];
    assert!(server.tools.contains(&String::from("tools/toolB")));
}

// ── ee MCP proxy: same permission broker as direct ACP methods ───────────────

#[test]
fn proxy_write_denial_leaves_buffer_unchanged() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("proxy.txt");
    fs::write(&target, "original").unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "write_text_file",
            "path": target.display().to_string(),
            "content": "agent-wrote-this",
        }),
    );
    // The approval prompt is queued in the pane (same path as ACP writes).
    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    let prompt = app.agents.approvals.front().unwrap();
    assert!(prompt.title.contains("fs/write_text_file"));
    assert!(prompt.detail.contains("proxy.txt"));

    // Deny through the same approval UI used for direct ACP methods.
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "approval resolved", |app| app.agents.approvals.is_empty());
    let reply = proxy_recv(&mut stream);

    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "original",
        "denied write must not touch disk"
    );
    let reply = reply.get("result").unwrap();
    assert!(reply.get("error").is_some(), "proxy must report the denial: {reply}");
}

#[test]
fn proxy_write_allow_writes_through_buffer() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("proxy-allow.txt");
    fs::write(&target, "original").unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "write_text_file",
            "path": target.display().to_string(),
            "content": "agent-wrote-this",
        }),
    );
    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // Allow once

    wait_until(&mut app, "file written", |_app| {
        fs::read_to_string(&target).map(|text| text == "agent-wrote-this").unwrap_or(false)
    });
    let reply = proxy_recv(&mut stream);
    assert!(
        reply.get("result").and_then(|r| r.get("value")).is_some(),
        "allowed write returns ok: {reply}"
    );
}

#[test]
fn proxy_workspace_roots_and_search_text_return_structured_results() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("search.txt");
    fs::write(&target, "alpha\nneedle beta\n").unwrap();
    let canonical_root = fs::canonicalize(temp.path()).unwrap();
    let canonical_target = fs::canonicalize(&target).unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(&mut stream, 1, json!({ "method": "workspace_roots" }));
    settle(&mut app);
    let roots_reply = proxy_recv(&mut stream);
    assert_eq!(
        roots_reply["result"]["value"]["roots"][0],
        json!(canonical_root.display().to_string())
    );

    proxy_send(&mut stream, 2, json!({ "method": "search_text", "query": "needle" }));
    settle(&mut app);
    let search_reply = proxy_recv(&mut stream);
    let matches = search_reply["result"]["value"]["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "search reply: {search_reply}");
    assert_eq!(matches[0]["path"], json!(canonical_target.display().to_string()));
    assert_eq!(matches[0]["line"], json!(2));
    assert_eq!(matches[0]["context"], json!("needle beta"));
}

#[test]
fn proxy_phase1_extras_return_structured_results() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let visible = temp.path().join("visible.rs");
    let hidden = temp.path().join(".hidden.rs");
    let ignored = temp.path().join("ignored.log");
    fs::write(temp.path().join(".ignore"), "ignored.log\n").unwrap();
    fs::write(&visible, "alpha\nregex-hit\nneedle\n").unwrap();
    fs::write(&hidden, "regex-hit hidden\n").unwrap();
    fs::write(&ignored, "needle ignored\n").unwrap();
    let canonical_visible = fs::canonicalize(&visible).unwrap();
    let canonical_hidden = fs::canonicalize(&hidden).unwrap();
    let canonical_ignored = fs::canonicalize(&ignored).unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(&mut stream, 1, json!({ "method": "list_directory_all", "path": temp.path() }));
    settle(&mut app);
    let list_reply = proxy_recv(&mut stream);
    let entries = list_reply["result"]["value"]["entries"].as_array().unwrap();
    assert!(
        entries.iter().any(|entry| entry["path"] == json!(canonical_hidden.display().to_string())
            && entry["hidden"] == json!(true))
    );
    assert!(
        entries.iter().any(|entry| entry["path"] == json!(canonical_ignored.display().to_string())
            && entry["ignored"] == json!(true))
    );

    proxy_send(&mut stream, 2, json!({ "method": "search_files_all", "pattern": "*.rs" }));
    settle(&mut app);
    let files_reply = proxy_recv(&mut stream);
    let file_matches = files_reply["result"]["value"]["matches"].as_array().unwrap();
    assert!(
        file_matches
            .iter()
            .any(|entry| entry["path"] == json!(canonical_hidden.display().to_string())
                && entry["hidden"] == json!(true))
    );

    proxy_send(&mut stream, 3, json!({ "method": "search_text_regex", "pattern": "regex-hit" }));
    settle(&mut app);
    let regex_reply = proxy_recv(&mut stream);
    let regex_matches = regex_reply["result"]["value"]["matches"].as_array().unwrap();
    assert!(
        regex_matches
            .iter()
            .any(|entry| entry["path"] == json!(canonical_visible.display().to_string()))
    );

    proxy_send(
        &mut stream,
        4,
        json!({ "method": "search_text_in_files", "query": "needle", "file_glob": "*.rs" }),
    );
    settle(&mut app);
    let scoped_reply = proxy_recv(&mut stream);
    let scoped_matches = scoped_reply["result"]["value"]["matches"].as_array().unwrap();
    assert_eq!(scoped_matches.len(), 1, "scoped reply: {scoped_reply}");
    assert_eq!(scoped_matches[0]["path"], json!(canonical_visible.display().to_string()));

    proxy_send(
        &mut stream,
        5,
        json!({
            "method": "search_text_regex",
            "pattern": "regex-hit",
            "file_glob": "*.rs",
        }),
    );
    settle(&mut app);
    let regex_scoped_reply = proxy_recv(&mut stream);
    let regex_scoped_matches = regex_scoped_reply["result"]["value"]["matches"].as_array().unwrap();
    assert_eq!(regex_scoped_matches.len(), 1, "regex scoped reply: {regex_scoped_reply}");
    assert_eq!(regex_scoped_matches[0]["path"], json!(canonical_visible.display().to_string()));
}

#[test]
fn proxy_phase1_path_tools_reject_outside_workspace_when_denied() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let outside = tempfile::tempdir().unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(
        &mut stream,
        1,
        json!({ "method": "list_directory", "path": outside.path().display().to_string() }),
    );
    wait_until(&mut app, "external read approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE); // Deny
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "external read approval resolved", |app| app.agents.approvals.is_empty());
    let reply = proxy_recv(&mut stream);
    let error = &reply["result"]["error"]["message"];
    assert!(
        error.as_str().unwrap_or_default().contains("external directory read denied"),
        "reply: {reply}"
    );
    assert!(app.agents.external_read_roots.is_empty(), "deny records no grant");
}

#[test]
fn proxy_external_read_session_grant_covers_later_reads_in_the_directory() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let outside = tempfile::tempdir().unwrap();
    let first = outside.path().join("first.txt");
    let second = outside.path().join("second.txt");
    fs::write(&first, "external-first").unwrap();
    fs::write(&second, "external-second").unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(
        &mut stream,
        1,
        json!({ "method": "read_buffer", "path": first.display().to_string() }),
    );
    wait_until(&mut app, "external read approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Right, KeyModifiers::NONE); // Allow directory for session
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "external read approval resolved", |app| app.agents.approvals.is_empty());
    let reply = proxy_recv(&mut stream);
    assert_eq!(reply["result"]["value"], json!("external-first"));
    let canonical = fs::canonicalize(outside.path()).unwrap();
    assert!(
        app.agents.external_read_roots.contains(&canonical),
        "session grant recorded: {:?}",
        app.agents.external_read_roots
    );

    proxy_send(
        &mut stream,
        2,
        json!({ "method": "read_buffer", "path": second.display().to_string() }),
    );
    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    assert_eq!(reply["result"]["value"], json!("external-second"));
    assert!(app.agents.approvals.is_empty(), "granted directory must not prompt again");

    // Reads are the only widened class: writes still fail closed without a
    // prompt, even inside the granted directory.
    proxy_send(
        &mut stream,
        3,
        json!({
            "method": "create_text_file",
            "path": outside.path().join("blocked.txt").display().to_string(),
            "content": "nope\n"
        }),
    );
    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    assert!(
        reply["result"]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("outside allowed workspace"),
        "write must stay fail-closed: {reply}"
    );
    assert!(app.agents.approvals.is_empty(), "denied write must not queue approval");
    assert!(!outside.path().join("blocked.txt").exists());

    // `/permissions revoke-read` drops the session grant; the next read asks
    // for approval again.
    type_text(&mut app, &format!("/permissions revoke-read {}", outside.path().display()));
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.agents.external_read_roots.is_empty(), "revoke drops the grant");
    proxy_send(
        &mut stream,
        4,
        json!({ "method": "read_buffer", "path": first.display().to_string() }),
    );
    wait_until(&mut app, "revoked read prompts again", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE); // Deny
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "revoked read denied", |app| app.agents.approvals.is_empty());
    let reply = proxy_recv(&mut stream);
    assert!(reply["result"]["error"].is_object(), "denied after revoke: {reply}");
}

#[test]
fn proxy_reference_prefix_reads_configured_sources_without_approval() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let reference = tempfile::tempdir().unwrap();
    let note = reference.path().join("note.txt");
    fs::write(&note, "pinned reference\n").unwrap();
    app.config.agents.references.insert(
        String::from("docs"),
        crate::config::AgentReferenceSettings::Local(reference.path().to_path_buf()),
    );
    app.agents.test_reference_cache_base = Some(reference.path().join("cache"));
    let mut stream = connect_proxy(&app);

    proxy_send(&mut stream, 1, json!({ "method": "read_buffer", "path": "docs/note.txt" }));
    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    assert_eq!(reply["result"]["value"], json!("pinned reference"));
    assert!(app.agents.approvals.is_empty(), "reference reads need no external approval");

    proxy_send(
        &mut stream,
        2,
        json!({
            "method": "create_text_file",
            "path": "docs/blocked.txt",
            "content": "nope\n"
        }),
    );
    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    assert!(
        reply["result"]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("path must be absolute"),
        "reference roots are never write roots: {reply}"
    );
    assert!(!reference.path().join("blocked.txt").exists());

    let absolute_write = reference.path().join("blocked-abs.txt");
    proxy_send(
        &mut stream,
        3,
        json!({
            "method": "create_text_file",
            "path": absolute_write.display().to_string(),
            "content": "nope\n"
        }),
    );
    settle(&mut app);
    let reply = proxy_recv(&mut stream);
    assert!(
        reply["result"]["error"]["message"].as_str().unwrap_or_default().contains("read-only"),
        "absolute writes into reference roots fail closed: {reply}"
    );
    assert!(!absolute_write.exists());
}

#[test]
fn proxy_phase1_caps_truncate_results() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    for index in 0..560 {
        fs::write(temp.path().join(format!("cap-{index:03}.rs")), "cap-hit\n").unwrap();
    }
    let mut stream = connect_proxy(&app);

    proxy_send(&mut stream, 1, json!({ "method": "list_directory_all", "path": temp.path() }));
    settle(&mut app);
    let list_reply = proxy_recv(&mut stream);
    let list_value = &list_reply["result"]["value"];
    assert_eq!(list_value["truncated"], json!(true), "reply: {list_reply}");
    assert!(list_value["entries"].as_array().unwrap().len() < 560, "reply: {list_reply}");

    proxy_send(&mut stream, 2, json!({ "method": "search_files_all", "pattern": "*.rs" }));
    settle(&mut app);
    let files_reply = proxy_recv(&mut stream);
    let files_value = &files_reply["result"]["value"];
    assert_eq!(files_value["truncated"], json!(true), "reply: {files_reply}");
    assert!(files_value["matches"].as_array().unwrap().len() < 560, "reply: {files_reply}");

    proxy_send(&mut stream, 3, json!({ "method": "search_text", "query": "cap-hit" }));
    settle(&mut app);
    let text_reply = proxy_recv(&mut stream);
    let text_value = &text_reply["result"]["value"];
    assert_eq!(text_value["truncated"], json!(true), "reply: {text_reply}");
    assert!(text_value["matches"].as_array().unwrap().len() < 560, "reply: {text_reply}");
}
