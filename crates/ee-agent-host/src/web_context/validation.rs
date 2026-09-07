//! Web-context module: validation.
use super::normalize::is_public_ipv4;
use super::normalize::is_public_ipv6;
use super::*;

/// Parses a strict HTTPS URL suitable for an outbound fetch target.
pub fn validate_https_url(input: &str) -> Result<Url, WebContextError> {
    let url =
        Url::parse(input).map_err(|_| WebContextError::new(WebContextErrorCode::UrlRejected))?;
    if url.scheme() != "https"
        || url.cannot_be_a_base()
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err(WebContextError::new(WebContextErrorCode::UrlRejected));
    }
    Ok(url)
}

/// Parses configured SearXNG endpoint. Provider credentials belong only in the
/// secret-backed authorization header, never in a URL query component.
pub(super) fn validate_search_endpoint_url(input: &str) -> Result<Url, WebContextError> {
    let url = validate_https_url(input)?;
    if url.query().is_some() {
        return Err(WebContextError::new(WebContextErrorCode::UrlRejected));
    }
    Ok(url)
}

/// Produces a provenance-safe URL. Request queries can carry signed URLs or
/// bearer-like credentials, so source records retain only origin and path.
pub(super) fn redact_url_for_display(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}

pub(super) fn redact_url_text_for_display(input: &str) -> String {
    Url::parse(input)
        .map_or_else(|_| String::from("CONFIGURED"), |url| redact_url_for_display(&url))
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;

    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

pub(super) fn canonical_url_host(url: &Url) -> Result<String, WebContextError> {
    url.host_str()
        .map(str::to_owned)
        .ok_or_else(|| WebContextError::new(WebContextErrorCode::UrlRejected))
}

/// Returns whether an IP address is globally routable enough for web retrieval.
/// Documentation, carrier-grade NAT, transition, and reserved ranges fail closed.
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}
