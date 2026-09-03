use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_orchestrator::{
    BudgetTracker, OrchestratorConfig, OrchestratorProvider, OrchestratorProviderConfig,
    SideEffectClass, TaskId, TaskNode, ToolDefinition, ToolResult,
};
use ee_agent_protocol::{Error as RpcError, RawJsonRpcMessage, RequestId, Response};
use serde_json::{Value, json};

use super::*;
use crate::config::DEFAULT_API_URL;

fn test_config() -> Config {
    Config {
        model: String::from("test/model"),
        model_family: None,
        rubber_duck_model: None,
        rubber_duck_model_family: None,
        rubber_duck: ee_agent_orchestrator::RubberDuckConfig::default(),
        api_url: String::from(DEFAULT_API_URL),
        api_key: Some(String::from("sk-test")),
        site_url: None,
        app_title: String::from("ee-test"),
        timeout: Duration::from_secs(1),
        system_prompt: String::from("system"),
        reasoning_effort: None,
        orchestrated: true,
        compact_min_messages: 4,
        compact_retained_tail: 2,
        compact_max_input_bytes: 65_536,
        auto_compact_threshold_percent: 80,
        retry_max_attempts: crate::config::DEFAULT_RETRY_MAX_ATTEMPTS,
        retry_base_delay: std::time::Duration::from_millis(
            crate::config::DEFAULT_RETRY_BASE_DELAY_MS,
        ),
        retry_max_delay: std::time::Duration::from_millis(
            crate::config::DEFAULT_RETRY_MAX_DELAY_MS,
        ),
        checkpoint_dir: None,
        context_window: crate::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
        max_iterations: ee_agent_orchestrator::config::DEFAULT_MAX_LOOP_ITERATIONS,
    }
}

#[test]
fn production_registry_uses_distinct_declared_model_families() {
    let mut config = test_config();
    config.model = "anthropic/root".into();
    config.model_family = Some("anthropic".into());
    config.rubber_duck_model = Some("openai/critic".into());
    config.rubber_duck_model_family = Some("openai".into());

    let (provider, warning) = openrouter_multi_model_provider(
        &config,
        PathBuf::from("/tmp/ee-openrouter-model-registry-test"),
    )
    .expect("provider");
    assert!(warning.is_none());
    let models = provider.registered_models();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, DEFAULT_MODEL_ID);
    assert_eq!(models[0].identity.model_id, "anthropic/root");
    assert_eq!(models[0].identity.family, ModelFamily::Anthropic);
    assert_eq!(models[1].id, RUBBER_DUCK_ROLE);
    assert_eq!(models[1].identity.model_id, "openai/critic");
    assert_eq!(models[1].identity.family, ModelFamily::OpenAi);
    assert_ne!(models[0].identity.family, models[1].identity.family);
}

#[test]
fn unsafe_critic_configuration_degrades_to_root_only() {
    let mut config = test_config();
    config.model_family = Some("anthropic".into());
    config.rubber_duck_model = Some(config.model.clone());
    config.rubber_duck_model_family = Some("anthropic".into());

    let (provider, warning) = openrouter_multi_model_provider(
        &config,
        PathBuf::from("/tmp/ee-openrouter-root-only-test"),
    )
    .expect("root provider remains usable");
    assert_eq!(provider.registered_models().len(), 1);
    assert!(warning.expect("warning").contains("model id must differ"));

    config.rubber_duck_model = Some("anthropic/other".into());
    let (provider, warning) = openrouter_multi_model_provider(
        &config,
        PathBuf::from("/tmp/ee-openrouter-same-family-test"),
    )
    .expect("root provider remains usable");
    assert_eq!(provider.registered_models().len(), 1);
    assert!(warning.expect("warning").contains("family must differ"));

    config.rubber_duck_model = Some("bad model id".into());
    config.rubber_duck_model_family = Some("openai".into());
    let (provider, warning) = openrouter_multi_model_provider(
        &config,
        PathBuf::from("/tmp/ee-openrouter-malformed-critic-test"),
    )
    .expect("malformed critic does not stop root");
    assert_eq!(provider.registered_models().len(), 1);
    assert!(warning.expect("warning").contains("invalid critic model metadata"));
}

