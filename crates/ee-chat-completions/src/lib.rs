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
pub mod cache_policy;
pub mod config_file;
pub mod endpoint;
mod error_classification;
pub mod generation;
pub mod merge;
pub mod native;
pub mod partial_json;
pub mod profile;
pub mod provider_options;
mod reasoning_replay;
pub mod remote_instructions;
mod request;
pub mod responses;
pub mod responses_compact;
pub(crate) mod responses_encode;
mod responses_wire;
pub mod session;
pub(crate) mod stream_meta;
pub mod transport;
pub mod wire;

pub use adapter::{
    ChatCompletionsAdapter, CompletionClient, CompletionFuture, ResponsesAdapter, StreamingClient,
    wait_cancelled,
};
pub use cache_policy::{
    BREAKPOINT_CAP, CacheCandidate, CacheCandidateRole, CacheCandidates, CacheDirective,
    CacheFormat, CacheMessageStrategy, CachePlan, CachePolicy, CachePolicyObject, CacheSupport,
    CacheTargets, cache_control, openai_request_ttl, parse_policy, parse_ttl, plan, warn_dropped,
};
pub use config_file::{MAX_CONFIG_BYTES, interpolate, read_document};
pub use endpoint::{EndpointError, TrustedEndpoint};
pub use generation::{
    apply_generation_chat, apply_generation_messages, apply_generation_responses,
};
pub use merge::{MAX_MERGE_DEPTH, MAX_MERGE_NODES, deep_merge};
pub use native::{
    CHAT_REPLAY_FIELDS, NATIVE_META_KEY, PROVIDER_META_KEY, replayable, replayable_for, seal,
    seal_response, seal_response_with_provider, seal_with_provider, stamp_provider_executed,
};
pub use partial_json::{MAX_PARTIAL_JSON_BYTES, parse_partial};
pub use profile::{
    BearerToken, CallOptions, EndpointProfile, MaxTokensField, RequestEncoding, RetryPolicy,
    SessionHeaderStyle, Timeouts, TokenSource, ToolChoices, merged_extensions, tool_choice_error,
};
pub use provider_options::{
    MAX_METADATA_ENTRIES, MAX_PROVIDER_OPTIONS_BYTES, ProviderOptions, REASONING_SUMMARY_VALUES,
    TEXT_VERBOSITY_VALUES, TRUNCATION_VALUES,
};
pub use remote_instructions::{
    REMOTE_INSTRUCTION_MAX_BYTES, REMOTE_INSTRUCTION_MAX_REDIRECTS, REMOTE_INSTRUCTION_TIMEOUT,
    http_instruction_fetcher,
};
pub use responses_compact::{COMPACT_PATH_SUFFIX, CompactBody, compact_body, decode_compaction};
pub use session::{MAX_SESSION_HEADER_VALUE_BYTES, prompt_cache_key, session_headers};
pub use stream_meta::{chunk_timeout, is_event_stream};
pub use transport::{
    BackendFuture, CASSETTE_VERSION, Cassette, ChatCompletionsClient, ChatRequest, ChunkFuture,
    ChunkSource, DEFAULT_REPLAY_CHUNK_BYTES, DeltaSink, EndpointRequest, EndpointTransport,
    HttpBackend, HttpResponse, MAX_CASSETTE_BYTES, MAX_CASSETTE_EXCHANGES, MAX_JSON_BODY_BYTES,
    MAX_RECORDED_BODY_BYTES, MAX_RECORDED_HEADERS, PreparedRequest, REDACTED, RecordedExchange,
    RecordedHeader, RecordedRequest, RecordedResponse, Recorder, RecordingBackend, Replay,
    SseControl, StreamAttempt, StreamFuture, drive_sse, drive_sse_with_chunk_timeout,
    recording_backend, with_buffered_retry, with_stream_retry,
};
pub use wire::{
    AssistantTurn, ChatRequestParts, RequestBodyOptions, StreamDelta, ToolCall, Usage,
    chat_request_parts, classify_http_error, classify_status_error, content_has_media,
    content_parts, content_text, decode_message, emit_finished_deltas, http_error_message,
    is_retryable, messages_from_transcript, messages_from_transcript_replaying,
    messages_from_transcript_with_cache, model_usage_of, parse_arguments_checked,
    parse_retry_after, request_body, response_from_turn, retry_after_of, retry_delay,
    timeout_phase, tool_call_id_of, tool_choice_value, tools_from_definitions,
    tools_from_definitions_with, usage_from_tokens,
};
