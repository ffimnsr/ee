//! Secret redaction utilities (Phase 7).
//!
//! Secrets must never reach debug logs, stderr panes, test snapshots, or
//! approval text.  This module owns the shared marker list and value
//! redaction helpers; `ee-cli` delegates its env/header display redaction to
//! it so the policy lives in exactly one place.

/// Secret-like key markers (case-insensitive substring match).
const SECRET_MARKERS: [&str; 6] = ["TOKEN", "KEY", "SECRET", "PASSWORD", "AUTH", "CREDENTIAL"];

/// Whether a key name (environment variable, header, form field) looks
/// secret-like.
#[must_use]
pub fn is_secret_key(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_MARKERS.iter().any(|marker| upper.contains(marker))
}

/// Redacts the display form of a key/value pair: secret-like keys always
/// show `***`; other values pass through unchanged.
#[must_use]
pub fn redact_pair(name: &str, value: &str) -> String {
    if is_secret_key(name) { String::from("***") } else { value.to_string() }
}

/// Redacts any occurrence of the given secret values inside `text`.
///
/// Longest values are replaced first so a value that is a substring of
/// another secret cannot leak the longer one.  Partial structured values
/// (e.g. a JSON line containing `"apiToken":"abc123"`) are covered because
/// the raw value itself is replaced wherever it appears.
#[must_use]
pub fn redact_secret_values(text: &str, secrets: &[String]) -> String {
    if secrets.is_empty() {
        return text.to_string();
    }
    let mut sorted = secrets.to_vec();
    sorted.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut redacted = text.to_string();
    for secret in sorted {
        if secret.is_empty() {
            continue;
        }
        redacted = redacted.replace(&secret, "***");
    }
    redacted
}

/// Redacts header values whose names look secret-like (MCP HTTP diagnostics).
#[must_use]
pub fn redact_headers(
    headers: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String)> {
    headers.iter().map(|(name, value)| (name.clone(), redact_pair(name, value))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn secret_key_markers_are_case_insensitive() {
        for name in [
            "API_TOKEN",
            "api_token",
            "SECRET_KEY",
            "password",
            "AUTH_TOKEN",
            "AWS_CREDENTIALS",
            "GITHUB_TOKEN",
        ] {
            assert!(is_secret_key(name), "{name} should be secret-like");
        }
        for name in ["EE_AGENT_MODE", "PATH", "HOME", "LANG"] {
            assert!(!is_secret_key(name), "{name} should not be secret-like");
        }
    }

    #[test]
    fn secret_pairs_redact_to_stars() {
        assert_eq!(redact_pair("API_TOKEN", "abc"), "***");
        assert_eq!(redact_pair("PATH", "/usr/bin"), "/usr/bin");
    }

    #[test]
    fn secret_values_are_replaced_everywhere() {
        let text = "using token abc123 and password s3cret in \"abc123\"";
        let redacted = redact_secret_values(text, &["abc123".to_string(), "s3cret".to_string()]);
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("s3cret"));
        assert_eq!(redacted.matches("***").count(), 3);
    }

    #[test]
    fn partial_structured_values_are_redacted() {
        let json = r#"{"apiToken":"secret-xyz","ok":true}"#;
        let redacted = redact_secret_values(json, &["secret-xyz".to_string()]);
        assert_eq!(redacted, r#"{"apiToken":"***","ok":true}"#);
    }

    #[test]
    fn longer_secrets_win_over_substrings() {
        let text = "aabb cc aabbcc";
        let redacted = redact_secret_values(text, &["aabb".to_string(), "aabbcc".to_string()]);
        // The longer value is replaced first; its replacement must not then
        // be re-matched by the shorter one.
        assert_eq!(redacted, "*** cc ***");
    }

    #[test]
    fn headers_redact_secret_like_names_only() {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer top-secret".to_string());
        headers.insert("X-Trace-Id".to_string(), "trace-1".to_string());
        let redacted = redact_headers(&headers);
        let map: BTreeMap<String, String> = redacted.into_iter().collect();
        assert_eq!(map.get("Authorization").map(String::as_str), Some("***"));
        assert_eq!(map.get("X-Trace-Id").map(String::as_str), Some("trace-1"));
    }

    #[test]
    fn empty_secret_list_passes_text_through() {
        let text = "no secrets here";
        assert_eq!(redact_secret_values(text, &[]), text);
    }
}