#[test]
fn single_model_configuration_reports_critic_unavailable() {
    let config = test_config();
    let (provider, warning) = openrouter_multi_model_provider(
        &config,
        PathBuf::from("/tmp/ee-openrouter-single-model-test"),
    )
    .expect("single-model provider");
    assert_eq!(provider.registered_models().len(), 1);
    assert!(warning.expect("warning").contains("no critic model configured"));
}

#[test]
fn config_debug_redacts_openrouter_api_key() {
    let config = test_config();
    let debug = format!("{config:?}");
    assert!(debug.contains("[redacted]"));
    assert!(!debug.contains("sk-test"));
}

fn sample_transcript() -> Vec<ModelMessage> {
    vec![
        ModelMessage::text(ModelRole::System, "Memory facts:\ncwd: /work"),
        ModelMessage::text(ModelRole::User, "hello"),
        ModelMessage::text(ModelRole::Assistant, "hi there"),
        ModelMessage::tool_result("call_1", ToolResult::success("file contents")),
    ]
}

fn sample_definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition::new("read_file", "reads a file")
            .side_effect_class(SideEffectClass::Read)
            .input_schema(json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }))]
}

fn sample_request() -> ModelRequest {
    ModelRequest::new(
        sample_transcript(),
        sample_definitions(),
        BudgetTracker::new(&OrchestratorConfig::default()).snapshot(),
        TaskNode::new(TaskId::new("task-1"), "hello", "hello"),
    )
}

fn openrouter_message(value: Value) -> OpenRouterMessage {
    extract_openrouter_message_for_test(&value)
}

fn extract_openrouter_message_for_test(value: &Value) -> OpenRouterMessage {
    // Reuses the crate's decoder on a full response envelope.
    crate::openrouter::extract_openrouter_message(value).expect("decodes message")
}

#[test]
fn transcript_converts_to_openrouter_messages() {
    let config = test_config();
    let messages = openrouter_messages_from_transcript(&config, &sample_transcript());

    assert_eq!(messages.len(), 5, "system prompt plus four transcript messages");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "system");
    assert_eq!(messages[1]["role"], "system");
    assert_eq!(messages[1]["content"], "Memory facts:\ncwd: /work");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[3]["role"], "assistant");
    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[4]["tool_call_id"], "call_1");
    assert_eq!(messages[4]["content"], "file contents");
}

#[test]
fn subagent_summaries_map_to_user_content() {
    let config = test_config();
    let transcript = vec![ModelMessage::text(ModelRole::Subagent, "summary")];
    let messages = openrouter_messages_from_transcript(&config, &transcript);
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "summary");
}

#[test]
fn definitions_convert_to_openrouter_function_schema() {
    let tools = openrouter_tools_from_definitions(&sample_definitions());

    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "read_file");
    assert_eq!(tools[0]["function"]["description"], "reads a file");
    assert_eq!(tools[0]["function"]["parameters"]["required"][0], "path");
}

#[test]
fn request_body_carries_transcript_tools_and_config() {
    let mut config = test_config();
    config.reasoning_effort = Some(String::from("medium"));
    let body = openrouter_body_for_request(&config, &sample_transcript(), &sample_definitions());

    assert_eq!(body["model"], "test/model");
    assert_eq!(body["stream"], false);
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(body["messages"][0]["content"], "system");
    assert_eq!(body["messages"][4]["role"], "tool");
}

#[test]
fn tool_call_converts_to_normalized_tool_intent() {
    let value = json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "tool_read_file",
                        "arguments": "{\"path\":\"/tmp/x\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let response = model_response_from_openrouter(openrouter_message(value));

    assert_eq!(response.tool_intents.len(), 1);
    let intent = &response.tool_intents[0];
    assert_eq!(intent.tool_call_id, "call_1");
    assert_eq!(intent.name, "read_file", "historical alias maps to the builtin");
    assert_eq!(intent.arguments["path"], "/tmp/x");
    assert!(!response.completed, "tool calls mean the turn continues");
    assert!(response.text.is_empty());
}

