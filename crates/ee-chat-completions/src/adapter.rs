//! Chat Completions as a normalized `ee-agent-orchestrator` model adapter.
//!
//! The adapter owns the mapping between a normalized [`ModelRequest`] and the
//! wire format, the cancellation contract, and the credential check; the
//! transport owns the HTTP round trip. Both the real transport and an injected
//! client run behind the same interface, so tests stay network-free.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::{
    CompactionOutcome, ModelAdapter, ModelError, ModelFuture, ModelRequest, ModelResponse,
    StreamSink,
};
use tokio::sync::watch;

use crate::native::seal_response_with_provider;
use crate::profile::{EndpointProfile, TokenSource};
use crate::responses::ResponsesCodec;
use crate::transport::{ChatCompletionsClient, ChatRequest, EndpointTransport};
use crate::wire::{
    AssistantTurn, ChatRequestParts, StreamDelta, chat_request_parts, model_usage_of,
    response_from_turn,
};

/// Boxed future returned by an injected completion client.
pub type CompletionFuture =
    Pin<Box<dyn Future<Output = Result<AssistantTurn, ProviderError>> + Send + 'static>>;

/// One buffered round trip, injectable so tests never need a network.
///
/// The request carries the assembled messages and tools plus the tool-call
/// control, so a caller can force a tool without rebuilding the profile.
pub type CompletionClient = dyn Fn(ChatRequest) -> CompletionFuture + Send + Sync;

/// One streaming round trip, injectable so tests never need a network.
pub type StreamingClient = dyn Fn(ChatRequest, StreamSink) -> CompletionFuture + Send + Sync;

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
        let buffered = client.clone();
        let completion: Arc<CompletionClient> = Arc::new(move |request| {
            let client = buffered.clone();
            Box::pin(async move { client.complete_request(&request).await })
        });
        let forwarding = client;
        let streaming: Arc<StreamingClient> = Arc::new(move |request, events| {
            let client = forwarding.clone();
            Box::pin(async move {
                let mut sink = |delta: StreamDelta| forward_delta(delta, &events);
                client.complete_streaming_request(&request, &mut sink).await
            })
        });
        Self { profile, token, completion, streaming: Some(streaming) }
    }
}

/// Rejects a structured-output request on a route whose catalog entry does not
/// declare support, before any request body is built.
///
/// The hidden forced tool works on every route, so this is a configuration
/// check: the caller asked for behaviour the selected model does not advertise,
/// and sending it anyway would make the failure a provider error instead of
/// actionable guidance.
fn ensure_structured_output(
    profile: &EndpointProfile,
    request: &ModelRequest,
) -> Result<(), ModelError> {
    if request.response_format.is_some() && !profile.structured_output {
        return Err(ModelError::Adapter(format!(
            "model `{}` does not advertise structured output; remove the response-format \
             request or select a model whose catalog entry sets `structured_output`",
            profile.model
        )));
    }
    Ok(())
}

/// Rejects a tool-call control the catalog does not advertise for the route.
///
/// An empty capability list means the snapshot declares nothing, so the
/// protocol default applies and the request is left alone. When the route does
/// declare values, a mismatch is a configuration error: sending it anyway
/// would turn an actionable mistake into a provider rejection.
fn ensure_tool_choice(profile: &EndpointProfile, request: &ModelRequest) -> Result<(), ModelError> {
    match profile.tool_choice_error(&request.tool_control, !request.tools.is_empty()) {
        Some(detail) => Err(ModelError::Adapter(detail)),
        None => Ok(()),
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
            ensure_structured_output(&profile, &request)?;
            ensure_tool_choice(&profile, &request)?;
            let request = wire_request(&profile, &request)?;
            let answer = tokio::select! {
                answer = completion(request) => {
                    answer.map_err(ModelError::Provider)?
                }
                () = wait_cancelled(cancel) => return Err(ModelError::Cancelled),
            };
            let mut response = response_from_turn(answer);
            ee_agent_orchestrator::apply_catalog_cost(&mut response, profile.cost);
            Ok(seal_response_with_provider(response, Some(&profile.label), &profile.model))
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
            ensure_structured_output(&profile, &request)?;
            ensure_tool_choice(&profile, &request)?;
            let wire = wire_request(&profile, &request)?;
            let answer = tokio::select! {
                answer = streaming(wire, events) => {
                    answer.map_err(ModelError::Provider)?
                }
                () = wait_cancelled(cancel) => return Err(ModelError::Cancelled),
            };
            let mut response = response_from_turn(answer);
            ee_agent_orchestrator::apply_catalog_cost(&mut response, profile.cost);
            Ok(seal_response_with_provider(response, Some(&profile.label), &profile.model))
        })
    }
}

