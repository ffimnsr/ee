//! Orchestrated OpenRouter mode: OpenRouter as a model adapter.
//!
//! [`OpenRouterModelAdapter`] implements
//! [`ModelAdapter`](ee_agent_orchestrator::ModelAdapter), so
//! `ee-openrouter-agent` can run through [`OrchestratorProvider`](ee_agent_orchestrator::OrchestratorProvider):
//! the orchestrator owns the bounded model–tool loop, the task graph, memory,
//! budgets, and policy gates, while OpenRouter only answers chat-completions
//! round trips.
//!
//! The transcript is converted to OpenRouter messages and the registry's
//! tool definitions to an OpenRouter function schema; text, reasoning, tool
//! calls, and the `finish_reason` completion signal map back onto the
//! normalized [`ModelResponse`].  The API key appears only in the
//! Authorization header and never in the transcript, memory, or logs.
//!
//! The HTTP round trip is behind a completion client so tests stay
//! network-free with scripted responses.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ee_acp_agent_server::ProviderError;
use ee_agent_orchestrator::{
    ModelAdapter, ModelContent, ModelError, ModelFuture, ModelMessage, ModelRequest, ModelResponse,
    ModelRole, PolicyEngine, ToolDefinition, ToolIntent, ToolPolicy,
};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::config::Config;
#[cfg(test)]
use crate::openrouter::openrouter_request_body_with_tools;
use crate::openrouter::{OpenRouterMessage, call_openrouter};

/// Builds the default policy for orchestrated OpenRouter sessions.
///
/// Read, execute, and delegate tools are available because orchestrated mode is
/// the production OpenRouter path. Write tools and destructive subclasses stay
/// denied until separately allowed by a narrower policy.
#[must_use]
pub fn openrouter_orchestrated_policy() -> PolicyEngine {
    PolicyEngine::new(ToolPolicy {
        allow_read: true,
        allow_write: false,
        allow_execute: true,
        allow_delegate: true,
        ..ToolPolicy::default()
    })
}

/// Boxed future returned by a completion client.
pub(crate) type OpenRouterCompletionFuture =
    Pin<Box<dyn Future<Output = Result<OpenRouterMessage, ProviderError>> + Send + 'static>>;

/// One chat-completions round trip, abstracted so tests stay network-free.
///
/// Arguments: `(config, api_key, messages, tools)`; the real client sends the
/// request body built from those parts.
pub(crate) type OpenRouterCompletionClient =
    dyn Fn(&Config, &str, &[Value], &[Value]) -> OpenRouterCompletionFuture + Send + Sync;

/// OpenRouter as a normalized [`ModelAdapter`].
pub struct OpenRouterModelAdapter {
    config: Config,
    completion: Arc<OpenRouterCompletionClient>,
}

impl OpenRouterModelAdapter {
    /// Builds an adapter with a real HTTP client honoring `config.timeout`.
    pub fn new(config: Config) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| format!("failed to build HTTP client: {error}"))?;
        Ok(Self::with_completion(config, real_completion(http)))
    }

    /// Builds an adapter with an injected completion client (tests).
    #[must_use]
    pub(crate) fn with_completion(
        config: Config,
        completion: Arc<OpenRouterCompletionClient>,
    ) -> Self {
        Self { config, completion }
    }
}

/// The real completion client: one OpenRouter chat-completions round trip.
fn real_completion(http: reqwest::Client) -> Arc<OpenRouterCompletionClient> {
    Arc::new(move |config, api_key, messages, tools| {
        let http = http.clone();
        let config = config.clone();
        let api_key = api_key.to_string();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        Box::pin(async move { call_openrouter(&http, &config, &api_key, &messages, &tools).await })
    })
}

