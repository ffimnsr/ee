//! HTTP round trips: authorized endpoints, SSE driving, bounded bodies, retries.
//!
//! [`EndpointTransport`] is the one place a bearer credential is attached to a
//! request and the one place a response body is read, so every dialect gets the
//! same endpoint rules, timeouts, bounded bodies, and error labeling.
//! [`ChatCompletionsClient`] adds the Chat Completions body and decoding on top;
//! the OpenAI Responses and Anthropic Messages codecs build their own bodies on
//! the same transport and retry boundary.
//!
//! Layout: `sse` frames server-sent events, `endpoint` owns the authorized
//! exchange, `client` is the Chat Completions codec, `retry` is the shared
//! retry boundary, and the record/replay backends let a test run the same codec
//! against a recorded provider exchange instead of the network.

mod backend;
mod cassette;
mod client;
mod endpoint;
mod recording;
mod replay;
mod retry;
mod sse;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod timeout_tests;

use std::future::Future;
use std::pin::Pin;

use ee_agent_acp_server::ProviderError;

use crate::wire::StreamDelta;

pub use backend::{
    BackendFuture, DEFAULT_REPLAY_CHUNK_BYTES, HttpBackend, HttpResponse, PreparedRequest,
};
pub use cassette::{
    CASSETTE_VERSION, Cassette, MAX_CASSETTE_BYTES, MAX_CASSETTE_EXCHANGES,
    MAX_RECORDED_BODY_BYTES, MAX_RECORDED_HEADERS, REDACTED, RecordedExchange, RecordedHeader,
    RecordedRequest, RecordedResponse,
};
pub use client::{ChatCompletionsClient, ChatRequest};
pub use endpoint::{EndpointRequest, EndpointTransport, MAX_JSON_BODY_BYTES};
pub use recording::{Recorder, RecordingBackend, recording_backend};
pub use replay::Replay;
pub use retry::{with_buffered_retry, with_stream_retry};
pub use sse::{ChunkFuture, ChunkSource, SseControl, drive_sse, drive_sse_with_chunk_timeout};

/// Boxed future returned by a streaming attempt.
pub type StreamFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send + 'a>>;

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
