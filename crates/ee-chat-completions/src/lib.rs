//! Provider-neutral OpenAI-compatible Chat Completions transport and codec.
//!
//! This crate owns every part of an HTTP model round trip that is a property of
//! the protocol rather than of a provider:
//!
//! - [`wire`] builds Chat Completions request bodies, converts normalized
//!   orchestrator transcripts and tool definitions, decodes buffered and streamed
//!   responses, and classifies HTTP failures into retry decisions. Its
//!   [`AssistantTurn`] and normalization helpers are shared by every dialect.
//! - [`endpoint`] refuses to hand a bearer credential to an origin that is not
//!   HTTPS (loopback `http` excepted for local proxies).
//! - [`transport`] attaches the credential, sends the request, reads bounded
//!   bodies, drives SSE framing, and applies the bounded retry policy that never
//!   retries after the first streamed delta reached the caller.
//! - [`adapter`] exposes Chat Completions as an `ee-agent-orchestrator`
//!   [`ModelAdapter`].
//!
//! [`EndpointProfile`] carries the parts a provider must supply — its display
//! label, credential variable name, endpoint, model, system prompt, timeout,
//! retry policy, extension headers, and extension body fields — so
//! provider-specific attribution headers, request shaping, environment names,
//! and error wording stay with the provider instead of leaking in here.
//!
//! Dialects are deliberately not merged: OpenAI Responses and Anthropic Messages
//! have their own codecs in their consuming crates, built on the same transport,
//! SSE framing, and retry boundary, and a request encoded for one dialect is
//! never replayed through another.
//!
//! [`ModelAdapter`]: ee_agent_orchestrator::ModelAdapter

pub mod adapter;
pub mod endpoint;
pub mod profile;
pub mod transport;
pub mod wire;

pub use adapter::{
    ChatCompletionsAdapter, CompletionClient, CompletionFuture, StreamingClient, wait_cancelled,
};
pub use endpoint::{EndpointError, TrustedEndpoint};
pub use profile::{BearerToken, EndpointProfile, RetryPolicy, TokenSource};
pub use transport::{
    ChatCompletionsClient, ChunkFuture, ChunkSource, DeltaSink, EndpointRequest, EndpointTransport,
    MAX_JSON_BODY_BYTES, SseControl, StreamAttempt, StreamFuture, drive_sse, is_event_stream,
    with_buffered_retry, with_stream_retry,
};
pub use wire::{
    AssistantTurn, StreamDelta, ToolCall, Usage, classify_http_error, classify_status_error,
    content_text, decode_message, emit_finished_deltas, http_error_message, is_retryable,
    messages_from_transcript, parse_retry_after, request_body, response_from_turn, retry_after_of,
    retry_delay, tool_call_id_of, tools_from_definitions, usage_from_tokens,
};
