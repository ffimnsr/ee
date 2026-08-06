//! OpenRouter session compaction helpers (Phase 12).
//!
//! `/compact` turns ask the configured model for a continuation summary over
//! the stored history.  The helpers here own the deterministic boundaries:
//! the compaction prompt shape, the serialized input-byte bound (oldest
//! messages are dropped first, keeping tool-call/tool-result pairs
//! consistent), pair-consistent tail retention, and secret-like redaction of
//! every message that leaves the provider.  The provider owns the actual
//! history replacement; nothing in this module writes session state.

use ee_agent_orchestrator::SensitiveDataGuard;
use serde_json::Value;

/// Builds the compaction prompt asking the model for the sections a
/// continuation summary must preserve; optional user instructions are
/// appended verbatim.
#[must_use]
pub(crate) fn build_compaction_prompt(instructions: Option<&str>) -> String {
    let mut prompt = String::from(
        "Write a compact continuation summary of this session's history. Cover:\n\
         - the user goal\n\
         - completed work\n\
         - the current state\n\
         - important files and symbols\n\
         - decisions and constraints\n\
         - pending work\n\
         - validation status\n\
         - risks and errors\n\
         Keep the summary high-signal: preserve facts a continuation needs, omit filler.",
    );
    if let Some(instructions) = instructions.filter(|text| !text.trim().is_empty()) {
        prompt.push_str("\nAdditional instructions: ");
        prompt.push_str(instructions.trim());
        prompt.push('\n');
    }
    prompt
}

/// Serialized byte size of the messages array (the `messages` member of a
/// chat-completions request body).
#[must_use]
pub(crate) fn messages_serialized_bytes(messages: &[Value]) -> usize {
    serde_json::to_string(&Value::Array(messages.to_vec()))
        .expect("messages always serialize")
        .len()
}

/// Redacts secret-like values inside one message value so compaction
/// requests never carry credentials.  Redaction operates on the serialized
/// text and re-parses it; a parse failure (impossible in practice) returns
/// the message untouched.
#[must_use]
pub(crate) fn redact_message(message: &Value) -> Value {
    let text = serde_json::to_string(message).expect("message always serializes");
    let redacted = SensitiveDataGuard::new().redact(&text);
    serde_json::from_str(&redacted).unwrap_or_else(|_| message.clone())
}

/// Drops the oldest messages until the serialized history fits `max_bytes`
/// (always keeping at least one message), keeping tool-call/tool-result
/// pairs consistent: dropping an assistant message with `tool_calls` also
/// drops its immediately-following tool results, and an orphaned leading
/// tool result is dropped alone.  Returns the number of messages dropped.
#[must_use]
pub(crate) fn trim_history_to_budget(history: &mut Vec<Value>, max_bytes: usize) -> usize {
    let mut dropped = 0usize;
    while history.len() > 1 && messages_serialized_bytes(history) > max_bytes {
        dropped += drop_front_pair_consistent(history);
    }
    dropped
}

/// Removes the front message, returning how many messages left; when it is
/// an assistant tool-call message, its contiguous tool results are removed
/// too, and when it is an orphaned tool result it is removed alone.
fn drop_front_pair_consistent(history: &mut Vec<Value>) -> usize {
    let front = &history[0];
    let is_assistant_tool_calls = front["role"] == "assistant" && front.get("tool_calls").is_some();
    let call_ids: Vec<String> = front
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| call.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    history.remove(0);
    let mut removed = 1usize;
    if is_assistant_tool_calls {
        while let Some(next) = history.first() {
            let belongs = next["role"] == "tool"
                && next
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| call_ids.iter().any(|known| known == id));
            if belongs {
                history.remove(0);
                removed += 1;
            } else {
                break;
            }
        }
    }
    removed
}

