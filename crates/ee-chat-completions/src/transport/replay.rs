//! Replay backend: serves recorded exchanges without a network.
//!
//! Replay is deterministic and fail-closed. A request is matched by method plus
//! exact body, preferring the recorded URL path when several exchanges share a
//! body. Repeated calls for the same request replay the recorded response again,
//! and an exchange sequence (for example a `429` followed by a `200`) is consumed
//! in recorded order, so retry behavior is reproducible.
//!
//! An unknown request is an error, never an empty success: a fixture that drifts
//! away from the code it exercises must fail the test that depends on it.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use ee_agent_acp_server::ProviderError;

use super::backend::{BackendFuture, DEFAULT_REPLAY_CHUNK_BYTES, HttpBackend, PreparedRequest};
use super::cassette::{CASSETTE_VERSION, Cassette, RecordedExchange, request_key, request_target};

/// Replay backend over one cassette.
pub struct Replay {
    exchanges: Vec<RecordedExchange>,
    chunk: usize,
    label: String,
    state: Mutex<ReplayState>,
}

#[derive(Default)]
struct ReplayState {
    /// Exchange indices per request key, in recorded order.
    queues: HashMap<String, VecDeque<usize>>,
    /// Last exchange served per request key, for repeat replay.
    last: HashMap<String, usize>,
}

impl Replay {
    /// Builds a replay backend with the default chunk size.
    #[must_use]
    pub fn new(cassette: Cassette) -> Self {
        Self::with_chunk_size(cassette, DEFAULT_REPLAY_CHUNK_BYTES)
    }

    /// Builds a replay backend that delivers bodies in `chunk`-sized reads.
    ///
    /// `chunk` is clamped to `1..=64 KiB`; a small chunk exercises SSE framing
    /// across split event boundaries.
    #[must_use]
    pub fn with_chunk_size(cassette: Cassette, chunk: usize) -> Self {
        let mut queues: HashMap<String, VecDeque<usize>> = HashMap::new();
        for (index, exchange) in cassette.exchanges.iter().enumerate() {
            queues.entry(exchange.request.key()).or_default().push_back(index);
        }
        Self {
            exchanges: cassette.exchanges,
            chunk: chunk.clamp(1, 64 * 1024),
            label: cassette.label,
            state: Mutex::new(ReplayState { queues, last: HashMap::new() }),
        }
    }

    /// Returns the exchanges this backend can replay, in recorded order.
    #[must_use]
    pub fn exchanges(&self) -> &[RecordedExchange] {
        &self.exchanges
    }

    /// Returns the cassette this backend was built from.
    #[must_use]
    pub fn cassette(&self) -> Cassette {
        Cassette {
            version: CASSETTE_VERSION,
            label: self.label.clone(),
            exchanges: self.exchanges.clone(),
        }
    }

    /// Selects one recorded exchange for `request`.
    fn take(&self, request: &PreparedRequest) -> Result<RecordedExchange, ProviderError> {
        let key = request_key(&request.method, &request.body);
        let target = request_target(&request.url);
        let mut state = self
            .state
            .lock()
            .map_err(|_| ProviderError::BackendFailure(String::from("replay state is poisoned")))?;
        let index = {
            let picked = state.queues.get_mut(&key).and_then(|queue| {
                // Prefer a same-route exchange; fall back to the oldest remaining
                // one so a rewritten origin still replays the same body.
                match queue.iter().position(|index| {
                    self.exchanges
                        .get(*index)
                        .is_some_and(|exchange| exchange.request.target() == target)
                }) {
                    Some(position) => queue.remove(position),
                    None => queue.pop_front(),
                }
            });
            match picked {
                Some(index) => index,
                None => match state.last.get(&key) {
                    Some(index) => *index,
                    None => return Err(self.missing(request, &target)),
                },
            }
        };
        state.last.insert(key, index);
        self.exchanges.get(index).cloned().ok_or_else(|| self.missing(request, &target))
    }

    /// Diagnostic for a request no exchange can satisfy.
    fn missing(&self, request: &PreparedRequest, target: &str) -> ProviderError {
        let mut available: Vec<String> = self
            .exchanges
            .iter()
            .take(8)
            .map(|exchange| format!("{} {}", exchange.request.method, exchange.request.target()))
            .collect();
        if self.exchanges.len() > available.len() {
            available.push(String::from("..."));
        }
        ProviderError::BackendFailure(format!(
            "no recorded exchange for {} {target} ({} request bytes); cassette has {} \
             exchange(s): {}",
            request.method,
            request.body.len(),
            self.exchanges.len(),
            available.join(", ")
        ))
    }
}

