//! Tool-call dialect normalization.
//!
//! Provider-specific tool-call payloads (OpenAI/OpenRouter `function` calls,
//! Anthropic `tool_use` blocks, and local-model JSON tool calls) normalize
//! into framework [`ToolIntent`] values.  Payloads are validated strictly:
//! missing ids, names, or unparseable arguments are rejected with a
//! [`ModelError::InvalidResponse`] instead of producing partial tool calls,
//! so the loop fails closed on malformed provider output.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ModelError;
use crate::tools::ToolIntent;

/// Provider tool-call dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolCallDialect {
    /// OpenAI / OpenRouter `tool_calls` with `function` entries.
    OpenAi,
    /// Anthropic `content` blocks with `tool_use` entries.
    Anthropic,
    /// Local-model JSON tool calls with `{ id, name, arguments }` entries.
    LocalJson,
}

impl ToolCallDialect {
    /// Stable lowercase label for diagnostics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::LocalJson => "local_json",
        }
    }
}

/// Normalizes a provider tool-call payload into [`ToolIntent`] values.
///
/// Each dialect accepts either a bare array of tool-call entries or the
/// conventional wrapper object (`{"tool_calls": [...]}` for OpenAI,
/// `{"content": [...]}` for Anthropic, `{"tool_calls": [...]}` for
/// LocalJson).  Returns an [`ModelError::InvalidResponse`] on the first
/// malformed entry.
///
/// - OpenAI: `{ "id", "type": "function", "function": { "name",
///   "arguments": "<json string>" } }`
/// - Anthropic: `{ "id", "type": "tool_use", "name", "input": { ... } }`
/// - LocalJson: `{ "id", "name", "arguments": { ... } | "<json string>" }`
pub fn normalize_tool_calls(
    dialect: ToolCallDialect,
    payload: &Value,
) -> Result<Vec<ToolIntent>, ModelError> {
    let entries = tool_call_entries(dialect, payload)?;
    let mut intents = Vec::with_capacity(entries.len());
    for entry in entries {
        // Anthropic content blocks mix text and tool_use; only tool_use
        // blocks (or blocks without a type) are tool calls.
        if dialect == ToolCallDialect::Anthropic
            && entry.get("type").is_some_and(|kind| kind != "tool_use")
        {
            continue;
        }
        intents.push(match dialect {
            ToolCallDialect::OpenAi => normalize_openai(entry)?,
            ToolCallDialect::Anthropic => normalize_anthropic(entry)?,
            ToolCallDialect::LocalJson => normalize_local_json(entry)?,
        });
    }
    Ok(intents)
}

/// Extracts the tool-call entry array from a dialect payload, accepting
/// bare arrays and the conventional wrapper objects.
fn tool_call_entries(dialect: ToolCallDialect, payload: &Value) -> Result<&[Value], ModelError> {
    match payload {
        Value::Array(entries) => Ok(entries.as_slice()),
        Value::Object(object) => {
            let key = match dialect {
                ToolCallDialect::OpenAi | ToolCallDialect::LocalJson => "tool_calls",
                ToolCallDialect::Anthropic => "content",
            };
            object
                .get(key)
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .ok_or_else(|| malformed(dialect, format!("expected an array under `{key}`")))
        }
        other => Err(malformed(dialect, format!("expected an array, got {}", kind_of(other)))),
    }
}

fn normalize_openai(entry: &Value) -> Result<ToolIntent, ModelError> {
    let object = entry.as_object().ok_or_else(|| {
        malformed(ToolCallDialect::OpenAi, "tool call entry must be an object".into())
    })?;
    let id = string_field(object, "id", ToolCallDialect::OpenAi)?;
    let function = object
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| malformed(ToolCallDialect::OpenAi, "missing `function` object".into()))?;
    let name = string_field(function, "name", ToolCallDialect::OpenAi)?;
    let arguments = parse_arguments(ToolCallDialect::OpenAi, function.get("arguments"))?;
    Ok(ToolIntent::new(id, name, arguments))
}

fn normalize_anthropic(entry: &Value) -> Result<ToolIntent, ModelError> {
    let object = entry.as_object().ok_or_else(|| {
        malformed(ToolCallDialect::Anthropic, "tool call entry must be an object".into())
    })?;
    let id = string_field(object, "id", ToolCallDialect::Anthropic)?;
    let name = string_field(object, "name", ToolCallDialect::Anthropic)?;
    let arguments = object
        .get("input")
        .cloned()
        .ok_or_else(|| malformed(ToolCallDialect::Anthropic, "missing `input`".into()))?;
    Ok(ToolIntent::new(id, name, arguments))
}

fn normalize_local_json(entry: &Value) -> Result<ToolIntent, ModelError> {
    let object = entry.as_object().ok_or_else(|| {
        malformed(ToolCallDialect::LocalJson, "tool call entry must be an object".into())
    })?;
    let id = string_field(object, "id", ToolCallDialect::LocalJson)?;
    let name = string_field(object, "name", ToolCallDialect::LocalJson)?;
    let arguments = parse_arguments(ToolCallDialect::LocalJson, object.get("arguments"))?;
    Ok(ToolIntent::new(id, name, arguments))
}

