//! On-disk cassette format for recorded provider exchanges.
//!
//! A cassette is a small, reviewable JSON file: one ordered list of exchanges,
//! each holding the request a provider received and the response it sent back.
//! Credentials are redacted when recording, so a cassette can be committed and
//! replayed in CI.
//!
//! Matching is deliberately body-first: a request's identity is its method plus
//! its exact request body, with the URL path as a tie-breaker. That keeps a
//! fixture valid when a test rewrites the endpoint origin to an unreachable host
//! while still distinguishing routes that share a body.
//!
//! Size and shape are bounded on load (`MAX_CASSETTE_BYTES`,
//! `MAX_CASSETTE_EXCHANGES`, per-body limits), so a corrupt or hostile fixture
//! fails closed instead of exhausting memory.
//!
//! Credentials are stripped from headers and from URL query parameters, but a
//! request body still contains the prompt the developer sent. Review a cassette
//! before committing it, exactly as you would review a captured transcript.

use std::collections::HashSet;
use std::path::Path;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::backend::{HttpResponse, PreparedRequest};

/// Cassette schema version this build reads and writes.
pub const CASSETTE_VERSION: u32 = 1;

/// Maximum bytes read from one cassette file.
pub const MAX_CASSETTE_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum exchanges one cassette may carry.
pub const MAX_CASSETTE_EXCHANGES: usize = 4096;

/// Maximum bytes recorded for one request or response body.
pub const MAX_RECORDED_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Maximum headers recorded on one side of an exchange.
pub const MAX_RECORDED_HEADERS: usize = 128;

/// Placeholder written in place of a credential-bearing header value.
pub const REDACTED: &str = "<redacted>";

/// Header values replaced when recording or persisting an exchange.
///
/// Matched case-insensitively against the header name. Provider extension
/// headers carrying keys are included because a route may authenticate with a
/// vendor header instead of `Authorization`.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "x-auth-token",
];

/// Hop-by-hop and body-framing headers that are not meaningful once the body is
/// buffered, or that can disagree with the recorded body after decompression.
const DROPPED_HEADERS: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "content-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "trailer",
    "te",
    "upgrade",
];

/// One recorded header pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedHeader {
    /// Header name, lowercased as `reqwest` stores it.
    pub name: String,
    /// Header value, or [`REDACTED`] for credential-bearing headers.
    pub value: String,
}

/// One recorded request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedRequest {
    /// HTTP method.
    pub method: String,
    /// Absolute URL as sent, including the profile's query parameters.
    pub url: String,
    /// Redacted request headers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<RecordedHeader>,
    /// Exact request body bytes, interpreted as UTF-8.
    #[serde(default)]
    pub body: String,
}

impl RecordedRequest {
    /// Captures one prepared request, redacting credential headers.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the request body is not UTF-8. Model requests
    /// are JSON or SSE-adjacent JSON, so this indicates a broken caller rather
    /// than a provider quirk.
    pub(crate) fn from_prepared(request: &PreparedRequest) -> Result<Self, String> {
        let body = String::from_utf8(request.body.clone())
            .map_err(|error| format!("request body is not UTF-8: {error}"))?;
        Ok(Self {
            method: request.method.clone(),
            url: redact_url(&request.url),
            headers: record_headers(&request.headers),
            body,
        })
    }

    /// Stable matching key: method plus exact body.
    #[must_use]
    pub(crate) fn key(&self) -> String {
        request_key(&self.method, self.body.as_bytes())
    }

    /// URL path and query, which distinguishes routes that share a body.
    #[must_use]
    pub(crate) fn target(&self) -> String {
        request_target(&self.url)
    }
}

/// One recorded response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers with framing and credential headers removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<RecordedHeader>,
    /// Exact response body, interpreted as UTF-8.
    #[serde(default)]
    pub body: String,
}

impl RecordedResponse {
    /// Captures one buffered response.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the body is not UTF-8. Provider bodies are JSON
    /// or SSE; a binary body cannot be replayed faithfully as text.
    pub(crate) fn from_parts(
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<Self, String> {
        let body = std::str::from_utf8(body)
            .map_err(|error| format!("response body is not UTF-8: {error}"))?
            .to_string();
        Ok(Self { status: status.as_u16(), headers: record_headers(headers), body })
    }

    /// Rebuilds the response a codec consumes.
    ///
    /// Invalid header names or values are skipped rather than failing the whole
    /// exchange, because `reqwest` would have rejected them on the live path
    /// anyway; the body is what the codec reads.
    pub(crate) fn to_http_response(&self, chunk: usize) -> Result<HttpResponse, String> {
        let status = StatusCode::from_u16(self.status)
            .map_err(|error| format!("recorded status {} is invalid: {error}", self.status))?;
        let mut headers = HeaderMap::new();
        for header in &self.headers {
            let Ok(name) = HeaderName::from_bytes(header.name.as_bytes()) else {
                continue;
            };
            let Ok(value) = HeaderValue::from_str(&header.value) else {
                continue;
            };
            headers.append(name, value);
        }
        Ok(HttpResponse::buffered_with_chunk_size(
            status,
            headers,
            self.body.as_bytes().to_vec(),
            chunk,
        ))
    }
}

/// One recorded request/response pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedExchange {
    /// The request the provider received.
    pub request: RecordedRequest,
    /// The response the provider sent.
    pub response: RecordedResponse,
}

