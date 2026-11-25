use super::*;

#[test]
fn proxy_terminal_denial_does_not_spawn_terminal() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let mut stream = connect_proxy(&app);
    proxy_send(
        &mut stream,
        1,
        json!({
            "method": "terminal_create",
            "command": "sleep",
            "args": ["30"],
        }),
    );
    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    let prompt = app.agents.approvals.front().unwrap();
    assert!(prompt.title.contains("terminal/create"));
    assert!(prompt.detail.contains("sleep 30"));

    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    wait_until(&mut app, "approval resolved", |app| app.agents.approvals.is_empty());
    let reply = proxy_recv(&mut stream);

    assert_eq!(app.agents.terminals.tracked_count(), 0, "denied terminal must not spawn");
    let reply = reply.get("result").unwrap();
    assert!(reply.get("error").is_some(), "proxy must report the denial: {reply}");
}

// ── ee MCP proxy: ACP-native MCP-over-ACP (Phase 6b) ─────────────────────────

#[test]
fn session_new_advertises_acp_native_proxy_when_agent_supports_acp() {
    let (mut app, _temp, fake) = mcp_app(acp_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let servers = session_new[0]
        .get("params")
        .and_then(|params| params.get("mcpServers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let acp = servers
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some("ee"))
        .expect("ee proxy entry forwarded");
    assert_eq!(acp.get("type").and_then(Value::as_str), Some("acp"));
    assert!(acp.get("serverId").is_some(), "acp entry carries the server id: {acp}");
    // Mutually exclusive with the stdio fallback (no `--mcp-proxy` args).
    assert!(!servers.iter().any(|entry| {
        entry
            .get("args")
            .and_then(Value::as_array)
            .is_some_and(|args| args.contains(&json!("--mcp-proxy")))
    }));
    // User-visible diagnostics: ACP-native mode.
    assert!(app.mcp_health_lines().iter().any(|line| line == "mcp proxy ee: acp-native"));
}

#[test]
fn session_new_falls_back_to_stdio_proxy_without_acp_capability() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);

    // The wire shape is covered by `session_new_forwards_proxy_stdio_entry_...`;
    // here the diagnostics must name the fallback mode.
    assert!(app.mcp_health_lines().iter().any(|line| line == "mcp proxy ee: stdio fallback"));
}

#[test]
fn session_new_omits_acp_native_proxy_when_proxy_disabled() {
    let (mut app, _temp, fake) = mcp_app(base_agent_script(), false, false);
    open_pane_and_wait_ready(&mut app);

    let session_new = fake.agent().requests_by_method("session/new");
    let servers = session_new[0]
        .get("params")
        .and_then(|params| params.get("mcpServers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        servers.iter().all(|entry| entry.get("name").and_then(Value::as_str) != Some("ee")),
        "no ee proxy entry when disabled: {servers:?}"
    );
    assert!(app.mcp_health_lines().iter().any(|line| line == "mcp: no servers started"));
}

#[test]
fn mcp_over_acp_write_denial_leaves_buffer_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("acp-proxy.txt");
    fs::write(&target, "original").unwrap();
    let target_text = target.display().to_string();
    let script = acp_connect_script(json!({
        "name": "ee_write_text_file",
        "arguments": { "path": target_text, "content": "agent-wrote-this" }
    }));
    let (mut app, fake) = mcp_app_in(&temp, script, false, true);
    open_pane_and_wait_ready(&mut app);

    // The write reaches the same approval prompt as direct ACP methods.
    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    let prompt = app.agents.approvals.front().unwrap();
    assert!(prompt.title.contains("fs/write_text_file"));
    assert!(prompt.detail.contains("acp-proxy.txt"));
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
    wait_until(&mut app, "approval resolved", |app| app.agents.approvals.is_empty());

    // The denial surfaces back through `mcp/message` as an isError tool
    // result; buffer and disk stay unchanged.
    let reply = fake_response(&fake.agent(), 202);
    let result = reply.get("result").expect("tool result payload");
    assert_eq!(result.get("isError"), Some(&json!(true)));
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("denial text");
    assert!(text.contains("denied"), "denial surfaced to the agent: {text}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "original");
}

#[test]
fn mcp_over_acp_terminal_denial_does_not_spawn_terminal() {
    let script = acp_connect_script(json!({
        "name": "ee_terminal_create",
        "arguments": { "command": "sleep", "args": ["30"] }
    }));
    let (mut app, _temp, fake) = mcp_app(script, false, true);
    open_pane_and_wait_ready(&mut app);

    wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
    let prompt = app.agents.approvals.front().unwrap();
    assert!(prompt.title.contains("terminal/create"));
    assert!(prompt.detail.contains("sleep 30"));
    run_ex(&mut app, "agents");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE); // Deny
    wait_until(&mut app, "approval resolved", |app| app.agents.approvals.is_empty());

    let reply = fake_response(&fake.agent(), 202);
    let result = reply.get("result").expect("tool result payload");
    assert_eq!(result.get("isError"), Some(&json!(true)));
    assert_eq!(app.agents.terminals.tracked_count(), 0, "denied terminal must not spawn");
}