impl ModelAdapter for OpenRouterModelAdapter {
    fn complete(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let config = self.config.clone();
        let completion = self.completion.clone();
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            let Some(api_key) = config.api_key.clone() else {
                return Err(ModelError::Adapter(
                    "OPENROUTER_API_KEY is not set; export it before starting ee".into(),
                ));
            };
            let messages = openrouter_messages_from_transcript(&config, &request.transcript);
            let tools = openrouter_tools_from_definitions(&request.tools);
            let completion = completion(&config, &api_key, &messages, &tools);
            let answer = tokio::select! {
                answer = completion => answer.map_err(|error| ModelError::Adapter(error.to_string()))?,
                () = wait_cancelled(cancel) => return Err(ModelError::Cancelled),
            };
            Ok(model_response_from_openrouter(answer))
        })
    }
}

async fn wait_cancelled(mut cancel: watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    while cancel.changed().await.is_ok() {
        if *cancel.borrow() {
            return;
        }
    }
}

/// Converts a normalized transcript into OpenRouter chat messages, prepending
/// the configured system prompt.  Tool observations carry their stable
/// tool-call id; subagent summaries map to user content.
pub(crate) fn openrouter_messages_from_transcript(
    config: &Config,
    transcript: &[ModelMessage],
) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "system", "content": config.system_prompt })];
    for message in transcript {
        let role = match message.role {
            ModelRole::System => "system",
            ModelRole::User | ModelRole::Subagent => "user",
            ModelRole::Assistant => "assistant",
            ModelRole::Tool => "tool",
        };
        let content = message_content_text(&message.content);
        let entry = if role == "tool" {
            json!({
                "role": "tool",
                "tool_call_id": tool_call_id_of(&message.content),
                "content": content,
            })
        } else {
            json!({ "role": role, "content": content })
        };
        messages.push(entry);
    }
    messages
}

/// Renders one message's content blocks as text.
fn message_content_text(content: &[ModelContent]) -> String {
    let mut parts = Vec::new();
    for block in content {
        match block {
            ModelContent::Text(text) => parts.push(text.clone()),
            ModelContent::ToolResult { result, .. } => parts.push(result.summary_text()),
            ModelContent::FileReference { path } => parts.push(format!("[file:{path}]")),
            ModelContent::TerminalReference { terminal_id } => {
                parts.push(format!("[terminal:{terminal_id}]"))
            }
            _ => {} // future content kinds stay out of the OpenRouter text view
        }
    }
    parts.join("\n")
}

