//! Responses-protocol wire helpers shared by the buffer and stream decoders.
//!
//! Keeping these out of the codec module holds `responses.rs` under the file
//! size budget while keeping provider-shape knowledge in one place: hosted tool
//! item names, provider-executed call construction, per-item stream identity,
//! and the typed stale-continuation classification.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::ToolCall;
use ee_agent_acp_server::ProviderError;
use ee_agent_orchestrator::ToolChoice;

/// Hosted tool item types the reference agent reports as provider-executed
/// calls, mapped to the stable tool name the transcript records.
pub(crate) fn hosted_tool_name(item_type: &str) -> Option<&'static str> {
    match item_type {
        "web_search_call" => Some("web_search"),
        "file_search_call" => Some("file_search"),
        "code_interpreter_call" => Some("code_interpreter"),
        "mcp_call" => Some("mcp"),
        _ => None,
    }
}

/// Builds a provider-executed call and keeps the provider's item as its result.
pub(crate) fn hosted_tool_call(item: &Value) -> ToolCall {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let name = hosted_tool_name(kind).unwrap_or(kind).to_string();
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!("{kind}-{}", item.get("id").map(Value::to_string).unwrap_or_default())
        });
    ToolCall::new(id, name, item.get("action").cloned().unwrap_or_else(|| json!({})))
        .provider_executed()
        .with_provider_result(item.clone())
}

/// Identity of a tool item that is starting or finishing, when it has one.
pub(crate) fn started_tool(item: &Value) -> Option<(String, String)> {
    let kind = item.get("type").and_then(Value::as_str)?;
    if kind == "function_call" {
        let id = item.get("call_id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
        let name = item.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())?;
        return Some((id.to_string(), name.to_string()));
    }
    let name = hosted_tool_name(kind)?;
    let call = hosted_tool_call(item);
    Some((call.id, name.to_string()))
}

/// Renders a nested error object as one diagnostic line.
pub(crate) fn error_detail(error: &Value) -> String {
    match error.get("message").and_then(Value::as_str) {
        Some(message) => message.to_string(),
        None => error.to_string(),
    }
}

/// Classifies a stale continuation anchor so the adapter can retry full.
///
/// The code is the provider's own stable identifier, never message text.
pub(crate) fn continuation_or_codec_error(
    label: &str,
    error: &Value,
    detail: String,
) -> ProviderError {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| error.pointer("/error/code").and_then(Value::as_str));
    if code == Some("previous_response_not_found") {
        return ProviderError::StaleContinuation(detail);
    }
    crate::wire::codec_error(label, detail)
}

/// Canonical stop signal for a buffered payload.
pub(crate) fn finish_reason(status: Option<&str>, value: &Value) -> String {
    match status {
        Some("completed") | None => String::from("stop"),
        Some("incomplete") => {
            let reason = value
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("incomplete");
            match reason {
                "max_output_tokens" => String::from("length"),
                other => other.to_string(),
            }
        }
        Some(other) => other.to_string(),
    }
}

/// Lowers a tool choice into the Responses field value.
pub(crate) fn responses_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({ "type": "function", "name": name }),
    }
}

/// Deterministic prompt-cache key for one route/session configuration.
pub(crate) fn prompt_cache_key(model: &str, system_prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update([0]);
    hasher.update(system_prompt.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect()
}

/// Parses the provider's output index from a streamed function-call event.
pub(crate) fn output_index(value: &Value, label: &str) -> Result<u64, ProviderError> {
    value.get("output_index").and_then(Value::as_u64).ok_or_else(|| {
        crate::wire::codec_error(label, "streamed function call event is missing output_index")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_items_map_to_stable_names() {
        for (kind, name) in [
            ("web_search_call", "web_search"),
            ("file_search_call", "file_search"),
            ("code_interpreter_call", "code_interpreter"),
            ("mcp_call", "mcp"),
        ] {
            assert_eq!(hosted_tool_name(kind), Some(name));
        }
        assert_eq!(hosted_tool_name("message"), None);
    }

    #[test]
    fn previous_response_not_found_is_typed() {
        let error = continuation_or_codec_error(
            "test",
            &json!({ "code": "previous_response_not_found" }),
            String::from("gone"),
        );
        assert!(matches!(error, ProviderError::StaleContinuation(_)));
    }
}