/// OpenAI Responses as a normalized [`ModelAdapter`].
///
/// The Responses codec owns the request body, the SSE decoding, and the retry
/// boundary; this adapter owns the orchestrator contract (credential check,
/// cancellation, streamed-update forwarding) exactly like
/// [`ChatCompletionsAdapter`].
pub struct ResponsesAdapter {
    profile: EndpointProfile,
    token: TokenSource,
    codec: Arc<ResponsesCodec>,
}

impl ResponsesAdapter {
    /// Builds an adapter with a real HTTP client honoring the profile timeout.
    ///
    /// # Errors
    ///
    /// Returns the HTTP client construction failure as a diagnostic string.
    pub fn new(profile: EndpointProfile, token: TokenSource) -> Result<Self, String> {
        let transport = EndpointTransport::new(profile.clone(), token.clone())?;
        Ok(Self { profile, token, codec: Arc::new(ResponsesCodec::new(transport)) })
    }

    /// Builds an adapter over an existing HTTP client (shared pools, tests).
    #[must_use]
    pub fn with_http(profile: EndpointProfile, token: TokenSource, http: reqwest::Client) -> Self {
        let transport = EndpointTransport::with_http(profile.clone(), token.clone(), http);
        Self { profile, token, codec: Arc::new(ResponsesCodec::new(transport)) }
    }

    /// Returns the provider profile this adapter was built for.
    #[must_use]
    pub fn profile(&self) -> &EndpointProfile {
        &self.profile
    }
}

impl ModelAdapter for ResponsesAdapter {
    fn complete(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let token = self.token.clone();
        let codec = Arc::clone(&self.codec);
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            token().map_err(ModelError::Adapter)?;
            ee_agent_orchestrator::validate_tool_names(&request.tools)
                .map_err(ModelError::Adapter)?;
            request.tool_control.validate(&request.tools).map_err(ModelError::Adapter)?;
            let profile = codec.profile();
            ensure_structured_output(profile, &request)?;
            ensure_tool_choice(profile, &request)?;
            let generation = merged_generation(profile, &request)?;
            let completion = codec.complete_controlled(
                &request.transcript,
                &request.tools,
                &request.tool_control,
                request.prompt_tokens,
                (!generation.is_empty()).then_some(&generation),
                request.session.as_ref(),
            );
            tokio::select! {
                response = completion => response.map_err(response_error),
                () = wait_cancelled(cancel) => Err(ModelError::Cancelled),
            }
        })
    }

    fn complete_streaming(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
        events: StreamSink,
    ) -> ModelFuture<Result<ModelResponse, ModelError>> {
        let token = self.token.clone();
        let codec = Arc::clone(&self.codec);
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            token().map_err(ModelError::Adapter)?;
            ee_agent_orchestrator::validate_tool_names(&request.tools)
                .map_err(ModelError::Adapter)?;
            request.tool_control.validate(&request.tools).map_err(ModelError::Adapter)?;
            let profile = codec.profile();
            ensure_structured_output(profile, &request)?;
            ensure_tool_choice(profile, &request)?;
            let generation = merged_generation(profile, &request)?;
            let mut sink = |delta: StreamDelta| forward_delta(delta, &events);
            let completion = async move {
                let mut request = request;
                let first = codec
                    .complete_streaming_controlled(
                        &request.transcript,
                        &request.tools,
                        &request.tool_control,
                        &mut sink,
                        request.prompt_tokens,
                        crate::CallOptions {
                            generation: (!generation.is_empty()).then_some(&generation),
                            session: request.session.as_ref(),
                            continuation: request.continuation.as_ref(),
                        },
                    )
                    .await;
                match first {
                    // A stale anchor is retried once with the full transcript;
                    // the failure is typed, never string-matched.
                    Err(ProviderError::StaleContinuation(_)) if request.continuation.is_some() => {
                        request.continuation = None;
                        codec
                            .complete_streaming_controlled(
                                &request.transcript,
                                &request.tools,
                                &request.tool_control,
                                &mut sink,
                                request.prompt_tokens,
                                crate::CallOptions {
                                    generation: (!generation.is_empty()).then_some(&generation),
                                    session: request.session.as_ref(),
                                    continuation: None,
                                },
                            )
                            .await
                    }
                    other => other,
                }
            };
            tokio::select! {
                response = completion => response.map_err(response_error),
                () = wait_cancelled(cancel) => Err(ModelError::Cancelled),
            }
        })
    }

    fn compact(
        &self,
        request: ModelRequest,
        cancel: watch::Receiver<bool>,
    ) -> ModelFuture<Result<CompactionOutcome, ModelError>> {
        let token = self.token.clone();
        let codec = Arc::clone(&self.codec);
        Box::pin(async move {
            if *cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            token().map_err(ModelError::Adapter)?;
            let profile = codec.profile();
            if !profile.compaction {
                return Err(ModelError::UnsupportedCompaction(format!(
                    "{} {} (responses)",
                    profile.label, profile.model
                )));
            }
            let generation = merged_generation(profile, &request)?;
            let generation = (!generation.is_empty()).then_some(generation);
            let completion = codec.compact(
                &request.transcript,
                &request.tools,
                generation.as_ref(),
                request.session.as_ref(),
            );
            tokio::select! {
                response = completion => response.map_err(response_error),
                () = wait_cancelled(cancel) => Err(ModelError::Cancelled),
            }
        })
    }
}

