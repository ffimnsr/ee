//! Small helpers shared by the three OpenCode protocol codecs.
//!
//! Everything provider-neutral (transport, retry boundary, SSE framing, usage
//! and argument parsing) lives in `ee-chat-completions`; this module only holds
//! the few shapes the dialects agree on inside this crate.

use std::borrow::Cow;

use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::{ModelMessage, ModelResponse};
use ee_chat_completions::EndpointProfile;
use serde_json::Value;

#[cfg(test)]
use serde_json::json;

/// Builds a codec failure whose text names the routed endpoint, never a
/// credential.
pub(crate) fn codec_error(label: &str, detail: impl std::fmt::Display) -> ProviderError {
    ee_chat_completions::wire::codec_error(label, detail)
}

/// Applies the catalog modality gate and media-size budget before a request
/// body exists.
///
/// Returns the borrowed transcript when nothing changed, so the common path
/// never copies the conversation.
pub(crate) fn guarded_transcript<'a>(
    profile: &EndpointProfile,
    transcript: &'a [ModelMessage],
) -> Result<Cow<'a, [ModelMessage]>, ProviderError> {
    match ee_agent_orchestrator::guard_transcript(profile.modalities, &profile.model, transcript) {
        Ok(None) => Ok(Cow::Borrowed(transcript)),
        Ok(Some(bounded)) => Ok(Cow::Owned(bounded)),
        Err(detail) => Err(codec_error(&profile.label, detail)),
    }
}

/// Fills a catalog cost and seals the response for the producing model.
pub(crate) fn finalize(mut response: ModelResponse, profile: &EndpointProfile) -> ModelResponse {
    ee_agent_orchestrator::apply_catalog_cost(&mut response, profile.cost);
    ee_chat_completions::seal_response(response, &profile.model)
}

/// Parses tool arguments from their wire form.
///
/// An empty value means "no arguments"; malformed JSON or a non-object keeps the
/// call identity with `Null` arguments, so the orchestrator rejects the call
/// before any tool side effect.
pub(crate) fn parse_arguments(raw: &str) -> Value {
    ee_chat_completions::wire::parse_arguments(raw)
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