#[test]
fn reasoning_converts_to_normalized_reasoning() {
    let value = json!({
        "choices": [{
            "message": { "reasoning": "think first", "content": "answer" },
            "finish_reason": "stop"
        }]
    });
    let response = model_response_from_openrouter(openrouter_message(value));

    assert_eq!(response.reasoning.as_deref(), Some("think first"));
    assert_eq!(response.text, "answer");
    assert!(response.completed);
}

#[test]
fn usage_converts_to_normalized_usage() {
    let response = model_response_from_openrouter(openrouter_message(json!({
        "choices": [{ "message": { "content": "answer" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 6_120, "completion_tokens": 2_311 }
    })));

    assert_eq!(response.usage.input_tokens, Some(6_120));
    assert_eq!(response.usage.output_tokens, Some(2_311));
}

#[test]
fn stop_reason_maps_to_completion_signal() {
    let stopped = model_response_from_openrouter(openrouter_message(json!({
        "choices": [{ "message": { "content": "done" }, "finish_reason": "stop" }]
    })));
    assert!(stopped.completed);
    assert!(stopped.tool_intents.is_empty());

    // Missing finish reason is treated as a completed stop (older APIs).
    let missing = model_response_from_openrouter(openrouter_message(json!({
        "choices": [{ "message": { "content": "done" } }]
    })));
    assert!(missing.completed);

    // Length-limited responses are not a completion signal.
    let truncated = model_response_from_openrouter(openrouter_message(json!({
        "choices": [{ "message": { "content": "half" }, "finish_reason": "length" }]
    })));
    assert!(!truncated.completed);
}

#[test]
fn request_body_builds_from_normalized_request() {
    let config = test_config();
    let body =
        openrouter_body_for_request(&config, &sample_request().transcript, &sample_request().tools);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
}

// ── Orchestrated mode over the framework server ──────────────────────

type ScriptedCompletion = test_support::ScriptedOpenRouterCompletion;

fn scripted_client(script: ScriptedCompletion) -> Arc<OpenRouterCompletionClient> {
    script.client()
}

fn never_client(script: ScriptedCompletion) -> Arc<OpenRouterCompletionClient> {
    script.client()
}

fn response_with_tool_args(id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
}

fn response_with_tool_call(id: &str, name: &str, path: &str) -> Value {
    response_with_tool_args(id, name, json!({ "path": path }))
}

fn response_with_text(text: &str) -> Value {
    json!({
        "choices": [{ "message": { "content": text }, "finish_reason": "stop" }]
    })
}

fn response_with_handoff(summary: &str) -> Value {
    response_with_text(
        &json!({
            "schema_version": 1,
            "summary": summary,
            "findings": [],
            "citations": { "files": [], "tools": [] },
            "unresolved": [],
            "recommended_actions": [],
        })
        .to_string(),
    )
}

fn response_with_reasoning(reasoning: &str, text: &str) -> Value {
    json!({
        "choices": [{
            "message": { "reasoning": reasoning, "content": text },
            "finish_reason": "stop"
        }]
    })
}

// Minimal server harness over the framework's memory transport.
struct Harness {
    handle: ee_acp_agent_server::MemoryTransportHandle,
    pending: Arc<Mutex<VecDeque<RawJsonRpcMessage>>>,
}

impl Harness {
    fn send(&self, frame: RawJsonRpcMessage) -> bool {
        self.handle.send(frame)
    }

    async fn next_frames(&self, count: usize) -> Vec<RawJsonRpcMessage> {
        for _ in 0..5_000 {
            let ready = {
                let mut pending = self.pending.lock().expect("harness pending poisoned");
                if pending.len() < count {
                    pending.extend(self.handle.take_outbound());
                }
                if pending.len() >= count { Some(pending.drain(..count).collect()) } else { None }
            };
            if let Some(frames) = ready {
                return frames;
            }
            tokio::task::yield_now().await;
        }
        panic!("timed out waiting for {count} outbound frames");
    }

    async fn shutdown(
        self,
        task: tokio::task::JoinHandle<Result<(), ee_acp_agent_server::AcpServerError>>,
    ) {
        drop(self.handle);
        task.await.expect("server task joins").expect("server exits cleanly on EOF");
    }
}

fn request(id: i64, method: &str, params: Value) -> RawJsonRpcMessage {
    RawJsonRpcMessage::request(method.to_string(), params, RequestId::Number(id))
        .expect("test request builds")
}

fn notification(method: &str, params: Value) -> RawJsonRpcMessage {
    RawJsonRpcMessage::notification(method.to_string(), params).expect("test notification builds")
}

fn request_result(frame: RawJsonRpcMessage) -> Value {
    let Response::Result { result, .. } = unwrap_response(frame) else {
        panic!("expected a result response");
    };
    result
}

fn request_error(frame: RawJsonRpcMessage) -> RpcError {
    let Response::Error { error, .. } = unwrap_response(frame) else {
        panic!("expected an error response");
    };
    error
}

fn unwrap_response(frame: RawJsonRpcMessage) -> Response<Value> {
    let RawJsonRpcMessage::Response(response) = frame else {
        panic!("expected a response frame, got {frame:?}");
    };
    response
}

fn raw_params_to_value(params: Option<ee_agent_protocol::RawJsonRpcParams>) -> Value {
    match params {
        None => Value::Null,
        Some(ee_agent_protocol::RawJsonRpcParams::Object(map)) => Value::Object(map),
        Some(ee_agent_protocol::RawJsonRpcParams::Array(array)) => Value::Array(array),
    }
}

fn spawn_server(
    adapter: OpenRouterModelAdapter,
    config: OrchestratorConfig,
) -> (Harness, tokio::task::JoinHandle<Result<(), ee_acp_agent_server::AcpServerError>>) {
    let provider_config = OrchestratorProviderConfig {
        implementation: ee_agent_protocol::Implementation::new(
            "ee-openrouter-agent",
            env!("CARGO_PKG_VERSION"),
        )
        .title("OpenRouter"),
        orchestrator: config,
        ..OrchestratorProviderConfig::default()
    };
    let provider = OrchestratorProvider::with_policy(
        provider_config,
        Arc::new(adapter),
        openrouter_orchestrated_policy(),
    );
    let server = ee_acp_agent_server::AcpAgentServer::new(
        provider,
        ee_acp_agent_server::AcpAgentServerConfig::default(),
    );
    let (transport, handle) = ee_acp_agent_server::MemoryTransport::new();
    let task = tokio::spawn(async move { server.run_with_transport(transport).await });
    (Harness { handle, pending: Arc::new(Mutex::new(VecDeque::new())) }, task)
}

async fn new_session(handle: &Harness, id: i64) -> String {
    handle.send(request(
        id,
        "session/new",
        json!({
            "cwd": "/work",
            "additionalDirectories": [],
            "mcpServers": [],
        }),
    ));
    let result = request_result(handle.next_frames(1).await.remove(0));
    let session_id = result["sessionId"].as_str().expect("session id").to_string();
    // The provider advertises its initial slash commands after the
    // session/new response; drain the update before the prompt flows.
    let frame = handle.next_frames(1).await.remove(0);
    let RawJsonRpcMessage::Notification(update) = &frame else {
        panic!("expected the available_commands_update, got {frame:?}");
    };
    assert_eq!(
        raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
        "available_commands_update"
    );
    session_id
}

async fn new_session_with_mcp(handle: &Harness, id: i64, mcp_servers: Value) -> String {
    handle.send(request(
        id,
        "session/new",
        json!({
            "cwd": "/work",
            "additionalDirectories": [],
            "mcpServers": mcp_servers,
        }),
    ));
    let result = request_result(handle.next_frames(1).await.remove(0));
    let session_id = result["sessionId"].as_str().expect("session id").to_string();
    // Same advertisement drain as `new_session`.
    let frame = handle.next_frames(1).await.remove(0);
    let RawJsonRpcMessage::Notification(update) = &frame else {
        panic!("expected the available_commands_update, got {frame:?}");
    };
    assert_eq!(
        raw_params_to_value(update.params.clone())["update"]["sessionUpdate"],
        "available_commands_update"
    );
    session_id
}

fn prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
    })
}

