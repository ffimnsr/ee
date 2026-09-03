use super::*;

fn ee_proxy_acp_mcp_servers() -> Value {
    json!([{ "type": "acp", "name": "ee", "serverId": "ee-mcp-proxy:test" }])
}

fn ee_tool(name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("{name} tool"),
        "inputSchema": { "type": "object", "properties": {} },
    })
}

/// Answers outbound client requests as a fake ACP MCP host while a
/// prompt runs; returns when the prompt response frame arrives.
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
            json!({
                "tools": tools,
                "resultType": "complete",
                "ttlMs": 0,
                "cacheScope": "private",
            }),
        );
        runner
    }

    async fn run(&mut self, handle: &Harness) -> (Vec<String>, String) {
        let mut thoughts = Vec::new();
        loop {
            let frame = handle.next_frames(1).await.remove(0);
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

#[tokio::test]
async fn orchestrated_mode_receives_ee_proxy_tools_and_dispatches_calls() {
    let script = ScriptedCompletion::new(vec![
        response_with_tool_args("tc-1", "ee_workspace_roots", json!({})),
        response_with_text("roots listed"),
    ]);
    let adapter =
        OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script.clone()));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    let session_id = new_session_with_mcp(&handle, 1, ee_proxy_acp_mcp_servers()).await;

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
        "session/prompt",
        prompt_params(&session_id, "list the workspace roots"),
    ));
    let (thoughts, stop_reason) = runner.run(&handle).await;
    assert_eq!(stop_reason, "end_turn");
    assert!(thoughts.is_empty(), "no diagnostics on the happy path: {thoughts:?}");

    // OpenRouter received `ee_workspace_roots` in its tool schemas.
    let bodies = script.bodies();
    assert_eq!(bodies.len(), 2, "tool round plus final answer");
    let tools = &bodies[0]["tools"];
    let names: Vec<&str> = tools
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert!(names.contains(&"ee_workspace_roots"), "{names:?}");
    assert!(
        tools
            .as_array()
            .expect("tools array")
            .iter()
            .all(|tool| !tool["function"]["name"].as_str().unwrap_or_default().contains('.')),
        "no provider-rejected characters in model-facing tool names"
    );

    // The model's call dispatched to MCP tools/call with the original name.
    let log = runner.log();
    assert!(
        log.iter().any(|line| line.contains("tools/call") && line.contains("ee_workspace_roots")),
        "{log:?}"
    );

    // The result came back from the fake ee proxy backend into the model.
    let messages = bodies[1]["messages"].as_array().expect("messages");
    let tool_messages: Vec<&Value> =
        messages.iter().filter(|message| message["role"] == "tool").collect();
    assert!(!tool_messages.is_empty(), "tool observation reached the model");
    assert!(
        tool_messages
            .iter()
            .any(|message| message["content"].as_str().unwrap_or_default().contains("/work")),
        "result came from the fake ee proxy backend"
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn orchestrated_mode_routes_ee_write_tool_to_host_approval() {
    let script = ScriptedCompletion::new(vec![
        response_with_tool_args(
            "tc-1",
            "ee_overwrite_text_file",
            json!({ "path": "/work/x.txt", "content": "data" }),
        ),
        response_with_text("approval requested, continuing"),
    ]);
    let adapter = OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    let session_id = new_session_with_mcp(&handle, 1, ee_proxy_acp_mcp_servers()).await;
    set_write_mode(&handle, &session_id, 2).await;

    let mut runner = PromptMcpRunner::standard_ee_answers(json!([
        ee_tool("ee_overwrite_text_file"),
        ee_tool("ee_workspace_roots"),
    ]));
    runner.answer_call(
        "ee_overwrite_text_file",
        json!({
            "resultType": "complete",
            "content": [{ "type": "text", "text": "approval requested" }],
        }),
    );

    handle.send(request(3, "session/prompt", prompt_params(&session_id, "overwrite the file")));
    let (_thoughts, stop_reason) = runner.run(&handle).await;
    assert_eq!(stop_reason, "end_turn", "host approval dispatch does not crash the turn");

    // Trusted ee write tools preserve their write classification, but host
    // approval owns the mutation decision and must receive the call.
    let log = runner.log();
    assert!(
        log.iter()
            .any(|line| line.contains("tools/call") && line.contains("ee_overwrite_text_file")),
        "ee write must reach host approval: {log:?}"
    );

    handle.shutdown(task).await;
}

#[tokio::test]
async fn orchestrated_mode_mcp_secrets_never_reach_model_or_events() {
    let script = ScriptedCompletion::new(vec![response_with_text("done")]);
    let adapter =
        OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script.clone()));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    // The session advertises a stdio server whose env carries a secret;
    // the binary does not exist, so discovery fails with a diagnostic
    // that must not leak the value.
    let session_id = new_session_with_mcp(
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

    let mut runner = PromptMcpRunner::standard_ee_answers(json!([ee_tool("ee_workspace_roots")]));
    runner.fail_connect = true;

    handle.send(request(
        2,
        "session/prompt",
        prompt_params(&session_id, "what MCP tools do I have"),
    ));
    let (thoughts, stop_reason) = runner.run(&handle).await;
    assert_eq!(stop_reason, "end_turn");

    // Thoughts (diagnostics) never contain the secret.
    let all_thoughts = thoughts.join("\n");
    assert!(
        !all_thoughts.contains("sekrit-value") && !all_thoughts.contains("API_TOKEN"),
        "secrets leaked into diagnostics: {all_thoughts}"
    );

    // Model messages and tool schemas never contain the secret.
    for body in script.bodies() {
        let serialized = body.to_string();
        assert!(
            !serialized.contains("sekrit-value") && !serialized.contains("API_TOKEN"),
            "secrets leaked into the model request: {serialized}"
        );
    }

    handle.shutdown(task).await;
}

#[tokio::test]
async fn orchestrated_mode_what_mcp_tools_regression_with_ee_proxy() {
    // Regression for "what MCP tools do I have": the model's tool list
    // includes the ee proxy tools, so the answer can list more than the
    // built-in `read_file`.
    let script = ScriptedCompletion::new(vec![response_with_text(
        "You have ee_workspace_roots, ee_search_text, and read_file",
    )]);
    let adapter =
        OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script.clone()));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    let session_id = new_session_with_mcp(&handle, 1, ee_proxy_acp_mcp_servers()).await;

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

    let bodies = script.bodies();
    let tools = bodies[0]["tools"].as_array().expect("tools array");
    let names: Vec<&str> =
        tools.iter().filter_map(|tool| tool["function"]["name"].as_str()).collect();
    assert!(names.contains(&"ee_workspace_roots"), "{names:?}");
    assert!(names.contains(&"ee_search_text"), "{names:?}");
    assert!(names.contains(&"read_file"), "builtins still present: {names:?}");
    assert!(tools.len() > 1, "more than one tool available: {names:?}");

    handle.shutdown(task).await;
}
