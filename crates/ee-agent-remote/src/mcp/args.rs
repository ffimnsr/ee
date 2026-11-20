//! Tool argument validation and bounded result construction.

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData, JsonObject,
};
use serde_json::Value;

use ee_mcp::proxy::MAX_TOOL_ARGUMENT_BYTES;

/// Largest serialized tool result before it is replaced by bounded text.
pub const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;

/// Requires an empty argument object.
///
/// # Errors
///
/// `invalid_params` when any argument is present.
pub fn require_no_arguments(request: &CallToolRequestParams) -> Result<(), ErrorData> {
    match &request.arguments {
        Some(arguments) if !arguments.is_empty() => Err(ErrorData::invalid_params(
            format!("tool {} takes no arguments", request.name),
            None,
        )),
        _ => Ok(()),
    }
}

/// Returns the argument object, enforcing the shared byte cap.
///
/// # Errors
///
/// `invalid_params` when arguments are missing or exceed the cap.
pub fn require_arguments(request: &CallToolRequestParams) -> Result<&JsonObject, ErrorData> {
    let Some(arguments) = &request.arguments else {
        return Err(ErrorData::invalid_params(
            format!("tool {} requires arguments", request.name),
            None,
        ));
    };
    let bytes = serde_json::to_vec(arguments)
        .map_err(|error| ErrorData::invalid_params(format!("invalid arguments: {error}"), None))?
        .len();
    if bytes > MAX_TOOL_ARGUMENT_BYTES {
        return Err(ErrorData::invalid_params(
            format!("arguments exceed the {MAX_TOOL_ARGUMENT_BYTES}-byte cap"),
            None,
        ));
    }
    Ok(arguments)
}

/// Required non-empty string argument.
///
/// # Errors
///
/// `invalid_params` when missing, not a string, or empty.
pub fn require_string<'a>(arguments: &'a JsonObject, name: &str) -> Result<&'a str, ErrorData> {
    let value = arguments.get(name).and_then(Value::as_str).ok_or_else(|| {
        ErrorData::invalid_params(format!("missing string argument {name:?}"), None)
    })?;
    if value.trim().is_empty() {
        return Err(ErrorData::invalid_params(
            format!("argument {name:?} must not be empty"),
            None,
        ));
    }
    Ok(value)
}

/// Optional non-empty string argument.
///
/// # Errors
///
/// `invalid_params` when present but not a non-empty string.
pub fn optional_string<'a>(
    arguments: &'a JsonObject,
    name: &str,
) -> Result<Option<&'a str>, ErrorData> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value)),
        Some(_) => Err(ErrorData::invalid_params(
            format!("argument {name:?} must be a non-empty string"),
            None,
        )),
    }
}

/// Optional unsigned integer argument.
///
/// # Errors
///
/// `invalid_params` when present but not `u64`.
pub fn optional_u64(arguments: &JsonObject, name: &str) -> Result<Option<u64>, ErrorData> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            ErrorData::invalid_params(format!("argument {name:?} must be an integer"), None)
        }),
    }
}

/// Optional string array argument.
///
/// # Errors
///
/// `invalid_params` when present but not an array of strings.
pub fn optional_string_array(arguments: &JsonObject, name: &str) -> Result<Vec<String>, ErrorData> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_string).ok_or_else(|| {
                    ErrorData::invalid_params(
                        format!("argument {name:?} must contain strings"),
                        None,
                    )
                })
            })
            .collect(),
        Some(_) => Err(ErrorData::invalid_params(
            format!("argument {name:?} must be an array of strings"),
            None,
        )),
    }
}

/// Converts a typed result into a bounded successful tool response.
///
/// Structured content is returned when the serialized payload fits the cap;
/// otherwise the response degrades to one truncated text block with an
/// explicit marker so drivers never receive a silently cut JSON document.
#[must_use]
pub fn complete_json(value: Value) -> CallToolResponse {
    let value = crate::redact::redact_outbound(&value);
    let text = serde_json::to_string(&value).unwrap_or_default();
    if text.len() <= MAX_TOOL_RESULT_BYTES {
        return CallToolResponse::from(CallToolResult::structured(value));
    }
    let marker = format!("\n… [truncated at {MAX_TOOL_RESULT_BYTES} bytes]");
    let mut boundary = MAX_TOOL_RESULT_BYTES.saturating_sub(marker.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    CallToolResponse::from(CallToolResult::success(vec![ContentBlock::text(format!(
        "{}{marker}",
        &text[..boundary]
    ))]))
}

/// Converts a tool-level failure into an `isError` result.
#[must_use]
pub fn tool_error(message: impl Into<String>) -> CallToolResponse {
    let message = crate::redact::redact_text(&message.into());
    CallToolResponse::from(CallToolResult::error(vec![ContentBlock::text(message)]))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn result_caps_degrade_to_marked_text() {
        let value = json!({ "blob": "x".repeat(MAX_TOOL_RESULT_BYTES + 100) });
        let CallToolResponse::Complete(result) = complete_json(value) else {
            panic!("expected a complete tool result");
        };
        let text = serde_json::to_value(result).expect("serialize response").to_string();
        assert!(text.contains("truncated at"));
        assert!(text.len() < MAX_TOOL_RESULT_BYTES + 1024);
    }

    #[test]
    fn argument_validators_fail_closed() {
        let request = CallToolRequestParams::new("t");
        assert!(require_no_arguments(&request).is_ok());
        assert!(require_arguments(&request).is_err());
        let mut arguments = JsonObject::new();
        arguments.insert("name".into(), json!(""));
        assert!(require_string(&arguments, "name").is_err());
        assert!(optional_u64(&arguments, "count").is_ok());
        assert!(optional_string_array(&arguments, "dirs").is_ok());
    }
}
