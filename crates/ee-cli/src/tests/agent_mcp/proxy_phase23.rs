use super::*;

#[test]
fn proxy_phase2_open_buffers_and_read_buffer_reflect_unsaved_state() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("dirty.rs");
    fs::write(&target, "alpha\nbeta\n").unwrap();
    let id = app.backend.open_buffer(Some(target.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "buffer active", |app| {
        app.backend.active().path.as_deref() == Some(target.as_path())
    });
    app.backend.replace_line_range(0, 0, &[String::from("dirty-alpha")]).unwrap();
    app.backend.flush_all_pending_edits().unwrap();
    app.backend.set_selections(&[SelectionRange { start: 0, end: 5 }]).unwrap();
    wait_until(&mut app, "buffer dirty", |app| !app.backend.active().pristine);

    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, json!({ "method": "open_buffers" }));
    settle(&mut app);
    let buffers_reply = proxy_recv(&mut stream);
    let buffers = buffers_reply["result"]["value"]["buffers"].as_array().unwrap();
    let entry =
        buffers.iter().find(|entry| entry["path"] == json!(target.display().to_string())).unwrap();
    assert_eq!(entry["dirty"], json!(true), "reply: {buffers_reply}");
    assert_eq!(entry["languageId"], json!("rust"), "reply: {buffers_reply}");

    proxy_send(
        &mut stream,
        2,
        json!({ "method": "read_buffer", "path": target.display().to_string() }),
    );
    settle(&mut app);
    let read_reply = proxy_recv(&mut stream);
    assert_eq!(read_reply["result"]["value"], json!("dirty-alpha\nbeta"));

    proxy_send(
        &mut stream,
        3,
        json!({ "method": "read_buffer_lines", "path": target.display().to_string(), "line": 1, "limit": 1 }),
    );
    settle(&mut app);
    let lines_reply = proxy_recv(&mut stream);
    assert_eq!(lines_reply["result"]["value"], json!("dirty-alpha"));
}

#[test]
fn proxy_phase2_replace_text_and_create_overwrite_file_work() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("replace.txt");
    fs::write(&target, "alpha\nbeta\n").unwrap();
    let mut stream = connect_proxy(&app);

    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "replace_text",
            "path": target.display().to_string(),
            "old_text": "alpha",
            "new_text": "omega"
        }),
    );
    wait_until(&mut app, "replace approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "replace approval resolved", |app| app.agents.approvals.is_empty());
    let replace_reply = proxy_recv(&mut stream);
    assert_eq!(replace_reply["result"]["value"]["editCount"], json!(1));
    assert_eq!(replace_reply["result"]["value"]["saved"], json!(true));
    assert_eq!(fs::read_to_string(&target).unwrap(), "omega\nbeta\n");

    let created = temp.path().join("created.txt");
    proxy_send(
        &mut stream,
        2,
        json!({
            "method": "create_text_file",
            "path": created.display().to_string(),
            "content": "fresh\n"
        }),
    );
    wait_until(&mut app, "create approval queued", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "create approval resolved", |app| app.agents.approvals.is_empty());
    let create_reply = proxy_recv(&mut stream);
    assert_eq!(
        create_reply["result"]["value"]["changedFile"],
        json!(created.display().to_string())
    );
    assert_eq!(fs::read_to_string(&created).unwrap(), "fresh\n");

    proxy_send(
        &mut stream,
        3,
        json!({
            "method": "overwrite_text_file",
            "path": created.display().to_string(),
            "content": "overwritten\n"
        }),
    );
    wait_until(&mut app, "overwrite approval queued", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "overwrite approval resolved", |app| app.agents.approvals.is_empty());
    let overwrite_reply = proxy_recv(&mut stream);
    assert_eq!(overwrite_reply["result"]["value"]["saved"], json!(true));
    assert_eq!(fs::read_to_string(&created).unwrap(), "overwritten\n");
}

#[test]
fn proxy_phase2_ambiguous_and_stale_edits_fail_closed() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let ambiguous = temp.path().join("ambiguous.txt");
    fs::write(&ambiguous, "dup\ndup\n").unwrap();
    let mut stream = connect_proxy(&app);
    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "replace_text",
            "path": ambiguous.display().to_string(),
            "old_text": "dup",
            "new_text": "once"
        }),
    );
    settle(&mut app);
    let ambiguous_reply = proxy_recv(&mut stream);
    assert!(
        ambiguous_reply["result"]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("expected exactly one match"),
        "reply: {ambiguous_reply}"
    );
    assert!(app.agents.approvals.is_empty(), "ambiguous edit must fail before approval");

    let stale = temp.path().join("stale.txt");
    fs::write(&stale, "alpha\nbeta\n").unwrap();
    let id = app.backend.open_buffer(Some(stale.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "stale buffer active", |app| {
        app.backend.active().path.as_deref() == Some(stale.as_path())
    });
    wait_until(&mut app, "stale buffer text loaded", |app| {
        app.backend.active().whole_text().as_deref() == Some("alpha\nbeta\n")
    });

    proxy_send(
        &mut stream,
        2,
        json!({
            "method": "replace_text",
            "path": stale.display().to_string(),
            "old_text": "alpha",
            "new_text": "omega"
        }),
    );
    wait_until(&mut app, "stale approval queued", |app| !app.agents.approvals.is_empty());
    app.backend.replace_line_range(1, 1, &[String::from("gamma")]).unwrap();
    app.backend.flush_all_pending_edits().unwrap();
    wait_until(&mut app, "user edit lands", |app| {
        app.backend.active().whole_text().as_deref() == Some("alpha\ngamma\n")
    });
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let stale_reply = proxy_recv(&mut stream);
    assert!(
        stale_reply["result"]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("re-read and retry"),
        "reply: {stale_reply}"
    );
    assert_eq!(app.backend.active().whole_text().as_deref(), Some("alpha\ngamma\n"));
}

