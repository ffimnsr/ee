//! Outgoing transcript normalization and interrupted-turn repair.
//!
//! Every request projection passes through [`normalize_transcript`] so a stored
//! conversation can be replayed even after a cancelled tool execution, a
//! provider stream cut, or a runtime delta inserted mid-turn. The stored
//! transcript is never mutated: callers normalize a clone.
//!
//! Rules mirror the reference agent's send-time normalization:
//!
//! - every assistant tool call is answered by exactly one tool result;
//! - missing results become a deterministic failure observation;
//! - empty tool output is labeled in the request copy only;
//! - system messages that land between a call and its results move after them;
//! - a turn marked [`TURN_ERROR_METADATA_KEY`] keeps only calls that ran (or
//!   whose arguments failed to parse) and drops its provider-native state.
//!
//! Tool dispatch concurrency is intentionally different from the reference:
//! `ee` parallelizes read-only tools only, capped by
//! `OrchestratorConfig::max_parallel_tools`, while the reference dispatches with
//! unbounded concurrency behind its permission gate. Normalization must not
//! assume either rule, so it only reasons about call/result correlation.

use std::collections::{BTreeSet, HashMap};

use crate::model::{
    ModelContent, ModelMessage, ModelRole, PROVIDER_EXECUTED_METADATA_PREFIX,
    TOOL_NAME_METADATA_KEY,
};

/// Text substituted for a tool call that never received an observation.
pub const MISSING_TOOL_RESULT_TEXT: &str = "Tool result missing";

/// Text substituted for a tool observation whose output was empty.
pub const EMPTY_TOOL_OUTPUT_TEXT: &str = "(no tool output)";

/// Text substituted for a call aborted by cancellation or a provider failure.
pub const ABORTED_TOOL_RESULT_TEXT: &str = "Tool execution aborted";

/// Metadata key marking a synthetic observation.
pub const SYNTHETIC_TOOL_RESULT_KEY: &str = "ee_synthetic_tool_result";

/// Metadata key marking an assistant turn that ended in an error or cut.
pub const TURN_ERROR_METADATA_KEY: &str = "ee_turn_error";

/// Metadata key prefix recording a tool call's bounded argument parse error.
pub const ARGUMENT_ERROR_METADATA_PREFIX: &str = "ee_argument_error:";

/// Normalizes one outgoing transcript without mutating the input.
#[must_use]
pub fn normalize_transcript(messages: &[ModelMessage]) -> Vec<ModelMessage> {
    let ordered = hold_system_until_results(messages);
    let repaired = synthesize_and_repair(ordered);
    label_empty_outputs(repaired)
}

/// Moves system messages that sit between an assistant tool call and its
/// results behind those results.
fn hold_system_until_results(messages: &[ModelMessage]) -> Vec<ModelMessage> {
    let mut output = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        output.push(message.clone());
        let call_ids = tool_call_ids(message);
        if call_ids.is_empty() {
            index += 1;
            continue;
        }
        index += 1;
        let mut results = Vec::new();
        let mut held_system = Vec::new();
        while index < messages.len() {
            let candidate = &messages[index];
            if candidate.role == ModelRole::System {
                held_system.push(candidate.clone());
                index += 1;
                continue;
            }
            if candidate.role == ModelRole::Tool
                && tool_result_id(candidate).is_some_and(|id| call_ids.contains(id))
            {
                results.push(candidate.clone());
                index += 1;
                continue;
            }
            break;
        }
        output.extend(results);
        output.extend(held_system);
    }
    output
}

