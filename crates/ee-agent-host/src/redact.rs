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

/// Redacts common credential forms from untrusted free-form text when exact
/// secret values are unavailable. This conservative pass complements
/// `redact_secret_values`; callers should still apply known-value redaction first.
#[must_use]
pub fn redact_sensitive_text(text: &str) -> String {
    let mut output = Vec::new();
    let mut private_key = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-----BEGIN ") && trimmed.ends_with(" PRIVATE KEY-----") {
            private_key = true;
            output.push(String::from("[REDACTED PRIVATE KEY]"));
            continue;
        }
        if private_key {
            if trimmed.starts_with("-----END ") && trimmed.ends_with(" PRIVATE KEY-----") {
                private_key = false;
            }
            continue;
        }
        output.push(redact_sensitive_line(line));
    }
    output.join("\n")
}

fn redact_sensitive_line(line: &str) -> String {
    let lower_line = line.to_ascii_lowercase();
    let trimmed_lower = lower_line.trim_start();
    if ["authorization:", "proxy-authorization:", "cookie:", "set-cookie:"]
        .iter()
        .any(|prefix| trimmed_lower.starts_with(prefix))
        || [
            "access_token=",
            "refresh_token=",
            "id_token=",
            "api_key=",
            "apikey=",
            "password=",
            "client_secret=",
        ]
        .iter()
        .any(|marker| lower_line.contains(marker))
    {
        return String::from("[REDACTED SECRET-BEARING LINE]");
    }

    for separator in ['=', ':'] {
        if let Some((key, _)) = line.split_once(separator)
            && is_secret_key(key.trim().trim_matches(|character: char| {
                !character.is_alphanumeric() && character != '_' && character != '-'
            }))
        {
            return String::from("[REDACTED SECRET-LIKE ASSIGNMENT]");
        }
    }

    let mut words = line.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    for index in 0..words.len() {
        let lower = words[index].to_ascii_lowercase();
        if lower == "bearer" && index + 1 < words.len() {
            words[index + 1] = String::from("***");
            continue;
        }
        let token = words[index]
            .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .to_ascii_lowercase();
        if ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "sk-", "xoxb-", "xoxp-"]
            .iter()
            .any(|prefix| token.starts_with(prefix))
            || (token.starts_with("akia") && token.len() >= 16)
        {
            words[index] = String::from("***");
            continue;
        }
        if let Some(scheme) = lower.find("://") {
            let authority_start = scheme + 3;
            if let Some(at) = words[index][authority_start..].find('@') {
                let at = authority_start + at;
                if words[index][authority_start..at].contains(':') {
                    words[index].replace_range(authority_start..at, "***");
                }
            }
        }
        if is_secret_key(words[index].trim_matches(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '-'
        })) && index + 2 < words.len()
            && matches!(words[index + 1].to_ascii_lowercase().as_str(), "is" | "was")
        {
            words[index + 2] = String::from("***");
        }
    }
    words.join(" ")
}

/// Recursively redacts JSON values held by secret-like object keys.
///
/// This preserves non-sensitive structure for diagnostics and exports without
/// exposing credentials embedded in protocol payloads.
#[must_use]
pub fn redact_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact_json).collect())
        }
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    let value = if is_secret_key(key) {
                        serde_json::Value::String(String::from("***"))
                    } else {
                        redact_json(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        _ => value.clone(),
    }
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
    fn json_redacts_nested_secret_like_keys() {
        let value = serde_json::json!({
            "token": "top-secret",
            "safe": [
                { "password": "nope" },
                { "nested": { "api_key": "also-secret", "name": "kept" } }
            ]
        });

        assert_eq!(
            redact_json(&value),
            serde_json::json!({
                "token": "***",
                "safe": [
                    { "password": "***" },
                    { "nested": { "api_key": "***", "name": "kept" } }
                ]
            })
        );
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

    #[test]
    fn free_form_redaction_covers_common_credential_shapes() {
        let text = concat!(
            "Authorization: Bearer abc.def.ghi\n",
            "request uses Bearer standalone-token\n",
            "fetch https://alice:password@example.test/data\n",
            "callback https://example.test/?access_token=query-secret\n",
            "Cookie: session=cookie-secret\n",
            "github pat ghp_1234567890abcdef\n",
            "API token is prose-secret\n",
            "-----BEGIN PRIVATE KEY-----\n",
            "private-key-material\n",
            "-----END PRIVATE KEY-----\n",
            "ordinary validation text stays"
        );
        let redacted = redact_sensitive_text(text);
        for secret in [
            "abc.def.ghi",
            "standalone-token",
            "alice:password",
            "query-secret",
            "cookie-secret",
            "ghp_1234567890abcdef",
            "prose-secret",
            "private-key-material",
        ] {
            assert!(!redacted.contains(secret), "leaked {secret}");
        }
        assert!(redacted.contains("ordinary validation text stays"));
    }
}