/// The last `count` messages, extended so retained tool-call/tool-result
/// pairs stay consistent: a leading tool result pulls in its assistant
/// tool-call message (which, by construction of the history, sits directly
/// before it and whose remaining results stay inside the suffix).
#[must_use]
pub(crate) fn retained_tail(history: &[Value], count: usize) -> Vec<Value> {
    let len = history.len();
    if len <= count {
        return history.to_vec();
    }
    let mut start = len - count;
    // Step back over leading tool results so the retained suffix starts at
    // the assistant tool-call message that owns them (which, by construction
    // of the history, sits directly before the group and whose remaining
    // results stay inside the suffix).
    while start > 0 && history[start]["role"] == "tool" {
        start -= 1;
    }
    history[start..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Value {
        json!({ "role": "user", "content": text })
    }

    fn assistant(text: &str) -> Value {
        json!({ "role": "assistant", "content": text })
    }

    fn assistant_tool_call(id: &str, path: &str) -> Value {
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": { "name": "tool_read_file", "arguments": json!({ "path": path }) }
            }]
        })
    }

    fn tool_result(id: &str, content: &str) -> Value {
        json!({ "role": "tool", "tool_call_id": id, "content": content })
    }

    #[test]
    fn compaction_prompt_covers_required_sections() {
        let prompt = build_compaction_prompt(None);
        for section in [
            "user goal",
            "completed work",
            "current state",
            "important files and symbols",
            "decisions and constraints",
            "pending work",
            "validation status",
            "risks and errors",
        ] {
            assert!(prompt.contains(section), "prompt must cover {section:?}");
        }
    }

    #[test]
    fn compaction_prompt_appends_instructions_verbatim() {
        let prompt = build_compaction_prompt(Some("  keep API v2  "));
        assert!(prompt.contains("keep API v2"), "{prompt}");
        assert!(prompt.contains("Additional instructions"), "{prompt}");
        let plain = build_compaction_prompt(None);
        assert!(!plain.contains("Additional instructions"), "{plain}");
    }

    #[test]
    fn serialized_bytes_match_request_member() {
        let messages = vec![user("hello"), assistant("hi")];
        let expected = serde_json::to_string(&Value::Array(messages.clone())).unwrap().len();
        assert_eq!(messages_serialized_bytes(&messages), expected);
    }

    #[test]
    fn redaction_masks_secret_like_values_inside_messages() {
        let message = json!({
            "role": "user",
            "content": "key is sk-live-1234567890 and OPENROUTER_API_KEY=sk-other-99"
        });
        let redacted = redact_message(&message);
        let text = redacted.to_string();
        assert!(!text.contains("sk-live-1234567890"), "{text}");
        assert!(!text.contains("sk-other-99"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
        assert_eq!(redacted["role"], "user", "structure preserved");
    }

    #[test]
    fn redaction_leaves_ordinary_messages_untouched() {
        let message = json!({ "role": "assistant", "content": "read /tmp/a.txt" });
        assert_eq!(redact_message(&message), message);
    }

    #[test]
    fn trim_drops_oldest_within_budget() {
        let mut history = vec![user("oldest"), user("second"), user("third"), assistant("newest")];
        let max = messages_serialized_bytes(&history) - 10;
        let dropped = trim_history_to_budget(&mut history, max);
        assert_eq!(dropped, 1);
        assert_eq!(history[0]["content"], "second");
        assert!(messages_serialized_bytes(&history) <= max);
    }

    #[test]
    fn trim_keeps_at_least_one_message() {
        let mut history = vec![user("only")];
        assert_eq!(trim_history_to_budget(&mut history, 0), 0);
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn trim_removes_tool_pairs_consistently() {
        let mut history = vec![
            user("round one"),
            assistant_tool_call("call_1", "/a"),
            tool_result("call_1", "data"),
            assistant("answer one"),
            user("round two"),
            assistant("answer two"),
        ];
        // Budget small enough that only the last round survives trimming;
        // the assistant tool call and its tool result leave together.
        let max = messages_serialized_bytes(&[user("round two"), assistant("answer two")]) + 1;
        let dropped = trim_history_to_budget(&mut history, max);
        assert_eq!(dropped, 4, "user + assistant tool call + tool result + answer dropped");
        assert_eq!(history, vec![user("round two"), assistant("answer two")]);
        assert!(history.iter().all(|message| message["role"] != "tool"), "no orphaned tool");
    }

    #[test]
    fn trim_removes_orphaned_leading_tool_result() {
        // A tool result whose assistant call was already dropped must never
        // survive at the front of the bounded history.
        let mut history =
            vec![tool_result("call_9", "orphan"), user("keep me"), assistant("keep too")];
        let max = messages_serialized_bytes(&[user("keep me"), assistant("keep too")]) + 1;
        let dropped = trim_history_to_budget(&mut history, max);
        assert_eq!(dropped, 1);
        assert_eq!(history[0]["role"], "user");
        assert!(history.iter().all(|message| message["role"] != "tool"));
    }

    #[test]
    fn tail_retains_last_messages_verbatim() {
        let history = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let tail = retained_tail(&history, 2);
        assert_eq!(tail, vec![user("c"), assistant("d")]);
        assert_eq!(retained_tail(&history, 10), history, "larger count keeps everything");
        assert_eq!(retained_tail(&history, 4), history);
    }

    #[test]
    fn tail_pulls_in_tool_call_with_its_results() {
        let history = vec![
            user("round one"),
            assistant_tool_call("call_1", "/a"),
            tool_result("call_1", "data"),
            user("round two"),
            assistant("answer two"),
        ];
        // Cutting at the last three would leave the tool result orphaned;
        // the pair-consistent tail starts at the assistant tool call.
        let tail = retained_tail(&history, 3);
        assert_eq!(tail.len(), 4);
        assert_eq!(tail[0]["role"], "assistant");
        assert!(tail[0].get("tool_calls").is_some());
        assert_eq!(tail[1]["role"], "tool");
        assert_eq!(tail[1]["tool_call_id"], "call_1");
        assert_eq!(tail[2]["content"], "round two");
        assert_eq!(tail[3]["content"], "answer two");
    }

    #[test]
    fn tail_mid_group_cut_pulls_back_to_the_tool_call() {
        let history = vec![
            user("round one"),
            assistant_tool_call("call_1", "/a"),
            tool_result("call_1", "first"),
            tool_result("call_1", "second"),
            user("round two"),
            assistant("answer two"),
        ];
        // Tail count 3 starts at the second tool result; the consistency
        // rule pulls the start back to the assistant tool call, keeping the
        // whole group inside the suffix.
        let tail = retained_tail(&history, 3);
        assert_eq!(tail.len(), 5);
        assert!(tail[0].get("tool_calls").is_some(), "assistant tool call retained");
        assert_eq!(tail[1]["tool_call_id"], "call_1");
        assert_eq!(tail[2]["tool_call_id"], "call_1");
        assert_eq!(tail[3]["content"], "round two");
        assert_eq!(tail[4]["content"], "answer two");
    }

    #[test]
    fn tail_without_pairs_is_exact_suffix() {
        let history = vec![user("a"), assistant("b"), user("c")];
        assert_eq!(retained_tail(&history, 1), vec![user("c")]);
    }
}
