//! Provider failure classification and retry timing.
//!
//! Classification is structural: the HTTP status, the error code the provider
//! put in the body, and a bounded phrase match decide the failure class, in the
//! order the reference agent uses. Timing owns the backoff, the server-hint cap,
//! and the separate budget for retries that never reached the provider.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ee_agent_acp_server::{Delivery, ProviderError, RateLimitInfo};
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::profile::RetryPolicy;

/// Builds the failure detail for a non-success HTTP status.
#[must_use]
pub fn http_error_message(label: &str, status: u16, value: &Value) -> String {
    if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
        format!("{label} HTTP {status}: {message}")
    } else {
        format!("{label} HTTP {status}: {value}")
    }
}

/// Classifies a non-success status after its body was read, reusing the
/// provider's own message when the body carries one.
///
/// The classification follows the reference agent's ordering: context overflow
/// and oversized payloads first, then content policy, quota, authentication, and
/// rate limiting. A `429` is never reclassified as a context overflow even when
/// its body mentions tokens.
#[must_use]
pub fn classify_status_error(
    label: &str,
    status: u16,
    headers: &HeaderMap,
    value: &Value,
) -> ProviderError {
    let retry_after = parse_retry_after(headers);
    let rate_limit = parse_rate_limit(headers, retry_after);
    let retryable = parse_should_retry(headers).unwrap_or(true);
    classify_response(
        status,
        &http_error_message(label, status, value),
        value,
        retry_after,
        rate_limit,
        retryable,
    )
}

/// Classifies a non-success status into a retry decision and a typed provider
/// error. Kept for callers that have a status and detail but no body.
#[must_use]
pub fn classify_http_error(
    status: u16,
    retry_after: Option<Duration>,
    detail: String,
) -> ProviderError {
    classify_response(status, &detail, &Value::Null, retry_after, None, true)
}

/// Codes and phrases that identify each failure class.
const CONTEXT_OVERFLOW_CODES: [&str; 3] =
    ["context_length_exceeded", "model_context_window_exceeded", "request_too_large"];
const CONTEXT_OVERFLOW_TEXT: [&str; 8] = [
    "context length",
    "context window",
    "maximum context",
    "too many tokens",
    "prompt is too long",
    "input is too long",
    "reduce the length of the messages",
    "exceeds the model's maximum",
];
const PAYLOAD_TOO_LARGE_TEXT: [&str; 4] =
    ["payload too large", "request entity too large", "body too large", "maximum request size"];
const CONTENT_POLICY_TEXT: [&str; 6] = [
    "content_policy",
    "content policy",
    "content filter",
    "safety system",
    "moderation",
    "usage policies",
];
const QUOTA_TEXT: [&str; 5] =
    ["insufficient_quota", "quota", "credit", "billing", "insufficient balance"];
const AUTH_TEXT: [&str; 5] =
    ["invalid api key", "incorrect api key", "invalid_api_key", "authentication", "unauthorized"];
const RATE_LIMIT_TEXT: [&str; 4] = ["rate limit", "rate_limit", "too many requests", "throttl"];

/// Classifies one non-success response.
fn classify_response(
    status: u16,
    detail: &str,
    value: &Value,
    retry_after: Option<Duration>,
    rate_limit: Option<RateLimitInfo>,
    retryable: bool,
) -> ProviderError {
    let text = detail.to_ascii_lowercase();
    let codes = error_codes(value);
    let has_code = |needles: &[&str]| {
        codes.iter().any(|code| needles.iter().any(|needle| code.contains(needle)))
    };
    let has_text = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));
    let client_scoped = (400..500).contains(&status);
    // A throttle is never an overflow: providers reuse token wording for
    // throughput limits ("too many tokens processed"), and some proxy a
    // rate-limit body through an overflow-shaped code. The status check must
    // therefore guard the code branch too, not only the phrase branch.
    let throttling = status == 429 || has_code(&["throttl"]);
    if client_scoped
        && !throttling
        && (has_code(&CONTEXT_OVERFLOW_CODES) || has_text(&CONTEXT_OVERFLOW_TEXT))
    {
        return ProviderError::ContextOverflow(detail.to_string());
    }
    if status == 413 || has_text(&PAYLOAD_TOO_LARGE_TEXT) {
        return ProviderError::PayloadTooLarge(detail.to_string());
    }
    if has_code(&["content_policy"]) || has_text(&CONTENT_POLICY_TEXT) {
        return ProviderError::ContentPolicy(detail.to_string());
    }
    if status == 402 || has_code(&["quota", "billing"]) || (status == 429 && has_text(&QUOTA_TEXT))
    {
        return ProviderError::QuotaExceeded { retry_after, detail: detail.to_string() };
    }
    if status == 401 || status == 403 || (status == 400 && has_text(&AUTH_TEXT)) {
        return ProviderError::BackendFailure(detail.to_string());
    }
    if throttling || has_text(&RATE_LIMIT_TEXT) {
        return ProviderError::RateLimited {
            retry_after,
            rate_limit,
            retryable,
            detail: detail.to_string(),
        };
    }
    if status == 408 || status == 409 || status >= 500 {
        return ProviderError::Transient { retry_after, retryable, detail: detail.to_string() };
    }
    if client_scoped {
        return ProviderError::InvalidRequest(detail.to_string());
    }
    ProviderError::BackendFailure(detail.to_string())
}

