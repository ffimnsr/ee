//! OpenAI Responses request-item encoders.
//!
//! The transcript projection lives here so the main codec stays readable:
//! input items, function-call items, reasoning-item replay, tool arrays, and
//! compaction checkpoints. Every encoder emits only documented Responses fields.

use ee_agent_orchestrator::{ModelContent, ModelMessage, ModelRole, ToolDefinition};
use serde_json::{Value, json};

use crate::{content_text, tool_call_id_of};

/// Converts one transcript message into an input item.
///
/// Messages whose rendered text is empty are omitted: Responses rejects empty
/// text parts.
pub(crate) fn input_item(message: &ModelMessage) -> Option<Value> {
    // A provider-executed result is already part of the provider's response; a
    // synthetic `function_call_output` would reference a call it never saw as a
    // local function.
    //
    // The provider's own item is replayed verbatim instead: hosted search, file
    // search, code interpreter, and MCP calls carry their state in that item, and
    // dropping it makes the next turn ask the model to continue a call it can no
    // longer see.
    if message.role == ModelRole::Tool
        && message.metadata.contains_key(ee_agent_orchestrator::PROVIDER_EXECUTED_RESULT_KEY)
    {
        return hosted_result_item(message);
    }
    let text = content_text(&message.content);
    match message.role {
        ModelRole::Tool => Some(json!({
            "type": "function_call_output",
            "call_id": tool_call_id_of(&message.content),
            "output": text,
        })),
        ModelRole::User | ModelRole::Subagent | ModelRole::System | ModelRole::Assistant => {
            let parts = response_parts(message.role, &message.content);
            if parts.is_empty() {
                return None;
            }
            let role = match message.role {
                ModelRole::System => "system",
                ModelRole::Assistant => "assistant",
                _ => "user",
            };
            Some(json!({
                "type": "message",
                "role": role,
                "content": parts,
            }))
        }
    }
}

/// Replays the provider's own item for a provider-executed tool result.
///
/// Only documented hosted item types travel this path. Anything else (a future
/// hosted kind, or a call the executor recorded locally) stays out of the
/// request rather than being re-serialized into an item shape the provider did
/// not send.
fn hosted_result_item(message: &ModelMessage) -> Option<Value> {
    let item = message.content.iter().find_map(|block| match block {
        ModelContent::ToolResult { result, .. } => result.structured_output.as_ref(),
        _ => None,
    })?;
    let kind = item.get("type").and_then(Value::as_str)?;
    crate::responses_wire::hosted_tool_name(kind)?;
    Some(item.clone())
}

/// Builds Responses content parts; text keeps the dialect's part names and
/// image payloads become `input_image` items.
fn response_parts(role: ModelRole, content: &[ModelContent]) -> Vec<Value> {
    let text_part = if role == ModelRole::Assistant { "output_text" } else { "input_text" };
    let mut parts = Vec::new();
    for block in content {
        match block {
            ModelContent::Text(text) => push_response_text(&mut parts, text_part, text),
            ModelContent::ToolResult { result, .. } => {
                push_response_text(&mut parts, text_part, &result.summary_text())
            }
            ModelContent::FileReference { path } => {
                push_response_text(&mut parts, text_part, &format!("[file:{path}]"))
            }
            ModelContent::TerminalReference { terminal_id } => {
                push_response_text(&mut parts, text_part, &format!("[terminal:{terminal_id}]"))
            }
            ModelContent::Image { data, mime_type, .. } => {
                parts.push(input_image(data, mime_type));
            }
            ModelContent::Audio { mime_type, .. } => {
                push_response_text(&mut parts, text_part, &format!("[audio:{mime_type}]"))
            }
            ModelContent::EmbeddedResource { uri, mime_type, text, blob } => match (text, blob) {
                (Some(text), _) => {
                    let rendered = format!("[resource:{uri}]\n{text}");
                    push_response_text(&mut parts, text_part, &rendered)
                }
                (None, Some(blob)) => match mime_type.as_deref() {
                    Some(mime) if mime.starts_with("image/") => {
                        parts.push(input_image(blob, mime));
                    }
                    Some("application/pdf") => {
                        parts.push(input_file(uri, blob, "application/pdf"));
                    }
                    mime => push_response_text(
                        &mut parts,
                        text_part,
                        &format!("[resource:{uri}:{}]", mime.unwrap_or("application/octet-stream")),
                    ),
                },
                (None, None) => {
                    push_response_text(&mut parts, text_part, &format!("[resource:{uri}]"))
                }
            },
            _ => {} // future content kinds stay out of the request
        }
    }
    parts
}

