//! Authorized endpoint exchange: credential attachment, bounded bodies, phase
//! budgets.

use std::future::Future;
use std::time::Duration;

use ee_agent_acp_server::{Delivery, ProviderError};
use reqwest::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;

use crate::profile::{BearerToken, EndpointProfile, RequestEncoding, TokenSource};
use crate::wire::classify_status_error;

use super::sse::{extend_bounded, next_chunk};

/// Maximum bytes read from one JSON response body (success or error).
pub const MAX_JSON_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Builds the request URL with the profile's query parameters appended.
///
/// Every pair is percent-encoded exactly once by the URL API, so a value that
/// contains `?`, `&`, or `=` cannot introduce another parameter. Callers keep
/// keys unique (the profile builder does), so the result never carries a
/// duplicate parameter.
///
/// # Errors
///
/// Returns the URL parser diagnostic when the endpoint is not a URL.
fn endpoint_with_query(
    endpoint: &str,
    query: &[(String, String)],
) -> Result<url::Url, url::ParseError> {
    let mut url = url::Url::parse(endpoint)?;
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
    }
    Ok(url)
}

/// An authorized endpoint: the only place a bearer credential is attached.
///
/// The endpoint comes from the provider's validated profile, so no caller can
/// redirect a credential to an origin of its own choosing, and extension headers
/// are inserted before the credential so a provider cannot override it.
#[derive(Clone)]
pub struct EndpointTransport {
    pub(crate) profile: EndpointProfile,
    pub(crate) token: TokenSource,
    pub(crate) http: reqwest::Client,
}

impl EndpointTransport {
    /// Builds a transport with the profile's phase budgets.
    ///
    /// The whole-exchange budget (when configured) is applied by the HTTP client;
    /// the header and idle-chunk budgets are enforced per phase by
    /// [`EndpointTransport::send_json`] and the body readers.
    ///
    /// # Errors
    ///
    /// Returns the HTTP client construction failure as a diagnostic string.
    pub fn new(profile: EndpointProfile, token: TokenSource) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder();
        if let Some(total) = profile.timeouts.total {
            builder = builder.timeout(total);
        }
        let http =
            builder.build().map_err(|error| format!("failed to build HTTP client: {error}"))?;
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
    /// Provider query parameters are percent-encoded exactly once, and the body
    /// is compressed only when the profile selects an encoding.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the credential is missing, a header is
    /// invalid, the body cannot be encoded, the header budget expires, or the
    /// request cannot be sent.
    pub async fn send_json(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let mut headers = self.headers()?;
        if let Some(header) = &self.profile.image_opt_in_header
            && request_contains_image(body)
        {
            headers.insert(header.clone(), HeaderValue::from_static("true"));
        }
        let url = self.endpoint_url()?;
        let label = self.profile.label.as_str();
        let payload = encode_body(body, &mut headers, self.profile.encoding, label)?;
        let request = self.http.post(url).headers(headers).body(payload);
        send_with_deadline(request.send(), self.profile.timeouts.headers, label).await
    }

    /// Endpoint URL with the profile's query parameters appended.
    fn endpoint_url(&self) -> Result<url::Url, ProviderError> {
        let label = self.profile.label.as_str();
        endpoint_with_query(self.profile.endpoint.as_str(), &self.profile.query).map_err(|error| {
            ProviderError::BackendFailure(format!("{label} endpoint is not a URL: {error}"))
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
        while let Some(chunk) =
            next_chunk(label, response, Some(self.profile.timeouts.chunk)).await?
        {
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

/// Serializes one request body, compressing it only when configured.
///
/// Compression is off by default: an endpoint that does not decode the body
/// would reject the request, so the setting is explicit and validated at
/// startup.
fn encode_body(
    body: &Value,
    headers: &mut HeaderMap,
    encoding: RequestEncoding,
    label: &str,
) -> Result<Vec<u8>, ProviderError> {
    let plain = serde_json::to_vec(body).map_err(|error| {
        ProviderError::BackendFailure(format!("{label} request body is not JSON: {error}"))
    })?;
    let Some(encoding_name) = encoding.content_encoding() else {
        return Ok(plain);
    };
    let compressed = match encoding {
        RequestEncoding::None => plain,
        RequestEncoding::Gzip => {
            use std::io::Write as _;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&plain).and_then(|()| encoder.finish()).map_err(|error| {
                ProviderError::BackendFailure(format!("{label} request body gzip failed: {error}"))
            })?
        }
        RequestEncoding::Zstd => {
            zstd::stream::encode_all(plain.as_slice(), 3).map_err(|error| {
                ProviderError::BackendFailure(format!("{label} request body zstd failed: {error}"))
            })?
        }
    };
    headers.insert(CONTENT_ENCODING, HeaderValue::from_static(encoding_name));
    Ok(compressed)
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

/// Whether a request body carries an inline image part.
///
/// Recognizes the three dialect shapes (`image_url`, `input_image`, and
/// Anthropic `image`) plus any base64 image data URI, so a route that gates
/// image processing behind an opt-in header only emits it when an image is
/// actually present.
pub(crate) fn request_contains_image(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            if object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "image_url" | "input_image" | "image"))
            {
                return true;
            }
            object.values().any(request_contains_image)
        }
        Value::Array(items) => items.iter().any(request_contains_image),
        Value::String(text) => text.starts_with("data:image/"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_encoded_exactly_once() {
        let url = endpoint_with_query(
            "https://example.test/v1/chat/completions",
            &[(String::from("trace"), String::from("a&b=c?d"))],
        )
        .expect("valid");
        assert_eq!(url.as_str(), "https://example.test/v1/chat/completions?trace=a%26b%3Dc%3Fd");
        let pairs: Vec<(String, String)> =
            url.query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
        assert_eq!(pairs, vec![(String::from("trace"), String::from("a&b=c?d"))]);
    }

    #[test]
    fn a_query_value_cannot_inject_another_parameter() {
        let url = endpoint_with_query(
            "https://example.test/v1/chat/completions",
            &[(String::from("a"), String::from("1&key=secret"))],
        )
        .expect("valid");
        let pairs: Vec<(String, String)> =
            url.query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "a");
        assert!(pairs.iter().all(|(key, _)| key != "key"));
    }

    #[test]
    fn an_endpoint_without_query_stays_unchanged() {
        let url = endpoint_with_query("https://example.test/v1/models", &[]).expect("valid");
        assert_eq!(url.as_str(), "https://example.test/v1/models");
    }
}
