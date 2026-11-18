//! Chat Completions round trips: body assembly, buffered and streaming calls.

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::ToolCallControl;
use serde_json::Value;

use crate::profile::{BearerToken, EndpointProfile, TokenSource};
use crate::stream_meta::is_event_stream;
use crate::wire::{
    AssistantTurn, StreamAccumulator, StreamDelta, decode_message, emit_finished_deltas,
    request_body, response_from_turn,
};

use super::{
    DeltaSink, EndpointRequest, EndpointTransport, SseControl, StreamAttempt, StreamFuture,
    drive_sse_with_chunk_timeout, with_buffered_retry, with_stream_retry,
};

/// One buffered round trip's wire inputs built from a normalized
/// [`ModelRequest`], so tool-call control travels with the request.
///
/// [`ModelRequest`]: ee_agent_orchestrator::ModelRequest
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    /// Assembled chat messages.
    pub messages: Vec<Value>,
    /// Assembled tool schemas.
    pub tools: Vec<Value>,
    /// Tool-choice and parallel-call control.
    pub tool_control: ToolCallControl,
    /// Sampling knobs for this round trip; `None` keeps the profile default.
    pub generation: Option<ee_agent_orchestrator::GenerationOptions>,
    /// Prompt size measured by the provider on the previous call, when known.
    pub prompt_tokens: Option<u64>,
    /// Session affinity headers for this round trip; `None` emits none.
    pub session: Option<ee_agent_orchestrator::SessionContext>,
}

/// One Chat Completions endpoint: profile, bearer-token source, HTTP client.
#[derive(Clone)]
pub struct ChatCompletionsClient {
    transport: EndpointTransport,
}

impl ChatCompletionsClient {
    /// Builds a client with an HTTP timeout taken from the profile.
    ///
    /// # Errors
    ///
    /// Returns the HTTP client construction failure as a diagnostic string.
    pub fn new(profile: EndpointProfile, token: TokenSource) -> Result<Self, String> {
        Ok(Self { transport: EndpointTransport::new(profile, token)? })
    }

    /// Builds a client over an existing HTTP client (shared pools, tests).
    #[must_use]
    pub fn with_http(profile: EndpointProfile, token: TokenSource, http: reqwest::Client) -> Self {
        Self { transport: EndpointTransport::with_http(profile, token, http) }
    }

    /// Wraps an already authorized transport, so a consumer that built the
    /// transport itself does not rebuild the profile.
    #[must_use]
    pub fn from_transport(transport: EndpointTransport) -> Self {
        Self { transport }
    }

    /// Returns the transport this client sends through.
    #[must_use]
    pub fn transport(&self) -> &EndpointTransport {
        &self.transport
    }

    /// Returns the provider profile this client was built for.
    #[must_use]
    pub fn profile(&self) -> &EndpointProfile {
        self.transport.profile()
    }

    /// Resolves the bearer token through the provider's token source.
    ///
    /// # Errors
    ///
    /// Returns the provider-worded message when no token is available. The
    /// message never contains a token value.
    pub fn bearer_token(&self) -> Result<BearerToken, String> {
        self.transport.bearer_token()
    }

    /// Builds the body for one wire request, including the request-level cache
    /// lifetime on routes that place no inline markers.
    fn body_for(&self, request: &ChatRequest, stream: bool) -> Value {
        let profile = self.transport.profile();
        let prompt = request.prompt_tokens.unwrap_or_else(|| {
            ee_agent_orchestrator::estimate_json_prompt_tokens(
                &Value::Array(request.messages.clone()),
                &Value::Array(request.tools.clone()),
            )
        });
        let generation = request
            .generation
            .clone()
            .map_or(profile.generation.clone(), |request| request.merged_over(&profile.generation));
        let mut body = request_body(
            &profile.model,
            &request.messages,
            &request.tools,
            crate::wire::RequestBodyOptions {
                stream,
                stream_usage: profile.stream_usage,
                max_tokens: profile.fitted_output_limit_with(prompt, generation.max_output_tokens),
                max_tokens_field: profile.max_tokens_field,
                tool_control: Some(&request.tool_control),
                generation: (!generation.is_empty()).then_some(&generation),
                extensions: profile.extensions.as_ref(),
            },
        );
        if let Some(Value::Object(ttl)) = crate::cache_policy::openai_request_ttl(&profile.cache)
            && let Some(object) = body.as_object_mut()
        {
            // Provider extension fields win: only fill a lifetime the profile
            // did not already set.
            for (key, value) in ttl {
                object.entry(key).or_insert(value);
            }
        }
        body
    }