async fn set_write_mode(handle: &Harness, session_id: &str, request_id: i64) {
    handle.send(request(
        request_id,
        "session/set_mode",
        json!({ "sessionId": session_id, "modeId": "write" }),
    ));
    assert_eq!(request_result(handle.next_frames(1).await.remove(0)), json!({}));
}

/// Drains the MCP diagnostics thought updates emitted at prompt start
/// (Phase 12) until the summary `mcp-diagnostics` message is seen.
/// Every frame here is a thought chunk (diagnostics precede the plan
/// update), so no push-back is needed.
async fn drain_mcp_diagnostics(handle: &Harness) {
    loop {
        let frame = handle.next_frames(1).await.remove(0);
        let RawJsonRpcMessage::Notification(update) = &frame else {
            panic!("expected an update while draining, got {frame:?}");
        };
        let params = raw_params_to_value(update.params.clone());
        assert_eq!(
            params["update"]["sessionUpdate"], "agent_thought_chunk",
            "only thought updates precede the plan"
        );
        if params["update"]["messageId"] == "mcp-diagnostics" {
            return;
        }
    }
}

#[tokio::test]
async fn orchestrated_mode_starts_through_acp_agent_server_without_network() {
    let script = ScriptedCompletion::new(vec![response_with_text("unused")]);
    let adapter = OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());

    handle.send(request(1, "initialize", json!({ "protocolVersion": 1 })));

    let result = request_result(handle.next_frames(1).await.remove(0));
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentInfo"]["name"], "ee-openrouter-agent");
    assert_eq!(result["agentInfo"]["title"], "OpenRouter");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn orchestrated_mode_streams_reasoning_and_final_answer_updates() {
    let script = ScriptedCompletion::new(vec![response_with_reasoning("plan step", "final")]);
    let adapter = OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    let session_id = new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

    drain_mcp_diagnostics(&handle).await;
    let frames = handle.next_frames(5).await;
    let thought_params = raw_params_to_value(match &frames[1] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected thought update, got {other:?}"),
    });
    assert_eq!(thought_params["update"]["sessionUpdate"], "agent_thought_chunk");
    assert_eq!(thought_params["update"]["content"]["text"], "plan step");
    let answer_params = raw_params_to_value(match &frames[2] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected answer update, got {other:?}"),
    });
    assert_eq!(answer_params["update"]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(answer_params["update"]["content"]["text"], "final");
    let final_params = raw_params_to_value(match &frames[3] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected host final response update, got {other:?}"),
    });
    assert_eq!(final_params["update"]["messageId"], "ee-final-response-1");
    assert!(
        final_params["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("completion: unverified"))
    );
    let result = request_result(frames[4].clone());
    assert_eq!(result["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn orchestrated_mode_cancels_pending_model_round_promptly() {
    let script = ScriptedCompletion::never();
    let adapter =
        OpenRouterModelAdapter::with_completion(test_config(), never_client(script.clone()));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    let session_id = new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "wait")));
    drain_mcp_diagnostics(&handle).await;
    let plan = handle.next_frames(1).await.remove(0);
    assert!(matches!(plan, RawJsonRpcMessage::Notification(_)), "plan update first");
    handle.send(notification("session/cancel", json!({ "sessionId": session_id })));

    let result = request_result(handle.next_frames(1).await.remove(0));
    assert_eq!(result["stopReason"], "cancelled");
    assert_eq!(script.bodies().len(), 1, "model request started before cancellation");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn orchestrated_mode_keeps_openrouter_key_out_of_messages_tools_and_events() {
    let secret = "sk-secret-phase-11";
    let mut config = test_config();
    config.api_key = Some(secret.to_string());
    let script =
        ScriptedCompletion::new(vec![response_with_reasoning("safe reasoning", "safe answer")]);
    let adapter = OpenRouterModelAdapter::with_completion(config, scripted_client(script.clone()));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    let session_id = new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

    drain_mcp_diagnostics(&handle).await;
    let frames = handle.next_frames(5).await;
    let result = request_result(frames[4].clone());
    assert_eq!(result["stopReason"], "end_turn");
    let bodies = serde_json::to_string(&script.bodies()).expect("bodies serialize");
    let events = frames.iter().map(|frame| format!("{frame:?}")).collect::<String>();

    assert!(!bodies.contains(secret), "API key must not enter model messages or tool schemas");
    assert!(!events.contains(secret), "API key must not enter ACP updates or prompt result");

    handle.shutdown(task).await;
}