/// Answers unresolved calls and trims failed turns.
fn synthesize_and_repair(messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
    let result_ids: BTreeSet<String> =
        messages.iter().filter_map(tool_result_id).map(str::to_string).collect();
    // Only the turn a user message opened is still active. Earlier assistant
    // turns are historical context (a forked subagent transcript can contain a
    // still-pending parent call), so they are never answered or trimmed here.
    let active_start = messages
        .iter()
        .rposition(|message| matches!(message.role, ModelRole::User | ModelRole::Subagent))
        .map_or(0, |index| index + 1);
    let call_names: HashMap<String, String> = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ModelContent::ToolCall { tool_call_id, name, .. } => {
                Some((tool_call_id.clone(), name.clone()))
            }
            _ => None,
        })
        .collect();

    let mut output = Vec::with_capacity(messages.len());
    for (index, message) in messages.into_iter().enumerate() {
        if message.role == ModelRole::Tool {
            output.push(reconcile_tool_name(message, &call_names));
            continue;
        }
        if message.role != ModelRole::Assistant || index < active_start {
            output.push(message);
            continue;
        }
        let failed =
            message.metadata.get(TURN_ERROR_METADATA_KEY).map(String::as_str) == Some("true");
        let mut message = message;
        let mut synthetic = Vec::new();
        let mut kept = Vec::with_capacity(message.content.len());
        for block in std::mem::take(&mut message.content) {
            let (tool_call_id, name) = match &block {
                ModelContent::ToolCall { tool_call_id, name, .. } => {
                    (tool_call_id.clone(), name.clone())
                }
                _ => {
                    kept.push(block);
                    continue;
                }
            };
            let answered = result_ids.contains(tool_call_id.as_str())
                || provider_executed(&message, &tool_call_id);
            if failed {
                // Keep only calls that have an observation or whose arguments
                // failed to parse; a call that never ran must not be replayed.
                if answered || has_argument_error(&message, &tool_call_id) {
                    kept.push(block);
                }
                continue;
            }
            kept.push(block);
            if answered {
                continue;
            }
            let result = match argument_error(&message, &tool_call_id) {
                Some(error) => crate::tools::ToolResult {
                    success: false,
                    text_output: error.to_string(),
                    structured_output: None,
                    error_kind: Some(crate::tools::ToolErrorKind::InvalidArguments),
                },
                None => crate::tools::ToolResult {
                    success: false,
                    text_output: MISSING_TOOL_RESULT_TEXT.to_string(),
                    structured_output: None,
                    error_kind: Some(crate::tools::ToolErrorKind::Backend),
                },
            };
            synthetic.push(ModelMessage::tool_result(tool_call_id, result).with_metadata([
                (SYNTHETIC_TOOL_RESULT_KEY.to_string(), String::from("true")),
                (TOOL_NAME_METADATA_KEY.to_string(), name),
            ]));
        }
        message.content = kept;
        if failed {
            message.native = None;
            if message.content.is_empty() {
                continue;
            }
        }
        output.push(message);
        output.extend(synthetic);
    }
    output
}

/// Copies the originating call's name onto its tool result.
fn reconcile_tool_name(
    mut message: ModelMessage,
    call_names: &HashMap<String, String>,
) -> ModelMessage {
    let Some(id) = tool_result_id(&message) else {
        return message;
    };
    if let Some(name) = call_names.get(id) {
        message.metadata.insert(TOOL_NAME_METADATA_KEY.to_string(), name.clone());
    }
    message
}

/// Labels empty tool output in the request copy.
fn label_empty_outputs(mut messages: Vec<ModelMessage>) -> Vec<ModelMessage> {
    for message in &mut messages {
        for block in &mut message.content {
            if let ModelContent::ToolResult { result, .. } = block
                && result.text_output.is_empty()
            {
                result.text_output = EMPTY_TOOL_OUTPUT_TEXT.to_string();
            }
        }
    }
    messages
}

