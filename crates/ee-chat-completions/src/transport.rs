//! HTTP round trips: authorized endpoints, SSE driving, bounded bodies, retries.
//!
//! [`EndpointTransport`] is the one place a bearer credential is attached to a
//! request and the one place a response body is read, so every dialect gets the
//! same endpoint rules, timeouts, bounded bodies, and error labeling.
//! [`ChatCompletionsClient`] adds the Chat Completions body and decoding on top;
//! the OpenAI Responses and Anthropic Messages codecs build their own bodies on
//! the same transport and retry boundary.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use ee_agent_acp_server::{Delivery, ProviderError};
use ee_agent_orchestrator::ToolCallControl;
use reqwest::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;

use crate::profile::{BearerToken, EndpointProfile, RequestEncoding, RetryPolicy, TokenSource};
use crate::wire::{
    AssistantTurn, SseDecoder, StreamAccumulator, StreamDelta, classify_status_error,
    decode_message, emit_finished_deltas, is_retryable, is_timeout, is_unreachable, request_body,
    response_from_turn, retry_after_of, retry_delay, unreachable_delay,
};

/// Maximum bytes read from one JSON response body (success or error).
pub const MAX_JSON_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Boxed future returned by a streaming attempt.
pub type StreamFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send + 'a>>;

/// Boxed future returned by a chunk source.
pub type ChunkFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, String>> + Send + 'a>>;

/// Sink a streaming attempt forwards displayable deltas to.
pub type DeltaSink<'a> = &'a mut (dyn FnMut(StreamDelta) -> Result<(), ProviderError> + Send);

/// One streaming round trip producing `T`.
///
/// A dialect codec implements this to plug its own framing and decoding into the
/// shared retry boundary: the boundary decides whether the attempt may be
/// repeated, the attempt decides how the wire bytes become deltas.
pub trait StreamAttempt<T>: Send {
    /// Runs one attempt, forwarding displayable deltas in arrival order.
    fn run<'a>(&'a mut self, sink: DeltaSink<'a>) -> StreamFuture<'a, T>;
}

/// Bytes of a streaming response, in arrival order.
///
/// Implemented for [`reqwest::Response`]; tests implement it directly so SSE
/// handling stays hermetic.
pub trait ChunkSource: Send {
    /// Returns the next chunk, or `None` at the end of the body.
    ///
    /// The error text is a provider-neutral detail; the caller frames it with
    /// its own label.
    fn next_chunk(&mut self) -> ChunkFuture<'_>;
}

impl ChunkSource for reqwest::Response {
    fn next_chunk(&mut self) -> ChunkFuture<'_> {
        Box::pin(async move {
            self.chunk()
                .await
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                .map_err(|error| error.to_string())
        })
    }
}

/// Whether an SSE consumer wants more events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseControl {
    /// Keep reading.
    Continue,
    /// Stop reading; the caller already has what it needs.
    Stop,
}

/// Drives SSE framing over a chunk source, invoking `on_event` per data payload.
///
/// Framing is dialect independent: both OpenAI Responses and Anthropic Messages
/// put the event name in the JSON payload, so consumers dispatch on `type`.
/// `[DONE]` and [`SseControl::Stop`] end the stream without reading further bytes;
/// trailing bytes are framed only when the body ended on its own.
///
/// # Errors
///
/// Returns a typed error when the body cannot be read, an event is not UTF-8, or
/// the consumer rejects an event.
pub async fn drive_sse<F>(
    label: &str,
    source: &mut dyn ChunkSource,
    on_event: F,
) -> Result<(), ProviderError>
where
    F: FnMut(&str) -> Result<SseControl, ProviderError>,
{
    drive_sse_with_chunk_timeout(label, source, None, on_event).await
}

