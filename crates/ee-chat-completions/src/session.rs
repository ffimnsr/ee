//! Session affinity headers and prompt-cache key derivation.
//!
//! Providers key prompt caches and per-session rate limits on an affinity id,
//! not on the turn's own session id. The reference agent groups a whole agent
//! tree under one affinity and derives `promptCacheKey` from it. This module
//! lowers the normalized
//! [`SessionContext`](ee_agent_orchestrator::SessionContext) into headers and
//! keeps the naming dialect provider-owned:
//!
//! - [`SessionHeaderStyle::OpenCode`] copies `x-opencode-*` exactly, because
//!   OpenCode Zen keys caches on those names.
//! - [`SessionHeaderStyle::Ee`] keeps the same semantics under `x-ee-*` names
//!   so we never impersonate OpenCode on another provider.

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::SessionContext;
use reqwest::header::{HeaderName, HeaderValue, USER_AGENT};

use crate::profile::{EndpointProfile, SessionHeaderStyle};
use crate::transport::EndpointTransport;

/// Maximum bytes in one validated session header value.
pub const MAX_SESSION_HEADER_VALUE_BYTES: usize = 256;

/// Builds the session header set for one request.
///
/// Values are validated against an opaque-token allowlist before they become
/// headers, so a client-supplied id can never inject a filesystem path, a
/// username, a newline, or another header.
///
/// # Errors
///
/// Returns a typed failure naming the field (never the value) when an id is
/// empty, too long, or carries a disallowed character.
pub fn session_headers(
    profile: &EndpointProfile,
    context: &SessionContext,
) -> Result<Vec<(HeaderName, HeaderValue)>, ProviderError> {
    let label = profile.label.as_str();
    validate_session_value(label, "session_id", &context.session_id)?;
    if let Some(parent) = context.parent_session_id.as_deref() {
        validate_session_value(label, "parent_session_id", parent)?;
    }
    if let Some(project) = context.project_id.as_deref() {
        validate_session_value(label, "project_id", project)?;
    }
    if let Some(client) = context.client_name.as_deref() {
        validate_session_value(label, "client_name", client)?;
    }
    let affinity = context.affinity();
    let mut headers = Vec::new();
    let (session_id_name, parent_name, project_name, affinity_alias, client_name) =
        match profile.session_header_style {
            SessionHeaderStyle::OpenCode => (
                "x-opencode-session-id",
                "x-opencode-parent-session-id",
                "x-opencode-project",
                "x-opencode-session",
                "x-opencode-client",
            ),
            SessionHeaderStyle::Ee => (
                "x-ee-session-id",
                "x-ee-parent-session-id",
                "x-ee-project",
                "x-ee-session",
                "x-ee-client",
            ),
        };
    headers.push(header(session_id_name, &context.session_id));
    if let Some(parent) = context.parent_session_id.as_deref() {
        headers.push(header(parent_name, parent));
        headers.push(header("x-parent-session-id", parent));
    }
    headers.push(header("x-session-affinity", affinity));
    // The reference sets `X-Session-Id` to the affinity, not the session id.
    headers.push(header("x-session-id", affinity));
    headers.push(header(affinity_alias, affinity));
    if let Some(project) = context.project_id.as_deref() {
        headers.push(header(project_name, project));
    }
    if let Some(client) = context.client_name.as_deref() {
        headers.push(header(client_name, client));
    }
    headers.push((
        USER_AGENT,
        HeaderValue::from_str(&profile.user_agent).map_err(|_| {
            ProviderError::BackendFailure(format!("{label}: invalid User-Agent value"))
        })?,
    ));
    Ok(headers)
}

fn header(name: &'static str, value: &str) -> (HeaderName, HeaderValue) {
    // The value was validated by the caller; the fallback keeps the function
    // total without echoing the raw value anywhere.
    let parsed = HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(""));
    (HeaderName::from_static(name), parsed)
}

/// Rejects ids outside the opaque-token allowlist.
///
/// Allowed: ASCII alphanumerics plus `- _ . : @ +`; 1..=256 bytes. This blocks
/// path separators, whitespace, control characters, and anything that could be
/// mistaken for credential material.
fn validate_session_value(
    label: &str,
    field: &'static str,
    value: &str,
) -> Result<(), ProviderError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_SESSION_HEADER_VALUE_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@' | b'+')
        });
    if valid {
        Ok(())
    } else {
        Err(ProviderError::BackendFailure(format!(
            "{label}: invalid session {field} for provider affinity headers"
        )))
    }
}