    /// Performs one buffered round trip without retrying.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ProviderError`] for transport, status, or shape
    /// failures.
    pub async fn complete_once(
        &self,
        messages: &[Value],
        tools: &[Value],
    ) -> Result<AssistantTurn, ProviderError> {
        self.complete_once_with_prompt(messages, tools, None).await
    }

    /// Performs one buffered round trip without retrying, using the
    /// provider-reported prompt size when the caller has one.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ProviderError`] for transport, status, or shape
    /// failures.
    pub async fn complete_once_with_prompt(
        &self,
        messages: &[Value],
        tools: &[Value],
        prompt_tokens: Option<u64>,
    ) -> Result<AssistantTurn, ProviderError> {
        self.complete_request_once(&ChatRequest {
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            tool_control: ToolCallControl::auto(),
            generation: None,
            prompt_tokens,
            session: None,
        })
        .await
    }

    /// Performs one buffered round trip without retrying.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ProviderError`] for transport, status, or shape
    /// failures.
    pub async fn complete_request_once(
        &self,
        request: &ChatRequest,
    ) -> Result<AssistantTurn, ProviderError> {
        let transport = self.transport.with_session(request.session.as_ref())?;
        let body = self.body_for(request, false);
        let mut response = EndpointRequest::new(&transport, &body).send().await?;
        let value = transport.read_json(&mut response, "response").await?;
        decode_message(&value).ok_or_else(|| {
            ProviderError::BackendFailure(format!(
                "{} response did not include choices[0].message: {value}",
                self.transport.profile().label
            ))
        })
    }

