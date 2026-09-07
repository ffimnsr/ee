//! Proxy tool errors: backend failure types and stable web-context error codes.
use serde::{Deserialize, Serialize};

/// Error produced by an [`EeProxyBackend`] operation.
///
/// Backend failures never become JSON-RPC protocol errors: they surface as
/// `isError` tool results so the caller sees the message. The
/// `is_permission_denied` flag lets hosts distinguish host policy denials
/// from ordinary operation failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyToolError {
    /// Human-readable failure description surfaced as tool content.
    pub message: String,
    /// Whether the failure was a permission denial (host policy).
    pub is_permission_denied: bool,
}

impl std::fmt::Display for ProxyToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProxyToolError {}

/// Stable error codes for remote web-context tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebToolErrorCode {
    WebDisabled,
    WebSearchUnavailable,
    NetworkApprovalRequired,
    UrlRejected,
    DnsRejected,
    RedirectRejected,
    UnsupportedContentType,
    ResponseTooLarge,
    NetworkTimeout,
    NetworkFailure,
}

impl WebToolErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WebDisabled => "web_disabled",
            Self::WebSearchUnavailable => "web_search_unavailable",
            Self::NetworkApprovalRequired => "network_approval_required",
            Self::UrlRejected => "url_rejected",
            Self::DnsRejected => "dns_rejected",
            Self::RedirectRejected => "redirect_rejected",
            Self::UnsupportedContentType => "unsupported_content_type",
            Self::ResponseTooLarge => "response_too_large",
            Self::NetworkTimeout => "network_timeout",
            Self::NetworkFailure => "network_failure",
        }
    }
}

/// Stable structured failure for remote web-context tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebToolError {
    pub code: WebToolErrorCode,
    pub message: String,
}

impl WebToolError {
    #[must_use]
    pub fn new(code: WebToolErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl From<WebToolError> for ProxyToolError {
    fn from(error: WebToolError) -> Self {
        Self {
            message: format!("{}: {}", error.code.as_str(), error.message),
            is_permission_denied: matches!(error.code, WebToolErrorCode::NetworkApprovalRequired),
        }
    }
}

pub(crate) fn unavailable_proxy_tool<T>(name: &str) -> Result<T, ProxyToolError> {
    Err(ProxyToolError {
        message: format!("{name} are unavailable in this proxy mode"),
        is_permission_denied: false,
    })
}
