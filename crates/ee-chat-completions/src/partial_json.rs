//! Bounded best-effort JSON parsing for streamed tool arguments.
//!
//! Providers stream tool-call arguments as JSON text, so a UI wants a preview
//! before the call finishes. The preview is display-only: the loop never
//! executes from a partially parsed object, it waits for the authoritative
//! final parse. This parser therefore makes one promise only — bounded work,
//! no panics — and returns `None` instead of guessing when the input cannot be
//! repaired.
//!
//! Adapted in spirit from `partial-json` (Promplate Dev Team, MIT) as used by
//! the reference agent at `packages/ai/src/protocols/utils/partial-json.ts`.

use serde_json::Value;

/// Longest argument string the preview parser will touch.
///
/// Tool arguments are model output, not user files: anything larger is already
/// a failed call, and reparsing it on every delta would burn CPU per chunk.
pub const MAX_PARTIAL_JSON_BYTES: usize = 64 * 1024;

/// Maximum container nesting the repair pass accepts.
const MAX_PARTIAL_JSON_DEPTH: usize = 32;

/// Parses complete JSON, or repairs one truncated prefix into best-effort JSON.
///
/// Returns `None` when the input is empty, over the size cap, nested past the
/// depth cap, or not repairable.
#[must_use]
pub fn parse_partial(input: &str) -> Option<Value> {
    if input.is_empty() || input.len() > MAX_PARTIAL_JSON_BYTES {
        return None;
    }
    if let Ok(value) = serde_json::from_str(input) {
        return Some(value);
    }
    let repaired = repair(input)?;
    serde_json::from_str(&repaired).ok()
}

/// Closes an unterminated string, drops a dangling separator, and appends the
/// missing closing brackets in reverse order.
fn repair(input: &str) -> Option<String> {
    let mut repaired = String::with_capacity(input.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for character in input.chars() {
        if in_string {
            repaired.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                repaired.push(character);
            }
            '{' | '[' => {
                stack.push(character);
                if stack.len() > MAX_PARTIAL_JSON_DEPTH {
                    return None;
                }
                repaired.push(character);
            }
            '}' => {
                if stack.pop() != Some('{') {
                    return None;
                }
                repaired.push(character);
            }
            ']' => {
                if stack.pop() != Some('[') {
                    return None;
                }
                repaired.push(character);
            }
            _ => repaired.push(character),
        }
    }
    if in_string {
        // A trailing escape would escape the quote we are about to add.
        if escaped {
            repaired.pop();
        }
        repaired.push('"');
    }
    let trimmed = repaired.trim_end();
    let dangling_value = trimmed.ends_with(':') || trimmed.ends_with(',');
    if dangling_value {
        // A key without a value is repairable with `null`; a trailing comma is
        // not valid JSON, so drop it first.
        if trimmed.ends_with(',') {
            repaired.truncate(trimmed.len() - 1);
        } else {
            repaired.push_str("null");
        }
    } else if trimmed.is_empty() && stack.is_empty() {
        return None;
    }
    for opener in stack.into_iter().rev() {
        repaired.push(if opener == '{' { '}' } else { ']' });
    }
    Some(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn complete_json_passes_through() {
        assert_eq!(parse_partial(r#"{"a":1}"#), Some(json!({ "a": 1 })));
    }

    #[test]
    fn truncated_string_is_closed() {
        assert_eq!(parse_partial(r#"{"path":"src/li"#), Some(json!({ "path": "src/li" })));
    }

    #[test]
    fn truncated_after_key_gets_null() {
        assert_eq!(parse_partial(r#"{"a":"#), Some(json!({ "a": null })));
    }

    #[test]
    fn truncated_array_is_closed() {
        assert_eq!(parse_partial(r#"{"items":[1,2"#), Some(json!({ "items": [1, 2] })));
    }

    #[test]
    fn trailing_comma_is_dropped() {
        assert_eq!(parse_partial(r#"{"a":1,"#), Some(json!({ "a": 1 })));
    }

    #[test]
    fn empty_and_oversize_inputs_are_rejected() {
        assert_eq!(parse_partial(""), None);
        let huge = "a".repeat(MAX_PARTIAL_JSON_BYTES + 1);
        assert_eq!(parse_partial(&huge), None);
    }

    #[test]
    fn unbalanced_brackets_are_rejected() {
        assert_eq!(parse_partial(r#"{"a":1]"#), None);
    }

    #[test]
    fn deep_nesting_is_rejected() {
        let deep = "[".repeat(MAX_PARTIAL_JSON_DEPTH + 1);
        assert_eq!(parse_partial(&deep), None);
    }

    #[test]
    fn trailing_escape_does_not_break_the_repair() {
        assert_eq!(parse_partial(r#"{"a":"x\"#), Some(json!({ "a": "x" })));
    }
}