/// Drives SSE framing with an optional idle budget between chunks.
///
/// `chunk` is the maximum silence tolerated between body reads. A stream that
/// produces no bytes for longer than the budget fails with
/// `Timeout { phase: "chunk" }` (`code = "TimeoutChunk"`), which is distinct from
/// a whole-request timeout so retry policy can treat them differently.
///
/// # Errors
///
/// Returns a typed error when the body cannot be read, an event is not UTF-8,
/// the consumer rejects an event, or the idle budget expires.
#[allow(clippy::needless_pass_by_value)]
pub async fn drive_sse_with_chunk_timeout<F>(
    label: &str,
    source: &mut dyn ChunkSource,
    chunk: Option<Duration>,
    mut on_event: F,
) -> Result<(), ProviderError>
where
    F: FnMut(&str) -> Result<SseControl, ProviderError>,
{
    let mut decoder = SseDecoder::new(label);
    loop {
        let Some(chunk) = next_chunk(label, source, chunk).await? else {
            break;
        };
        for event in decoder.push(&chunk)? {
            if event == "[DONE]" || on_event(&event)? == SseControl::Stop {
                return Ok(());
            }
        }
    }
    for event in decoder.finish()? {
        if event == "[DONE]" || on_event(&event)? == SseControl::Stop {
            return Ok(());
        }
    }
    Ok(())
}

/// Reads the next body chunk, enforcing the idle budget when one is set.
async fn next_chunk(
    label: &str,
    source: &mut dyn ChunkSource,
    chunk: Option<Duration>,
) -> Result<Option<Vec<u8>>, ProviderError> {
    let read = match chunk {
        Some(limit) => match tokio::time::timeout(limit, source.next_chunk()).await {
            Ok(result) => result,
            Err(_) => return Err(chunk_timeout(label)),
        },
        None => source.next_chunk().await,
    };
    read.map_err(|detail| {
        ProviderError::BackendFailure(format!("{label} streaming response failed: {detail}"))
    })
}

/// Typed idle-chunk timeout.
#[must_use]
pub fn chunk_timeout(label: &str) -> ProviderError {
    ProviderError::Transport {
        delivery: Delivery::Ambiguous,
        code: Some(String::from("TimeoutChunk")),
        detail: format!("{label} stream produced no data within the chunk budget"),
    }
}

/// Whether a response body is an SSE stream.
#[must_use]
pub fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("text/event-stream"))
}

/// Appends `chunk` to `buffer`, failing closed beyond `limit` bytes.
pub(crate) fn extend_bounded(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
) -> Result<(), String> {
    if buffer.len().saturating_add(chunk.len()) > limit {
        return Err(format!("body exceeded the {limit}-byte limit"));
    }
    buffer.extend_from_slice(chunk);
    Ok(())
}

/// An authorized endpoint: the only place a bearer credential is attached.
///
/// The endpoint comes from the provider's validated profile, so no caller can
/// redirect a credential to an origin of its own choosing, and extension headers
/// are inserted before the credential so a provider cannot override it.
#[derive(Clone)]
pub struct EndpointTransport {
    pub(crate) profile: EndpointProfile,
    pub(crate) token: TokenSource,
    pub(crate) http: reqwest::Client,
}

