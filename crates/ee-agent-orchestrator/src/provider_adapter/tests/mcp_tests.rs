//! Provider-adapter tests: mcp.
use super::conversation::record_agent_message;
use super::conversation::record_user_message;
use super::*;

// ── Phase 12: MCP bridge through the framework server ────────────────

fn session_new_params_with_mcp(cwd: &str, mcp_servers: Value) -> Value {
    json!({
        "cwd": cwd,
        "additionalDirectories": [],
        "mcpServers": mcp_servers,
    })
}

/// Answers outbound client requests as a fake ACP MCP host while a
/// prompt runs, collecting thought updates; returns when the prompt
/// response frame arrives.
struct PromptMcpRunner {
    inner: std::collections::HashMap<String, Value>,
    calls: std::collections::HashMap<String, Value>,
    fail_connect: bool,
    /// Every inner MCP request logged as `method: params`.
    mcp_requests: std::sync::Mutex<Vec<String>>,
}

impl PromptMcpRunner {
    fn new() -> Self {
        Self {
            inner: std::collections::HashMap::new(),
            calls: std::collections::HashMap::new(),
            fail_connect: false,
            mcp_requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn answer(&mut self, method: &str, result: Value) {
        self.inner.insert(method.to_string(), result);
    }

    fn answer_call(&mut self, tool_name: &str, result: Value) {
        self.calls.insert(tool_name.to_string(), result);
    }

    fn log(&self) -> Vec<String> {
        self.mcp_requests.lock().expect("runner log poisoned").clone()
    }

    /// Standard ee proxy discovery answers (connect + discover + list).
    fn standard_ee_answers(tools: Value) -> Self {
        let mut runner = Self::new();
        runner.answer(
            "server/discover",
            json!({
                "resultType": "complete",
                "supportedVersions": ["2026-07-28"],
                "capabilities": { "tools": {} },
                "ttlMs": 0,
                "cacheScope": "private",
            }),
        );
        runner.answer(
            "tools/list",
            json!({ "tools": tools, "resultType": "complete", "ttlMs": 0, "cacheScope": "private" }),
        );
        runner
    }

    /// Drives the harness until the prompt response; returns the thought
    /// updates and the stop reason.
    async fn run(&mut self, handle: &Harness) -> (Vec<String>, String) {
        let mut thoughts = Vec::new();
        loop {
            let frame = handle.next_frame().await;
            match frame {
                RawJsonRpcMessage::Request(request) => {
                    let params = raw_params_to_value(request.params.clone());
                    let method = request.method.to_string();
                    let response = self.response_for(&method, &params);
                    handle.send(RawJsonRpcMessage::response(request.id.clone(), Ok(response)));
                }
                RawJsonRpcMessage::Notification(notification) => {
                    let params = raw_params_to_value(notification.params.clone());
                    if params["update"]["sessionUpdate"] == "agent_thought_chunk" {
                        thoughts.push(
                            params["update"]["content"]["text"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        );
                    }
                }
                RawJsonRpcMessage::Response(response) => {
                    let Response::Result { result, .. } = response else {
                        panic!("unexpected prompt error response");
                    };
                    let stop_reason = result["stopReason"].as_str().unwrap_or_default().to_string();
                    return (thoughts, stop_reason);
                }
            }
        }
    }

    fn response_for(&mut self, method: &str, params: &Value) -> Value {
        match method {
            "mcp/connect" => {
                if self.fail_connect {
                    json!({})
                } else {
                    json!({ "connectionId": "conn-1" })
                }
            }
            "mcp/disconnect" => json!({}),
            "mcp/message" => {
                let inner_method = params.get("method").and_then(Value::as_str).unwrap_or_default();
                self.mcp_requests
                    .lock()
                    .expect("runner log poisoned")
                    .push(format!("{inner_method}: {params}"));
                if inner_method == "tools/call" {
                    let tool_name =
                        params.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
                    self.calls.get(tool_name).cloned().unwrap_or_else(|| {
                        panic!("no canned tools/call response for {tool_name:?}")
                    })
                } else {
                    self.inner
                        .get(inner_method)
                        .cloned()
                        .unwrap_or_else(|| panic!("no canned inner response for {inner_method:?}"))
                }
            }
            other => panic!("unexpected client request {other}"),
        }
    }
}

fn ee_proxy_acp_mcp_servers() -> Value {
    json!([{ "type": "acp", "name": "ee", "serverId": "ee-mcp-proxy:test" }])
}

fn ee_tool(name: &str) -> Value {
    json!({ "name": name, "description": format!("{name} tool"), "inputSchema": { "type": "object", "properties": {} } })
}

async fn mcp_new_session(handle: &Harness, id: i64, mcp_servers: Value) -> String {
    handle.send(request(id, "session/new", session_new_params_with_mcp("/work", mcp_servers)));
    let result = request_result(handle.next_frame().await);
    result["sessionId"].as_str().expect("session id").to_string()
}

#[tokio::test]
async fn provider_adapter_advertises_mcp_capabilities_acp() {
    let model = Arc::new(FakeModel::new(Vec::new()));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let (handle, task) = spawn_server(provider);

    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));
    let result = request_result(handle.next_frame().await);
    assert_eq!(
        result["agentCapabilities"]["mcpCapabilities"]["acp"], true,
        "orchestrated providers host MCP-over-ACP"
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_session_new_retains_redacted_mcp_servers() {
    let model = Arc::new(FakeModel::new(Vec::new()));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
    let probe = provider.clone();
    let (handle, task) = spawn_server(provider);

    let session_id = mcp_new_session(
        &handle,
        1,
        json!([
            { "type": "acp", "name": "ee", "serverId": "ee-mcp-proxy:test" },
            {
                "name": "filesystem",
                "command": "/usr/bin/server",
                "args": [],
                "env": [{ "name": "API_TOKEN", "value": "sekrit-value" }],
            },
        ]),
    )
    .await;

    let descriptors = probe.session_mcp_servers(&session_id);
    assert_eq!(descriptors.len(), 2, "descriptors retained per session");
    let debug = format!("{descriptors:?}");
    assert!(
        !debug.contains("sekrit-value") && !debug.contains("API_TOKEN"),
        "env secrets must never reach Debug output: {debug}"
    );
    assert!(debug.contains("filesystem"), "server names stay visible");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_session_new_rejects_unsupported_mcp_transport() {
    let model = Arc::new(FakeModel::new(Vec::new()));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let (handle, task) = spawn_server(provider);

    handle.send(request(
        1,
        "session/new",
        session_new_params_with_mcp(
            "/work",
            json!([{
                "type": "http",
                "name": "remote",
                "url": "https://example.com/mcp",
                "headers": [],
            }]),
        ),
    ));
    let error = request_error(handle.next_frame().await);
    assert!(
        error.message.contains("streamable-http"),
        "fail closed with a clear reason: {}",
        error.message
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_prompt_exposes_ee_proxy_tools_and_dispatches_calls() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![crate::tools::ToolIntent::new(
            "tc-1",
            "ee_workspace_roots",
            json!({}),
        )]),
        ModelResponse::new().text(plan_response("roots listed")).completed(),
    ]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
    let probe = provider.clone();
    let (handle, task) = spawn_server(provider);
    let session_id = mcp_new_session(&handle, 1, ee_proxy_acp_mcp_servers()).await;
    let _ = handle.next_frame().await; // initial available-commands update

    let mut runner = PromptMcpRunner::standard_ee_answers(json!([ee_tool("ee_workspace_roots")]));
    runner.answer_call(
        "ee_workspace_roots",
        json!({
            "resultType": "complete",
            "content": [{ "type": "text", "text": "/work\n/shared" }],
            "structuredContent": { "roots": ["/work", "/shared"] },
        }),
    );

    handle.send(request(
        2,
        "session/set_mode",
        json!({ "sessionId": session_id, "modeId": PLAN_MODE_ID }),
    ));
    assert_eq!(request_result(handle.next_frame().await), json!({}));
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "list roots")));
    let (thoughts, stop_reason) = runner.run(&handle).await;
    assert_eq!(stop_reason, "end_turn");