#[test]
fn openrouter_orchestrated_policy_admits_editor_writes_without_bypassing_host_gate() {
    use ee_agent_orchestrator::{SideEffectClass, SideEffectSubclass};

    let policy = openrouter_orchestrated_policy();
    let context = ee_agent_orchestrator::PolicyContext::default();
    assert!(
        policy
            .check(
                &ToolDefinition::new("create_terminal", "runs")
                    .side_effect_class(SideEffectClass::Execute),
                context,
            )
            .allow
    );
    assert!(
        policy
            .check(
                &ToolDefinition::new("delegate_task", "delegates")
                    .side_effect_class(SideEffectClass::Delegate),
                context,
            )
            .allow
    );
    assert!(
        policy
            .check(
                &ToolDefinition::new("write_file", "writes")
                    .side_effect_class(SideEffectClass::Write)
                    .side_effect_subclass(SideEffectSubclass::Overwrite),
                context,
            )
            .allow,
        "admission only reaches ACP fs/writeTextFile; BridgeUiHandler still requires approval"
    );
    assert!(
        !policy
            .check(
                &ToolDefinition::new("delete_file", "deletes")
                    .side_effect_class(SideEffectClass::Write)
                    .side_effect_subclass(SideEffectSubclass::Delete),
                context,
            )
            .allow,
        "unrelated destructive writes remain denied before any host request"
    );
}

