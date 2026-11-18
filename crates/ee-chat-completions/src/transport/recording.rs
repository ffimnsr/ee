//! Recording backend: captures real exchanges into a reviewable cassette.
//!
//! Recording wraps any [`HttpBackend`], so a live transport is recorded without
//! changing the codec that drives it. The wrapper buffers each response body
//! (bounded by [`MAX_RECORDED_BODY_BYTES`]) because a cassette must hold the
//! whole exchange before it can be written; recording is a deliberate,
//! developer-initiated action, not a hot path.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ee_agent_acp_server::ProviderError;

use super::backend::{BackendFuture, HttpBackend, HttpResponse, PreparedRequest};
use super::cassette::{
    CASSETTE_VERSION, Cassette, MAX_RECORDED_BODY_BYTES, RecordedExchange, RecordedRequest,
    RecordedResponse,
};

/// Collected exchanges for one recording session.
///
/// The recorder is shared with the transport it instruments; call
/// [`Recorder::cassette`] once the run finishes to persist what was captured.
#[derive(Debug, Default)]
pub struct Recorder {
    exchanges: Mutex<Vec<RecordedExchange>>,
    label: String,
}

impl Recorder {
    /// Builds an empty recorder for `label`.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self { exchanges: Mutex::new(Vec::new()), label: label.into() }
    }

    /// Number of captured exchanges.
    #[must_use]
    pub fn len(&self) -> usize {
        self.exchanges.lock().map_or(0, |exchanges| exchanges.len())
    }

    /// Whether nothing was captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops every captured exchange.
    pub fn clear(&self) {
        if let Ok(mut exchanges) = self.exchanges.lock() {
            exchanges.clear();
        }
    }

    /// Returns a cassette holding a snapshot of the captured exchanges.
    #[must_use]
    pub fn cassette(&self) -> Cassette {
        let exchanges =
            self.exchanges.lock().map(|exchanges| exchanges.clone()).unwrap_or_default();
        Cassette { version: CASSETTE_VERSION, label: self.label.clone(), exchanges }
    }

    /// Records one exchange.
    ///
    /// A poisoned lock keeps the already-captured exchanges untouched and drops
    /// this one; recording must never fail a live provider call.
    fn push(&self, exchange: RecordedExchange) {
        if let Ok(mut exchanges) = self.exchanges.lock() {
            exchanges.push(exchange);
        }
    }
}

/// Backend wrapper that records every exchange passing through it.
pub struct RecordingBackend {
    inner: Arc<dyn HttpBackend>,
    recorder: Arc<Recorder>,
    chunk: Duration,
}

impl HttpBackend for RecordingBackend {
    fn send(&self, request: PreparedRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            let recorded_request = RecordedRequest::from_prepared(&request).map_err(|detail| {
                ProviderError::BackendFailure(format!("cannot record request: {detail}"))
            })?;
            let mut response = self.inner.send(request).await?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response
                .read_bounded(MAX_RECORDED_BODY_BYTES, Some(self.chunk))
                .await
                .map_err(|detail| {
                    ProviderError::BackendFailure(format!("cannot record response: {detail}"))
                })?;
            let recorded_response =
                RecordedResponse::from_parts(status, &headers, &body).map_err(|detail| {
                    ProviderError::BackendFailure(format!("cannot record response: {detail}"))
                })?;
            self.recorder
                .push(RecordedExchange { request: recorded_request, response: recorded_response });
            Ok(HttpResponse::buffered(status, headers, body))
        })
    }
}

/// Wraps `inner` so every exchange is captured into a new [`Recorder`].
///
/// The returned backend replaces the live one on a transport; the recorder is
/// the handle used to write the cassette when the run finishes. `chunk` is the
/// idle budget applied between body reads while buffering a live stream.
#[must_use]
pub fn recording_backend(
    inner: Arc<dyn HttpBackend>,
    label: impl Into<String>,
    chunk: Duration,
) -> (Arc<dyn HttpBackend>, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::new(label));
    let backend: Arc<dyn HttpBackend> =
        Arc::new(RecordingBackend { inner, recorder: recorder.clone(), chunk });
    (backend, recorder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use reqwest::header::{HeaderMap, HeaderValue};

    /// A backend that answers every request with one canned response.
    struct CannedBackend {
        status: u16,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    impl HttpBackend for CannedBackend {
        fn send(&self, _request: PreparedRequest) -> BackendFuture<'_> {
            let status = StatusCode::from_u16(self.status).expect("status");
            let headers = self.headers.clone();
            let body = self.body.clone();
            Box::pin(async move { Ok(HttpResponse::buffered(status, headers, body)) })
        }
    }

    fn request() -> PreparedRequest {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer sk-secret"));
        PreparedRequest {
            method: String::from("POST"),
            url: String::from("https://example.test/v1/chat/completions"),
            headers,
            body: b"{\"model\":\"example\"}".to_vec(),
        }
    }

    #[tokio::test]
    async fn recording_captures_redacted_request_and_response() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let inner: Arc<dyn HttpBackend> =
            Arc::new(CannedBackend { status: 200, headers, body: b"{\"ok\":true}".to_vec() });
        let (backend, recorder) = recording_backend(inner, "example", Duration::from_secs(5));

        backend.send(request()).await.expect("recorded send");

        assert_eq!(recorder.len(), 1);
        let cassette = recorder.cassette();
        assert_eq!(cassette.label, "example");
        let exchange = &cassette.exchanges[0];
        assert_eq!(exchange.request.body, "{\"model\":\"example\"}");
        let authorization = exchange
            .request
            .headers
            .iter()
            .find(|header| header.name == "authorization")
            .expect("authorization recorded");
        assert_eq!(authorization.value, super::super::cassette::REDACTED);
        assert!(cassette.to_json_pretty().expect("serializes").contains("<redacted>"));
        assert_eq!(exchange.response.status, 200);
        assert_eq!(exchange.response.body, "{\"ok\":true}");
    }

    #[tokio::test]
    async fn recording_keeps_the_caller_body_unchanged() {
        let inner: Arc<dyn HttpBackend> = Arc::new(CannedBackend {
            status: 200,
            headers: HeaderMap::new(),
            body: b"payload".to_vec(),
        });
        let (backend, recorder) = recording_backend(inner, "example", Duration::from_secs(5));

        let mut response = backend.send(request()).await.expect("recorded send");
        let body = response.read_bounded(1024, None).await.expect("reads body");

        assert_eq!(body, b"payload");
        assert_eq!(recorder.cassette().exchanges[0].response.body, "payload");
    }

    #[tokio::test]
    async fn clear_drops_captured_exchanges() {
        let inner: Arc<dyn HttpBackend> = Arc::new(CannedBackend {
            status: 200,
            headers: HeaderMap::new(),
            body: b"{}".to_vec(),
        });
        let (backend, recorder) = recording_backend(inner, "example", Duration::from_secs(5));
        backend.send(request()).await.expect("send");
        assert_eq!(recorder.len(), 1);

        recorder.clear();

        assert!(recorder.is_empty());
        assert!(recorder.cassette().exchanges.is_empty());
    }
}