    // The model received the MCP tool with a provider-compatible name.
    let requests = model.requests();
    let tools = &requests[0].tools;
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(names.contains(&"ee_workspace_roots"), "{names:?}");
    assert!(
        tools.iter().all(|tool| !tool.name.contains('.')),
        "no provider-rejected characters: {names:?}"
    );

    // The model's call dispatched to MCP tools/call with the original name.
    let log = runner.log();
    assert!(
        log.iter().any(|line| line.contains("tools/call") && line.contains("ee_workspace_roots")),
        "{log:?}"
    );

    // The tool result reached the transcript.
    let transcript = requests[1].transcript.clone();
    let all_text: String = transcript
        .iter()
        .flat_map(|message| message.content.iter())
        .map(|block| match block {
            crate::model::ModelContent::Text(text) => text.clone(),
            crate::model::ModelContent::ToolResult { result, .. } => result.summary_text(),
            _ => String::new(),
        })
        .collect();
    assert!(all_text.contains("/work"), "tool output in transcript: {all_text}");

    // MCP tools are deregistered after the prompt; builtins remain.
    let names = probe.session_tool_names(&session_id);
    assert!(!names.contains(&"ee_workspace_roots".to_string()), "{names:?}");
    assert!(names.contains(&"read_file".to_string()), "{names:?}");