    /// Performs one buffered round trip under the profile's retry policy.
    ///
    /// # Errors
    ///
    /// Returns the last typed failure once the retry budget is exhausted.
    pub async fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
    ) -> Result<AssistantTurn, ProviderError> {
        self.complete_with_prompt(messages, tools, None).await
    }

    /// Performs one buffered round trip under the profile's retry policy, using
    /// the provider-reported prompt size when the caller has one.
    ///
    /// # Errors
    ///
    /// Returns the last typed failure once the retry budget is exhausted.
    pub async fn complete_with_prompt(
        &self,
        messages: &[Value],
        tools: &[Value],
        prompt_tokens: Option<u64>,
    ) -> Result<AssistantTurn, ProviderError> {
        self.complete_request(&ChatRequest {
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            tool_control: ToolCallControl::auto(),
            generation: None,
            prompt_tokens,
            session: None,
        })
        .await
    }

    /// Performs one buffered round trip under the profile's retry policy.
    ///
    /// # Errors
    ///
    /// Returns the last typed failure once the retry budget is exhausted.
    pub async fn complete_request(
        &self,
        request: &ChatRequest,
    ) -> Result<AssistantTurn, ProviderError> {
        with_buffered_retry(&self.transport.profile().retry, || self.complete_request_once(request))
            .await
    }

    /// Performs one streaming round trip without retrying.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ProviderError`] for transport, status, or decode
    /// failures.
    pub async fn complete_streaming_once(
        &self,
        messages: &[Value],
        tools: &[Value],
        sink: DeltaSink<'_>,
    ) -> Result<AssistantTurn, ProviderError> {
        self.complete_streaming_once_with_prompt(messages, tools, sink, None).await
    }

    /// Performs one streaming round trip without retrying, using the
    /// provider-reported prompt size when the caller has one.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ProviderError`] for transport, status, or decode
    /// failures.
    pub async fn complete_streaming_once_with_prompt(
        &self,
        messages: &[Value],
        tools: &[Value],
        sink: DeltaSink<'_>,
        prompt_tokens: Option<u64>,
    ) -> Result<AssistantTurn, ProviderError> {
        self.complete_streaming_request_once(
            &ChatRequest {
                messages: messages.to_vec(),
                tools: tools.to_vec(),
                tool_control: ToolCallControl::auto(),
                generation: None,
                prompt_tokens,
                session: None,
            },
            sink,
        )
        .await
    }

    /// Performs one streaming round trip without retrying.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ProviderError`] for transport, status, or decode
    /// failures.
    pub async fn complete_streaming_request_once(
        &self,
        request: &ChatRequest,
        sink: DeltaSink<'_>,
    ) -> Result<AssistantTurn, ProviderError> {
        let mut attempt = HttpStreamAttempt { client: self, request };
        attempt.run(sink).await
    }

    /// Performs one streaming round trip under the profile's retry policy.
    ///
    /// A retry happens only when no delta reached `sink` yet.
    ///
    /// # Errors
    ///
    /// Returns the last typed failure once the retry budget is exhausted.
    pub async fn complete_streaming(
        &self,
        messages: &[Value],
        tools: &[Value],
        sink: DeltaSink<'_>,
    ) -> Result<AssistantTurn, ProviderError> {
        self.complete_streaming_with_prompt(messages, tools, sink, None).await
    }

    /// Performs one streaming round trip under the profile's retry policy, using
    /// the provider-reported prompt size when the caller has one.
    ///
    /// A retry happens only when no delta reached `sink` yet.
    ///
    /// # Errors
    ///
    /// Returns the last typed failure once the retry budget is exhausted.
    pub async fn complete_streaming_with_prompt(
        &self,
        messages: &[Value],
        tools: &[Value],
        sink: DeltaSink<'_>,
        prompt_tokens: Option<u64>,
    ) -> Result<AssistantTurn, ProviderError> {
        self.complete_streaming_request(
            &ChatRequest {
                messages: messages.to_vec(),
                tools: tools.to_vec(),
                tool_control: ToolCallControl::auto(),
                generation: None,
                prompt_tokens,
                session: None,
            },
            sink,
        )
        .await
    }

    /// Performs one streaming round trip under the profile's retry policy.
    ///
    /// A retry happens only when no delta reached `sink` yet.
    ///
    /// # Errors
    ///
    /// Returns the last typed failure once the retry budget is exhausted.
    pub async fn complete_streaming_request(
        &self,
        request: &ChatRequest,
        sink: DeltaSink<'_>,
    ) -> Result<AssistantTurn, ProviderError> {
        let mut attempt = HttpStreamAttempt { client: self, request };
        with_stream_retry(&self.transport.profile().retry, sink, &mut attempt).await
    }

    /// Streams one response, decoding SSE events as they arrive.
    async fn send_streaming(
        &self,
        request: &ChatRequest,
        sink: DeltaSink<'_>,
    ) -> Result<AssistantTurn, ProviderError> {
        let profile = self.transport.profile();
        let label = profile.label.as_str();
        let transport = self.transport.with_session(request.session.as_ref())?;
        let body = self.body_for(request, true);
        let mut response = EndpointRequest::new(&transport, &body).send().await?;
        let headers = response.headers().clone();
        if !is_event_stream(&headers) {
            let value = transport.read_json(&mut response, "streaming response").await?;
            let answer = decode_message(&value).ok_or_else(|| {
                ProviderError::BackendFailure(format!(
                    "{label} response did not include choices[0].message: {value}"
                ))
            })?;
            // A buffered answer still reaches the caller as deltas, so the
            // client sees the same event order either way.
            emit_finished_deltas(&response_from_turn(answer.clone()), sink)?;
            return Ok(answer);
        }
        let mut accumulator = StreamAccumulator::new(label);
        drive_sse_with_chunk_timeout(
            label,
            &mut response,
            Some(profile.timeouts.chunk),
            &mut |event| {
                if profile.raw_stream_events {
                    sink(StreamDelta::Raw { frame: event.to_string() })?;
                }
                let value: Value = serde_json::from_str(event).map_err(|error| {
                    ProviderError::BackendFailure(format!(
                        "{label} stream event was not JSON: {error}"
                    ))
                })?;
                for delta in accumulator.apply(&value)? {
                    sink(delta)?;
                }
                Ok(SseControl::Continue)
            },
        )
        .await?;
        accumulator.finish()
    }
}

/// One real streaming attempt for [`ChatCompletionsClient`].
struct HttpStreamAttempt<'a> {
    client: &'a ChatCompletionsClient,
    request: &'a ChatRequest,
}

impl StreamAttempt<AssistantTurn> for HttpStreamAttempt<'_> {
    fn run<'a>(&'a mut self, sink: DeltaSink<'a>) -> StreamFuture<'a, AssistantTurn> {
        Box::pin(self.client.send_streaming(self.request, sink))
    }
}