impl EndpointTransport {
    /// Returns a transport that carries `context`'s affinity headers.
    ///
    /// The original transport is untouched, so a per-request session never
    /// leaks into another session's calls. `None` and a disabled profile return
    /// an unmodified clone.
    ///
    /// # Errors
    ///
    /// Returns a typed failure when a session value is not an opaque token.
    pub fn with_session(&self, context: Option<&SessionContext>) -> Result<Self, ProviderError> {
        let Some(context) = context else {
            return Ok(self.clone());
        };
        if !self.profile.session_headers {
            return Ok(self.clone());
        }
        let headers = session_headers(&self.profile, context)?;
        let mut profile = self.profile.clone();
        profile.headers.extend(headers);
        Ok(Self { profile, token: self.token.clone(), http: self.http.clone() })
    }
}

/// Prompt-cache key for one request.
///
/// A session context wins: the key derives from its affinity so every turn of
/// the same session (and every child of it) reuses one cache lineage. Without
/// a context the caller falls back to its deterministic configuration hash.
#[must_use]
pub fn prompt_cache_key(context: Option<&SessionContext>) -> Option<String> {
    context.map(SessionContext::cache_key).filter(|key| {
        !key.is_empty()
            && key.len() <= MAX_SESSION_HEADER_VALUE_BYTES
            && key.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn profile(style: SessionHeaderStyle) -> EndpointProfile {
        EndpointProfile::new(
            "Example",
            "EXAMPLE_API_KEY",
            crate::TrustedEndpoint::parse("https://example.test/v1").expect("endpoint"),
            "model",
        )
        .with_session_header_style(style)
        .with_user_agent("ee-agent-test/1.2.3")
    }

    fn value<'a>(headers: &'a [(HeaderName, HeaderValue)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header, _)| header.as_str() == name)
            .and_then(|(_, value)| value.to_str().ok())
    }

    #[test]
    fn child_sends_parent_affinity_and_its_own_session_id() {
        let context = SessionContext::new("child").with_parent("parent");
        let headers = session_headers(&profile(SessionHeaderStyle::Ee), &context).expect("headers");
        assert_eq!(value(&headers, "x-ee-session-id"), Some("child"));
        assert_eq!(value(&headers, "x-session-affinity"), Some("parent"));
        assert_eq!(value(&headers, "x-session-id"), Some("parent"));
        assert_eq!(value(&headers, "x-ee-session"), Some("parent"));
        assert_eq!(value(&headers, "x-parent-session-id"), Some("parent"));
    }

    #[test]
    fn opencode_style_uses_the_reference_header_names() {
        let context = SessionContext::new("sess-1").with_project("proj");
        let headers =
            session_headers(&profile(SessionHeaderStyle::OpenCode), &context).expect("headers");
        assert_eq!(value(&headers, "x-opencode-session-id"), Some("sess-1"));
        assert_eq!(value(&headers, "x-opencode-session"), Some("sess-1"));
        assert_eq!(value(&headers, "x-opencode-project"), Some("proj"));
    }

    #[test]
    fn headers_never_carry_paths_usernames_or_credentials() {
        let mut context = SessionContext::new("s-1").with_client("ee-cli");
        context.project_id = Some("proj".into());
        let headers = session_headers(&profile(SessionHeaderStyle::Ee), &context).expect("headers");
        for (name, value) in &headers {
            let text = value.to_str().expect("ascii");
            if name.as_str() != "user-agent" {
                assert!(!text.contains('/'), "no path separator in {text:?}");
                assert!(!text.contains('\\'), "no path separator in {text:?}");
            }
            assert!(!text.to_ascii_lowercase().contains("sk-"));
        }
        assert_eq!(value(&headers, "user-agent"), Some("ee-agent-test/1.2.3"));
    }

    #[test]
    fn path_like_ids_are_rejected_without_echoing_the_value() {
        let context = SessionContext::new("../../etc/passwd");
        let error = session_headers(&profile(SessionHeaderStyle::Ee), &context).expect_err("error");
        assert!(error.to_string().contains("session_id"), "{error}");
        assert!(!error.to_string().contains("passwd"), "{error}");
    }

    #[test]
    fn unset_context_emits_nothing() {
        let transport = EndpointTransport::with_http(
            profile(SessionHeaderStyle::Ee),
            std::sync::Arc::new(|| Ok(crate::profile::BearerToken::new("token"))),
            reqwest::Client::new(),
        );
        let cloned = transport.with_session(None).expect("clone");
        assert!(cloned.profile().headers.is_empty());
    }

    #[test]
    fn two_sessions_with_identical_prompts_produce_different_cache_keys() {
        let first = SessionContext::new("session-a");
        let second = SessionContext::new("session-b");
        let first_key = prompt_cache_key(Some(&first)).expect("key");
        let second_key = prompt_cache_key(Some(&second)).expect("key");
        assert_ne!(first_key, second_key);
    }

    #[test]
    fn identity_falls_back_to_the_configuration_hash() {
        let _ = profile(SessionHeaderStyle::Ee);
        assert!(prompt_cache_key(None).is_none());
    }
}
