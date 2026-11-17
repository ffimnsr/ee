//! Deterministic bounding for tool output admitted to the transcript.
//!
//! The reference agent truncates one tool result at 2,000 lines / 50 KiB,
//! writes the overflow to a retained file, and returns the path plus
//! `truncated` metadata. Overflow is therefore lossy for the model but
//! recoverable for the agent.
//!
//! Bounding happens when a result is admitted, not only during compaction, so
//! an oversized result is never re-sent verbatim by every step until
//! compaction triggers. The existing head/tail prune stays as a second stage
//! for already-bounded results.

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

use crate::sensitive_data::SensitiveDataGuard;
use crate::session_log::SessionLog;
use crate::tools::ToolResult;

/// Default maximum lines kept inline from one tool result.
pub const DEFAULT_MAX_TOOL_OUTPUT_LINES: usize = 2_000;
/// Default maximum bytes kept inline from one tool result (50 KiB).
pub const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 50 * 1024;
/// Structured-output key carrying the truncation flag.
pub const TOOL_OUTPUT_TRUNCATED_KEY: &str = "truncated";
/// Structured-output key carrying the retained overflow path.
pub const TOOL_OUTPUT_PATH_KEY: &str = "output_path";

/// Per-result bounds applied before projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolOutputLimits {
    /// Maximum lines kept inline.
    pub max_lines: usize,
    /// Maximum UTF-8 bytes kept inline.
    pub max_bytes: usize,
}

impl Default for ToolOutputLimits {
    fn default() -> Self {
        Self { max_lines: DEFAULT_MAX_TOOL_OUTPUT_LINES, max_bytes: DEFAULT_MAX_TOOL_OUTPUT_BYTES }
    }
}