/// Stable tool-call id of a tool observation message.
fn tool_call_id_of(content: &[ModelContent]) -> String {
    content
        .iter()
        .find_map(|block| match block {
            ModelContent::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Converts normalized tool definitions into an OpenRouter function schema.
pub(crate) fn openrouter_tools_from_definitions(definitions: &[ToolDefinition]) -> Vec<Value> {
    definitions
        .iter()
        .map(|definition| {
            json!({
                "type": "function",
                "function": {
                    "name": definition.name,
                    "description": definition.description,
                    "parameters": definition.input_schema,
                }
            })
        })
        .collect()
}

/// Maps a model tool-call name onto the registry's tool name.  The historical
/// `tool_read_file` alias maps to the built-in `read_file` tool.
fn map_tool_name(name: &str) -> String {
    match name {
        "tool_read_file" => "read_file".to_string(),
        other => other.to_string(),
    }
}

/// Converts a decoded OpenRouter assistant message into a normalized
/// [`ModelResponse`]: text, reasoning, tool intents, and the completion
/// signal derived from `finish_reason`.
pub(crate) fn model_response_from_openrouter(answer: OpenRouterMessage) -> ModelResponse {
    let completed =
        answer.tool_calls.is_empty() && answer.finish_reason.as_deref().unwrap_or("stop") == "stop";
    let intents = answer
        .tool_calls
        .into_iter()
        .map(|call| ToolIntent::new(call.id, map_tool_name(&call.name), call.arguments))
        .collect();
    let mut response = ModelResponse::new().text(answer.content).tool_intents(intents);
    if !answer.reasoning.is_empty() {
        response = response.reasoning(answer.reasoning);
    }
    if completed {
        response = response.completed();
    }
    response
}

/// Builds the OpenRouter request body for a completion round (tests).
#[cfg(test)]
pub(crate) fn openrouter_body_for_request(
    config: &Config,
    transcript: &[ModelMessage],
    definitions: &[ToolDefinition],
) -> Value {
    openrouter_request_body_with_tools(
        config,
        &openrouter_messages_from_transcript(config, transcript),
        &openrouter_tools_from_definitions(definitions),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future;
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
            api_url: String::from(DEFAULT_API_URL),
            api_key: Some(String::from("sk-test")),
            site_url: None,
            app_title: String::from("ee-test"),
            timeout: Duration::from_secs(1),
            system_prompt: String::from("system"),
            reasoning_effort: None,
            orchestrated: true,
        }
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
        let body =
            openrouter_body_for_request(&config, &sample_transcript(), &sample_definitions());

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
        let body = openrouter_body_for_request(
            &config,
            &sample_request().transcript,
            &sample_request().tools,
        );
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    }

    // ── Orchestrated mode over the framework server ──────────────────────

    /// Scripted completion client: replays canned responses and records every
    /// request body; network-free by construction.
    #[derive(Clone)]
    struct ScriptedCompletion {
        script: Arc<Mutex<VecDeque<Value>>>,
        bodies: Arc<Mutex<Vec<Value>>>,
    }

    impl ScriptedCompletion {
        fn new(responses: Vec<Value>) -> Self {
            Self {
                script: Arc::new(Mutex::new(responses.into())),
                bodies: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn bodies(&self) -> Vec<Value> {
            self.bodies.lock().expect("bodies poisoned").clone()
        }

        fn never() -> Self {
            Self::new(Vec::new())
        }
    }

    fn scripted_client(script: ScriptedCompletion) -> Arc<OpenRouterCompletionClient> {
        Arc::new(move |_config, _api_key, messages, tools| {
            let script = script.clone();
            let messages = messages.to_vec();
            let tools = tools.to_vec();
            Box::pin(async move {
                script
                    .bodies
                    .lock()
                    .expect("bodies poisoned")
                    .push(json!({ "messages": messages, "tools": tools }));
                let response = script
                    .script
                    .lock()
                    .expect("script poisoned")
                    .pop_front()
                    .unwrap_or_else(|| {
                        json!({ "choices": [{ "message": { "content": "" }, "finish_reason": "stop" }] })
                    });
                Ok(extract_openrouter_message_for_test(&response))
            })
        })
    }

    fn never_client(script: ScriptedCompletion) -> Arc<OpenRouterCompletionClient> {
        Arc::new(move |_config, _api_key, messages, tools| {
            let script = script.clone();
            let messages = messages.to_vec();
            let tools = tools.to_vec();
            Box::pin(async move {
                script
                    .bodies
                    .lock()
                    .expect("bodies poisoned")
                    .push(json!({ "messages": messages, "tools": tools }));
                future::pending::<Result<OpenRouterMessage, ProviderError>>().await
            })
        })
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
                    while pending.len() < count {
                        let fresh = self.handle.take_outbound();
                        if fresh.is_empty() {
                            break;
                        }
                        pending.extend(fresh);
                    }
                    if pending.len() >= count {
                        Some(pending.drain(..count).collect())
                    } else {
                        None
                    }
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
        RawJsonRpcMessage::notification(method.to_string(), params)
            .expect("test notification builds")
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
        result["sessionId"].as_str().expect("session id").to_string()
    }

    fn prompt_params(session_id: &str, text: &str) -> Value {
        json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }],
        })
    }

    #[tokio::test]
    async fn orchestrated_mode_starts_through_acp_agent_server_without_network() {
        let script = ScriptedCompletion::new(vec![response_with_text("unused")]);
        let adapter =
            OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script));
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
        let adapter =
            OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script));
        let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
        let session_id = new_session(&handle, 1).await;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

        let frames = handle.next_frames(4).await;
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
        let result = request_result(frames[3].clone());
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
        let adapter =
            OpenRouterModelAdapter::with_completion(config, scripted_client(script.clone()));
        let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
        let session_id = new_session(&handle, 1).await;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

        let frames = handle.next_frames(4).await;
        let result = request_result(frames[3].clone());
        assert_eq!(result["stopReason"], "end_turn");
        let bodies = serde_json::to_string(&script.bodies()).expect("bodies serialize");
        let events = frames.iter().map(|frame| format!("{frame:?}")).collect::<String>();

        assert!(!bodies.contains(secret), "API key must not enter model messages or tool schemas");
        assert!(!events.contains(secret), "API key must not enter ACP updates or prompt result");

        handle.shutdown(task).await;
    }

    #[test]
    fn openrouter_orchestrated_policy_allows_execute_and_delegate_not_write() {
        let policy = openrouter_orchestrated_policy();
        let context = ee_agent_orchestrator::PolicyContext::default();
        assert!(
            policy
                .check(
                    &ToolDefinition::new("create_terminal", "runs")
                        .side_effect_class(ee_agent_orchestrator::SideEffectClass::Execute),
                    context,
                )
                .allow
        );
        assert!(
            policy
                .check(
                    &ToolDefinition::new("delegate_task", "delegates")
                        .side_effect_class(ee_agent_orchestrator::SideEffectClass::Delegate),
                    context,
                )
                .allow
        );
        assert!(
            !policy
                .check(
                    &ToolDefinition::new("write_file", "writes")
                        .side_effect_class(ee_agent_orchestrator::SideEffectClass::Write),
                    context,
                )
                .allow
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
        // model request, then the answer streams and the turn ends.
        handle.send(RawJsonRpcMessage::response(
            fs_request.id.clone(),
            Ok(json!({ "content": "file contents" })),
        ));
        let frames = handle.next_frames(3).await;
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

        let result = request_result(frames[2].clone());
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

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "run echo")));
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
        let frames = handle.next_frames(3).await;
        let completed_update = raw_params_to_value(match &frames[0] {
            RawJsonRpcMessage::Notification(update) => update.params.clone(),
            other => panic!("expected completed tool-call update, got {other:?}"),
        });
        assert_eq!(completed_update["update"]["status"], "completed");
        assert_eq!(request_result(frames[2].clone())["stopReason"], "end_turn");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn orchestrated_mode_executes_delegate_task_by_default() {
        let script = ScriptedCompletion::new(vec![
            response_with_tool_args("call_1", "delegate_task", json!({ "prompt": "inspect" })),
            response_with_text("child answer"),
            response_with_text("parent done"),
        ]);
        let adapter =
            OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script.clone()));
        let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
        let session_id = new_session(&handle, 1).await;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "delegate")));
        let frames = handle.next_frames(6).await;
        let completed_update = raw_params_to_value(match &frames[3] {
            RawJsonRpcMessage::Notification(update) => update.params.clone(),
            other => panic!("expected completed delegate update, got {other:?}"),
        });
        assert_eq!(completed_update["update"]["sessionUpdate"], "tool_call_update");
        assert_eq!(completed_update["update"]["status"], "completed");
        assert_eq!(request_result(frames[5].clone())["stopReason"], "end_turn");
        assert_eq!(script.bodies().len(), 3, "parent, child, parent model rounds");

        handle.shutdown(task).await;
    }

    #[tokio::test]
    async fn orchestrated_mode_respects_max_tool_calls_budget() {
        let script = ScriptedCompletion::new(vec![
            response_with_tool_call("call_1", "read_file", "/tmp/a"),
            response_with_tool_call("call_2", "read_file", "/tmp/b"),
        ]);
        let adapter =
            OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script));
        let config =
            OrchestratorConfig { max_tool_calls_per_turn: 1, ..OrchestratorConfig::default() };
        let (handle, task) = spawn_server(adapter, config);
        let session_id = new_session(&handle, 1).await;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "read files")));
        // plan, pending, in-progress, fs request for the first tool call.
        let frames = handle.next_frames(4).await;
        let RawJsonRpcMessage::Request(fs_request) = &frames[3] else {
            panic!("expected the fs request, got {:?}", frames[3]);
        };
        handle.send(RawJsonRpcMessage::response(
            fs_request.id.clone(),
            Ok(json!({ "content": "a" })),
        ));
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
}
