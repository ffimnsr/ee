//! Web-context module: transport.
use super::*;

/// HTTP method allowed at the host-owned transport boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebTransportMethod {
    Get,
    Post,
}

/// Request given to a transport after URL, host-approval, and DNS checks.
///
/// Implementations disable automatic redirects and cookie storage, use only
/// host-owned headers and bodies, and stop reading at `max_response_bytes`.
#[derive(Clone)]
pub struct WebTransportRequest {
    pub method: WebTransportMethod,
    pub url: Url,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub max_response_bytes: usize,
}

impl fmt::Debug for WebTransportRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebTransportRequest")
            .field("method", &self.method)
            .field("url", &redact_url_for_display(&self.url))
            .field("headers", &crate::redact::redact_headers(&self.headers))
            .field("body", &"REDACTED")
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

/// Response returned by a transport.
///
/// `connected_peer` must be address of TCP peer actually connected, not a
/// later DNS lookup. `body_truncated` reports that reading stopped at limit.
#[derive(Debug, Clone)]
pub struct WebTransportResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub body_truncated: bool,
    pub connected_peer: IpAddr,
}

/// Minimal asynchronous network boundary for an offline-testable web-context
/// service. Implementations must observe `cancellation` while resolving and
/// receiving a response body.
pub trait WebTransport: Send + Sync {
    /// Resolve host before each connection attempt.
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<IpAddr>, WebContextError>>;

    /// Perform one host-owned HTTPS request without following redirects.
    fn request<'a>(
        &'a self,
        request: &'a WebTransportRequest,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<WebTransportResponse, WebContextError>>;
}

/// Production HTTPS-only transport with redirects, proxies, cookies, and caller
/// headers disabled. [`WebContextService`] validates DNS and connected peer.
pub struct ReqwestWebTransport {
    pub(super) client: reqwest::Client,
}

impl ReqwestWebTransport {
    /// Builds bounded transport using Rustls through workspace `reqwest`.
    pub fn new(limits: &WebContextLimits) -> Result<Self, WebContextError> {
        limits.validate().map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
        let request_timeout = Duration::from_millis(limits.request_timeout_ms);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(request_timeout.min(Duration::from_secs(10)))
            .timeout(request_timeout)
            .build()
            .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
        Ok(Self { client })
    }
}

impl WebTransport for ReqwestWebTransport {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<IpAddr>, WebContextError>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(cancellation_error()),
                addresses = tokio::net::lookup_host((host, 443)) => addresses
                    .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))
                    .map(|addresses| addresses.map(|address| address.ip()).collect()),
            }
        })
    }

    fn request<'a>(
        &'a self,
        request: &'a WebTransportRequest,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<WebTransportResponse, WebContextError>> {
        Box::pin(async move {
            let mut headers = reqwest::header::HeaderMap::new();
            for (name, value) in &request.headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
                let value = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|_| WebContextError::new(WebContextErrorCode::NetworkFailure))?;
                headers.insert(name, value);
            }
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(cancellation_error()),
                response = self.client.request(
                    match request.method {
                        WebTransportMethod::Get => reqwest::Method::GET,
                        WebTransportMethod::Post => reqwest::Method::POST,
                    },
                    request.url.clone(),
                ).headers(headers).body(request.body.clone()).send() => response.map_err(reqwest_error)?,
            };
            let connected_peer = response
                .remote_addr()
                .ok_or_else(|| WebContextError::new(WebContextErrorCode::DnsRejected))?
                .ip();
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value.to_str().ok().map(|value| (name.as_str().to_owned(), value.to_owned()))
                })
                .collect();
            let mut body = Vec::with_capacity(request.max_response_bytes.min(16 * 1024));
            let mut stream = response.bytes_stream();
            while let Some(chunk) = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(cancellation_error()),
                chunk = stream.next() => chunk,
            } {
                let chunk = chunk.map_err(reqwest_error)?;
                let remaining =
                    request.max_response_bytes.saturating_add(1).saturating_sub(body.len());
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if body.len() > request.max_response_bytes {
                    body.truncate(request.max_response_bytes);
                    return Ok(WebTransportResponse {
                        status,
                        headers,
                        body,
                        body_truncated: true,
                        connected_peer,
                    });
                }
            }
            Ok(WebTransportResponse {
                status,
                headers,
                body,
                body_truncated: false,
                connected_peer,
            })
        })
    }
}

pub(super) fn cancellation_error() -> WebContextError {
    WebContextError::new(WebContextErrorCode::NetworkFailure)
}

pub(super) fn ensure_not_cancelled(
    cancellation: &CancellationToken,
) -> Result<(), WebContextError> {
    if cancellation.is_cancelled() { Err(cancellation_error()) } else { Ok(()) }
}

pub(super) async fn run_with_timeout<R>(
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: impl std::future::Future<Output = Result<R, WebContextError>>,
) -> Result<R, WebContextError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancellation_error()),
        result = tokio::time::timeout(timeout, operation) => match result {
            Ok(result) => result,
            Err(_) => Err(WebContextError::new(WebContextErrorCode::NetworkTimeout)),
        },
    }
}

pub(super) fn reqwest_error(error: reqwest::Error) -> WebContextError {
    let code = if error.is_timeout() {
        WebContextErrorCode::NetworkTimeout
    } else {
        WebContextErrorCode::NetworkFailure
    };
    WebContextError::new(code)
}