/// Keeps a closed stream distinguishable from a transport failure without
/// leaking the dropped channel's contents.
fn sink_error(error: ModelError) -> ProviderError {
    ProviderError::BackendFailure(format!("streamed update was rejected: {error}"))
}

/// Forwards one wire delta onto the orchestrator's stream sink.
///
/// Tool-input deltas are display-only: the final parsed arguments still arrive
/// with the completed response, so a preview can never be executed.
fn forward_delta(delta: StreamDelta, events: &StreamSink) -> Result<(), ProviderError> {
    let result = match delta {
        StreamDelta::Text(text) => events.text(text),
        StreamDelta::Reasoning(text) => events.reasoning(text),
        StreamDelta::ToolInputStart { id, name } => events.tool_input_start(id, name),
        StreamDelta::ToolInputDelta { id, text, parsed } => {
            events.tool_input_delta(id, text, parsed)
        }
        StreamDelta::ToolInputEnd { id } => events.tool_input_end(id),
        StreamDelta::Usage(usage) => events.usage(model_usage_of(usage)),
        StreamDelta::Raw { frame } => events.raw(frame),
    };
    result.map_err(sink_error)
}

/// Maps a Responses codec failure onto the orchestrator's error type.
fn response_error(error: ProviderError) -> ModelError {
    match error {
        ProviderError::Cancellation => ModelError::Cancelled,
        other => ModelError::Provider(other),
    }
}

/// Builds the wire request for one normalized request.
///
/// Tool definitions and tool-call control are validated before any body
/// exists, so a duplicate name or an unknown forced tool is a local
/// configuration error rather than a billable provider failure.
fn wire_request(
    profile: &EndpointProfile,
    request: &ModelRequest,
) -> Result<ChatRequest, ModelError> {
    ee_agent_orchestrator::validate_tool_names(&request.tools).map_err(ModelError::Adapter)?;
    request.tool_control.validate(&request.tools).map_err(ModelError::Adapter)?;
    let guarded = ee_agent_orchestrator::guard_transcript(
        profile.modalities,
        &profile.model,
        &request.transcript,
    );
    let transcript = guarded.as_deref().unwrap_or(&request.transcript);
    let generation = merged_generation(profile, request)?;
    let (messages, tools) = chat_request_parts(&ChatRequestParts {
        system_prompt: &profile.system_prompt,
        transcript,
        definitions: &request.tools,
        cache: &profile.cache,
        model: &profile.model,
        provider: Some(profile.label.as_str()),
        reasoning_field: profile.reasoning_field.as_deref(),
        tools_strict: profile.tools_strict,
        freeform: &profile.freeform,
        prompt_tokens: request.prompt_tokens,
    });
    Ok(ChatRequest {
        messages,
        tools,
        tool_control: request.tool_control.clone(),
        generation: (!generation.is_empty()).then_some(generation),
        prompt_tokens: request.prompt_tokens,
        session: request.session.clone(),
    })
}