impl EndpointTransport {
    /// Builds a transport with the profile's phase budgets.
    ///
    /// The whole-exchange budget (when configured) is applied by the HTTP client;
    /// the header and idle-chunk budgets are enforced per phase by
    /// [`EndpointTransport::send_json`] and the body readers.
    ///
    /// # Errors
    ///
    /// Returns the HTTP client construction failure as a diagnostic string.
    pub fn new(profile: EndpointProfile, token: TokenSource) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder();
        if let Some(total) = profile.timeouts.total {
            builder = builder.timeout(total);
        }
        let http =
            builder.build().map_err(|error| format!("failed to build HTTP client: {error}"))?;
        Ok(Self::with_http(profile, token, http))
    }

    /// Builds a transport over an existing HTTP client (shared pools, tests).
    #[must_use]
    pub fn with_http(profile: EndpointProfile, token: TokenSource, http: reqwest::Client) -> Self {
        Self { profile, token, http }
    }

    /// Returns the provider profile this transport was built for.
    #[must_use]
    pub fn profile(&self) -> &EndpointProfile {
        &self.profile
    }

    /// Returns the shared HTTP client.
    #[must_use]
    pub fn http_client(&self) -> &reqwest::Client {
        &self.http
    }

    /// Resolves the bearer token through the provider's token source.
    ///
    /// # Errors
    ///
    /// Returns the provider-worded message when no token is available. The
    /// message never contains a token value.
    pub fn bearer_token(&self) -> Result<BearerToken, String> {
        (self.token)()
    }

    /// Builds the request headers: provider extension headers first, then the
    /// content type and the bearer credential, which always win.
    ///
    /// # Errors
    ///
    /// Returns a typed error when no credential is available or its header value
    /// is invalid; the message never contains the token.
    pub fn headers(&self) -> Result<HeaderMap, ProviderError> {
        let token = (self.token)().map_err(ProviderError::BackendFailure)?;
        let mut headers = HeaderMap::new();
        for (name, value) in &self.profile.headers {
            headers.insert(name.clone(), value.clone());
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token.expose())).map_err(|error| {
                ProviderError::BackendFailure(format!(
                    "invalid {} header value: {error}",
                    self.profile.credential_var
                ))
            })?,
        );
        Ok(headers)
    }

    /// Sends one JSON request to the profile endpoint.
    ///
    /// Provider query parameters are percent-encoded exactly once, and the body
    /// is compressed only when the profile selects an encoding.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the credential is missing, a header is
    /// invalid, the body cannot be encoded, the header budget expires, or the
    /// request cannot be sent.
    pub async fn send_json(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let mut headers = self.headers()?;
        let url = self.endpoint_url()?;
        let label = self.profile.label.as_str();
        let payload = encode_body(body, &mut headers, self.profile.encoding, label)?;
        let request = self.http.post(url).headers(headers).body(payload);
        send_with_deadline(request.send(), self.profile.timeouts.headers, label).await
    }

    /// Endpoint URL with the profile's query parameters appended.
    fn endpoint_url(&self) -> Result<url::Url, ProviderError> {
        let label = self.profile.label.as_str();
        let mut url = url::Url::parse(self.profile.endpoint.as_str()).map_err(|error| {
            ProviderError::BackendFailure(format!("{label} endpoint is not a URL: {error}"))
        })?;
        if !self.profile.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in &self.profile.query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    /// Reads a response body as JSON, bounded by [`MAX_JSON_BODY_BYTES`].
    ///
    /// `context` names the body in diagnostics (`response`, `error response`).
    ///
    /// # Errors
    ///
    /// Returns a typed error when the body cannot be read, exceeds the bound, or
    /// is not JSON.
    pub async fn read_json(
        &self,
        response: &mut reqwest::Response,
        context: &str,
    ) -> Result<Value, ProviderError> {
        let label = self.profile.label.as_str();
        let mut buffer = Vec::new();
        while let Some(chunk) =
            next_chunk(label, response, Some(self.profile.timeouts.chunk)).await?
        {
            extend_bounded(&mut buffer, &chunk, MAX_JSON_BODY_BYTES).map_err(|detail| {
                ProviderError::BackendFailure(format!("{label} {context} {detail}"))
            })?;
        }
        serde_json::from_slice(&buffer).map_err(|error| {
            ProviderError::BackendFailure(format!("{label} {context} was not JSON: {error}"))
        })
    }

    /// Classifies a non-success status after its body was read.
    ///
    /// Never fails; returns the typed error for the status.
    #[must_use]
    pub fn status_error(
        &self,
        status: reqwest::StatusCode,
        headers: &HeaderMap,
        value: &Value,
    ) -> ProviderError {
        classify_status_error(self.profile.label.as_str(), status.as_u16(), headers, value)
    }
}

/// One JSON request/response exchange over an authorized endpoint.
///
/// The prepared body is borrowed, so a retry replays the same body without
/// cloning it.
pub struct EndpointRequest<'a> {
    transport: &'a EndpointTransport,
    body: &'a Value,
}

