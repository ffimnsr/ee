//! Sensitive-data detection and redaction.
//!
//! Values that look like credentials (API keys, JWTs, high-entropy tokens)
//! are replaced with [`REDACTED`] before memory insertion, trace export, and
//! final-response assembly.  `KEY=value` assignments whose key is sensitive
//! are masked by [`redact_assignments`]; standalone token-like values are
//! masked by [`redact_values`].  Detection is deliberately conservative:
//! short identifiers, plain words, and repeated filler text are never treated
//! as secrets, so redaction is deterministic and non-destructive for ordinary
//! content.

/// Marker replacing sensitive values.
pub const REDACTED: &str = "[redacted]";
/// Substrings that mark a key or assignment name as sensitive.
pub const SENSITIVE_KEY_MARKERS: [&str; 8] =
    ["api_key", "apikey", "token", "secret", "password", "passwd", "authorization", "credential"];

/// Whether a key name (or assignment name) looks sensitive.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEY_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Masks `SENSITIVE=value` assignments inside a string value, leaving the
/// assignment name visible for diagnostics but hiding the value.
#[must_use]
pub fn redact_assignments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut segment_start = 0usize;
    let mut index = 0usize;
    while index < text.len() {
        if bytes[index] == b'=' {
            let name = text[segment_start..index].trim();
            if is_sensitive_key(name) {
                let mut end = index + 1;
                while end < text.len() {
                    let byte = bytes[end];
                    if byte.is_ascii_whitespace() || byte == b',' || byte == b'"' || byte == b'\'' {
                        break;
                    }
                    end += 1;
                }
                while end < text.len() && !text.is_char_boundary(end) {
                    end += 1;
                }
                out.push_str(&text[segment_start..index]);
                out.push_str(&format!("={REDACTED}"));
                index = end;
                segment_start = end;
                continue;
            }
        }
        index += 1;
    }
    out.push_str(&text[segment_start..]);
    out
}

/// Whether a single token looks like a credential.
///
/// Conservative rules: prefixed keys (`sk-`, `ghp_`, ...) with a plausible
/// value length, and long high-entropy runs (32+ chars with 5+ distinct
/// characters).  Short words, paths, ids, and repeated filler never match.
#[must_use]
pub fn is_secret_like(token: &str) -> bool {
    for prefix in ["sk-", "pk-", "rk-", "ghp_", "gho_", "xoxb-", "xoxp-", "AKIA"] {
        if token.len() >= prefix.len() + 4 && token.starts_with(prefix) {
            return true;
        }
    }
    token.len() >= 32 && distinct_chars(token) >= 5
}

/// Number of distinct characters in a token; a repeated-filler proxy for
/// entropy.  All-`x` padding and short ids never pass the threshold.
fn distinct_chars(token: &str) -> usize {
    token.chars().collect::<std::collections::HashSet<_>>().len()
}

/// Masks standalone secret-like tokens inside `text`, leaving everything else
/// byte-identical.  JWT spans (`eyJ...`) are scanned first because they embed
/// `.` separators inside the token.
#[must_use]
pub fn redact_values(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut segment_start = 0usize;
    let mut index = 0usize;
    while index < text.len() {
        if text[index..].starts_with("eyJ") {
            let start = index;
            index += 3;
            while index < text.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || bytes[index] == b'_'
                    || bytes[index] == b'-'
                    || bytes[index] == b'.')
            {
                index += 1;
            }
            while index < text.len() && !text.is_char_boundary(index) {
                index += 1;
            }
            let span = &text[start..index];
            if span.len() >= 24 && span.contains('.') {
                out.push_str(&text[segment_start..start]);
                out.push_str(REDACTED);
                segment_start = index;
            }
            continue;
        }
        let byte = bytes[index];
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            let start = index;
            while index < text.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || bytes[index] == b'_'
                    || bytes[index] == b'-')
            {
                index += 1;
            }
            let token = &text[start..index];
            if is_secret_like(token) {
                out.push_str(&text[segment_start..start]);
                out.push_str(REDACTED);
                segment_start = index;
            }
            continue;
        }
        if byte.is_ascii() {
            index += 1;
            continue;
        }
        // Multi-byte character: advance to the next char boundary so later
        // slices never split a character.
        index += 1;
        while index < text.len() && !text.is_char_boundary(index) {
            index += 1;
        }
    }
    out.push_str(&text[segment_start..]);
    out
}

/// Combined redaction: sensitive assignments first, then standalone
/// token-like values.
#[must_use]
pub fn redact(text: &str) -> String {
    let assigned = redact_assignments(text);
    redact_values(&assigned)
}

/// Redaction guard used at every sensitive boundary (memory insertion, trace
/// export, final-response assembly).
#[derive(Debug, Clone, Copy, Default)]
pub struct SensitiveDataGuard;

impl SensitiveDataGuard {
    /// Creates a guard.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Redacts sensitive assignments and token-like values in `text`.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        redact(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_values_are_redacted() {
        assert_eq!(redact("sk-live-1234567890"), REDACTED);
        assert_eq!(redact("OPENROUTER_API_KEY=sk-live-123"), "OPENROUTER_API_KEY=[redacted]");
        assert_eq!(redact("token=ghp_abcdefghijklmnop"), "token=[redacted]");
        assert_eq!(redact("Authorization: Bearer sk-abc123"), "Authorization: Bearer [redacted]");
    }

    #[test]
    fn jwt_shaped_values_are_redacted() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature";
        assert!(jwt.len() >= 24);
        assert_eq!(redact(jwt), REDACTED);
        assert_eq!(redact("session cookie eyJx.y.zw"), "session cookie eyJx.y.zw");
    }

    #[test]
    fn env_var_like_secrets_are_masked() {
        assert_eq!(
            redact("export OPENROUTER_API_KEY=sk-other-456"),
            "export OPENROUTER_API_KEY=[redacted]"
        );
        assert_eq!(redact("password=hunter2"), "password=[redacted]");
        assert_eq!(redact("path=/work/a.txt"), "path=/work/a.txt");
    }

    #[test]
    fn ordinary_content_is_untouched() {
        for text in [
            "hello world",
            "file contents",
            "/tmp/work/out.rs",
            "task-1",
            "4444444",
            "xxxx",
            "the quick brown fox",
            "cargo check: passed — clean",
        ] {
            assert_eq!(redact(text), text, "{text:?} must not be redacted");
        }
    }

    #[test]
    fn long_repeated_filler_is_not_secret_like() {
        let filler = "x".repeat(4000);
        assert!(!is_secret_like(&filler));
        assert_eq!(redact_values(&filler), filler);
    }

    #[test]
    fn guard_chains_assignments_and_values() {
        let guard = SensitiveDataGuard::new();
        assert_eq!(guard.redact("sk-live-1234567890"), REDACTED);
        assert_eq!(guard.redact("note api_key=sk-x"), "note api_key=[redacted]");
        assert_eq!(guard.redact("plain"), "plain");
    }

    #[test]
    fn key_detection_is_case_insensitive() {
        for key in ["api_key", "API_KEY", "BearerToken", "password_hash", "clientSecret"] {
            assert!(is_sensitive_key(key), "{key}");
        }
        for key in ["path", "title", "session_id", "tool_call_id", "message"] {
            assert!(!is_sensitive_key(key), "{key}");
        }
    }

    #[test]
    fn redact_is_deterministic() {
        let input = "OPENROUTER_API_KEY=sk-live-123 and \
                     eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJhIjoiYiJ9.sig";
        let first = redact(input);
        for _ in 0..5 {
            assert_eq!(redact(input), first);
        }
    }
}