fn tool_call_ids(message: &ModelMessage) -> BTreeSet<&str> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ModelContent::ToolCall { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_result_id(message: &ModelMessage) -> Option<&str> {
    message.content.iter().find_map(|block| match block {
        ModelContent::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
        _ => None,
    })
}

fn provider_executed(message: &ModelMessage, tool_call_id: &str) -> bool {
    let key = format!("{PROVIDER_EXECUTED_METADATA_PREFIX}{tool_call_id}");
    message.metadata.get(key.as_str()).map(String::as_str) == Some("true")
}

fn has_argument_error(message: &ModelMessage, tool_call_id: &str) -> bool {
    argument_error(message, tool_call_id).is_some()
}

fn argument_error<'a>(message: &'a ModelMessage, tool_call_id: &str) -> Option<&'a str> {
    let key = format!("{ARGUMENT_ERROR_METADATA_PREFIX}{tool_call_id}");
    message.metadata.get(key.as_str()).map(String::as_str)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::ModelMessage;

    fn call(id: &str, name: &str) -> ModelMessage {
        ModelMessage::tool_call(id, name, json!({ "path": "a" }))
    }

    #[test]
    fn missing_result_gains_exactly_one_synthetic_observation() {
        let messages = vec![ModelMessage::text(ModelRole::User, "read"), call("tc-1", "read_file")];
        let normalized = normalize_transcript(&messages);
        let synthesized: Vec<&ModelMessage> = normalized
            .iter()
            .filter(|message| {
                message.metadata.get(SYNTHETIC_TOOL_RESULT_KEY).map(String::as_str) == Some("true")
            })
            .collect();
        assert_eq!(synthesized.len(), 1);
        let ModelContent::ToolResult { tool_call_id, result } = &synthesized[0].content[0] else {
            panic!("expected a tool result");
        };
        assert_eq!(tool_call_id, "tc-1");
        assert_eq!(result.text_output, MISSING_TOOL_RESULT_TEXT);
        assert!(!result.success);

        // Normalizing twice must not add a second observation.
        let again = normalize_transcript(&normalized);
        assert_eq!(
            again
                .iter()
                .filter(|message| {
                    message.metadata.get(SYNTHETIC_TOOL_RESULT_KEY).map(String::as_str)
                        == Some("true")
                })
                .count(),
            1
        );
    }

    #[test]
    fn empty_output_is_labeled_only_in_the_request_copy() {
        let mut transcript = crate::model::Transcript::new();
        transcript.push_tool_result("tc-1", crate::tools::ToolResult::success(""));
        let normalized = normalize_transcript(transcript.messages());
        let ModelContent::ToolResult { result, .. } = &normalized[0].content[0] else {
            panic!("expected a tool result");
        };
        assert_eq!(result.text_output, EMPTY_TOOL_OUTPUT_TEXT);
        let ModelContent::ToolResult { result, .. } = &transcript.messages()[0].content[0] else {
            panic!("expected a tool result");
        };
        assert!(result.text_output.is_empty(), "the stored transcript stays empty");
    }

    #[test]
    fn system_message_between_a_call_and_its_result_moves_after_the_result() {
        let messages = vec![
            call("tc-1", "read_file"),
            ModelMessage::text(ModelRole::System, "runtime delta"),
            ModelMessage::tool_result("tc-1", crate::tools::ToolResult::success("body")),
        ];
        let normalized = normalize_transcript(&messages);
        assert_eq!(normalized[0].role, ModelRole::Assistant);
        assert_eq!(normalized[1].role, ModelRole::Tool);
        assert_eq!(normalized[2].role, ModelRole::System);
        assert_eq!(normalized[2].text_content(), "runtime delta");
    }

    #[test]
    fn failed_turn_drops_unrun_calls_and_keeps_input_errors() {
        let mut message = call("tc-1", "read_file");
        message.content.push(ModelContent::ToolCall {
            tool_call_id: "tc-2".into(),
            name: "read_file".into(),
            arguments: json!(null),
        });
        message.content.push(ModelContent::ToolCall {
            tool_call_id: "tc-3".into(),
            name: "write_file".into(),
            arguments: json!({}),
        });
        message.metadata.insert(TURN_ERROR_METADATA_KEY.into(), "true".into());
        message.metadata.insert(
            format!("{ARGUMENT_ERROR_METADATA_PREFIX}tc-2"),
            String::from("tool arguments were not valid JSON: expected value"),
        );
        let native = crate::model::OpaqueProviderState::new(json!({ "signature": "sig" }));
        message.native = native;
        let messages = vec![
            ModelMessage::text(ModelRole::User, "go"),
            message,
            ModelMessage::tool_result("tc-1", crate::tools::ToolResult::success("body")),
        ];
        let normalized = normalize_transcript(&messages);
        let assistant = normalized
            .iter()
            .find(|message| message.role == ModelRole::Assistant)
            .expect("assistant turn");
        let ids: Vec<&str> = assistant
            .content
            .iter()
            .filter_map(|block| match block {
                ModelContent::ToolCall { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["tc-1", "tc-2"], "the executed and input-error calls survive");
        assert!(assistant.native.is_none(), "cut provider state is dropped");
        assert!(
            normalized
                .iter()
                .all(|message| !message.metadata.contains_key(SYNTHETIC_TOOL_RESULT_KEY)),
            "a failed turn is not answered with synthetic results"
        );
    }

    #[test]
    fn provider_executed_calls_need_no_synthetic_result() {
        let mut message = call("tc-1", "web_search");
        message
            .metadata
            .insert(format!("{PROVIDER_EXECUTED_METADATA_PREFIX}tc-1"), String::from("true"));
        let normalized =
            normalize_transcript(&[ModelMessage::text(ModelRole::User, "go"), message]);
        assert!(
            normalized
                .iter()
                .all(|message| !message.metadata.contains_key(SYNTHETIC_TOOL_RESULT_KEY))
        );
    }

    #[test]
    fn tool_name_is_reconciled_from_the_originating_call() {
        let messages = vec![
            call("tc-1", "read_file"),
            ModelMessage::tool_result("tc-1", crate::tools::ToolResult::success("body"))
                .with_metadata([(TOOL_NAME_METADATA_KEY.into(), "stale_name".into())]),
        ];
        let normalized = normalize_transcript(&messages);
        assert_eq!(
            normalized[1].metadata.get(TOOL_NAME_METADATA_KEY).map(String::as_str),
            Some("read_file")
        );
    }
}
