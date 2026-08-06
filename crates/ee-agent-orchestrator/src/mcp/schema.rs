//! MCP input schema → `ToolDefinition` input schema conversion (Phase 12).
//!
//! MCP tool `inputSchema` values are JSON Schema objects; they pass through
//! to the orchestrator's `ToolDefinition` mostly unchanged (the loop's
//! argument validation and the provider-facing schema compiler both consume
//! plain JSON schema).  Structural violations fail closed before a tool is
//! advertised: a non-object schema, or a declared `type` other than
//! `object`, rejects the tool.

use serde_json::{Value, json};

/// Converts one MCP `inputSchema` into a `ToolDefinition` input schema.
///
/// The schema must be a JSON object whose `type` (when present) is
/// `"object"`; the result is the same object, with `"type": "object"`
/// normalized in so downstream validators always see an object schema.
///
/// # Errors
///
/// Returns a bounded diagnostic when the schema is structurally invalid.
pub(crate) fn convert_input_schema(schema: &Value) -> Result<Value, String> {
    let object = schema
        .as_object()
        .ok_or_else(|| "MCP tool inputSchema must be a JSON object".to_string())?;
    if let Some(kind) = object.get("type").and_then(Value::as_str)
        && kind != "object"
    {
        return Err(format!("MCP tool inputSchema type must be \"object\", got {kind:?}"));
    }
    let mut converted = object.clone();
    if !converted.contains_key("type") {
        converted.insert("type".into(), json!("object"));
    }
    Ok(Value::Object(converted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_schema_passes_through() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        });
        assert_eq!(convert_input_schema(&schema).expect("valid"), schema);
    }

    #[test]
    fn type_less_schema_is_normalized_to_object() {
        let schema = json!({ "properties": { "path": { "type": "string" } } });
        let converted = convert_input_schema(&schema).expect("valid");
        assert_eq!(converted["type"], "object");
    }

    #[test]
    fn nested_schema_shapes_are_preserved() {
        let schema = json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "old_text": { "type": "string" } },
                        "required": ["old_text"],
                    },
                },
                "limit": { "type": "integer" },
            },
            "required": ["edits"],
            "additionalProperties": false,
        });
        let converted = convert_input_schema(&schema).expect("valid");
        assert_eq!(converted, schema);
    }

    #[test]
    fn non_object_schema_is_rejected() {
        let error = convert_input_schema(&json!("not an object")).expect_err("rejected");
        assert!(error.contains("JSON object"), "{error}");
        let error = convert_input_schema(&json!(["array"])).expect_err("rejected");
        assert!(error.contains("JSON object"), "{error}");
    }

    #[test]
    fn non_object_type_is_rejected() {
        let error = convert_input_schema(&json!({ "type": "array" })).expect_err("rejected");
        assert!(error.contains("must be \"object\""), "{error}");
        let error = convert_input_schema(&json!({ "type": "string" })).expect_err("rejected");
        assert!(error.contains("must be \"object\""), "{error}");
    }
}
