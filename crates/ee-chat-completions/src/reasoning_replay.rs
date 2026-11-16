//! Chat Completions reasoning replay.
//!
//! Provider reasoning state is replayed only to the model that produced it. The
//! helpers here shape the assistant fields a chat request carries back
//! (`reasoning_details` verbatim, plus the scalar field the provider used) and
//! merge streamed detail deltas so the replayed array keeps the provider's own
//! entry boundaries.

use ee_agent_orchestrator::{ModelContent, ModelMessage};
use serde_json::{Value, json};

use crate::native;

/// Replays the assistant tool calls a turn's observations answer.
///
/// A tool message without the call it answers is rejected by every dialect, so
/// the assistant turn is kept in the transcript and re-encoded here.
pub(crate) fn replay_tool_calls(entry: &mut Value, message: &ModelMessage, empty_content: bool) {
    let calls: Vec<Value> = message
        .content
        .iter()
        .filter_map(|block| match block {
            ModelContent::ToolCall { tool_call_id, name, arguments } => Some(json!({
                "id": tool_call_id,
                "type": "function",
                "function": { "name": name, "arguments": arguments.to_string() },
            })),
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        return;
    }
    if let Some(object) = entry.as_object_mut() {
        object.insert("tool_calls".into(), Value::Array(calls));
        // OpenAI-compatible schemas require `content: null` when a turn only
        // asks for tools.
        if empty_content {
            object.insert("content".into(), Value::Null);
        }
    }
}

/// Merges replayable assistant reasoning fields into one chat message.
///
/// `reasoning_details` keeps every entry and field it arrived with, so an
/// OpenRouter upstream sees the same `index`-keyed structure it emitted. A
/// scalar field is echoed under the name the provider used, unless the route
/// configures one explicitly.
pub(crate) fn replay_reasoning(
    entry: &mut Value,
    message: &ModelMessage,
    model: &str,
    configured_field: Option<&str>,
) {
    let Some(state) = message.native.as_ref().and_then(|native| native::replayable(native, model))
    else {
        return;
    };
    let Some(object) = entry.as_object_mut() else {
        return;
    };
    if let Some(details) = state.get("reasoning_details").filter(|value| value.is_array()) {
        object.insert("reasoning_details".into(), details.clone());
    }
    let scalar = configured_field
        .and_then(|name| {
            state
                .get(name)
                .and_then(Value::as_str)
                .or(message.reasoning_summary.as_deref())
                .filter(|text| !text.is_empty())
                .map(|text| (name.to_string(), text.to_string()))
        })
        .or_else(|| {
            native::CHAT_REPLAY_FIELDS.iter().find_map(|name| {
                state
                    .get(*name)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| ((*name).to_string(), text.to_string()))
            })
        });
    if let Some(opaque) = state.get("reasoning_opaque").and_then(Value::as_str) {
        object.insert("reasoning_opaque".into(), Value::String(opaque.to_string()));
    }
    if let Some((name, text)) = scalar {
        object.insert(name, Value::String(text));
    }
}

/// Collects the assistant reasoning fields this dialect can replay verbatim.
///
/// `reasoning_details` is preserved including `index`. A scalar spelling is
/// added only when the provider actually sent one: a fully structured reply is
/// replayed as details alone, which is what the provider expects back.
pub(crate) fn native_reasoning_of(message: &Value) -> Option<Value> {
    let mut object = serde_json::Map::new();
    let details = message.get("reasoning_details").filter(|value| value.is_array()).cloned();
    let scalar = ["reasoning_content", "reasoning", "reasoning_text"].iter().find_map(|key| {
        message
            .get(*key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| ((*key).to_string(), text.to_string()))
    });
    if let Some(details) = details {
        object.insert("reasoning_details".into(), details);
        if let Some((key, text)) = scalar {
            object.insert(key, Value::String(text));
        }
    } else if let Some((key, text)) = scalar {
        object.insert(key, Value::String(text));
    }
    if let Some(opaque) =
        message.get("reasoning_opaque").and_then(Value::as_str).filter(|text| !text.is_empty())
    {
        object.insert("reasoning_opaque".into(), Value::String(opaque.to_string()));
    }
    (!object.is_empty()).then_some(Value::Object(object))
}

/// Appends one streamed reasoning detail, merging it into the previous entry
/// when the dialect streams one entry in pieces.
pub(crate) fn append_reasoning_detail(details: &mut Vec<Value>, detail: Value) {
    if let Some(previous) = details.last()
        && let Some(merged) = merge_reasoning_detail(previous, &detail)
    {
        if let Some(last) = details.last_mut() {
            *last = merged;
        }
        return;
    }
    details.push(detail);
}

/// Merges two consecutive reasoning details of the same kind.
///
/// Text and summary deltas accumulate into one entry so the replayed array keeps
/// the provider's entry boundaries. Encrypted entries are opaque and never
/// merge, and differing identity fields start a new entry.
fn merge_reasoning_detail(previous: &Value, detail: &Value) -> Option<Value> {
    fn kind(value: &Value) -> &str {
        value.get("type").and_then(Value::as_str).unwrap_or_default()
    }
    if kind(previous) != kind(detail) {
        return None;
    }
    let text_key = match kind(previous) {
        "reasoning.text" | "text" => "text",
        "reasoning.summary" | "summary" => "summary",
        // Encrypted and unknown entries stay exactly as they arrived.
        _ => return None,
    };
    for key in ["id", "format", "signature"] {
        let left = previous.get(key).filter(|value| !value.is_null());
        let right = detail.get(key).filter(|value| !value.is_null());
        if let (Some(left), Some(right)) = (left, right)
            && left != right
        {
            return None;
        }
    }
    let mut merged = previous.clone();
    let text = format!(
        "{}{}",
        previous.get(text_key).and_then(Value::as_str).unwrap_or_default(),
        detail.get(text_key).and_then(Value::as_str).unwrap_or_default()
    );
    if let Some(object) = merged.as_object_mut() {
        if !text.is_empty() {
            object.insert(text_key.into(), Value::String(text));
        }
        for key in ["id", "format", "index", "signature"] {
            if let Some(value) = detail.get(key).filter(|value| !value.is_null()) {
                object.insert(key.into(), value.clone());
            }
        }
    }
    Some(merged)
}