#[tokio::test]
async fn orchestrated_mode_executes_read_file_through_client_bridge() {
    let script = ScriptedCompletion::new(vec![
        response_with_tool_call("call_1", "tool_read_file", "/tmp/notes.txt"),
        response_with_text("done"),
    ]);
    let adapter =
        OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script.clone()));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    let session_id = new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "read a file")));
    drain_mcp_diagnostics(&handle).await;
    // plan, pending tool-call, in-progress tool-call, then the
    // framework-owned fs request.
    let frames = handle.next_frames(4).await;
    let RawJsonRpcMessage::Request(fs_request) = &frames[3] else {
        panic!("fourth frame is the fs request, got {:?}", frames[3]);
    };
    assert_eq!(fs_request.method.as_ref(), "fs/read_text_file");
    let fs_params = raw_params_to_value(fs_request.params.clone());
    assert_eq!(fs_params["sessionId"], session_id);
    assert_eq!(fs_params["path"], "/tmp/notes.txt");

    // Answer the bridge call; the tool observation lands in the next
    // model request, then the answer and evidence-gated final report
    // stream before the unchanged ACP response.
    handle.send(RawJsonRpcMessage::response(
        fs_request.id.clone(),
        Ok(json!({ "content": "file contents" })),
    ));
    let frames = handle.next_frames(4).await;
    let completed_update = raw_params_to_value(match &frames[0] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected completed tool-call update, got {other:?}"),
    });
    assert_eq!(completed_update["update"]["sessionUpdate"], "tool_call_update");
    assert_eq!(completed_update["update"]["status"], "completed");

    let message_params = raw_params_to_value(match &frames[1] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected message update, got {other:?}"),
    });
    assert_eq!(message_params["update"]["sessionUpdate"], "agent_message_chunk");
    assert!(message_params.to_string().contains("done"));
    let final_params = raw_params_to_value(match &frames[2] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected host final response update, got {other:?}"),
    });
    assert_eq!(final_params["update"]["messageId"], "ee-final-response-1");

    let result = request_result(frames[3].clone());
    assert_eq!(result["stopReason"], "end_turn");

    // The second model round carried the tool observation.
    let bodies = script.bodies();
    assert_eq!(bodies.len(), 2);
    let second = &bodies[1]["messages"];
    let tool_message = second
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool observation appended");
    assert_eq!(tool_message["tool_call_id"], "call_1");
    assert!(tool_message["content"].as_str().unwrap_or_default().contains("file contents"));

    handle.shutdown(task).await;
}

