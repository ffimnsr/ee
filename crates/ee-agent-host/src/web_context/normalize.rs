//! Web-context module: normalize.
use super::*;

pub(super) fn normalize_approved_host(host: &str) -> Result<String, WebContextConfigError> {
    let value = host.trim();
    if value.is_empty() || value.contains('/') || value.contains('?') || value.contains('#') {
        return Err(WebContextConfigError::PreapprovedHost);
    }
    let url = Url::parse(&format!("https://{value}/"))
        .map_err(|_| WebContextConfigError::PreapprovedHost)?;
    if url.username() != "" || url.password().is_some() || url.port().is_some() {
        return Err(WebContextConfigError::PreapprovedHost);
    }
    let normalized = url.host_str().ok_or(WebContextConfigError::PreapprovedHost)?;
    if normalized != value.to_ascii_lowercase() {
        return Err(WebContextConfigError::PreapprovedHost);
    }
    Ok(normalized.to_owned())
}

pub(super) fn search_headers(config: &AgentWebContextConfig) -> BTreeMap<String, String> {
    let mut headers = fixed_headers();
    if let Some(authorization) = &config.search_authorization {
        authorization.apply_to(config.provider, &mut headers);
    }
    headers
}

pub(super) fn fixed_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("accept".to_owned(), "text/plain, text/html, application/json".to_owned()),
        ("user-agent".to_owned(), "ee-web-context/1".to_owned()),
    ])
}

pub(super) fn header_value<'a>(
    headers: &'a BTreeMap<String, String>,
    name: &str,
) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

pub(super) fn content_length_exceeds(headers: &BTreeMap<String, String>, limit: usize) -> bool {
    header_value(headers, "content-length")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .is_some_and(|size| size > limit)
}

pub(super) fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

pub(super) fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

pub(super) fn normalize_text(value: &str, max_bytes: usize) -> String {
    let (bounded, _) = truncate_utf8(value, max_bytes);
    bounded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Keeps one normalized search response within its configured aggregate text
/// budget. URLs and hosts remain intact for usable source records; later
/// snippets/results are truncated or omitted rather than retaining unbounded
/// provider content.
pub(super) fn truncate_search_results(
    results: &mut Vec<WebSearchResult>,
    max_bytes: usize,
) -> bool {
    let mut bounded = Vec::with_capacity(results.len());
    let mut remaining = max_bytes;
    let mut truncated = false;

    for mut result in results.drain(..) {
        let metadata_bytes = result.title.len() + result.url.len() + result.host.len();
        if metadata_bytes > remaining {
            truncated = true;
            break;
        }
        remaining -= metadata_bytes;
        let (snippet, snippet_truncated) = truncate_utf8(&result.snippet, remaining);
        remaining -= snippet.len();
        result.snippet = snippet;
        bounded.push(result);
        truncated |= snippet_truncated;
    }

    *results = bounded;
    truncated
}

pub(super) fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !matches!(
        (a, b, c),
        (0, _, _)
            | (10, _, _)
            | (100, 64..=127, _)
            | (127, _, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, _)
            | (192, 88, 99)
            | (192, 168, _)
            | (192, 175, 48)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

pub(super) fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if address.is_unspecified() || address.is_loopback() || address.is_multicast() {
        return false;
    }
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    if matches!(
        segments[0],
        0x0000..=0x00ff | 0x0100 | 0xfc00..=0xfdff | 0xfe80..=0xfebf | 0xfec0..=0xfeff
    ) {
        return false;
    }
    !matches!((segments[0], segments[1]), (0x2001, 0x0db8) | (0x2001, 0x0002) | (0x2001, 0x0000))
}