impl HttpBackend for Replay {
    fn send(&self, request: PreparedRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            let exchange = self.take(&request)?;
            exchange.response.to_http_response(self.chunk).map_err(ProviderError::BackendFailure)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::cassette::{RecordedHeader, RecordedRequest, RecordedResponse};
    use crate::transport::sse::ChunkSource;
    use reqwest::StatusCode;
    use reqwest::header::HeaderMap;

    fn exchange(body: &str, target: &str, status: u16, response: &str) -> RecordedExchange {
        RecordedExchange {
            request: RecordedRequest {
                method: String::from("POST"),
                url: format!("https://example.test{target}"),
                headers: Vec::new(),
                body: body.to_string(),
            },
            response: RecordedResponse {
                status,
                headers: vec![RecordedHeader {
                    name: String::from("content-type"),
                    value: String::from("application/json"),
                }],
                body: response.to_string(),
            },
        }
    }

    fn request(body: &str, target: &str) -> PreparedRequest {
        PreparedRequest {
            method: String::from("POST"),
            url: format!("https://replay.test{target}"),
            headers: HeaderMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn cassette(exchanges: Vec<RecordedExchange>) -> Cassette {
        Cassette { version: CASSETTE_VERSION, label: String::from("test"), exchanges }
    }

    #[tokio::test]
    async fn replays_the_recorded_status_headers_and_body() {
        let replay = Replay::new(cassette(vec![exchange(
            "{}",
            "/v1/chat/completions",
            200,
            "{\"ok\":true}",
        )]));
        let mut response =
            replay.send(request("{}", "/v1/chat/completions")).await.expect("replays");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("content-type").expect("type"), "application/json");
        assert_eq!(response.read_bounded(1024, None).await.expect("body"), b"{\"ok\":true}");
    }

    #[tokio::test]
    async fn repeated_requests_replay_deterministically() {
        let replay =
            Replay::new(cassette(vec![exchange("{}", "/v1/chat/completions", 200, "first")]));

        for _ in 0..3 {
            let mut response =
                replay.send(request("{}", "/v1/chat/completions")).await.expect("replays");
            assert_eq!(response.read_bounded(1024, None).await.expect("body"), b"first");
        }
    }

    #[tokio::test]
    async fn exchange_sequences_are_consumed_in_recorded_order() {
        let replay = Replay::new(cassette(vec![
            exchange("{}", "/v1/chat/completions", 429, "slow down"),
            exchange("{}", "/v1/chat/completions", 200, "ok"),
        ]));

        let mut first = replay.send(request("{}", "/v1/chat/completions")).await.expect("first");
        let mut second = replay.send(request("{}", "/v1/chat/completions")).await.expect("second");

        assert_eq!(first.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(first.read_bounded(1024, None).await.expect("body"), b"slow down");
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(second.read_bounded(1024, None).await.expect("body"), b"ok");
    }

    #[tokio::test]
    async fn unknown_requests_fail_closed_with_a_diagnostic() {
        let replay = Replay::new(cassette(vec![exchange(
            "{\"model\":\"a\"}",
            "/v1/chat/completions",
            200,
            "{}",
        )]));

        let error = replay
            .send(request("{\"model\":\"b\"}", "/v1/chat/completions"))
            .await
            .expect_err("no fixture");

        let detail = error.to_string();
        assert!(detail.contains("no recorded exchange"), "{detail}");
        assert!(detail.contains("POST /v1/chat/completions"), "{detail}");
    }

    #[tokio::test]
    async fn path_prefers_the_matching_route_for_a_shared_body() {
        let replay = Replay::new(cassette(vec![
            exchange("same", "/v1/chat/completions", 200, "chat"),
            exchange("same", "/v1/responses", 201, "responses"),
        ]));

        let mut responses =
            replay.send(request("same", "/v1/responses")).await.expect("replays responses");

        assert_eq!(responses.status(), StatusCode::CREATED);
        assert_eq!(responses.read_bounded(1024, None).await.expect("body"), b"responses");
    }

    #[tokio::test]
    async fn buffered_bodies_are_delivered_in_bounded_chunks() {
        let replay = Replay::with_chunk_size(
            cassette(vec![exchange("{}", "/v1/chat/completions", 200, "abcdefgh")]),
            3,
        );
        let mut response =
            replay.send(request("{}", "/v1/chat/completions")).await.expect("replays");

        let mut chunks = Vec::new();
        while let Some(chunk) = response.next_chunk().await.expect("chunk") {
            chunks.push(chunk);
        }

        assert_eq!(chunks, vec![b"abc".to_vec(), b"def".to_vec(), b"gh".to_vec()]);
    }
}