fn string_field(
    object: &serde_json::Map<String, Value>,
    key: &str,
    dialect: ToolCallDialect,
) -> Result<String, ModelError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed(dialect, format!("missing non-empty `{key}`")))
}

/// Parses tool arguments: an object, or a JSON-encoded string (OpenAI and
/// LocalJson conventions).
fn parse_arguments(dialect: ToolCallDialect, value: Option<&Value>) -> Result<Value, ModelError> {
    match value {
        Some(Value::Object(object)) => Ok(Value::Object(object.clone())),
        Some(Value::String(raw)) => serde_json::from_str(raw)
            .map_err(|error| malformed(dialect, format!("unparseable `arguments` JSON: {error}"))),
        Some(other) => Err(malformed(
            dialect,
            format!("`arguments` must be an object or JSON string, got {}", kind_of(other)),
        )),
        None => Err(malformed(dialect, "missing `arguments`".into())),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn malformed(dialect: ToolCallDialect, detail: String) -> ModelError {
    ModelError::InvalidResponse(format!(
        "malformed {} tool-call payload: {detail}",
        dialect.label()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// OpenAI-style `tool_calls` with two function calls.
    const OPENAI_FIXTURE: &str = r#"{
      "tool_calls": [
        {
          "id": "call_1",
          "type": "function",
          "function": { "name": "read_file", "arguments": "{\"path\":\"/tmp/a.txt\"}" }
        },
        {
          "id": "call_2",
          "type": "function",
          "function": { "name": "write_file", "arguments": "{\"path\":\"/tmp/b.txt\",\"text\":\"hi\"}" }
        }
      ]
    }"#;

    /// Anthropic-style `content` with two `tool_use` blocks.
    const ANTHROPIC_FIXTURE: &str = r#"{
      "content": [
        { "type": "text", "text": "reading now" },
        { "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": { "path": "/tmp/a.txt" } },
        { "type": "tool_use", "id": "toolu_2", "name": "search", "input": { "query": "struct" } }
      ]
    }"#;

    /// Local-model JSON tool calls with object arguments.
    const LOCAL_FIXTURE: &str = r#"{
      "tool_calls": [
        { "id": "tc-1", "name": "read_file", "arguments": { "path": "/tmp/a.txt" } },
        { "id": "tc-2", "name": "grep", "arguments": "{\"pattern\":\"fn main\"}" }
      ]
    }"#;

    /// Malformed OpenAI fixture: `arguments` is not JSON.
    const MALFORMED_OPENAI_FIXTURE: &str = r#"{
      "tool_calls": [
        { "id": "call_1", "type": "function", "function": { "name": "read_file", "arguments": "{not json" } }
      ]
    }"#;

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture parses")
    }

    // ── OpenAI ──────────────────────────────────────────────────────────────

    #[test]
    fn openai_function_calls_normalize_to_tool_intents() {
        let intents = normalize_tool_calls(ToolCallDialect::OpenAi, &parse(OPENAI_FIXTURE))
            .expect("normalizes");
        assert_eq!(intents.len(), 2);
        assert_eq!(intents[0].tool_call_id, "call_1");
        assert_eq!(intents[0].name, "read_file");
        assert_eq!(intents[0].arguments, json!({ "path": "/tmp/a.txt" }));
        assert_eq!(intents[1].tool_call_id, "call_2");
        assert_eq!(intents[1].name, "write_file");
        assert_eq!(intents[1].arguments, json!({ "path": "/tmp/b.txt", "text": "hi" }));
    }

    #[test]
    fn openai_bare_array_is_accepted() {
        let payload = json!([
            { "id": "c", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }
        ]);
        let intents = normalize_tool_calls(ToolCallDialect::OpenAi, &payload).expect("normalizes");
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].name, "read_file");
    }

    #[test]
    fn openai_malformed_arguments_are_rejected() {
        let error = normalize_tool_calls(ToolCallDialect::OpenAi, &parse(MALFORMED_OPENAI_FIXTURE))
            .expect_err("malformed rejected");
        assert!(
            matches!(error, ModelError::InvalidResponse(ref reason) if reason.contains("openai"))
        );
    }

    #[test]
    fn openai_missing_fields_are_rejected() {
        for payload in [
            json!({ "tool_calls": [ { "type": "function", "function": { "name": "x", "arguments": "{}" } } ] }),
            json!({ "tool_calls": [ { "id": "c", "type": "function", "function": { "arguments": "{}" } } ] }),
            json!({ "tool_calls": [ { "id": "c", "type": "function", "function": { "name": "x" } } ] }),
            json!({ "tool_calls": [ { "id": "c", "type": "function", "function": { "name": "x", "arguments": 42 } } ] }),
            json!({ "tool_calls": "not an array" }),
            json!({}),
        ] {
            let error = normalize_tool_calls(ToolCallDialect::OpenAi, &payload)
                .expect_err("malformed rejected");
            assert!(matches!(error, ModelError::InvalidResponse(_)), "{payload}");
        }
    }

    // ── Anthropic ────────────────────────────────────────────────────────────

    #[test]
    fn anthropic_tool_use_normalizes_skipping_text_blocks() {
        let intents = normalize_tool_calls(ToolCallDialect::Anthropic, &parse(ANTHROPIC_FIXTURE))
            .expect("normalizes");
        assert_eq!(intents.len(), 2);
        assert_eq!(intents[0].tool_call_id, "toolu_1");
        assert_eq!(intents[0].name, "read_file");
        assert_eq!(intents[0].arguments, json!({ "path": "/tmp/a.txt" }));
        assert_eq!(intents[1].name, "search");
    }

    #[test]
    fn anthropic_malformed_entries_are_rejected() {
        for payload in [
            json!({ "content": [ { "id": "t", "input": {} } ] }),
            json!({ "content": [ { "id": "t", "name": "x" } ] }),
            json!({ "content": [ { "type": "tool_use", "name": "x", "input": {} } ] }),
            json!({ "content": 7 }),
        ] {
            let error = normalize_tool_calls(ToolCallDialect::Anthropic, &payload)
                .expect_err("malformed rejected");
            assert!(
                matches!(error, ModelError::InvalidResponse(ref reason) if reason.contains("anthropic")),
                "{payload}"
            );
        }
    }

    // ── LocalJson ────────────────────────────────────────────────────────────

    #[test]
    fn local_json_tool_calls_normalize() {
        let intents = normalize_tool_calls(ToolCallDialect::LocalJson, &parse(LOCAL_FIXTURE))
            .expect("normalizes");
        assert_eq!(intents.len(), 2);
        assert_eq!(intents[0].tool_call_id, "tc-1");
        assert_eq!(intents[0].arguments, json!({ "path": "/tmp/a.txt" }));
        assert_eq!(intents[1].arguments, json!({ "pattern": "fn main" }));
    }

    #[test]
    fn local_json_malformed_entries_are_rejected() {
        for payload in [
            json!({ "tool_calls": [ { "name": "x", "arguments": {} } ] }),
            json!({ "tool_calls": [ { "id": "c", "arguments": {} } ] }),
            json!({ "tool_calls": [ { "id": "c", "name": "x", "arguments": [] } ] }),
            json!("nope"),
        ] {
            let error = normalize_tool_calls(ToolCallDialect::LocalJson, &payload)
                .expect_err("malformed rejected");
            assert!(matches!(error, ModelError::InvalidResponse(_)), "{payload}");
        }
    }

    // ── Shared ───────────────────────────────────────────────────────────────

    #[test]
    fn empty_tool_call_list_normalizes_to_empty_intents() {
        for dialect in
            [ToolCallDialect::OpenAi, ToolCallDialect::Anthropic, ToolCallDialect::LocalJson]
        {
            let intents = normalize_tool_calls(dialect, &json!([])).expect("empty array");
            assert!(intents.is_empty(), "{dialect:?}");
            let wrapper = match dialect {
                ToolCallDialect::Anthropic => json!({ "content": [] }),
                _ => json!({ "tool_calls": [] }),
            };
            let intents = normalize_tool_calls(dialect, &wrapper).expect("empty wrapper");
            assert!(intents.is_empty(), "{dialect:?}");
        }
    }

    #[test]
    fn empty_string_ids_and_names_are_rejected() {
        for dialect in
            [ToolCallDialect::OpenAi, ToolCallDialect::Anthropic, ToolCallDialect::LocalJson]
        {
            let payload = match dialect {
                ToolCallDialect::OpenAi => json!([
                    { "id": "", "type": "function", "function": { "name": "x", "arguments": "{}" } }
                ]),
                ToolCallDialect::Anthropic => {
                    json!([ { "id": "t", "name": "", "input": {} } ])
                }
                ToolCallDialect::LocalJson => {
                    json!([ { "id": "c", "name": "", "arguments": {} } ])
                }
            };
            let error = normalize_tool_calls(dialect, &payload).expect_err("empty name rejected");
            assert!(matches!(error, ModelError::InvalidResponse(_)), "{dialect:?}");
        }
    }

    #[test]
    fn dialect_labels_are_stable() {
        assert_eq!(ToolCallDialect::OpenAi.label(), "openai");
        assert_eq!(ToolCallDialect::Anthropic.label(), "anthropic");
        assert_eq!(ToolCallDialect::LocalJson.label(), "local_json");
    }

    #[test]
    fn dialects_roundtrip_through_json() {
        for dialect in
            [ToolCallDialect::OpenAi, ToolCallDialect::Anthropic, ToolCallDialect::LocalJson]
        {
            let json = serde_json::to_string(&dialect).expect("serializes");
            let restored: ToolCallDialect = serde_json::from_str(&json).expect("parses");
            assert_eq!(restored, dialect);
        }
    }
}