/// One cassette file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cassette {
    /// Schema version; must equal [`CASSETTE_VERSION`].
    pub version: u32,
    /// Human label for the recorded route, for example `gemini-chat-stream`.
    #[serde(default)]
    pub label: String,
    /// Exchanges in the order they were recorded.
    #[serde(default)]
    pub exchanges: Vec<RecordedExchange>,
}

impl Cassette {
    /// Builds an empty cassette for `label`.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self { version: CASSETTE_VERSION, label: label.into(), exchanges: Vec::new() }
    }

    /// Parses and validates a cassette document.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic for invalid JSON, a version mismatch, or any bound
    /// in [`Cassette::validate`].
    pub fn from_json(text: &str) -> Result<Self, String> {
        let cassette: Self = serde_json::from_str(text)
            .map_err(|error| format!("cassette is not valid JSON: {error}"))?;
        cassette.validate()?;
        Ok(cassette)
    }

    /// Serializes the cassette as pretty JSON.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when serialization fails (only on an unrepresentable
    /// value; exchanges are plain data).
    pub fn to_json_pretty(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|error| format!("cassette cannot be serialized: {error}"))
    }

    /// Loads and validates a cassette file, bounded by [`MAX_CASSETTE_BYTES`].
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the file is too large, unreadable, invalid, or
    /// out of bounds.
    pub fn load(path: &Path) -> Result<Self, String> {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("cannot stat cassette {}: {error}", path.display()))?;
        if metadata.len() > MAX_CASSETTE_BYTES {
            return Err(format!(
                "cassette {} is {} bytes, over the {MAX_CASSETTE_BYTES}-byte limit",
                path.display(),
                metadata.len()
            ));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read cassette {}: {error}", path.display()))?;
        Self::from_json(&text)
            .map_err(|error| format!("invalid cassette {}: {error}", path.display()))
    }

    /// Writes the cassette as pretty JSON, creating parent directories.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when serialization or the write fails.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("cannot create cassette directory {}: {error}", parent.display())
            })?;
        }
        let text = self.to_json_pretty()?;
        std::fs::write(path, text)
            .map_err(|error| format!("cannot write cassette {}: {error}", path.display()))
    }

    /// Validates version, exchange count, and every exchange's bounds.
    ///
    /// # Errors
    ///
    /// Returns the first violated bound as a diagnostic.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != CASSETTE_VERSION {
            return Err(format!(
                "cassette version {} is unsupported; this build reads version \
                 {CASSETTE_VERSION}",
                self.version
            ));
        }
        if self.exchanges.len() > MAX_CASSETTE_EXCHANGES {
            return Err(format!(
                "cassette has {} exchanges, over the {MAX_CASSETTE_EXCHANGES} limit",
                self.exchanges.len()
            ));
        }
        for (index, exchange) in self.exchanges.iter().enumerate() {
            validate_request(index, &exchange.request)?;
            validate_response(index, &exchange.response)?;
        }
        Ok(())
    }
}

fn validate_request(index: usize, request: &RecordedRequest) -> Result<(), String> {
    if request.method.trim().is_empty() {
        return Err(format!("exchange {index}: request method is empty"));
    }
    if request.url.trim().is_empty() {
        return Err(format!("exchange {index}: request URL is empty"));
    }
    if request.body.len() > MAX_RECORDED_BODY_BYTES {
        return Err(format!(
            "exchange {index}: request body is {} bytes, over the \
             {MAX_RECORDED_BODY_BYTES}-byte limit",
            request.body.len()
        ));
    }
    validate_headers(index, "request", &request.headers)
}

fn validate_response(index: usize, response: &RecordedResponse) -> Result<(), String> {
    if !(100..=599).contains(&response.status) {
        return Err(format!("exchange {index}: response status {} is invalid", response.status));
    }
    if response.body.len() > MAX_RECORDED_BODY_BYTES {
        return Err(format!(
            "exchange {index}: response body is {} bytes, over the \
             {MAX_RECORDED_BODY_BYTES}-byte limit",
            response.body.len()
        ));
    }
    validate_headers(index, "response", &response.headers)
}

fn validate_headers(index: usize, side: &str, headers: &[RecordedHeader]) -> Result<(), String> {
    if headers.len() > MAX_RECORDED_HEADERS {
        return Err(format!(
            "exchange {index}: {side} has {} headers, over the {MAX_RECORDED_HEADERS} limit",
            headers.len()
        ));
    }
    Ok(())
}