fn push_response_text(parts: &mut Vec<Value>, part_type: &str, text: &str) {
    if !text.is_empty() {
        parts.push(json!({ "type": part_type, "text": text }));
    }
}

fn input_image(data: &str, mime_type: &str) -> Value {
    json!({
        "type": "input_image",
        "image_url": format!("data:{mime_type};base64,{data}"),
    })
}

/// Responses file part for a PDF payload.
fn input_file(uri: &str, data: &str, mime_type: &str) -> Value {
    let filename =
        uri.rsplit('/').next().filter(|name| !name.is_empty()).unwrap_or("attachment.pdf");
    json!({
        "type": "input_file",
        "filename": filename,
        "file_data": format!("data:{mime_type};base64,{data}"),
    })
}

/// Flat Responses function tools for one request.
pub(crate) fn tools_array(tools: &[ToolDefinition]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|definition| {
                json!({
                    "type": "function",
                    "name": definition.name,
                    "description": definition.description,
                    "parameters": definition.input_schema,
                })
            })
            .collect(),
    )
}

/// Top-level `compaction` input items carried by one transcript message.
pub(crate) fn compaction_items(message: &ModelMessage) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ModelContent::CompactionCheckpoint(checkpoint) => {
                let mut item = json!({
                    "type": "compaction",
                    "encrypted_content": checkpoint.encrypted(),
                });
                if let Some(id) = checkpoint.id.as_deref() {
                    item["id"] = Value::String(id.to_string());
                }
                Some(item)
            }
            _ => None,
        })
        .collect()
}

/// Replays the assistant tool calls a turn's observations answer.
///
/// Responses keeps a function call as its own input item, so the assistant turn
/// is kept in the transcript and re-encoded here; a `function_call_output`
/// without its matching call is rejected.
pub(crate) fn function_call_items(message: &ModelMessage) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ModelContent::ToolCall { tool_call_id, name, arguments } => {
                // Hosted calls are replayed by the provider that ran them, not as
                // local function calls.
                if message.metadata.contains_key(&format!(
                    "{}{tool_call_id}",
                    ee_agent_orchestrator::PROVIDER_EXECUTED_METADATA_PREFIX
                )) {
                    return None;
                }
                Some(json!({
                    "type": "function_call",
                    "call_id": tool_call_id,
                    "name": name,
                    "arguments": arguments.to_string(),
                }))
            }
            _ => None,
        })
        .collect()
}

/// Key holding the replayable Responses reasoning items.
pub(crate) const REASONING_ITEMS: &str = "reasoning_items";

/// Reasoning input items a transcript message may replay to `model`.
///
/// Only documented fields are replayed: `id` addresses the item, `summary`
/// carries its text, and `encrypted_content` is the stateless proof of
/// generation that lets the model resume a tool loop without server storage.
pub(crate) fn replay_reasoning_items(
    message: &ModelMessage,
    provider: Option<&str>,
    model: &str,
) -> Vec<Value> {
    let Some(state) =
        message.native.as_ref().and_then(|native| crate::replayable_for(native, provider, model))
    else {
        return Vec::new();
    };
    let Some(items) = state.get(REASONING_ITEMS).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut replayed = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        let mut replay = json!({ "type": "reasoning", "summary": [] });
        if let Some(summary) = item.get("summary").filter(|summary| summary.is_array()) {
            replay["summary"] = summary.clone();
        }
        for key in ["id", "encrypted_content"] {
            if let Some(value) = item.get(key).filter(|value| !value.is_null()) {
                replay[key] = value.clone();
            }
        }
        let has_summary = replay["summary"].as_array().is_some_and(|summary| !summary.is_empty());
        if has_summary || replay.get("encrypted_content").is_some() {
            replayed.push(replay);
        }
    }
    replayed
}
