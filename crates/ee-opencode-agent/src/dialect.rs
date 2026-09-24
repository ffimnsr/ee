//! Small helpers shared by the three OpenCode protocol codecs.
//!
//! Everything provider-neutral (transport, retry boundary, SSE framing, usage
//! and argument parsing) lives in `ee-chat-completions`; this module only holds
//! the few shapes the dialects agree on inside this crate.

use ee_acp_agent_server::ProviderError;
use serde_json::{Value, json};

/// Builds a codec failure whose text names the routed endpoint, never a
/// credential.
pub(crate) fn codec_error(label: &str, detail: impl std::fmt::Display) -> ProviderError {
    ProviderError::BackendFailure(format!("{label}: {detail}"))
}

/// Parses tool arguments from their wire form.
///
/// An empty value means "no arguments"; malformed JSON or a non-object keeps the
/// call identity with `Null` arguments, so the orchestrator rejects the call
/// before any tool side effect.
pub(crate) fn parse_arguments(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) if value.is_object() => value,
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_arguments_mean_no_arguments() {
        assert_eq!(parse_arguments(""), json!({}));
        assert_eq!(parse_arguments("  "), json!({}));
    }

    #[test]
    fn object_arguments_pass_through() {
        assert_eq!(parse_arguments("{\"path\":\"/x\"}"), json!({ "path": "/x" }));
    }

    #[test]
    fn malformed_or_non_object_arguments_fail_closed() {
        assert_eq!(parse_arguments("{\"path\":\"/x"), Value::Null);
        assert_eq!(parse_arguments("[1,2]"), Value::Null);
        assert_eq!(parse_arguments("null"), Value::Null);
    }

    #[test]
    fn codec_errors_name_the_label_only() {
        assert_eq!(
            codec_error("OpenCode go kimi-k3", "response failed").to_string(),
            "provider backend failure: OpenCode go kimi-k3: response failed"
        );
        assert!(!codec_error("OpenCode zen gpt-5.5", "boom").to_string().contains("Bearer"));
    }
}