/// Merges the request's sampling knobs over the profile defaults and validates
/// the result against the model's declared capabilities.
///
/// Validation happens before any body exists, so an unsupported knob is a local
/// configuration error naming the model and the knob, never a billable provider
/// rejection.
fn merged_generation(
    profile: &EndpointProfile,
    request: &ModelRequest,
) -> Result<ee_agent_orchestrator::GenerationOptions, ModelError> {
    let generation = request.generation.clone().map_or_else(
        || profile.generation.clone(),
        |request| request.merged_over(&profile.generation),
    );
    generation.validate().map_err(ModelError::Adapter)?;
    if let Some(knob) = generation.unsupported(profile.sampling) {
        return Err(ModelError::Adapter(format!(
            "model {:?} does not document the {knob} sampling knob",
            profile.model
        )));
    }
    Ok(generation)
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
    use serde_json::{Value, json};

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
    async fn undeclared_media_becomes_a_placeholder_before_the_client_runs() {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let completion: Arc<CompletionClient> = Arc::new(move |_request: ChatRequest| {
            let recorder = recorder.clone();
            Box::pin(async move {
                recorder.lock().expect("seen").push(json!({
                    "messages": _request.messages,
                }));
                Ok(AssistantTurn {
                    content: String::new(),
                    reasoning: String::new(),
                    raw: Value::Null,
                    native_reasoning: None,
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: None,
                    response_id: None,
                })
            })
        });
        let mut profile = profile();
        profile.modalities =
            ee_agent_orchestrator::Modalities { input: &["text", "image/png"], output: &["text"] };
        let adapter = ChatCompletionsAdapter::with_completion(profile, token(), completion);
        let mut request = request();
        request.transcript = vec![
            ee_agent_orchestrator::ModelMessage::new(ee_agent_orchestrator::ModelRole::User)
                .with_content(vec![ee_agent_orchestrator::ModelContent::Image {
                    data: String::from("AAAA"),
                    mime_type: String::from("image/webp"),
                    uri: None,
                }]),
        ];
        let (_cancel_tx, cancel) = watch::channel(false);

        adapter.complete(request, cancel).await.expect("placeholder request is sent");

        let seen = seen.lock().expect("seen");
        let text = seen[0]["messages"].to_string();
        assert!(text.contains("image/webp"), "{text}");
        assert!(text.contains("omitted"), "{text}");
    }

    #[tokio::test]
    async fn injected_completion_receives_normalized_messages_and_tools() {
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let completion: Arc<CompletionClient> = Arc::new(move |request: ChatRequest| {
            let recorder = recorder.clone();
            Box::pin(async move {
                recorder.lock().expect("seen").push(json!({
                    "messages": request.messages,
                    "tools": request.tools,
                }));
                Ok(AssistantTurn {
                    content: String::from("answer"),
                    reasoning: String::from("thinking"),
                    raw: json!({ "role": "assistant", "content": "answer" }),
                    native_reasoning: None,
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: Some(crate::wire::Usage {
                        input_tokens: Some(7),
                        output_tokens: Some(3),
                        total_tokens: None,
                        ..Default::default()
                    }),
                    response_id: None,
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
        let completion: Arc<CompletionClient> = Arc::new(move |_request: ChatRequest| {
            let counter = counter.clone();
            Box::pin(async move {
                *counter.lock().expect("calls") += 1;
                Ok(AssistantTurn {
                    content: String::new(),
                    reasoning: String::new(),
                    raw: json!({ "role": "assistant" }),
                    native_reasoning: None,
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: None,
                    response_id: None,
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
            Arc::new(|_request: ChatRequest| Box::pin(async { panic!("must not be called") }));
        let adapter = ChatCompletionsAdapter::with_completion(profile(), token(), completion);
        let (_cancel_tx, cancel) = watch::channel(true);

        let error = adapter.complete(request(), cancel).await.expect_err("cancelled");

        assert!(matches!(error, ModelError::Cancelled));
    }

    #[tokio::test]
    async fn streaming_without_a_streaming_client_emits_the_finished_answer_once() {
        let completion: Arc<CompletionClient> = Arc::new(|_request: ChatRequest| {
            Box::pin(async {
                Ok(AssistantTurn {
                    content: String::from("answer"),
                    reasoning: String::from("thinking"),
                    raw: json!({ "role": "assistant", "content": "answer" }),
                    native_reasoning: None,
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: None,
                    response_id: None,
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

    #[tokio::test]
    async fn structured_output_requires_catalog_support() {
        let completion: Arc<CompletionClient> = Arc::new(|_request: ChatRequest| {
            Box::pin(async { panic!("the completion client must not run") })
        });
        let adapter = ChatCompletionsAdapter::with_completion(profile(), token(), completion);
        let mut request = request();
        request.response_format = Some(ee_agent_orchestrator::ResponseFormat::JsonObject);
        let (_cancel_tx, cancel) = watch::channel(false);

        let error = adapter.complete(request, cancel).await.expect_err("gate rejects");
        let text = error.to_string();
        assert!(text.contains("structured output"), "{text}");
        assert!(text.contains("example-model"), "{text}");
    }

    #[tokio::test]
    async fn structured_output_reaches_a_declaring_route() {
        let seen = Arc::new(Mutex::new(0usize));
        let recorder = seen.clone();
        let completion: Arc<CompletionClient> = Arc::new(move |_request: ChatRequest| {
            let recorder = recorder.clone();
            Box::pin(async move {
                *recorder.lock().expect("seen") += 1;
                Ok(AssistantTurn {
                    content: String::from("{}"),
                    reasoning: String::new(),
                    raw: Value::Null,
                    native_reasoning: None,
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: None,
                    response_id: None,
                })
            })
        });
        let adapter = ChatCompletionsAdapter::with_completion(
            profile().with_structured_output(true),
            token(),
            completion,
        );
        let mut request = request();
        request.response_format = Some(ee_agent_orchestrator::ResponseFormat::JsonObject);
        let (_cancel_tx, cancel) = watch::channel(false);

        adapter.complete(request, cancel).await.expect("declared route runs");
        assert_eq!(*seen.lock().expect("seen"), 1, "the client ran exactly once");
    }

    #[tokio::test]
    async fn tool_choice_outside_the_catalog_list_is_rejected_before_sending() {
        let completion: Arc<CompletionClient> = Arc::new(|_request: ChatRequest| {
            Box::pin(async { panic!("the completion client must not run") })
        });
        let adapter = ChatCompletionsAdapter::with_completion(
            profile().with_tool_choices(&["auto", "none"]),
            token(),
            completion,
        );
        let mut request = request();
        request.tool_control = ee_agent_orchestrator::ToolCallControl::named("read_file");
        let (_cancel_tx, cancel) = watch::channel(false);

        let error = adapter.complete(request, cancel).await.expect_err("gate rejects");
        let text = error.to_string();
        assert!(text.contains("tool_choice `tool`"), "{text}");
        assert!(text.contains("example-model"), "{text}");
        assert!(text.contains("auto, none"), "{text}");
    }

    #[tokio::test]
    async fn a_declared_tool_choice_reaches_the_route() {
        let seen = Arc::new(Mutex::new(0usize));
        let recorder = seen.clone();
        let completion: Arc<CompletionClient> = Arc::new(move |_request: ChatRequest| {
            let recorder = recorder.clone();
            Box::pin(async move {
                *recorder.lock().expect("seen") += 1;
                Ok(AssistantTurn {
                    content: String::from("ok"),
                    reasoning: String::new(),
                    raw: Value::Null,
                    native_reasoning: None,
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: None,
                    response_id: None,
                })
            })
        });
        let adapter = ChatCompletionsAdapter::with_completion(
            profile().with_tool_choices(&["auto", "tool"]),
            token(),
            completion,
        );
        let mut request = request();
        request.tool_control = ee_agent_orchestrator::ToolCallControl::named("read_file");
        let (_cancel_tx, cancel) = watch::channel(false);

        adapter.complete(request, cancel).await.expect("declared choice runs");
        assert_eq!(*seen.lock().expect("seen"), 1);
    }

    #[tokio::test]
    async fn an_empty_tool_choice_list_keeps_the_protocol_default() {
        let seen = Arc::new(Mutex::new(0usize));
        let recorder = seen.clone();
        let completion: Arc<CompletionClient> = Arc::new(move |_request: ChatRequest| {
            let recorder = recorder.clone();
            Box::pin(async move {
                *recorder.lock().expect("seen") += 1;
                Ok(AssistantTurn {
                    content: String::from("ok"),
                    reasoning: String::new(),
                    raw: Value::Null,
                    native_reasoning: None,
                    tool_calls: Vec::new(),
                    finish_reason: Some(String::from("stop")),
                    usage: None,
                    response_id: None,
                })
            })
        });
        let adapter = ChatCompletionsAdapter::with_completion(profile(), token(), completion);
        let mut request = request();
        request.tool_control = ee_agent_orchestrator::ToolCallControl::named("read_file");
        let (_cancel_tx, cancel) = watch::channel(false);

        adapter.complete(request, cancel).await.expect("undeclared route runs");
        assert_eq!(*seen.lock().expect("seen"), 1);
    }
}
