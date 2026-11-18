//! HTTP exchange seam: prepared request in, normalized response out.
//!
//! Every provider codec builds a body and sends it through [`EndpointTransport`],
//! which delegates to an [`HttpBackend`]. The live backend uses `reqwest`; the
//! replay and recording backends in [`super::replay`] and [`super::recording`]
//! implement the same trait, so a codec runs unchanged against a recorded
//! fixture instead of the network.
//!
//! [`HttpResponse`] is deliberately narrow: status, headers, and an ordered
//! chunk source. That is all a codec consults, and it is exactly what a fixture
//! can reproduce without constructing a `reqwest::Response`.
//!
//! [`EndpointTransport`]: super::EndpointTransport

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use ee_agent_acp_server::{Delivery, ProviderError};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;

use super::sse::{ChunkFuture, ChunkSource, extend_bounded, next_chunk};

/// Default chunk size for a buffered (replayed) body.
///
/// Replay splits the recorded body into fixed chunks so SSE consumers see the
/// same framing work they do against a live stream, deterministically.
pub const DEFAULT_REPLAY_CHUNK_BYTES: usize = 512;

/// Boxed future returned by [`HttpBackend::send`].
pub type BackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HttpResponse, ProviderError>> + Send + 'a>>;

/// One fully prepared HTTP request.
///
/// The body is already encoded (and possibly compressed) exactly as it will go
/// on the wire, so a recorder stores the bytes a provider actually received.
#[derive(Debug, Clone)]
pub struct PreparedRequest {
    /// HTTP method; every model exchange is a `POST`.
    pub method: String,
    /// Absolute URL, including the profile's query parameters.
    pub url: String,
    /// Final headers, including the provider credential.
    pub headers: HeaderMap,
    /// Final request body bytes.
    pub body: Vec<u8>,
}

/// One HTTP response whose body can be read once, chunk by chunk.
#[derive(Debug)]
pub struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Body,
}

#[derive(Debug)]
enum Body {
    Live(reqwest::Response),
    Buffered { bytes: Vec<u8>, chunk: usize, offset: usize },
}

impl HttpResponse {
    /// Wraps a live `reqwest` response.
    #[must_use]
    pub fn live(response: reqwest::Response) -> Self {
        let status = response.status();
        let headers = response.headers().clone();
        Self { status, headers, body: Body::Live(response) }
    }

    /// Builds a buffered response with the default replay chunk size.
    #[must_use]
    pub fn buffered(status: StatusCode, headers: HeaderMap, bytes: Vec<u8>) -> Self {
        Self::buffered_with_chunk_size(status, headers, bytes, DEFAULT_REPLAY_CHUNK_BYTES)
    }

    /// Builds a buffered response split into `chunk`-sized reads.
    ///
    /// `chunk` is clamped to `1..=64 KiB` so a fixture cannot force a
    /// pathological number of reads or a single unbounded chunk.
    #[must_use]
    pub fn buffered_with_chunk_size(
        status: StatusCode,
        headers: HeaderMap,
        bytes: Vec<u8>,
        chunk: usize,
    ) -> Self {
        Self {
            status,
            headers,
            body: Body::Buffered { bytes, chunk: chunk.clamp(1, 64 * 1024), offset: 0 },
        }
    }

    /// HTTP status of the response.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Response headers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Reads the remaining body, failing past `limit` bytes.
    ///
    /// Used by the recorder, which must hold the whole exchange before it can
    /// persist it. `chunk` applies the profile's idle budget between reads; a
    /// buffered replay body returns immediately. Callers that stream keep
    /// calling [`ChunkSource::next_chunk`].
    ///
    /// # Errors
    ///
    /// Returns a provider-neutral detail when a body read fails or the body
    /// exceeds `limit`.
    pub async fn read_bounded(
        &mut self,
        limit: usize,
        chunk: Option<Duration>,
    ) -> Result<Vec<u8>, String> {
        let mut buffer = Vec::new();
        while let Some(bytes) =
            next_chunk("recording", self, chunk).await.map_err(|error| error.to_string())?
        {
            extend_bounded(&mut buffer, &bytes, limit)?;
        }
        Ok(buffer)
    }
}

impl ChunkSource for HttpResponse {
    fn next_chunk(&mut self) -> ChunkFuture<'_> {
        Box::pin(async move {
            match &mut self.body {
                Body::Live(response) => response.next_chunk().await,
                Body::Buffered { bytes, chunk, offset } => {
                    if *offset >= bytes.len() {
                        return Ok(None);
                    }
                    let end = (*offset + *chunk).min(bytes.len());
                    let slice = bytes[*offset..end].to_vec();
                    *offset = end;
                    Ok(Some(slice))
                }
            }
        })
    }
}

/// One transport backend.
///
/// Implementations must be safe to share across tasks: a transport is cloned per
/// request and may serve concurrent sessions.
pub trait HttpBackend: Send + Sync {
    /// Sends one prepared request.
    fn send(&self, request: PreparedRequest) -> BackendFuture<'_>;
}

/// Live `reqwest` backend with the profile's header-phase budget.
pub(crate) struct LiveBackend {
    http: reqwest::Client,
    headers_budget: Duration,
    label: String,
}

impl LiveBackend {
    pub(crate) fn new(http: reqwest::Client, headers_budget: Duration, label: String) -> Self {
        Self { http, headers_budget, label }
    }
}

impl HttpBackend for LiveBackend {
    fn send(&self, request: PreparedRequest) -> BackendFuture<'_> {
        Box::pin(async move {
            let url = url::Url::parse(&request.url).map_err(|error| {
                ProviderError::BackendFailure(format!(
                    "{} endpoint is not a URL: {error}",
                    self.label
                ))
            })?;
            let prepared = self.http.post(url).headers(request.headers).body(request.body);
            let response =
                send_with_deadline(prepared.send(), self.headers_budget, &self.label).await?;
            Ok(HttpResponse::live(response))
        })
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