impl<'a> EndpointRequest<'a> {
    /// Prepares a request; nothing is sent until [`EndpointRequest::send`].
    #[must_use]
    pub fn new(transport: &'a EndpointTransport, body: &'a Value) -> Self {
        Self { transport, body }
    }

    /// Sends the request and returns the successful response.
    ///
    /// A non-success status becomes a typed error, after reading a bounded error
    /// body when the endpoint sent one.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the credential is missing, a header is
    /// invalid, the request cannot be sent, or the status is not successful.
    pub async fn send(&self) -> Result<reqwest::Response, ProviderError> {
        let mut response = self.transport.send_json(self.body).await?;
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let value = self.transport.read_json(&mut response, "error response").await?;
            return Err(self.transport.status_error(status, &headers, &value));
        }
        Ok(response)
    }
}

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
        drive_sse_with_chunk_timeout(label, &mut response, Some(profile.timeouts.chunk), |event| {
            if profile.raw_stream_events {
                sink(StreamDelta::Raw { frame: event.to_string() })?;
            }
            let value: Value = serde_json::from_str(event).map_err(|error| {
                ProviderError::BackendFailure(format!("{label} stream event was not JSON: {error}"))
            })?;
            for delta in accumulator.apply(&value)? {
                sink(delta)?;
            }
            Ok(SseControl::Continue)
        })
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

/// Maps one failed HTTP send onto a typed transport failure.
///
/// A connect failure never reached the provider; anything else may have, so the
/// delivery stays ambiguous and a billable retry is not attempted blindly.
fn transport_error(label: &str, error: &reqwest::Error) -> ProviderError {
    let code = if error.is_timeout() { Some(String::from("Timeout")) } else { io_code(error) };
    let delivery = if error.is_connect() { Delivery::NotSent } else { Delivery::Ambiguous };
    ProviderError::Transport { delivery, code, detail: format!("{label} request failed: {error}") }
}

/// Awaits one request phase under its own budget.
///
/// The header phase is bounded separately from the whole request so a provider
/// that accepts the connection but never answers is distinguishable from a
/// stream that stalls after the headers arrived.
async fn send_with_deadline<F, T>(
    future: F,
    limit: Duration,
    label: &str,
) -> Result<T, ProviderError>
where
    F: Future<Output = Result<T, reqwest::Error>>,
{
    match tokio::time::timeout(limit, future).await {
        Ok(result) => result.map_err(|error| transport_error(label, &error)),
        Err(_) => Err(ProviderError::Transport {
            delivery: Delivery::Ambiguous,
            code: Some(String::from("TimeoutHeaders")),
            detail: format!("{label} response headers did not arrive within the header budget"),
        }),
    }
}

/// Serializes one request body, compressing it only when configured.
///
/// Compression is off by default: an endpoint that does not decode the body
/// would reject the request, so the setting is explicit and validated at
/// startup.
fn encode_body(
    body: &Value,
    headers: &mut HeaderMap,
    encoding: RequestEncoding,
    label: &str,
) -> Result<Vec<u8>, ProviderError> {
    let plain = serde_json::to_vec(body).map_err(|error| {
        ProviderError::BackendFailure(format!("{label} request body is not JSON: {error}"))
    })?;
    let Some(encoding_name) = encoding.content_encoding() else {
        return Ok(plain);
    };
    let compressed = match encoding {
        RequestEncoding::None => plain,
        RequestEncoding::Gzip => {
            use std::io::Write as _;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&plain).and_then(|()| encoder.finish()).map_err(|error| {
                ProviderError::BackendFailure(format!("{label} request body gzip failed: {error}"))
            })?
        }
        RequestEncoding::Zstd => {
            zstd::stream::encode_all(plain.as_slice(), 3).map_err(|error| {
                ProviderError::BackendFailure(format!("{label} request body zstd failed: {error}"))
            })?
        }
    };
    headers.insert(CONTENT_ENCODING, HeaderValue::from_static(encoding_name));
    Ok(compressed)
}