#[test]
fn proxy_phase3_diagnostics_return_bounded_editor_state() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("diag.rs");
    fs::write(&target, "alpha\nbeta\n").unwrap();
    let id = app.backend.open_buffer(Some(target.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "diagnostic buffer active", |app| {
        app.backend.active().path.as_deref() == Some(target.as_path())
    });
    app.backend.diagnostics = vec![Diagnostic {
        range: Range { start: 0, end: 5 },
        severity: DiagnosticSeverity::Error,
        message: String::from("broken alpha"),
        source: Some(String::from("fake-lsp")),
        code: Some(String::from("E-DEMO")),
    }];

    let mut stream = connect_proxy(&app);
    proxy_send(&mut stream, 1, json!({ "method": "get_diagnostics" }));
    settle(&mut app);
    let workspace_reply = proxy_recv(&mut stream);
    let diagnostics = workspace_reply["result"]["value"]["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "reply: {workspace_reply}");
    assert_eq!(diagnostics[0]["path"], json!(target.display().to_string()));
    assert_eq!(diagnostics[0]["severity"], json!("error"));
    assert_eq!(diagnostics[0]["message"], json!("broken alpha"));
    assert_eq!(diagnostics[0]["source"], json!("fake-lsp"));
    assert_eq!(diagnostics[0]["code"], json!("E-DEMO"));
    assert_eq!(diagnostics[0]["range"]["startLine"], json!(1));

    proxy_send(
        &mut stream,
        2,
        json!({ "method": "get_file_diagnostics", "path": target.display().to_string() }),
    );
    settle(&mut app);
    let file_reply = proxy_recv(&mut stream);
    assert_eq!(file_reply["result"]["value"]["total"], json!(1), "reply: {file_reply}");
}

#[test]
fn proxy_phase3_symbols_references_and_code_actions_use_agent_payloads() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let target = temp.path().join("symbols.rs");
    fs::write(&target, "fn main() {\n    thing();\n}\n").unwrap();
    let id = app.backend.open_buffer(Some(target.clone())).unwrap();
    app.backend.switch_to_id(id).unwrap();
    wait_until(&mut app, "symbol buffer active", |app| {
        app.backend.active().path.as_deref() == Some(target.as_path())
    });
    let view_id = app.backend.active().view_id.clone();
    app.backend.pending_agent_tool_results.push((
        view_id.clone(),
        String::from("document_symbols"),
        json!({
            "symbols": [{
                "name": "main",
                "kind": "function",
                "range": { "startLine": 1, "startCharacter": 1, "endLine": 3, "endCharacter": 2 },
                "selectionRange": { "startLine": 1, "startCharacter": 4, "endLine": 1, "endCharacter": 8 },
                "containerPath": target.display().to_string()
            }]
        }),
    ));
    app.backend.pending_agent_tool_results.push((
        view_id.clone(),
        String::from("references"),
        json!({
            "references": [{
                "path": target.display().to_string(),
                "range": { "startLine": 2, "startCharacter": 5, "endLine": 2, "endCharacter": 10 }
            }]
        }),
    ));
    app.backend.pending_agent_tool_results.push((
        view_id,
        String::from("list_code_actions"),
        json!({
            "actions": [{
                "title": "Replace thing",
                "kind": "quickfix",
                "hasCommand": false,
                "edits": [{
                    "range": { "startLine": 2, "startCharacter": 5, "endLine": 2, "endCharacter": 10 },
                    "newText": "other"
                }]
            }]
        }),
    ));

    let mut stream = connect_proxy(&app);
    proxy_send(
        &mut stream,
        1,
        json!({ "method": "document_symbols", "path": target.display().to_string() }),
    );
    settle(&mut app);
    let symbols_reply = proxy_recv(&mut stream);
    assert_eq!(symbols_reply["result"]["value"]["symbols"][0]["name"], json!("main"));

    proxy_send(
        &mut stream,
        2,
        json!({ "method": "references", "path": target.display().to_string(), "line": 2, "character": 5 }),
    );
    settle(&mut app);
    let references_reply = proxy_recv(&mut stream);
    assert_eq!(
        references_reply["result"]["value"]["references"][0]["range"]["startLine"],
        json!(2)
    );

    proxy_send(
        &mut stream,
        3,
        json!({ "method": "list_code_actions", "path": target.display().to_string(), "line": 2, "character": 5 }),
    );
    settle(&mut app);
    let actions_reply = proxy_recv(&mut stream);
    let action_id =
        actions_reply["result"]["value"]["actions"][0]["actionId"].as_str().unwrap().to_string();
    assert_eq!(actions_reply["result"]["value"]["actions"][0]["title"], json!("Replace thing"));

    proxy_send(
        &mut stream,
        4,
        json!({
            "method": "apply_code_action",
            "path": target.display().to_string(),
            "action_id": action_id
        }),
    );
    wait_until(&mut app, "code action approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "code action approval resolved", |app| app.agents.approvals.is_empty());
    let apply_reply = proxy_recv(&mut stream);
    assert_eq!(apply_reply["result"]["value"]["editCount"], json!(1));
    assert!(fs::read_to_string(&target).unwrap().contains("other();"));
}

#[test]
fn proxy_phase3_format_and_rename_apply_buffer_edits() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let format_target = temp.path().join("format.rs");
    fs::write(&format_target, "fn main(){\n}\n").unwrap();
    let format_id = app.backend.open_buffer(Some(format_target.clone())).unwrap();
    app.backend.switch_to_id(format_id).unwrap();
    wait_until(&mut app, "format buffer active", |app| {
        app.backend.active().path.as_deref() == Some(format_target.as_path())
    });
    let format_view_id = app.backend.active().view_id.clone();
    app.backend.pending_agent_tool_results.push((
        format_view_id,
        String::from("format_preview"),
        json!({
            "edits": [{
                "range": { "startLine": 1, "startCharacter": 10, "endLine": 1, "endCharacter": 10 },
                "newText": " "
            }]
        }),
    ));

    let rename_target = temp.path().join("rename.rs");
    fs::write(&rename_target, "fn old_name() {}\nfn call() { old_name(); }\n").unwrap();
    let rename_id = app.backend.open_buffer(Some(rename_target.clone())).unwrap();
    app.backend.switch_to_id(rename_id).unwrap();
    wait_until(&mut app, "rename buffer active", |app| {
        app.backend.active().path.as_deref() == Some(rename_target.as_path())
    });
    let rename_view_id = app.backend.active().view_id.clone();
    let rename_payload = json!({
        "files": [{
            "path": rename_target.display().to_string(),
            "edits": [
                {
                    "range": { "startLine": 1, "startCharacter": 4, "endLine": 1, "endCharacter": 12 },
                    "newText": "new_name"
                },
                {
                    "range": { "startLine": 2, "startCharacter": 13, "endLine": 2, "endCharacter": 21 },
                    "newText": "new_name"
                }
            ]
        }]
    });
    app.backend.pending_agent_tool_results.push((
        rename_view_id.clone(),
        String::from("preview_rename"),
        rename_payload.clone(),
    ));
    app.backend.pending_agent_tool_results.push((
        rename_view_id,
        String::from("preview_rename"),
        rename_payload,
    ));

    let mut stream = connect_proxy(&app);
    proxy_send(
        &mut stream,
        1,
        json!({ "method": "format_file", "path": format_target.display().to_string() }),
    );
    wait_until(&mut app, "format approval queued", |app| !app.agents.approvals.is_empty());
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "format approval resolved", |app| app.agents.approvals.is_empty());
    let format_reply = proxy_recv(&mut stream);
    assert_eq!(format_reply["result"]["value"]["editCount"], json!(1));
    assert_eq!(fs::read_to_string(&format_target).unwrap(), "fn main() {\n}\n");

    proxy_send(
        &mut stream,
        2,
        json!({
            "method": "preview_rename_symbol",
            "path": rename_target.display().to_string(),
            "line": 1,
            "character": 4,
            "new_name": "new_name"
        }),
    );
    settle(&mut app);
    let preview_reply = proxy_recv(&mut stream);
    assert_eq!(preview_reply["result"]["value"]["totalEdits"], json!(2));

    proxy_send(
        &mut stream,
        3,
        json!({
            "method": "rename_symbol",
            "path": rename_target.display().to_string(),
            "line": 1,
            "character": 4,
            "new_name": "new_name"
        }),
    );
    wait_until(&mut app, "rename approval queued", |app| !app.agents.approvals.is_empty());
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "rename approval resolved", |app| app.agents.approvals.is_empty());
    let rename_reply = proxy_recv(&mut stream);
    assert_eq!(rename_reply["result"]["value"]["editCount"], json!(2));
    assert!(fs::read_to_string(&rename_target).unwrap().contains("new_name();"));
}