// ── Approval policy (Phase 7) ────────────────────────────────────────────────

#[test]
fn approval_policy_allow_session_auto_allows_identical_writes() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let target = temp.path().join("policy.txt");
    fs::write(&target, "v0").unwrap();

    let mut stream = connect_proxy(&app);
    for round in 1..=3 {
        proxy_send(
            &mut stream,
            round,
            json!({
                "method": "write_text_file",
                "path": target.display().to_string(),
                "content": format!("agent-v{round}"),
            }),
        );
        wait_until(&mut app, "round approval or auto-resolve", |app| {
            !app.agents.approvals.is_empty()
                || fs::read_to_string(&target).map(|t| t.contains("agent-v")).unwrap_or(false)
        });
        if !app.agents.approvals.is_empty() {
            run_ex(&mut app, "agents");
            press(&mut app, KeyCode::Right, KeyModifiers::NONE); // Allow session
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        }
        wait_until(&mut app, "round applied", |_app| {
            fs::read_to_string(&target)
                .map(|t| t.contains(&format!("agent-v{round}")))
                .unwrap_or(false)
        });
        let _ = proxy_recv(&mut stream);
    }
    // Rounds 2 and 3 were auto-allowed by the session policy (no prompts).
    assert_eq!(app.agents.approvals.len(), 0);
    assert!(fs::read_to_string(&target).unwrap().contains("agent-v3"));
}

#[test]
fn approval_policy_deny_session_auto_denies_identical_writes() {
    let (mut app, temp, _fake) = mcp_app(base_agent_script(), false, true);
    open_pane_and_wait_ready(&mut app);
    let target = temp.path().join("policy-deny.txt");
    fs::write(&target, "v0").unwrap();

    let mut stream = connect_proxy(&app);
    for round in 1..=2 {
        proxy_send(
            &mut stream,
            round,
            json!({
                "method": "write_text_file",
                "path": target.display().to_string(),
                "content": format!("agent-v{round}"),
            }),
        );
        if round == 1 {
            // First round prompts; the user records a session-scoped denial.
            wait_until(&mut app, "approval queued", |app| !app.agents.approvals.is_empty());
            run_ex(&mut app, "agents");
            press(&mut app, KeyCode::Right, KeyModifiers::NONE);
            press(&mut app, KeyCode::Right, KeyModifiers::NONE);
            press(&mut app, KeyCode::Right, KeyModifiers::NONE); // Deny session
            press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            wait_until(&mut app, "round resolved", |app| app.agents.approvals.is_empty());
        } else {
            // Round 2 is auto-denied by the session policy: no approval is
            // queued; the reply lands on its own.
            settle(&mut app);
        }
        let reply = proxy_recv(&mut stream);
        assert!(
            reply.get("result").and_then(|r| r.get("error")).is_some(),
            "round {round} must be denied: {reply}"
        );
    }
    assert_eq!(fs::read_to_string(&target).unwrap(), "v0", "disk unchanged");
}

// ── Shutdown orchestration (Phase 7) ─────────────────────────────────────────

#[test]
fn shutdown_cancels_hung_turn_kills_terminals_and_stops_hosts() {
    let script = base_agent_script().wait_for("session/prompt"); // never answers
    let (mut app, _temp, _fake) = mcp_app(script, false, false);
    open_pane_and_wait_ready(&mut app);

    // Start a turn that can never complete.
    type_text(&mut app, "hello");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    wait_until(&mut app, "turn running", |app| {
        app.agents.threads[0].state == ThreadUiState::Running
    });

    // Spawn a long-running agent terminal through the approval path.
    let mut request = ee_agent_protocol::CreateTerminalRequest::new(
        ee_agent_protocol::SessionId::new("s1"),
        "sleep",
    );
    request.args = vec![String::from("30")];
    let spawned = app.agents.terminals.spawn(&request, Some("fake")).expect("terminal spawns");
    assert_eq!(app.agents.terminals.tracked_count(), 1);

    // Shutdown must complete within a bounded window despite the hung agent.
    let start = Instant::now();
    app.shutdown_agents();
    assert!(start.elapsed() < Duration::from_secs(5), "shutdown must not hang");

    assert!(app.agents.host.is_none(), "agent host must stop");
    assert!(app.agents.mcp.host.is_none());
    assert_eq!(app.agents.terminals.tracked_count(), 0, "terminals must be killed");
    assert!(app.agents.threads.is_empty());
    assert!(app.agents.approvals.is_empty());
    let _ = spawned;
}

// ── Disabled-mode regression (Phase 7) ───────────────────────────────────────

#[test]
fn runtime_disabled_agents_do_not_start_mcp_or_agent_hosts() {
    // agents.enabled is false but MCP servers exist: `:agents` must not
    // start either host.
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        "[agents.servers.fake]\ncommand = \"unused\"\n\n[mcp.servers.tools]\ntransport = \"stdio\"\ncommand = \"mcp-tools\"\n",
    )
    .unwrap();
    fs::write(temp.path().join(".ee.toml"), "[agents]\nenabled = false\n").unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    run_ex(&mut app, "agents");
    assert!(app.agents.host.is_none(), "agent host must not start");
    assert!(app.agents.mcp.host.is_none(), "mcp host must not start");
    assert_eq!(app.agents.layout, AgentPaneLayout::Closed);
}

