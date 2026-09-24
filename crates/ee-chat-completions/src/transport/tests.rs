use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use reqwest::header::HeaderName;
use serde_json::json;

use super::*;
use crate::endpoint::TrustedEndpoint;
use crate::wire::classify_http_error;

fn policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(50),
        max_delay: Duration::from_secs(5),
    }
}

fn profile() -> EndpointProfile {
    EndpointProfile::new(
        "Example",
        "EXAMPLE_API_KEY",
        TrustedEndpoint::parse("https://example.test/v1/chat/completions").expect("https"),
        "example-model",
    )
}

fn client(token: TokenSource) -> ChatCompletionsClient {
    ChatCompletionsClient::with_http(profile(), token, reqwest::Client::new())
}

fn static_token(token: &str) -> TokenSource {
    let token = token.to_string();
    Arc::new(move || Ok(BearerToken::new(token.clone())))
}

/// A streaming attempt that fails without emitting output for its first
/// `failures` runs, then streams a text delta and succeeds.
struct FailsBeforeOutput {
    runs: Arc<AtomicU32>,
    failures: u32,
}

impl StreamAttempt<AssistantTurn> for FailsBeforeOutput {
    fn run<'a>(&'a mut self, sink: DeltaSink<'a>) -> StreamFuture<'a, AssistantTurn> {
        Box::pin(async move {
            let index = self.runs.fetch_add(1, Ordering::SeqCst);
            if index < self.failures {
                return Err(transient("connection reset"));
            }
            sink(StreamDelta::Text(String::from("done")))?;
            Ok(turn("done"))
        })
    }
}

/// A streaming attempt that always streams one delta and then fails.
struct FailsAfterOutput {
    runs: Arc<AtomicU32>,
}

impl StreamAttempt<AssistantTurn> for FailsAfterOutput {
    fn run<'a>(&'a mut self, sink: DeltaSink<'a>) -> StreamFuture<'a, AssistantTurn> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            sink(StreamDelta::Text(String::from("partial")))?;
            Err(transient("connection reset"))
        })
    }
}

/// A chunk source replaying scripted chunks, optionally failing mid-stream.
struct ScriptedChunks {
    chunks: Vec<Vec<u8>>,
    failure: Option<String>,
}

impl ChunkSource for ScriptedChunks {
    fn next_chunk(&mut self) -> ChunkFuture<'_> {
        Box::pin(async move {
            if self.chunks.is_empty() {
                return match self.failure.take() {
                    Some(detail) => Err(detail),
                    None => Ok(None),
                };
            }
            Ok(Some(self.chunks.remove(0)))
        })
    }
}

fn transient(detail: &str) -> ProviderError {
    ProviderError::Transient { retry_after: None, detail: detail.to_string() }
}

fn turn(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: text.to_string(),
        reasoning: String::new(),
        raw: json!({ "role": "assistant", "content": text }),
        tool_calls: Vec::new(),
        finish_reason: Some(String::from("stop")),
        usage: None,
    }
}

fn clone_error(error: &ProviderError) -> ProviderError {
    match error {
        ProviderError::RateLimited { retry_after, detail } => {
            ProviderError::RateLimited { retry_after: *retry_after, detail: detail.clone() }
        }
        ProviderError::Transient { retry_after, detail } => {
            ProviderError::Transient { retry_after: *retry_after, detail: detail.clone() }
        }
        ProviderError::BackendFailure(detail) => ProviderError::BackendFailure(detail.clone()),
        ProviderError::InvalidRequest(detail) => ProviderError::InvalidRequest(detail.clone()),
        other => ProviderError::BackendFailure(other.to_string()),
    }
}

#[tokio::test(start_paused = true)]
async fn buffered_retry_never_retries_credential_and_policy_failures() {
    for status in [401, 403] {
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let result: Result<(), ProviderError> = with_buffered_retry(&policy(3), move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(classify_http_error(status, None, format!("HTTP {status}")))
            }
        })
        .await;

        assert!(matches!(result, Err(ProviderError::BackendFailure(_))));
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "status {status} must never retry");
    }
}

#[tokio::test(start_paused = true)]
async fn buffered_retry_repeats_rate_limited_and_transient_failures() {
    for error in [
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(1)),
            detail: "slow".into(),
        },
        ProviderError::Transient { retry_after: None, detail: "down".into() },
    ] {
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let result = with_buffered_retry(&policy(2), move || {
            let counter = counter.clone();
            let error = clone_error(&error);
            async move {
                let index = counter.fetch_add(1, Ordering::SeqCst);
                if index < 2 { Err(error) } else { Ok(index) }
            }
        })
        .await;

        assert_eq!(result.expect("third attempt succeeds"), 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }
}

#[tokio::test(start_paused = true)]
async fn buffered_retry_stops_at_the_attempt_budget() {
    let attempts = Arc::new(AtomicU32::new(0));
    let counter = attempts.clone();
    let result: Result<(), ProviderError> = with_buffered_retry(&policy(2), move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::Transient {
                retry_after: Some(Duration::from_secs(600)),
                detail: "still down".into(),
            })
        }
    })
    .await;

    assert!(matches!(result, Err(ProviderError::Transient { .. })));
    assert_eq!(attempts.load(Ordering::SeqCst), 3, "initial attempt plus two retries");
}

