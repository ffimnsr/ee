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
    ModelRole, PolicyEngine, StreamSink, ToolDefinition, ToolIntent, ToolPolicy,
};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::config::Config;
#[cfg(test)]
use crate::openrouter::openrouter_request_body_with_tools;
use crate::openrouter::{
    OpenRouterMessage, OpenRouterStreamDelta, call_openrouter, call_openrouter_streaming,
};

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

/// One streaming OpenRouter chat-completions round trip.
pub(crate) type OpenRouterStreamingClient = dyn Fn(&Config, &str, &[Value], &[Value], StreamSink) -> OpenRouterCompletionFuture
    + Send
    + Sync;

/// OpenRouter as a normalized [`ModelAdapter`].
pub struct OpenRouterModelAdapter {
    config: Config,
    completion: Arc<OpenRouterCompletionClient>,
    streaming: Option<Arc<OpenRouterStreamingClient>>,
}

impl OpenRouterModelAdapter {
    /// Builds an adapter with a real HTTP client honoring `config.timeout`.
    pub fn new(config: Config) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| format!("failed to build HTTP client: {error}"))?;
        Ok(Self {
            config,
            completion: real_completion(http.clone()),
            streaming: Some(real_streaming(http)),
        })
    }

    /// Builds an adapter with an injected completion client (tests).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_completion(
        config: Config,
        completion: Arc<OpenRouterCompletionClient>,
    ) -> Self {
        Self { config, completion, streaming: None }
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

fn real_streaming(http: reqwest::Client) -> Arc<OpenRouterStreamingClient> {
    Arc::new(move |config, api_key, messages, tools, events| {
        let http = http.clone();
        let config = config.clone();
        let api_key = api_key.to_string();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        Box::pin(async move {
            call_openrouter_streaming(&http, &config, &api_key, &messages, &tools, |delta| {
                match delta {
                    OpenRouterStreamDelta::Text(text) => events.text(text).map_err(|error| {
                        ProviderError::BackendFailure(format!(
                            "failed to forward OpenRouter text stream: {error}"
                        ))
                    }),
                    OpenRouterStreamDelta::Reasoning(text) => {
                        events.reasoning(text).map_err(|error| {
                            ProviderError::BackendFailure(format!(
                                "failed to forward OpenRouter reasoning stream: {error}"
                            ))
                        })
                    }
                }
            })
            .await
        })
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

    fn complete_streaming(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
        events: StreamSink,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let Some(streaming) = self.streaming.clone() else {
            let completion = self.complete(request, cancel);
            return Box::pin(async move {
                let response = completion.await?;
                if let Some(reasoning) =
                    response.reasoning.as_deref().filter(|text| !text.is_empty())
                {
                    events.reasoning(reasoning.to_string())?;
                }
                if !response.text.is_empty() {
                    events.text(response.text.clone())?;
                }
                Ok(response)
            });
        };
        let config = self.config.clone();
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
            let completion = streaming(&config, &api_key, &messages, &tools, events);
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
            compact_min_messages: 4,
            compact_retained_tail: 2,
            compact_max_input_bytes: 65_536,
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
                    if pending.len() < count {
                        pending.extend(self.handle.take_outbound());
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

        drain_mcp_diagnostics(&handle).await;
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
        let adapter =
            OpenRouterModelAdapter::with_completion(config, scripted_client(script.clone()));
        let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
        let session_id = new_session(&handle, 1).await;

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "hello")));

        drain_mcp_diagnostics(&handle).await;
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
        drain_mcp_diagnostics(&handle).await;
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
        drain_mcp_diagnostics(&handle).await;
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

    // ── Phase 12: ee MCP proxy through orchestrated OpenRouter ───────────

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
        /// Tool-call updates streamed during the prompt (status + content).
        tool_updates: std::sync::Mutex<Vec<Value>>,
    }

    impl PromptMcpRunner {
        fn new() -> Self {
            Self {
                inner: std::collections::HashMap::new(),
                calls: std::collections::HashMap::new(),
                fail_connect: false,
                mcp_requests: std::sync::Mutex::new(Vec::new()),
                tool_updates: std::sync::Mutex::new(Vec::new()),
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

        /// Tool-call updates streamed during the prompt.
        fn tool_updates(&self) -> Vec<Value> {
            self.tool_updates.lock().expect("runner updates poisoned").clone()
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
                        if params["update"]["sessionUpdate"] == "tool_call_update" {
                            self.tool_updates
                                .lock()
                                .expect("runner updates poisoned")
                                .push(params["update"].clone());
                        }
                    }
                    RawJsonRpcMessage::Response(response) => {
                        let Response::Result { result, .. } = response else {
                            panic!("unexpected prompt error response");
                        };
                        let stop_reason =
                            result["stopReason"].as_str().unwrap_or_default().to_string();
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
                    let inner_method =
                        params.get("method").and_then(Value::as_str).unwrap_or_default();
                    self.mcp_requests
                        .lock()
                        .expect("runner log poisoned")
                        .push(format!("{inner_method}: {params}"));
                    if inner_method == "tools/call" {
                        let tool_name = params
                            .pointer("/params/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.calls.get(tool_name).cloned().unwrap_or_else(|| {
                            panic!("no canned tools/call response for {tool_name:?}")
                        })
                    } else {
                        self.inner.get(inner_method).cloned().unwrap_or_else(|| {
                            panic!("no canned inner response for {inner_method:?}")
                        })
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

        let mut runner =
            PromptMcpRunner::standard_ee_answers(json!([ee_tool("ee_workspace_roots")]));
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
            log.iter()
                .any(|line| line.contains("tools/call") && line.contains("ee_workspace_roots")),
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
    async fn orchestrated_mode_policy_blocks_ee_write_tool_before_dispatch() {
        let script = ScriptedCompletion::new(vec![
            response_with_tool_args(
                "tc-1",
                "ee_overwrite_text_file",
                json!({ "path": "/work/x.txt", "content": "data" }),
            ),
            response_with_text("denied, continuing"),
        ]);
        let adapter =
            OpenRouterModelAdapter::with_completion(test_config(), scripted_client(script));
        let (handle, task) = spawn_server(adapter, OrchestratorConfig::default());
        let session_id = new_session_with_mcp(&handle, 1, ee_proxy_acp_mcp_servers()).await;

        let mut runner = PromptMcpRunner::standard_ee_answers(json!([
            ee_tool("ee_overwrite_text_file"),
            ee_tool("ee_workspace_roots"),
        ]));

        handle.send(request(2, "session/prompt", prompt_params(&session_id, "overwrite the file")));
        let (_thoughts, stop_reason) = runner.run(&handle).await;
        assert_eq!(stop_reason, "end_turn", "policy denial does not crash the turn");

        // The denial streams as a failed tool-call update with the policy
        // reason (never a wire call).
        let updates = runner.tool_updates();
        assert!(
            updates.iter().any(|update| {
                update["status"] == "failed"
                    && update["toolCallId"] == "tc-1"
                    && update.to_string().contains("write tools require explicit policy")
            }),
            "{updates:?}"
        );

        // The write tool never reached the MCP wire.
        let log = runner.log();
        assert!(
            log.iter().all(
                |line| !line.contains("tools/call") || !line.contains("ee_overwrite_text_file")
            ),
            "MCP write tools cannot bypass orchestrator policy: {log:?}"
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

        let mut runner =
            PromptMcpRunner::standard_ee_answers(json!([ee_tool("ee_workspace_roots")]));
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
}