/// Network error code of the underlying I/O failure, when one is available.
fn io_code(error: &reqwest::Error) -> Option<String> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            let code = match io.kind() {
                std::io::ErrorKind::NotFound => Some("ENOTFOUND"),
                std::io::ErrorKind::ConnectionRefused => Some("ECONNREFUSED"),
                std::io::ErrorKind::NetworkUnreachable => Some("ENETUNREACH"),
                std::io::ErrorKind::HostUnreachable => Some("EHOSTUNREACH"),
                std::io::ErrorKind::NetworkDown => Some("ENETDOWN"),
                std::io::ErrorKind::TimedOut => Some("Timeout"),
                _ => None,
            };
            if let Some(code) = code {
                return Some(code.to_string());
            }
        }
        source = current.source();
    }
    // A connect failure without an errno is a name-resolution failure, which is
    // this machine having no path to the provider.
    error.is_connect().then(|| String::from("ENOTFOUND"))
}

/// Runs a buffered attempt under a retry policy.
///
/// Only rate-limited and transient failures are retried, and only while the
/// attempt budget lasts; every other failure returns immediately.
///
/// # Errors
///
/// Returns the last failure when the budget is exhausted.
pub async fn with_buffered_retry<T, F, Fut>(
    policy: &RetryPolicy,
    mut attempt: F,
) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut retries = 0u32;
    let mut timeouts = 0u32;
    let mut unreachable_since: Option<Instant> = None;
    loop {
        match attempt().await {
            Err(error) if is_retryable(&error) => {
                let unreachable = is_unreachable(&error);
                if unreachable {
                    // Unreachable failures do not spend the general allowance;
                    // an outage usually outlasts it and every attempt fails
                    // before reaching the provider.
                    let since = *unreachable_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= policy.unreachable_window {
                        return Err(error);
                    }
                } else {
                    if retries >= policy.max_attempts {
                        return Err(error);
                    }
                    if is_timeout(&error) {
                        timeouts += 1;
                        if timeouts > policy.max_timeout_retries {
                            return Err(error);
                        }
                    }
                    retries += 1;
                }
                let delay = if unreachable {
                    unreachable_delay(policy, retries)
                } else {
                    retry_delay(policy, retries.saturating_sub(1), retry_after_of(&error))
                };
                tokio::time::sleep(delay).await;
            }
            other => return other,
        }
    }
}

/// Runs a streaming attempt under a retry policy.
///
/// A retry happens only when no delta reached `sink` yet: once streamed output
/// exists, repeating the request would duplicate it, so the failure surfaces
/// instead. This is the "no retry after first output" rule.
///
/// # Errors
///
/// Returns the last failure when the budget is exhausted.
pub async fn with_stream_retry<T>(
    policy: &RetryPolicy,
    sink: DeltaSink<'_>,
    attempt: &mut dyn StreamAttempt<T>,
) -> Result<T, ProviderError> {
    let mut retries = 0u32;
    let mut timeouts = 0u32;
    let mut unreachable_since: Option<Instant> = None;
    loop {
        let mut started = false;
        let result = {
            let mut forward = |delta: StreamDelta| {
                started = true;
                (*sink)(delta)
            };
            attempt.run(&mut forward).await
        };
        // Once streamed output exists, repeating the request would duplicate it,
        // so the failure surfaces instead.
        let error = match result {
            Ok(value) => return Ok(value),
            Err(error) if started => return Err(error),
            Err(error) => error,
        };
        let unreachable = is_unreachable(&error);
        if !is_retryable(&error) {
            return Err(error);
        }
        if unreachable {
            let since = *unreachable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= policy.unreachable_window {
                return Err(error);
            }
        } else {
            if retries >= policy.max_attempts {
                return Err(error);
            }
            if is_timeout(&error) {
                timeouts += 1;
                if timeouts > policy.max_timeout_retries {
                    return Err(error);
                }
            }
            retries += 1;
        }
        let delay = if unreachable {
            unreachable_delay(policy, retries)
        } else {
            retry_delay(policy, retries.saturating_sub(1), retry_after_of(&error))
        };
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod timeout_tests;