/// Error codes the provider put in the body, lowercased.
fn error_codes(value: &Value) -> Vec<String> {
    let mut codes = Vec::new();
    for pointer in ["/error/code", "/error/type", "/code", "/type"] {
        if let Some(code) = value.pointer(pointer) {
            match code {
                Value::String(text) => codes.push(text.to_ascii_lowercase()),
                Value::Number(number) => codes.push(number.to_string()),
                _ => {}
            }
        }
    }
    codes
}

/// Whether a provider error may be retried (rate limits, transient failures, and
/// transport failures that never reached the provider).
#[must_use]
pub fn is_retryable(error: &ProviderError) -> bool {
    match error {
        ProviderError::RateLimited { retryable, .. }
        | ProviderError::Transient { retryable, .. } => *retryable,
        ProviderError::Transport { delivery, .. } => {
            matches!(delivery, Delivery::NotSent | Delivery::Rejected)
        }
        _ => false,
    }
}

/// Server-provided retry hint, when the error carries one.
#[must_use]
pub fn retry_after_of(error: &ProviderError) -> Option<Duration> {
    match error {
        ProviderError::RateLimited { retry_after, .. }
        | ProviderError::Transient { retry_after, .. }
        | ProviderError::QuotaExceeded { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// Whether the failure says this machine has no network path to the provider.
///
/// These retries have their own budget: an outage usually outlasts the general
/// allowance while every attempt fails before reaching the provider.
#[must_use]
pub fn is_unreachable(error: &ProviderError) -> bool {
    const UNREACHABLE_CODES: [&str; 5] =
        ["ENOTFOUND", "EAI_AGAIN", "ENETUNREACH", "EHOSTUNREACH", "ENETDOWN"];
    match error {
        ProviderError::Transport { delivery: Delivery::NotSent, code: Some(code), .. } => {
            UNREACHABLE_CODES.contains(&code.as_str())
        }
        _ => false,
    }
}

/// Whether the transport failed by timing out.
///
/// Phase-specific codes (`TimeoutHeaders`, `TimeoutChunk`) count as timeouts so
/// the retry budget treats them the same way; the code still names the phase for
/// diagnostics and for a caller that wants to distinguish them.
#[must_use]
pub fn is_timeout(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Transport { code: Some(code), .. } if code.starts_with("Timeout")
    )
}

/// Which phase of a timed-out transport request expired, when known.
#[must_use]
pub fn timeout_phase(error: &ProviderError) -> Option<&'static str> {
    let ProviderError::Transport { code: Some(code), .. } = error else {
        return None;
    };
    match code.as_str() {
        "TimeoutHeaders" | "Timeout:headers" => Some("headers"),
        "TimeoutChunk" | "Timeout:chunk" => Some("chunk"),
        "Timeout" => Some("total"),
        _ => None,
    }
}

/// Parses a `Retry-After` header value (delta-seconds form; HTTP dates are rare
/// and rejected rather than mis-parsed).
#[must_use]
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    value.parse::<u64>().ok().map(Duration::from_secs)
}

/// Bounded rate-limit metadata from `x-ratelimit-*` headers.
///
/// Returns `None` when the response carried no such header, so a plain `429`
/// stays a plain `429`.
#[must_use]
pub fn parse_rate_limit(
    headers: &HeaderMap,
    retry_after: Option<Duration>,
) -> Option<RateLimitInfo> {
    let limit = header_u64(headers, "x-ratelimit-limit");
    let remaining = header_u64(headers, "x-ratelimit-remaining");
    let reset = header_u64(headers, "x-ratelimit-reset").map(Duration::from_secs);
    (limit.is_some() || remaining.is_some() || reset.is_some()).then_some(RateLimitInfo {
        retry_after,
        limit,
        remaining,
        reset,
    })
}

/// `x-should-retry` override: `Some(false)` suppresses a retry the status would
/// otherwise allow, `Some(true)` forces one.
#[must_use]
pub fn parse_should_retry(headers: &HeaderMap) -> Option<bool> {
    match headers.get("x-should-retry")?.to_str().ok()?.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// Retry delay for `attempt` (0-based): backoff with bounded jitter, widened to
/// the server hint when the hint asks for longer, and capped so a hostile
/// `Retry-After` cannot stall a call for hours.
#[must_use]
pub fn retry_delay(policy: &RetryPolicy, attempt: u32, retry_after: Option<Duration>) -> Duration {
    let backoff = backoff_millis(policy, attempt);
    let delay = match retry_after {
        Some(hint) => backoff.max(hint.min(policy.retry_after_max).as_millis() as u64),
        None => backoff,
    };
    Duration::from_millis(delay.saturating_add(jitter_millis(delay)))
}

/// Delay for a retry that never reached the provider.
///
/// These attempts do not consume the general allowance, so their gaps are
/// capped independently of the general backoff.
#[must_use]
pub fn unreachable_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let backoff = backoff_millis(policy, attempt).min(30_000);
    Duration::from_millis(backoff.saturating_add(jitter_millis(backoff)))
}

fn backoff_millis(policy: &RetryPolicy, attempt: u32) -> u64 {
    let base = policy.base_delay.as_millis() as u64;
    base.saturating_mul(1u64 << attempt.min(10)).min(policy.max_delay.as_millis() as u64)
}

/// Bounded jitter (±20%) so bursts do not retry in lockstep.
fn jitter_millis(delay: u64) -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.subsec_nanos() as u64)
        % (delay / 5 + 1)
}