#[tokio::test(start_paused = true)]
async fn stream_retry_repeats_only_before_the_first_delta() {
    let runs = Arc::new(AtomicU32::new(0));
    let mut attempt = FailsBeforeOutput { runs: runs.clone(), failures: 1 };
    let mut deltas = Vec::new();
    let mut sink = |delta: StreamDelta| {
        deltas.push(delta);
        Ok(())
    };

    let answer = with_stream_retry(&policy(2), &mut sink, &mut attempt).await.expect("retried");

    assert_eq!(runs.load(Ordering::SeqCst), 2, "failed attempt retried once");
    assert_eq!(deltas, vec![StreamDelta::Text(String::from("done"))]);
    assert_eq!(answer.content, "done");
}

#[tokio::test(start_paused = true)]
async fn stream_retry_never_repeats_after_output_reached_the_sink() {
    let runs = Arc::new(AtomicU32::new(0));
    let mut attempt = FailsAfterOutput { runs: runs.clone() };
    let mut deltas = Vec::new();
    let mut sink = |delta: StreamDelta| {
        deltas.push(delta);
        Ok(())
    };

    let error = with_stream_retry(&policy(3), &mut sink, &mut attempt)
        .await
        .expect_err("no retry after output");

    assert!(matches!(error, ProviderError::Transient { .. }), "{error:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "output already reached the caller");
    assert_eq!(deltas, vec![StreamDelta::Text(String::from("partial"))]);
}

#[tokio::test(start_paused = true)]
async fn missing_token_reports_the_provider_worded_error() {
    let missing: TokenSource = Arc::new(|| {
        Err(String::from("OPENROUTER_API_KEY is not set; export it before starting ee"))
    });
    let client = client(missing);

    let error = client.transport.headers().expect_err("no credential");

    assert!(
        matches!(
            &error,
            ProviderError::BackendFailure(message)
                if message == "OPENROUTER_API_KEY is not set; export it before starting ee"
        ),
        "{error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn header_errors_never_echo_the_credential() {
    let secret = "sk-secret\nwith-break";
    let client = client(static_token(secret));

    let error = client.transport.headers().expect_err("invalid header value");

    assert!(error.to_string().contains("EXAMPLE_API_KEY"), "{error}");
    assert!(!error.to_string().contains("sk-secret"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn headers_carry_the_bearer_token_and_provider_extensions() {
    let profile = profile().with_headers(vec![(
        HeaderName::from_static("x-title"),
        HeaderValue::from_static("ee-test"),
    )]);
    let client = ChatCompletionsClient::with_http(
        profile,
        static_token("sk-secret"),
        reqwest::Client::new(),
    );

    let headers = client.transport.headers().expect("headers build");

    assert_eq!(headers.get(AUTHORIZATION).expect("authorization"), "Bearer sk-secret");
    assert_eq!(headers.get("x-title").expect("title"), "ee-test");
    assert_eq!(headers.get(CONTENT_TYPE).expect("content type"), "application/json");
}

#[tokio::test(start_paused = true)]
async fn bearer_token_is_resolved_through_the_token_source() {
    let client = client(static_token("sk-secret"));
    assert_eq!(client.bearer_token().expect("token").expose(), "sk-secret");
}

#[test]
fn bounded_bodies_fail_closed_past_the_limit() {
    let mut buffer = Vec::new();
    extend_bounded(&mut buffer, b"1234", 4).expect("exactly at the limit");
    assert_eq!(buffer, b"1234");
    let error = extend_bounded(&mut buffer, b"5", 4).expect_err("over the limit");
    assert_eq!(error, "body exceeded the 4-byte limit");
}

#[tokio::test]
async fn drive_sse_frames_events_and_stops_on_done() {
    let mut source = ScriptedChunks {
        chunks: vec![b"data: first\n\ndata: second\n\ndata: [DONE]\n\ndata: ignored\n\n".to_vec()],
        failure: None,
    };
    let mut events = Vec::new();

    drive_sse("Test", &mut source, |event| {
        events.push(event.to_string());
        Ok(SseControl::Continue)
    })
    .await
    .expect("drives");

    assert_eq!(events, vec![String::from("first"), String::from("second")]);
}

#[tokio::test]
async fn drive_sse_stops_when_the_consumer_asks() {
    let mut source =
        ScriptedChunks { chunks: vec![b"data: first\n\ndata: second\n\n".to_vec()], failure: None };
    let mut events = Vec::new();

    drive_sse("Test", &mut source, |event| {
        events.push(event.to_string());
        Ok(SseControl::Stop)
    })
    .await
    .expect("drives");

    assert_eq!(events, vec![String::from("first")]);
}

#[tokio::test]
async fn drive_sse_reports_body_failures_with_the_label() {
    let mut source = ScriptedChunks {
        chunks: vec![b"data: first\n\n".to_vec()],
        failure: Some(String::from("connection reset")),
    };

    let error = drive_sse("Test", &mut source, |_event| Ok(SseControl::Continue))
        .await
        .expect_err("body failed");

    assert_eq!(
        error.to_string(),
        "provider backend failure: Test streaming response failed: connection reset"
    );
}

#[tokio::test]
async fn drive_sse_flushes_trailing_events_without_a_terminator() {
    let mut source = ScriptedChunks { chunks: vec![b"data: tail".to_vec()], failure: None };
    let mut events = Vec::new();

    drive_sse("Test", &mut source, |event| {
        events.push(event.to_string());
        Ok(SseControl::Continue)
    })
    .await
    .expect("drives");

    assert_eq!(events, vec![String::from("tail")]);
}
