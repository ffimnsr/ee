//! Chat Completions as a normalized `ee-agent-orchestrator` model adapter.
//!
//! The adapter owns the mapping between a normalized [`ModelRequest`] and the
//! wire format, the cancellation contract, and the credential check; the
//! transport owns the HTTP round trip. Both the real transport and an injected
//! client run behind the same interface, so tests stay network-free.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ee_acp_agent_server::ProviderError;
use ee_agent_orchestrator::{
    ModelAdapter, ModelError, ModelFuture, ModelRequest, ModelResponse, StreamSink,
};
use serde_json::Value;
use tokio::sync::watch;

use crate::profile::{EndpointProfile, TokenSource};
use crate::transport::ChatCompletionsClient;
use crate::wire::{
    AssistantTurn, StreamDelta, messages_from_transcript, response_from_turn,
    tools_from_definitions,
};

/// Boxed future returned by an injected completion client.
pub type CompletionFuture =
    Pin<Box<dyn Future<Output = Result<AssistantTurn, ProviderError>> + Send + 'static>>;

/// One buffered round trip, injectable so tests never need a network.
///
/// Arguments are `(messages, tools)`; the transport that serves them is bound
/// when the adapter is built.
pub type CompletionClient = dyn Fn(&[Value], &[Value]) -> CompletionFuture + Send + Sync;

/// One streaming round trip, injectable so tests never need a network.
pub type StreamingClient = dyn Fn(&[Value], &[Value], StreamSink) -> CompletionFuture + Send + Sync;

/// Chat Completions as a normalized [`ModelAdapter`].
pub struct ChatCompletionsAdapter {
    profile: EndpointProfile,
    token: TokenSource,
    completion: Arc<CompletionClient>,
    streaming: Option<Arc<StreamingClient>>,
}

impl ChatCompletionsAdapter {
    /// Builds an adapter with a real HTTP client honoring the profile timeout.
    ///
    /// # Errors
    ///
    /// Returns the HTTP client construction failure as a diagnostic string.
    pub fn new(profile: EndpointProfile, token: TokenSource) -> Result<Self, String> {
        let client = ChatCompletionsClient::new(profile.clone(), token.clone())?;
        Ok(Self::with_client(profile, token, client))
    }

    /// Builds an adapter over an existing HTTP client (shared pools, tests).
    #[must_use]
    pub fn with_http(profile: EndpointProfile, token: TokenSource, http: reqwest::Client) -> Self {
        let client = ChatCompletionsClient::with_http(profile.clone(), token.clone(), http);
        Self::with_client(profile, token, client)
    }

    /// Builds an adapter with an injected buffered client and no streaming.
    ///
    /// [`ModelAdapter::complete_streaming`] then emits the finished answer as a
    /// single reasoning and text chunk.
    #[must_use]
    pub fn with_completion(
        profile: EndpointProfile,
        token: TokenSource,
        completion: Arc<CompletionClient>,
    ) -> Self {
        Self { profile, token, completion, streaming: None }
    }

    /// Returns the provider profile this adapter was built for.
    #[must_use]
    pub fn profile(&self) -> &EndpointProfile {
        &self.profile
    }

    fn with_client(
        profile: EndpointProfile,
        token: TokenSource,
        client: ChatCompletionsClient,
    ) -> Self {
        let label = profile.label.clone();
        let buffered = client.clone();
        let completion: Arc<CompletionClient> = Arc::new(move |messages, tools| {
            let client = buffered.clone();
            let messages = messages.to_vec();
            let tools = tools.to_vec();
            Box::pin(async move { client.complete(&messages, &tools).await })
        });
        let forwarding = client;
        let streaming: Arc<StreamingClient> = Arc::new(move |messages, tools, events| {
            let client = forwarding.clone();
            let messages = messages.to_vec();
            let tools = tools.to_vec();
            let label = label.clone();
            Box::pin(async move {
                let mut sink = |delta: StreamDelta| match delta {
                    StreamDelta::Text(text) => events.text(text).map_err(|error| {
                        ProviderError::BackendFailure(format!(
                            "failed to forward {label} text stream: {error}"
                        ))
                    }),
                    StreamDelta::Reasoning(text) => events.reasoning(text).map_err(|error| {
                        ProviderError::BackendFailure(format!(
                            "failed to forward {label} reasoning stream: {error}"
                        ))
                    }),
                };
                client.complete_streaming(&messages, &tools, &mut sink).await
            })
        });
        Self { profile, token, completion, streaming: Some(streaming) }
    }
}

