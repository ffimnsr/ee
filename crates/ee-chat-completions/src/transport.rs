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

use ee_acp_agent_server::ProviderError;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;

use crate::profile::{BearerToken, EndpointProfile, RetryPolicy, TokenSource};
use crate::wire::{
    AssistantTurn, SseDecoder, StreamAccumulator, StreamDelta, classify_status_error,
    decode_message, emit_finished_deltas, is_retryable, request_body, response_from_turn,
    retry_after_of, retry_delay,
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
    mut on_event: F,
) -> Result<(), ProviderError>
where
    F: FnMut(&str) -> Result<SseControl, ProviderError>,
{
    let mut decoder = SseDecoder::new(label);
    loop {
        let Some(chunk) = source.next_chunk().await.map_err(|detail| {
            ProviderError::BackendFailure(format!("{label} streaming response failed: {detail}"))
        })?
        else {
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
    profile: EndpointProfile,
    token: TokenSource,
    http: reqwest::Client,
}

impl EndpointTransport {
    /// Builds a transport with an HTTP timeout taken from the profile.
    ///
    /// # Errors
    ///
    /// Returns the HTTP client construction failure as a diagnostic string.
    pub fn new(profile: EndpointProfile, token: TokenSource) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(profile.timeout)
            .build()
            .map_err(|error| format!("failed to build HTTP client: {error}"))?;
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
    /// # Errors
    ///
    /// Returns a typed error when the credential is missing, a header is
    /// invalid, or the request cannot be sent.
    pub async fn send_json(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let headers = self.headers()?;
        self.http
            .post(self.profile.endpoint.as_str())
            .headers(headers)
            .json(body)
            .send()
            .await
            .map_err(|error| {
                ProviderError::BackendFailure(format!(
                    "{} request failed: {error}",
                    self.profile.label
                ))
            })
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
        while let Some(chunk) = response.next_chunk().await.map_err(|detail| {
            ProviderError::BackendFailure(format!("{label} streaming response failed: {detail}"))
        })? {
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
        let body = request_body(
            &self.transport.profile().model,
            messages,
            tools,
            false,
            self.transport.profile().extensions.as_ref(),
        );
        let mut response = EndpointRequest::new(&self.transport, &body).send().await?;
        let value = self.transport.read_json(&mut response, "response").await?;
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
        with_buffered_retry(&self.transport.profile().retry, || self.complete_once(messages, tools))
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
        let mut attempt = HttpStreamAttempt { client: self, messages, tools };
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
        let mut attempt = HttpStreamAttempt { client: self, messages, tools };
        with_stream_retry(&self.transport.profile().retry, sink, &mut attempt).await
    }

    /// Streams one response, decoding SSE events as they arrive.
    async fn send_streaming(
        &self,
        messages: &[Value],
        tools: &[Value],
        sink: DeltaSink<'_>,
    ) -> Result<AssistantTurn, ProviderError> {
        let profile = self.transport.profile();
        let label = profile.label.as_str();
        let body = request_body(&profile.model, messages, tools, true, profile.extensions.as_ref());
        let mut response = EndpointRequest::new(&self.transport, &body).send().await?;
        let headers = response.headers().clone();
        if !is_event_stream(&headers) {
            let value = self.transport.read_json(&mut response, "streaming response").await?;
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
        drive_sse(label, &mut response, |event| {
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
    messages: &'a [Value],
    tools: &'a [Value],
}

impl StreamAttempt<AssistantTurn> for HttpStreamAttempt<'_> {
    fn run<'a>(&'a mut self, sink: DeltaSink<'a>) -> StreamFuture<'a, AssistantTurn> {
        Box::pin(self.client.send_streaming(self.messages, self.tools, sink))
    }
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
    let mut last_error: Option<ProviderError> = None;
    for index in 0..=policy.max_attempts {
        match attempt().await {
            Err(error) if is_retryable(&error) && index < policy.max_attempts => {
                let hint = retry_after_of(&error);
                last_error = Some(error);
                tokio::time::sleep(retry_delay(policy, index, hint)).await;
            }
            other => return other,
        }
    }
    Err(last_error.expect("at least one attempt ran"))
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
    let mut last_error: Option<ProviderError> = None;
    for index in 0..=policy.max_attempts {
        let mut started = false;
        let result = {
            let mut forward = |delta: StreamDelta| {
                started = true;
                (*sink)(delta)
            };
            attempt.run(&mut forward).await
        };
        match result {
            Err(error) if !started && is_retryable(&error) && index < policy.max_attempts => {
                let hint = retry_after_of(&error);
                last_error = Some(error);
                tokio::time::sleep(retry_delay(policy, index, hint)).await;
            }
            other => return other,
        }
    }
    Err(last_error.expect("at least one attempt ran"))
}

#[cfg(test)]
mod tests;
