//! Web-context module: errors.
use super::*;

/// Rejected host-service configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebContextConfigError {
    ResponseByteLimit,
    TextByteLimit,
    SearchResultLimit,
    RedirectLimit,
    RequestTimeoutLimit,
    ConcurrencyLimit,
    SearchEndpoint,
    ProviderEndpoint,
    ProviderOptions,
    ProviderAuthorization,
    ProviderContentBudget,
    PreapprovedHost,
    BrowserRunRetryAttempts,
    BrowserRunRetryDelay,
}

impl fmt::Display for WebContextConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ResponseByteLimit => "response byte limit must be within supported bounds",
            Self::TextByteLimit => "text byte limit must be within response byte limit",
            Self::SearchResultLimit => "search result limit must be within supported bounds",
            Self::RedirectLimit => "redirect limit exceeds service maximum",
            Self::RequestTimeoutLimit => "request timeout must be within supported bounds",
            Self::ConcurrencyLimit => "concurrency limit must be within supported bounds",
            Self::SearchEndpoint => "search endpoint is not a strict HTTPS URL",
            Self::ProviderEndpoint => {
                "selected provider does not accept this endpoint configuration"
            }
            Self::ProviderOptions => "selected provider options are invalid",
            Self::ProviderAuthorization => "selected provider requires an authorization secret",
            Self::ProviderContentBudget => "provider content budget exceeds configured text limits",
            Self::PreapprovedHost => "preapproved host is invalid",
            Self::BrowserRunRetryAttempts => {
                "Browser Run retry attempts must be within supported bounds"
            }
            Self::BrowserRunRetryDelay => {
                "Browser Run retry delays must be within supported bounds"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for WebContextConfigError {}

/// Stable public failure codes for web-context calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebContextErrorCode {
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

impl WebContextErrorCode {
    /// Stable wire-format error code.
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

    pub(super) const fn message(self) -> &'static str {
        match self {
            Self::WebDisabled => "web context is disabled",
            Self::WebSearchUnavailable => "web search is unavailable",
            Self::NetworkApprovalRequired => "network host approval is required",
            Self::UrlRejected => "URL was rejected by web safety policy",
            Self::DnsRejected => "DNS resolution was rejected by web safety policy",
            Self::RedirectRejected => "redirect was rejected by web safety policy",
            Self::UnsupportedContentType => "response content type is not supported",
            Self::ResponseTooLarge => "response exceeds configured size limit",
            Self::NetworkTimeout => "network request timed out",
            Self::NetworkFailure => "network request failed",
        }
    }
}

impl fmt::Display for WebContextErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Redacted, typed web-context failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebContextError {
    pub code: WebContextErrorCode,
    pub message: String,
    /// Canonical host requiring approval. Never contains URL paths, queries,
    /// headers, credentials, or other request data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl WebContextError {
    pub fn new(code: WebContextErrorCode) -> Self {
        Self { code, message: code.message().to_owned(), host: None }
    }

    pub(super) fn network_approval_required(host: String) -> Self {
        Self {
            code: WebContextErrorCode::NetworkApprovalRequired,
            message: WebContextErrorCode::NetworkApprovalRequired.message().to_owned(),
            host: Some(host),
        }
    }
}

impl fmt::Display for WebContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WebContextError {}