/// Matching key for one request: method plus exact body bytes.
#[must_use]
pub(crate) fn request_key(method: &str, body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update([0]);
    hasher.update(body);
    hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// URL path plus query, or the raw URL when it does not parse.
#[must_use]
pub(crate) fn request_target(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            let mut target = parsed.path().to_string();
            if let Some(query) = parsed.query() {
                target.push('?');
                target.push_str(query);
            }
            target
        }
        Err(_) => url.to_string(),
    }
}

/// Query parameter names that commonly carry a credential.
const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "key",
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "token",
    "auth",
    "signature",
    "sig",
    "client_secret",
];

/// Replaces credential-bearing query values with [`REDACTED`].
///
/// Providers that authenticate with the standard bearer header are unaffected;
/// this covers routes that accept a key in the query string so a cassette never
/// carries one.
fn redact_url(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    if parsed.query().is_none() {
        return parsed.to_string();
    }
    let pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(name, value)| {
            let sensitive = SENSITIVE_QUERY_KEYS.iter().any(|key| name.eq_ignore_ascii_case(key));
            (name.into_owned(), if sensitive { REDACTED.to_string() } else { value.into_owned() })
        })
        .collect();
    parsed.query_pairs_mut().clear().extend_pairs(pairs);
    parsed.to_string()
}

/// Records headers, redacting credentials and dropping framing headers.
fn record_headers(headers: &HeaderMap) -> Vec<RecordedHeader> {
    let dropped: HashSet<&str> = DROPPED_HEADERS.iter().copied().collect();
    let mut recorded = Vec::new();
    for (name, value) in headers {
        let name = name.as_str();
        if dropped.contains(name) {
            continue;
        }
        let value = if is_sensitive_header(name) {
            REDACTED.to_string()
        } else {
            value.to_str().unwrap_or(REDACTED).to_string()
        };
        recorded.push(RecordedHeader { name: name.to_string(), value });
    }
    recorded
}

/// Whether a header value must never be persisted.
#[must_use]
fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS.iter().any(|sensitive| name.eq_ignore_ascii_case(sensitive))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(body: &str, status: u16) -> RecordedExchange {
        RecordedExchange {
            request: RecordedRequest {
                method: String::from("POST"),
                url: String::from("https://example.test/v1/chat/completions?alt=sse"),
                headers: Vec::new(),
                body: body.to_string(),
            },
            response: RecordedResponse {
                status,
                headers: vec![RecordedHeader {
                    name: String::from("content-type"),
                    value: String::from("application/json"),
                }],
                body: String::from("{}"),
            },
        }
    }

    #[test]
    fn round_trip_preserves_exchanges() {
        let cassette = Cassette { label: String::from("test"), ..Cassette::new("test") };
        let mut cassette = cassette;
        cassette.exchanges.push(exchange("{\"a\":1}", 200));

        let json = cassette.to_json_pretty().expect("serializes");
        let parsed = Cassette::from_json(&json).expect("parses");

        assert_eq!(parsed, cassette);
    }

    #[test]
    fn unsupported_version_fails_closed() {
        let error =
            Cassette::from_json(r#"{"version": 99, "exchanges": []}"#).expect_err("rejects");
        assert!(error.contains("version 99"), "{error}");
    }

    #[test]
    fn invalid_status_fails_closed() {
        let mut cassette = Cassette::new("test");
        cassette.exchanges.push(exchange("{}", 999));
        let error = cassette.validate().expect_err("rejects");
        assert!(error.contains("status 999"), "{error}");
    }

    #[test]
    fn sensitive_headers_are_redacted_and_framing_dropped() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer sk-secret"));
        headers.insert("x-goog-api-key", HeaderValue::from_static("secret"));
        headers.insert("content-length", HeaderValue::from_static("2"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let recorded = record_headers(&headers);
        let by_name = |name: &str| recorded.iter().find(|header| header.name == name);

        assert_eq!(by_name("authorization").expect("authorization").value, REDACTED);
        assert_eq!(by_name("x-goog-api-key").expect("api key").value, REDACTED);
        assert!(by_name("content-length").is_none());
        assert_eq!(by_name("content-type").expect("content type").value, "application/json");
    }

    #[test]
    fn request_keys_ignore_the_origin_but_not_the_body() {
        let one = request_key("POST", b"{\"a\":1}");
        assert_eq!(one, request_key("POST", b"{\"a\":1}"));
        assert_ne!(one, request_key("POST", b"{\"a\":2}"));
        assert_ne!(one, request_key("GET", b"{\"a\":1}"));
    }

    #[test]
    fn request_target_keeps_path_and_query() {
        assert_eq!(
            request_target("https://example.test/v1/chat/completions?alt=sse&x=1"),
            "/v1/chat/completions?alt=sse&x=1"
        );
        assert_eq!(request_target("not a url"), "not a url");
    }

    #[test]
    fn query_credentials_are_redacted() {
        assert_eq!(
            redact_url("https://example.test/v1?key=sk-secret&alt=sse"),
            "https://example.test/v1?key=%3Credacted%3E&alt=sse"
        );
        assert_eq!(redact_url("https://example.test/v1"), "https://example.test/v1");
        assert_eq!(redact_url("not a url"), "not a url");
    }
}