#[tokio::test]
async fn orchestrated_mode_executes_terminal_create_through_client_bridge() {
    let script = ScriptedCompletion::new(vec![
        response_with_tool_args("call_1", "create_terminal", json!({ "command": "echo hi" })),
        response_with_text("done"),
    ]);
    let adapter =
        OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script.clone()));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    let session_id = new_session(&handle, 1).await;
    set_write_mode(&handle, &session_id, 2).await;

    handle.send(request(3, "session/prompt", prompt_params(&session_id, "run echo")));
    drain_mcp_diagnostics(&handle).await;
    let frames = handle.next_frames(4).await;
    let RawJsonRpcMessage::Request(terminal_request) = &frames[3] else {
        panic!("fourth frame is terminal/create, got {:?}", frames[3]);
    };
    assert_eq!(terminal_request.method.as_ref(), "terminal/create");
    let terminal_params = raw_params_to_value(terminal_request.params.clone());
    assert_eq!(terminal_params["sessionId"], session_id);
    assert_eq!(terminal_params["command"], "echo hi");

    handle.send(RawJsonRpcMessage::response(
        terminal_request.id.clone(),
        Ok(json!({ "terminalId": "term-1" })),
    ));
    let frames = handle.next_frames(4).await;
    let completed_update = raw_params_to_value(match &frames[0] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected completed tool-call update, got {other:?}"),
    });
    assert_eq!(completed_update["update"]["status"], "completed");
    assert_eq!(request_result(frames[3].clone())["stopReason"], "end_turn");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn orchestrated_mode_executes_delegate_task_in_write_mode() {
    let script = ScriptedCompletion::new(vec![
        response_with_tool_args(
            "call_1",
            "delegate_task",
            json!({ "prompt": "inspect", "role_name": "summarizer" }),
        ),
        response_with_handoff("child answer"),
        response_with_text("parent done"),
    ]);
    let adapter =
        OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script.clone()));
    let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
    let session_id = new_session(&handle, 1).await;
    set_write_mode(&handle, &session_id, 2).await;

    handle.send(request(3, "session/prompt", prompt_params(&session_id, "delegate")));
    drain_mcp_diagnostics(&handle).await;
    let frames = handle.next_frames(7).await;
    let completed_update = raw_params_to_value(match &frames[3] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected completed delegate update, got {other:?}"),
    });
    assert_eq!(completed_update["update"]["sessionUpdate"], "tool_call_update");
    assert_eq!(completed_update["update"]["status"], "completed");
    assert_eq!(request_result(frames[6].clone())["stopReason"], "end_turn");
    assert_eq!(script.bodies().len(), 3, "parent, child, parent model rounds");

    handle.shutdown(task).await;
}

#[tokio::test]
async fn orchestrated_mode_respects_max_tool_calls_budget() {
    let script = ScriptedCompletion::new(vec![
        response_with_tool_call("call_1", "read_file", "/tmp/a"),
        response_with_tool_call("call_2", "read_file", "/tmp/b"),
    ]);
    let adapter = OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script));
    let config = OrchestratorConfig { max_tool_calls_per_turn: 1, ..OrchestratorConfig::default() };
    let (handle, task) = spawn_server(adapter, config);
    let session_id = new_session(&handle, 1).await;

    handle.send(request(2, "session/prompt", prompt_params(&session_id, "read files")));
    drain_mcp_diagnostics(&handle).await;
    // plan, pending, in-progress, fs request for the first tool call.
    let frames = handle.next_frames(4).await;
    let RawJsonRpcMessage::Request(fs_request) = &frames[3] else {
        panic!("expected the fs request, got {:?}", frames[3]);
    };
    handle.send(RawJsonRpcMessage::response(fs_request.id.clone(), Ok(json!({ "content": "a" }))));
    // Completed update, then the loop stops on the budget denial.
    let frames = handle.next_frames(2).await;
    let completed_update = raw_params_to_value(match &frames[0] {
        RawJsonRpcMessage::Notification(update) => update.params.clone(),
        other => panic!("expected completed update, got {other:?}"),
    });
    assert_eq!(completed_update["update"]["sessionUpdate"], "tool_call_update");
    let error = request_error(frames[1].clone());
    assert!(
        error.message.contains("budget exceeded"),
        "second tool call denied by budget: {}",
        error.message
    );

    handle.shutdown(task).await;
}

// ── Phase 12: ee MCP proxy through orchestrated OpenRouter ───────────

mod mcp;