impl ModelAdapter for ChatCompletionsAdapter {
    fn complete(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let profile = self.profile.clone();
        let token = self.token.clone();
        let completion = self.completion.clone();
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            // The credential is checked before any request body exists, and the
            // message names the provider's own environment variable.
            token().map_err(ModelError::Adapter)?;
            let messages = messages_from_transcript(&profile.system_prompt, &request.transcript);
            let tools = tools_from_definitions(&request.tools);
            let answer = tokio::select! {
                answer = completion(&messages, &tools) => {
                    answer.map_err(|error| ModelError::Adapter(error.to_string()))?
                }
                () = wait_cancelled(cancel) => return Err(ModelError::Cancelled),
            };
            Ok(response_from_turn(answer))
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
        let profile = self.profile.clone();
        let token = self.token.clone();
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            token().map_err(ModelError::Adapter)?;
            let messages = messages_from_transcript(&profile.system_prompt, &request.transcript);
            let tools = tools_from_definitions(&request.tools);
            let answer = tokio::select! {
                answer = streaming(&messages, &tools, events) => {
                    answer.map_err(|error| ModelError::Adapter(error.to_string()))?
                }
                () = wait_cancelled(cancel) => return Err(ModelError::Cancelled),
            };
            Ok(response_from_turn(answer))
        })
    }
}

