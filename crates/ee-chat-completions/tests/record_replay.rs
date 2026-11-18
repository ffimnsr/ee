//! Record/replay harness: provider exchanges replayed from committed cassettes.
//!
//! These tests exercise the public record/replay surface the way a provider
//! regression test would: a codec runs unchanged, but the transport is backed by
//! a cassette instead of the network. The Gemini fixtures are recorded in the
//! OpenAI-compatible shape that route returns, so a provider-side change to event
//! framing, usage reporting, or finish reasons shows up here before it reaches a
//! live session.

use std::sync::Arc;
use std::time::Duration;

use ee_chat_completions::{
    Cassette, ChatCompletionsClient, EndpointProfile, EndpointTransport, RecordedExchange, Replay,
    RequestBodyOptions, RetryPolicy, StreamDelta, TokenSource, TrustedEndpoint, request_body,
};
use serde_json::{Value, json};

const GEMINI_CHAT_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions";
const GEMINI_MODEL: &str = "gemini-2.5-flash";

fn token() -> TokenSource {
    Arc::new(|| Ok(ee_chat_completions::BearerToken::new("test-token")))
}

fn profile(endpoint: &str, model: &str) -> EndpointProfile {
    EndpointProfile::new(
        "Replay",
        "REPLAY_API_KEY",
        TrustedEndpoint::parse(endpoint).expect("fixture endpoint is usable"),
        model,
    )
    .with_retry(RetryPolicy::new(2, Duration::from_millis(1), Duration::from_millis(5)))
}

fn messages() -> Vec<Value> {
    vec![json!({ "role": "user", "content": "Reply with the single word: pong" })]
}

fn client_from(
    cassette: Cassette,
    endpoint: &str,
    model: &str,
    chunk: usize,
) -> ChatCompletionsClient {
    let replay = Replay::with_chunk_size(cassette, chunk);
    let transport =
        EndpointTransport::with_backend(profile(endpoint, model), token(), Arc::new(replay));
    ChatCompletionsClient::from_transport(transport)
}

fn first_exchange(cassette: &Cassette) -> &RecordedExchange {
    cassette.exchanges.first().expect("fixture carries an exchange")
}

#[tokio::test]
async fn streaming_exchange_replays_from_a_cassette() {
    let cassette =
        Cassette::from_json(include_str!("fixtures/gemini-chat-stream.json")).expect("valid");
    // A 7-byte chunk size forces SSE frames to be split across reads.
    let client = client_from(cassette, GEMINI_CHAT_URL, GEMINI_MODEL, 7);
    let mut deltas = Vec::new();
    let mut sink = |delta: StreamDelta| {
        deltas.push(delta);
        Ok(())
    };

    let turn = client.complete_streaming(&messages(), &[], &mut sink).await.expect("replays");

    assert_eq!(turn.content, "pong");
    assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
    assert!(
        deltas.iter().any(|delta| matches!(delta, StreamDelta::Text(text) if text == "pong")),
        "{deltas:?}"
    );
    let usage = turn.usage.expect("fixture reports usage");
    assert_eq!(usage.input_tokens, Some(9));
    assert_eq!(usage.output_tokens, Some(1));
    assert_eq!(usage.total_tokens, Some(10));
}

#[tokio::test]
async fn buffered_exchange_replays_from_a_cassette() {
    let cassette =
        Cassette::from_json(include_str!("fixtures/gemini-chat-buffered.json")).expect("valid");
    let client = client_from(cassette, GEMINI_CHAT_URL, GEMINI_MODEL, 512);

    let turn = client.complete(&messages(), &[]).await.expect("replays");

    assert_eq!(turn.content, "pong");
    assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
    assert_eq!(turn.usage.expect("usage").total_tokens, Some(10));
}

#[tokio::test]
async fn replayed_retry_sequence_reaches_success() {
    let cassette = Cassette::from_json(include_str!("fixtures/openrouter-retry-rate-limited.json"))
        .expect("valid");
    assert_eq!(cassette.exchanges.len(), 2, "fixture pins the retry sequence");
    let client = client_from(
        cassette,
        "https://openrouter.ai/api/v1/chat/completions",
        "example/model",
        512,
    );

    let turn = client.complete(&messages(), &[]).await.expect("retry replays the 200");

    assert_eq!(turn.content, "pong");
    assert_eq!(turn.usage.expect("usage").total_tokens, Some(5));
}

#[tokio::test]
async fn replay_is_deterministic_across_repeated_calls() {
    let cassette =
        Cassette::from_json(include_str!("fixtures/gemini-chat-buffered.json")).expect("valid");
    let client = client_from(cassette, GEMINI_CHAT_URL, GEMINI_MODEL, 512);

    let first = client.complete(&messages(), &[]).await.expect("first replay");
    let second = client.complete(&messages(), &[]).await.expect("second replay");

    assert_eq!(first, second);
}

#[tokio::test]
async fn unknown_request_fails_closed() {
    let cassette =
        Cassette::from_json(include_str!("fixtures/gemini-chat-buffered.json")).expect("valid");
    let client = client_from(cassette, GEMINI_CHAT_URL, GEMINI_MODEL, 512);
    let drift = vec![json!({ "role": "user", "content": "a different prompt" })];

    let error = client.complete(&drift, &[]).await.expect_err("no fixture for this body");

    let detail = error.to_string();
    assert!(detail.contains("no recorded exchange"), "{detail}");
    assert!(
        detail.contains("/v1beta/openai/chat/completions"),
        "the diagnostic names the route: {detail}"
    );
}

#[tokio::test]
async fn recording_round_trips_through_disk() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("nested/gemini-chat-stream.json");
    let cassette =
        Cassette::from_json(include_str!("fixtures/gemini-chat-stream.json")).expect("valid");

    cassette.save(&path).expect("saves cassette");
    let loaded = Cassette::load(&path).expect("loads cassette");

    assert_eq!(loaded, cassette);
    assert_eq!(loaded.label, "gemini-chat-stream");
}

#[test]
fn fixture_request_bodies_match_the_request_builder() {
    let streaming =
        Cassette::from_json(include_str!("fixtures/gemini-chat-stream.json")).expect("valid");
    let built = request_body(GEMINI_MODEL, &messages(), &[], RequestBodyOptions::streaming());
    assert_eq!(
        first_exchange(&streaming).request.body,
        serde_json::to_string(&built).expect("serializes"),
        "re-recording is only needed when the request builder changes"
    );

    let buffered =
        Cassette::from_json(include_str!("fixtures/gemini-chat-buffered.json")).expect("valid");
    let built = request_body(GEMINI_MODEL, &messages(), &[], RequestBodyOptions::buffered());
    assert_eq!(
        first_exchange(&buffered).request.body,
        serde_json::to_string(&built).expect("serializes")
    );
}

#[test]
fn fixtures_carry_no_credentials() {
    for text in [
        include_str!("fixtures/gemini-chat-stream.json"),
        include_str!("fixtures/gemini-chat-buffered.json"),
        include_str!("fixtures/openrouter-retry-rate-limited.json"),
    ] {
        assert!(!text.contains("Bearer "), "a fixture leaked an authorization value");
        assert!(!text.contains("sk-"), "a fixture leaked an API key");
    }
}