impl ToolOutputLimits {
    /// Rejects a zero bound, which would discard every result.
    pub fn validate(&self) -> Result<(), crate::error::OrchestratorError> {
        if self.max_lines == 0 || self.max_bytes == 0 {
            return Err(crate::error::OrchestratorError::InvalidState(
                "tool output bounds must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

/// One bounding decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedToolOutput {
    /// Text kept inline; unchanged when `truncated` is false.
    pub text: String,
    /// Whether any content was cut.
    pub truncated: bool,
    /// Lines shown inline.
    pub shown_lines: usize,
    /// Lines in the original text.
    pub total_lines: usize,
    /// Whether the cut landed inside a line rather than at a line break.
    pub last_line_cut: bool,
}

impl BoundedToolOutput {
    /// Unchanged output.
    fn unchanged(text: &str) -> Self {
        Self {
            text: text.to_string(),
            truncated: false,
            shown_lines: count_lines(text),
            total_lines: count_lines(text),
            last_line_cut: false,
        }
    }
}

/// Bounds `text` to `limits` without splitting a UTF-8 sequence or a grapheme
/// cluster.
#[must_use]
pub fn bound_tool_output(text: &str, limits: ToolOutputLimits) -> BoundedToolOutput {
    let total_lines = count_lines(text);
    if total_lines <= limits.max_lines && text.len() <= limits.max_bytes {
        return BoundedToolOutput::unchanged(text);
    }
    let line_end = after_lines(text, limits.max_lines);
    let byte_end = floor_grapheme_boundary(text, limits.max_bytes);
    let end = line_end.min(byte_end);
    let shown = trim_trailing_line_break(&text[..end]);
    let shown_lines = count_lines(&text[..end]);
    let last_line_cut =
        shown_lines > 0 && shown.len() < text.len() && !text[shown.len()..].starts_with('\n');
    BoundedToolOutput {
        text: shown.to_string(),
        truncated: true,
        shown_lines,
        total_lines,
        last_line_cut,
    }
}

/// Applies bounds to `result` and retains the full text when it overflows.
///
/// The retained copy goes through the existing content-addressed session log,
/// so it shares the session's owner-only lifecycle and never escapes the state
/// directory. Secret-like spans are redacted before the copy is stored.
pub fn apply_to_result(
    result: &mut ToolResult,
    limits: ToolOutputLimits,
    log: Option<&SessionLog>,
) {
    let bounded = bound_tool_output(&result.text_output, limits);
    if !bounded.truncated {
        return;
    }
    let redacted = SensitiveDataGuard::new().redact(&result.text_output);
    let saved = log.and_then(|log| log.store_artifact(redacted.as_bytes()).ok());
    let path = saved.as_ref().and_then(|artifact| artifact.path.clone());
    let marker = marker(&bounded, path.as_deref());
    result.text_output = format!("{}\n\n{marker}", bounded.text);
    merge_metadata(result, bounded.truncated, path.as_deref());
}

fn marker(bounded: &BoundedToolOutput, path: Option<&std::path::Path>) -> String {
    let range = if bounded.shown_lines == 0 {
        "0 lines".to_string()
    } else {
        format!("lines 1-{}", bounded.shown_lines)
    };
    let cut = if bounded.last_line_cut { " (last line cut)" } else { "" };
    match path {
        Some(path) => format!(
            "[showing {range} of {}{cut}; full output saved to {}]",
            bounded.total_lines,
            path.display()
        ),
        None => format!(
            "[showing {range} of {}{cut}; full output could not be saved: no durable session log]",
            bounded.total_lines
        ),
    }
}

fn merge_metadata(result: &mut ToolResult, truncated: bool, path: Option<&std::path::Path>) {
    let mut metadata = match result.structured_output.take() {
        Some(serde_json::Value::Object(object)) => object,
        Some(other) => {
            let mut object = serde_json::Map::new();
            object.insert("value".to_string(), other);
            object
        }
        None => serde_json::Map::new(),
    };
    metadata.insert(TOOL_OUTPUT_TRUNCATED_KEY.to_string(), serde_json::Value::Bool(truncated));
    if let Some(path) = path {
        metadata.insert(
            TOOL_OUTPUT_PATH_KEY.to_string(),
            serde_json::Value::String(path.display().to_string()),
        );
    }
    result.structured_output = Some(serde_json::Value::Object(metadata));
}

/// Lines in `text`, not counting an empty one after a final line break.
fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.bytes().filter(|byte| *byte == b'\n').count() + usize::from(!text.ends_with('\n'))
}

/// Byte index where the line after the first `count` lines starts.
fn after_lines(text: &str, count: usize) -> usize {
    let mut index = 0;
    for _ in 0..count {
        match text[index..].find('\n') {
            Some(offset) => index += offset + 1,
            None => return text.len(),
        }
    }
    index
}

/// Largest grapheme-cluster boundary at or below `budget` bytes.
fn floor_grapheme_boundary(text: &str, budget: usize) -> usize {
    if budget >= text.len() {
        return text.len();
    }
    let mut boundary = 0;
    for (index, grapheme) in text.grapheme_indices(true) {
        if index + grapheme.len() > budget {
            break;
        }
        boundary = index + grapheme.len();
    }
    boundary
}

fn trim_trailing_line_break(text: &str) -> &str {
    text.strip_suffix("\r\n").or_else(|| text.strip_suffix('\n')).unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_is_unchanged() {
        let text = "a".repeat(1_000);
        let bounded = bound_tool_output(&text, ToolOutputLimits::default());
        assert!(!bounded.truncated);
        assert_eq!(bounded.text, text);
    }

    #[test]
    fn a_thousand_lines_and_forty_kib_is_unchanged() {
        let text = (0..1_000).map(|index| format!("line {index}")).collect::<Vec<_>>().join("\n");
        let bounded = bound_tool_output(&text, ToolOutputLimits::default());
        assert!(!bounded.truncated, "{} bytes", text.len());
        assert_eq!(bounded.text, text);
    }

    #[test]
    fn oversized_output_cuts_at_the_line_and_byte_bound() {
        let text = (0..5_000).map(|index| format!("line {index}")).collect::<Vec<_>>().join("\n");
        let bounded = bound_tool_output(&text, ToolOutputLimits::default());
        assert!(bounded.truncated);
        assert!(bounded.shown_lines <= DEFAULT_MAX_TOOL_OUTPUT_LINES);
        assert!(bounded.text.len() <= DEFAULT_MAX_TOOL_OUTPUT_BYTES);
    }

    #[test]
    fn cuts_never_split_a_utf8_sequence_or_grapheme() {
        // Four-byte characters plus a combining sequence and an emoji ZWJ pair.
        let text = "a\u{0301}e\u{200d}\u{1f469}\u{200d}\u{1f469}".repeat(20_000);
        let bounded =
            bound_tool_output(&text, ToolOutputLimits { max_lines: 10, max_bytes: 4_097 });
        assert!(bounded.truncated);
        assert!(std::str::from_utf8(bounded.text.as_bytes()).is_ok());
        // The retained prefix must be a whole number of grapheme clusters.
        assert_eq!(
            UnicodeSegmentation::graphemes(bounded.text.as_str(), true).collect::<String>(),
            bounded.text
        );
    }

    #[test]
    fn zero_bounds_are_rejected() {
        assert!(
            ToolOutputLimits { max_lines: 0, max_bytes: 10 }.validate().is_err(),
            "zero line bound is rejected"
        );
    }

    #[test]
    fn overflow_is_retained_through_the_session_log() {
        let log = SessionLog::in_memory("s-1");
        let mut result = ToolResult::success("x".repeat(2 * DEFAULT_MAX_TOOL_OUTPUT_BYTES));
        apply_to_result(&mut result, ToolOutputLimits::default(), Some(&log));
        assert!(result.text_output.contains("full output"));
        let structured = result.structured_output.expect("metadata");
        assert_eq!(structured[TOOL_OUTPUT_TRUNCATED_KEY], serde_json::json!(true));
        assert_eq!(structured[TOOL_OUTPUT_PATH_KEY], serde_json::Value::Null);
        assert_eq!(log.records().expect("records").len(), 0, "no record for a body artifact");
    }

    #[test]
    fn durable_overflow_is_owner_only_and_stays_inside_the_state_dir() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let sink = Arc::new(crate::session_log::FileSessionLogSink::new(
            dir.path().join("session-log"),
            1024 * 1024,
        ));
        let log =
            SessionLog::with_sink("s-1", sink, crate::session_log::SessionLogConfig::default());
        let mut result = ToolResult::success("y".repeat(2 * DEFAULT_MAX_TOOL_OUTPUT_BYTES));
        apply_to_result(&mut result, ToolOutputLimits::default(), Some(&log));
        let structured = result.structured_output.expect("metadata");
        let path = structured[TOOL_OUTPUT_PATH_KEY].as_str().expect("durable path");
        let path = std::path::PathBuf::from(path);
        assert!(path.starts_with(dir.path()), "artifact stays inside the state dir");
        let file = std::fs::metadata(&path).expect("artifact exists");
        assert_eq!(file.permissions().mode() & 0o777, 0o600, "owner-only file");
        let parent = std::fs::metadata(path.parent().expect("parent")).expect("parent exists");
        assert_eq!(parent.permissions().mode() & 0o777, 0o700, "owner-only directory");
    }
}