#[test]
fn agents_mcp_command_disabled_path_reports_message() {
    let (mut app, _temp, _fake) = mcp_app(base_agent_script(), true, false);
    app.config.agents.enabled = false;
    run_ex(&mut app, "agents_mcp prompts");
    let status = app.backend.status_message.clone().unwrap_or_default();
    assert!(status.contains("agents mode disabled"), "status: {status}");
    assert!(app.agents.mcp.host.is_none());
}

// ── Secret redaction wiring (Phase 7) ────────────────────────────────────────

#[test]
fn configured_secret_values_are_collected_for_redaction() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join(".ee.toml"),
        "[agents]\nenabled = true\n\n[agents.servers.fake]\ncommand = \"unused\"\nenv = { API_TOKEN = \"super-secret-value\" }\n",
    )
    .unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let app = App::from_path(None).unwrap();
    drop(_cwd_restore);
    drop(_cwd_lock);

    let secrets = app.agents_secret_values();
    assert!(secrets.contains(&String::from("super-secret-value")));

    let text = "trace: using super-secret-value now";
    let redacted = ee_agent_host::redact::redact_secret_values(text, &secrets);
    assert_eq!(redacted, "trace: using *** now");
    assert!(!redacted.contains("super-secret-value"));
}

#[test]
fn resolved_secret_values_are_collected_for_redaction_only_after_launch() {
    let temp = tempfile::tempdir().unwrap();
    // User-layer reference (XDG): resolves regardless of the workspace trust
    // decision, so the merge and launch path stay independent of it.
    let xdg = temp.path().join("xdg");
    fs::create_dir_all(xdg.join("ee")).unwrap();
    fs::write(
        xdg.join("ee").join("config.toml"),
        "[agents]\nenabled = true\n\n[agents.servers.fake]\ncommand = \"unused\"\nenv = { OPENROUTER_API_KEY = \"secret://openrouter-api-key\" }\n",
    )
    .unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let _xdg_guard = EnvVarGuard::set("XDG_CONFIG_HOME", xdg);
    let mut app = App::from_path(None).unwrap();

    // Inject a fake secrets store holding the referenced key; the reference
    // must not resolve before the launch configuration exists.
    let keychain = crate::secrets::test_support::StoredKeychain::new();
    let binding = crate::secrets::HostBinding::from_identifier_bytes(b"test-machine-id\n").unwrap();
    let store = crate::secrets::SecretStore::new(
        Box::new(keychain),
        binding,
        temp.path().join("ee/secrets/v1.json"),
    );
    store
        .set(
            &crate::secrets::SecretName::new("openrouter-api-key").unwrap(),
            &zeroize::Zeroizing::new(String::from("sk-resolved-42")),
        )
        .unwrap();
    app.agents.test_secret_store = Some(store);

    let before = app.agents_secret_values();
    assert!(
        !before.iter().any(|v| v == "sk-resolved-42"),
        "secret not resolved before agent launch"
    );

    // Opening the agents pane builds the launch configuration, which resolves
    // the reference and records the value for redaction.
    run_ex(&mut app, "agents");
    app.pump_agents();
    drop(_cwd_restore);
    drop(_cwd_lock);

    let secrets = app.agents_secret_values();
    assert!(
        secrets.contains(&String::from("sk-resolved-42")),
        "resolved value collected at launch"
    );
    assert!(!secrets.contains(&String::from("secret://openrouter-api-key")));

    let text = "stderr: token sk-resolved-42 leaked";
    let redacted = ee_agent_host::redact::redact_secret_values(text, &secrets);
    assert_eq!(redacted, "stderr: token *** leaked");
    assert!(!redacted.contains("sk-resolved-42"));
}

#[test]
fn workspace_secret_policy_follows_the_host_local_decision() {
    use crate::secrets::resolve::WorkspaceRefPolicy;
    use crate::workspace_trust::{WorkspaceTrustDecision, WorkspaceTrustStore};

    let temp = tempfile::tempdir().unwrap();
    let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
    let _cwd_restore = CurrentDirGuard::capture();
    std::env::set_current_dir(temp.path()).unwrap();
    let mut app = App::from_path(None).unwrap();

    let state = temp.path().join("state");
    fs::create_dir_all(&state).unwrap();
    app.agents.test_trust_store_base = Some(state.clone());

    // Undecided workspaces deny workspace-layer references (fail closed).
    assert_eq!(app.workspace_secret_ref_policy(), WorkspaceRefPolicy::Deny);

    let store = WorkspaceTrustStore::at(&state, temp.path()).unwrap();
    store.set_decision(WorkspaceTrustDecision::Trusted).unwrap();
    assert_eq!(app.workspace_secret_ref_policy(), WorkspaceRefPolicy::Resolve);

    store.set_decision(WorkspaceTrustDecision::Untrusted).unwrap();
    assert_eq!(app.workspace_secret_ref_policy(), WorkspaceRefPolicy::Deny);
}