/// Resolves when the cancellation signal flips, so every model call stays
/// cancellable without polling.
///
/// Every adapter awaits this alongside its model call, so a cancelled turn never
/// waits for a slow endpoint.
pub async fn wait_cancelled(mut cancel: watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    while cancel.changed().await.is_ok() {
        if *cancel.borrow() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ee_agent_orchestrator::{
        BudgetTracker, OrchestratorConfig, SideEffectClass, TaskId, TaskNode, ToolDefinition,
    };
    use serde_json::json;

    use super::*;
    use crate::endpoint::TrustedEndpoint;
    use crate::profile::BearerToken;

    fn profile() -> EndpointProfile {
        EndpointProfile::new(
            "Example",
            "EXAMPLE_API_KEY",
            TrustedEndpoint::parse("https://example.test/v1/chat/completions").expect("https"),
            "example-model",
        )
        .with_system_prompt("system")
    }

    fn token() -> TokenSource {
        Arc::new(|| Ok(BearerToken::new("sk-test")))
    }

    fn request() -> ModelRequest {
        ModelRequest::new(
            vec![
                ee_agent_orchestrator::ModelMessage::text(
                    ee_agent_orchestrator::ModelRole::User,
                    "hello",
                ),
                ee_agent_orchestrator::ModelMessage::tool_result(
                    "call_1",
                    ee_agent_orchestrator::ToolResult::success("contents"),
                ),
            ],
            vec![
                ToolDefinition::new("read_file", "reads a file")
                    .side_effect_class(SideEffectClass::Read),
            ],
            BudgetTracker::new(&OrchestratorConfig::default()).snapshot(),
            TaskNode::new(TaskId::new("task-1"), "hello", "hello"),
        )
    }

    #[tokio::test]
    async fn injected_completion_receives_normalized_messages_and_tools() {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let completion: Arc<CompletionClient> = Arc::new(move |messages, tools| {
            let recorder = recorder.clone();
            let messages = messages.to_vec();
            let tools = tools.to_vec();
            Box::pin(async move {
                recorder
                    .lock()
                    .expect("seen")
                    .push(json!({ "messages": messages, "tools": tools }));
                Ok(AssistantTurn {
                    content: String::from("answer"),
                    reasoning: String::from("thinking"),
                    raw: json!({ "role": "assistant", "content": "answer" }),
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: Some(crate::wire::Usage {
                        input_tokens: Some(7),
                        output_tokens: Some(3),
                        total_tokens: None,
                    }),
                })
            })
        });
        let adapter = ChatCompletionsAdapter::with_completion(profile(), token(), completion);
        let (_cancel_tx, cancel) = watch::channel(false);

        let response = adapter.complete(request(), cancel).await.expect("completes");

        let seen = seen.lock().expect("seen").clone();
        assert_eq!(seen[0]["messages"][0]["role"], "system");
        assert_eq!(seen[0]["messages"][0]["content"], "system");
        assert_eq!(seen[0]["messages"][1]["content"], "hello");
        assert_eq!(seen[0]["messages"][2]["role"], "tool");
        assert_eq!(seen[0]["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(seen[0]["tools"][0]["function"]["name"], "read_file");
        assert_eq!(response.text, "answer");
        assert_eq!(response.reasoning.as_deref(), Some("thinking"));
        assert_eq!(response.usage.input_tokens, Some(7));
        assert_eq!(response.usage.output_tokens, Some(3));
        assert!(response.completed);
    }

    #[tokio::test]
    async fn missing_credential_fails_before_the_client_runs() {
        let calls = Arc::new(Mutex::new(0usize));
        let counter = calls.clone();
        let completion: Arc<CompletionClient> = Arc::new(move |_messages, _tools| {
            let counter = counter.clone();
            Box::pin(async move {
                *counter.lock().expect("calls") += 1;
                Ok(AssistantTurn {
                    content: String::new(),
                    reasoning: String::new(),
                    raw: json!({ "role": "assistant" }),
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: None,
                })
            })
        });
        let missing: TokenSource = Arc::new(|| Err(String::from("EXAMPLE_API_KEY is not set")));
        let adapter = ChatCompletionsAdapter::with_completion(profile(), missing, completion);
        let (_cancel_tx, cancel) = watch::channel(false);

        let error = adapter.complete(request(), cancel).await.expect_err("no credential");

        assert!(
            matches!(&error, ModelError::Adapter(message) if message == "EXAMPLE_API_KEY is not set")
        );
        assert_eq!(*calls.lock().expect("calls"), 0, "no request may run without a credential");
    }

    #[tokio::test]
    async fn cancellation_is_observed_before_the_request() {
        let completion: Arc<CompletionClient> =
            Arc::new(|_messages, _tools| Box::pin(async { panic!("must not be called") }));
        let adapter = ChatCompletionsAdapter::with_completion(profile(), token(), completion);
        let (_cancel_tx, cancel) = watch::channel(true);

        let error = adapter.complete(request(), cancel).await.expect_err("cancelled");

        assert!(matches!(error, ModelError::Cancelled));
    }

    #[tokio::test]
    async fn streaming_without_a_streaming_client_emits_the_finished_answer_once() {
        let completion: Arc<CompletionClient> = Arc::new(|_messages, _tools| {
            Box::pin(async {
                Ok(AssistantTurn {
                    content: String::from("answer"),
                    reasoning: String::from("thinking"),
                    raw: json!({ "role": "assistant", "content": "answer" }),
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: None,
                })
            })
        });
        let adapter = ChatCompletionsAdapter::with_completion(profile(), token(), completion);
        let (updates, receiver) = ee_agent_orchestrator::streaming::stream_channel();
        let (_cancel_tx, cancel) = watch::channel(false);

        let response = adapter
            .complete_streaming(request(), cancel.clone(), updates)
            .await
            .expect("completes");

        let merged = receiver.into_consumer().run(None, &cancel, "message").await;
        assert_eq!(response.text, "answer");
        assert_eq!(merged.text, "answer");
        assert_eq!(merged.reasoning, "thinking");
        assert_eq!(merged.text_chunks, 1);
        assert_eq!(merged.reasoning_chunks, 1);
    }
}