    // No discovery diagnostics on the happy path.
    assert!(thoughts.is_empty(), "{thoughts:?}");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_denies_network_gated_ee_web_fetch_before_acp_dispatch() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![crate::tools::ToolIntent::new(
            "tc-1",
            "ee_fetch_url",
            json!({ "url": "https://docs.example/start" }),
        )]),
        ModelResponse::new().text(plan_response("source fetched")).completed(),
    ]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
    let (handle, task) = spawn_server(provider);
    let session_id = mcp_new_session(&handle, 1, ee_proxy_acp_mcp_servers()).await;
    let _ = handle.next_frame().await; // initial available-commands update

    let mut runner = PromptMcpRunner::standard_ee_answers(json!([ee_tool("ee_fetch_url")]));
    runner.answer_call(
        "ee_fetch_url",
        json!({
            "resultType": "complete",
            "content": [{
                "type": "text",
                "text": "source: https://docs.example/final; trust: untrusted_external_content"
            }],
            "structuredContent": {
                "requestedUrl": "https://docs.example/start",
                "url": "https://docs.example/final",
                "provenance": "https://docs.example/final",
                "trust": "untrusted_external_content"
            },
        }),
    );

    handle.send(request(
        2,
        "session/set_mode",
        json!({ "sessionId": session_id, "modeId": PLAN_MODE_ID }),
    ));
    assert_eq!(request_result(handle.next_frame().await), json!({}));
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "fetch docs")));
    let (_thoughts, stop_reason) = runner.run(&handle).await;
    assert_eq!(stop_reason, "end_turn");

    let requests = model.requests();
    assert!(
        requests[0].tools.iter().any(|tool| tool.name == "ee_fetch_url"),
        "web fetch is exposed to the model: {:?}",
        requests[0].tools
    );
    let log = runner.log();
    assert!(
        !log.iter().any(|line| line.contains("tools/call") && line.contains("ee_fetch_url")),
        "external-network fetch must be denied before it reaches ACP tools/call: {log:?}"
    );
    let transcript: String = requests[1]
        .transcript
        .iter()
        .flat_map(|message| message.content.iter())
        .map(|block| match block {
            crate::model::ModelContent::Text(text) => text.clone(),
            crate::model::ModelContent::ToolResult { result, .. } => result.summary_text(),
            _ => String::new(),
        })
        .collect();
    assert!(transcript.contains("policy"), "{transcript}");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_dispatches_ee_write_tool_to_host_approval() {
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new().tool_intents(vec![crate::tools::ToolIntent::new(
            "tc-1",
            "ee_write_text_file",
            json!({ "path": "/work/x.txt", "content": "data" }),
        )]),
        ModelResponse::new().text("approval requested, continuing").completed(),
    ]));
    // ee writes retain write classification but dispatch to editor-host approval.
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let (handle, task) = spawn_server(provider);
    let session_id = mcp_new_session(&handle, 1, ee_proxy_acp_mcp_servers()).await;
    let _ = handle.next_frame().await; // initial available-commands update

    let mut runner = PromptMcpRunner::standard_ee_answers(json!([
        ee_tool("ee_workspace_roots"),
        ee_tool("ee_write_text_file"),
    ]));
    runner.answer_call(
        "ee_write_text_file",
        json!({
            "resultType": "complete",
            "content": [{ "type": "text", "text": "approval requested" }],
        }),
    );

    handle.send(request(
        2,
        "session/set_mode",
        json!({ "sessionId": session_id, "modeId": WRITE_MODE_ID }),
    ));
    assert_eq!(request_result(handle.next_frame().await), json!({}));
    handle.send(request(3, "session/prompt", prompt_params(&session_id, "write a file")));
    let (_thoughts, stop_reason) = runner.run(&handle).await;
    assert_eq!(stop_reason, "end_turn", "approval dispatch does not crash the turn");

    // The host receives the write call and owns approval before mutation.
    let log = runner.log();
    assert!(
        log.iter().any(|line| line.contains("tools/call") && line.contains("ee_write_text_file")),
        "ee write must reach host approval: {log:?}"
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_mcp_failures_surface_diagnostics_without_secrets() {
    let model = Arc::new(FakeModel::new(vec![ModelResponse::new().text("ok").completed()]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model);
    let (handle, task) = spawn_server(provider);
    let session_id = mcp_new_session(
        &handle,
        1,
        json!([
            { "type": "acp", "name": "ee", "serverId": "ee-mcp-proxy:test" },
            {
                "name": "filesystem",
                "command": "/nonexistent/ee-server",
                "args": [],
                "env": [{ "name": "API_TOKEN", "value": "sekrit-value" }],
            },
        ]),
    )
    .await;

    // The ee proxy connect fails; the stdio spawn fails (no binary).
    let mut runner = PromptMcpRunner::new();
    runner.fail_connect = true;

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));
    let (thoughts, stop_reason) = runner.run(&handle).await;
    assert_eq!(stop_reason, "end_turn", "discovery failures do not crash the turn");
    assert!(
        thoughts.iter().any(|thought| thought.contains("unavailable")),
        "connect failure surfaced: {thoughts:?}"
    );
    assert!(
        thoughts.iter().any(|thought| thought.contains("no MCP tools were registered")
            || thought.contains("could not be registered")),
        "no-tools diagnostic surfaced: {thoughts:?}"
    );
    let all = thoughts.join("\n");
    assert!(
        !all.contains("sekrit-value") && !all.contains("API_TOKEN"),
        "secrets must never reach diagnostics: {all}"
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_model_can_list_mcp_tools_beyond_read_file() {
    // Regression for "what MCP tools do I have": with the ee proxy
    // present, the model's tool list includes the MCP tools.
    let model = Arc::new(FakeModel::new(vec![
        ModelResponse::new()
            .text("You have ee_workspace_roots, ee_search_text, and read_file")
            .completed(),
    ]));
    let provider = OrchestratorProvider::new(OrchestratorProviderConfig::default(), model.clone());
    let (handle, task) = spawn_server(provider);
    let session_id = mcp_new_session(&handle, 1, ee_proxy_acp_mcp_servers()).await;

    let mut runner = PromptMcpRunner::standard_ee_answers(json!([
        ee_tool("ee_workspace_roots"),
        ee_tool("ee_search_text"),
    ]));

    handle.send(request(
        2,
        "session/prompt",
        prompt_params(&session_id, "what MCP tools do I have"),
    ));
    let (_thoughts, stop_reason) = runner.run(&handle).await;
    assert_eq!(stop_reason, "end_turn");

    let tools = &model.requests()[0].tools;
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(names.contains(&"ee_workspace_roots"), "{names:?}");
    assert!(names.contains(&"ee_search_text"), "{names:?}");
    assert!(names.contains(&"read_file"), "builtins still present: {names:?}");
    assert!(tools.len() > 1, "more than a single tool: {names:?}");
    assert!(
        tools.iter().all(|tool| !tool.name.contains('.')),
        "no dots in model-facing names: {names:?}"
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn provider_adapter_compact_prompt_skips_mcp_discovery_and_tools() {
    let model =
        Arc::new(FakeModel::new(vec![ModelResponse::new().text("SUMMARY TEXT").completed()]));
    let provider = OrchestratorProvider::new(
        OrchestratorProviderConfig {
            telemetry: TelemetryConfig {
                enabled: true,
                max_turns: 2,
                max_events_per_turn: 16,
                max_bytes_per_turn: 4_096,
            },
            ..OrchestratorProviderConfig::default()
        },
        model.clone(),
    );
    let probe = provider.clone();
    let (handle, task) = spawn_server(provider);
    let session_id = new_session(&handle, 1).await;
    let conversation = probe
        .sessions
        .lock()
        .expect("sessions")
        .get(&session_id)
        .expect("session")
        .conversation
        .clone();
    record_user_message(&conversation, "recent user goal");
    record_agent_message(&conversation, "recent agent result");

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "/compact")));
    // No MCP diagnostics and no plan update: the compaction path bypasses
    // server discovery and the model–tool loop entirely.
    let frames = handle.next_frames(2).await;
    let RawJsonRpcMessage::Notification(report) = &frames[0] else {
        panic!("expected the compaction report update, got {:?}", frames[0]);
    };
    let params = raw_params_to_value(report.params.clone());
    assert_eq!(params["sessionId"], session_id);
    assert_eq!(params["update"]["sessionUpdate"], "agent_message_chunk");
    assert!(
        params["update"]["content"]["text"].as_str().unwrap().contains("Session compacted:"),
        "{params}"
    );
    let result = request_result(frames[1].clone());
    assert_eq!(result["stopReason"], "end_turn");
    assert!(handle.outbound().is_empty(), "no further frames: {:?}", handle.outbound());

    // The model saw exactly one request with no tools; the summary is
    // stored as session memory and no task was created.
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty(), "no tools exposed during compaction");
    let compaction_system = requests[0].transcript[0].text_content();
    assert!(compaction_system.contains("Recent conversation:"), "{compaction_system}");
    assert!(compaction_system.contains("recent user goal"), "{compaction_system}");
    assert!(compaction_system.contains("recent agent result"), "{compaction_system}");
    let (tasks, memory) = probe.session_state(&session_id).expect("session state");
    assert_eq!(tasks.len(), 0, "no task graph entry for compaction");
    assert_eq!(memory.query("summary:session").expect("stored").value, "SUMMARY TEXT");
    let telemetry = probe.export_telemetry_jsonl().expect("exports telemetry");
    let record: Value =
        serde_json::from_str(telemetry.lines().next().expect("record")).expect("telemetry json");
    assert_eq!(record["summary"]["modelCalls"], 1);

    handle.shutdown(task).await;
}
