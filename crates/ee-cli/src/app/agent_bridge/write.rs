//! Write/read revision + diff helpers, agent payloads, and the action log.

use std::path::PathBuf;

use ee_agent_host::AgentError;
use similar::TextDiff;

use crate::policy::{DecisionReason, TrustCategory};

/// One recorded agent file operation (future checkpoint/restore source) or
/// redacted automatic trust decision (Phase 6 audit).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ActionLogEntry {
    Read {
        path: PathBuf,
        bytes: usize,
        session_id: String,
    },
    Write {
        path: PathBuf,
        old_fingerprint: u64,
        new_fingerprint: u64,
        tool_call_id: Option<String>,
        session_id: String,
    },
    /// Redacted automatic trust decision: rule id, operation category,
    /// machine-readable reason, and remaining use budget.  Never carries
    /// raw paths, command environment, secret values, or MCP arguments.
    TrustDecision {
        rule_id: Option<String>,
        category: TrustCategory,
        reason: DecisionReason,
        remaining_uses: Option<u64>,
        session_id: String,
    },
    /// Redacted durable trust-rule lifecycle event, separate from decisions.
    TrustRuleMutation {
        rule_id: Option<String>,
        action: String,
        source: String,
    },
    /// External provenance only. Retains final canonical source URL, never a
    /// separate request body, response text, headers, credentials, or search query.
    ExternalSource {
        action: String,
        host: String,
        url: String,
        retrieved_at: String,
        sha256: Option<String>,
        byte_count: usize,
        result_count: usize,
        cached: bool,
        truncated: bool,
        provenance: String,
        session_id: String,
    },
}

/// FNV-1a content fingerprint (deterministic, non-cryptographic).
pub(super) fn fingerprint(content: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Debug)]
pub(super) struct BridgeWriteOutcome {
    pub(super) old_content: String,
    pub(super) byte_count: u64,
    pub(super) new_revision: String,
    pub(super) saved: bool,
    pub(super) dirty: bool,
}

pub(super) fn text_revision_id(content: &str) -> String {
    format!("{:016x}", fingerprint(content))
}

pub(super) fn buffer_revision_id(buf: &crate::buffer::BufState) -> String {
    if buf.is_vlf {
        return format!(
            "vlf:{}:{}:{}",
            buf.vlf_generation, buf.vlf_cache_start_line, buf.vlf_approx_line_count
        );
    }
    text_revision_id(&buf.whole_text().unwrap_or_default())
}

pub(super) fn buffer_saved_state(buf: &crate::buffer::BufState) -> bool {
    buf.save_complete && buf.last_save_succeeded && !buf.last_save_permission_denied
}

// ── Text helpers ─────────────────────────────────────────────────────────────

/// Splits file content into editor line model entries.
///
/// A trailing newline does not produce a phantom empty line (the backend
/// model stores lines without newline terminators).
pub(super) fn split_lines(content: &str) -> Vec<String> {
    let mut lines: Vec<String> = content.split('\n').map(str::to_owned).collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Line-diff hunks: `(old_start, old_end_exclusive, new_lines)`.
///
/// Adjacent changed regions merge into one hunk; a pure insertion reports
/// `old_start == old_end` (no old lines consumed).
pub(super) fn diff_hunks(
    old_lines: &[String],
    new_lines: &[String],
) -> Vec<(usize, usize, Vec<String>)> {
    let old_text = old_lines.join("\n");
    let new_text = new_lines.join("\n");
    let diff = TextDiff::from_lines(&old_text, &new_text);

    let mut hunks = Vec::new();
    for group in diff.grouped_ops(0) {
        let old_start = group.first().map(|op| op.old_range().start).unwrap_or(0);
        let old_end = group.last().map(|op| op.old_range().end).unwrap_or(old_start);
        let mut inserted = Vec::new();
        for op in &group {
            if matches!(op.tag(), similar::DiffTag::Insert | similar::DiffTag::Replace) {
                inserted.extend_from_slice(&new_lines[op.new_range()]);
            }
        }
        hunks.push((old_start, old_end, inserted));
    }
    hunks
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentDocumentSymbolsPayload {
    pub(super) symbols: Vec<ee_mcp::DocumentSymbolEntry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentReferencesPayload {
    pub(super) references: Vec<ee_mcp::ReferenceEntry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentCodeActionPayload {
    pub(super) actions: Vec<AgentCodeActionPayloadEntry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentCodeActionPayloadEntry {
    pub(super) title: String,
    pub(super) kind: Option<String>,
    pub(super) has_command: bool,
    pub(super) edits: Vec<ee_mcp::PlannedTextEdit>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentTextEditsPayload {
    pub(super) edits: Vec<ee_mcp::PlannedTextEdit>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentRenamePayload {
    pub(super) files: Vec<ee_mcp::PlannedFileEdit>,
}

fn utf16_column_to_byte_offset(text: &str, utf16_column: usize) -> usize {
    let mut utf16_seen = 0usize;
    for (byte, ch) in text.char_indices() {
        if utf16_seen >= utf16_column {
            return byte;
        }
        utf16_seen = utf16_seen.saturating_add(ch.len_utf16());
        if utf16_seen > utf16_column {
            return byte;
        }
    }
    text.len()
}

fn text_offset_for_range_position(
    text: &str,
    line: usize,
    character_utf16: usize,
) -> Result<usize, AgentError> {
    let lines: Vec<&str> = text.split('\n').collect();
    let Some(line_text) = lines.get(line) else {
        return Err(AgentError::invalid_params(format!(
            "line {} is beyond the end of the document",
            line + 1
        )));
    };
    let prefix =
        lines.iter().take(line).fold(0usize, |acc, value| acc.saturating_add(value.len() + 1));
    Ok(prefix.saturating_add(utf16_column_to_byte_offset(line_text, character_utf16)))
}

pub(super) fn apply_planned_text_edits_to_content(
    content: &str,
    edits: &[ee_mcp::PlannedTextEdit],
) -> Result<String, AgentError> {
    let mut with_offsets = Vec::with_capacity(edits.len());
    for edit in edits {
        let start_line = edit.range.start_line.saturating_sub(1) as usize;
        let start_character = edit.range.start_character.saturating_sub(1) as usize;
        let end_line = edit.range.end_line.saturating_sub(1) as usize;
        let end_character = edit.range.end_character.saturating_sub(1) as usize;
        let start = text_offset_for_range_position(content, start_line, start_character)?;
        let end = text_offset_for_range_position(content, end_line, end_character)?;
        if start > end {
            return Err(AgentError::invalid_params("planned edit range is inverted"));
        }
        with_offsets.push((start, end, edit.new_text.as_str()));
    }
    with_offsets.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    let mut next = content.to_string();
    for (start, end, replacement) in with_offsets {
        if start > next.len()
            || end > next.len()
            || !next.is_char_boundary(start)
            || !next.is_char_boundary(end)
        {
            return Err(AgentError::invalid_params(
                "planned edit range does not align with document text",
            ));
        }
        next.replace_range(start..end, replacement);
    }
    Ok(next)
}

#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_matches_editor_line_model() {
        assert_eq!(split_lines("a\nb\n"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(split_lines("a\nb"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(split_lines(""), vec![String::new()]);
        assert_eq!(split_lines("\n"), vec![String::new()]);
    }

    /// Simulates the backend application of hunks (inclusive line ranges,
    /// insertion anchoring) and returns the resulting lines.
    fn apply_hunks(old_lines: &[String], hunks: &[(usize, usize, Vec<String>)]) -> Vec<String> {
        let mut lines = old_lines.to_vec();
        for (start, end, new_lines) in hunks.iter().rev() {
            let start = *start;
            if start == *end {
                let anchor = lines.get(start).or_else(|| lines.last()).cloned().unwrap_or_default();
                let mut replacement = new_lines.clone();
                replacement.push(anchor);
                let last = lines.len().saturating_sub(1);
                let target = start.min(last);
                lines.splice(target..=target, replacement);
            } else {
                lines.splice(start..=end.saturating_sub(1), new_lines.iter().cloned());
            }
        }
        lines
    }

    #[test]
    fn diff_hunks_reconstruct_target_when_applied() {
        let old = split_lines("alpha\nbeta\ngamma");
        let new = split_lines("alpha\nBETA\ngamma\ndelta");
        let hunks = diff_hunks(&old, &new);
        assert_eq!(apply_hunks(&old, &hunks), new);
    }

    #[test]
    fn diff_hunks_merge_adjacent_changes() {
        let old = split_lines("one\ntwo\nthree");
        let new = split_lines("one\n2\n3\nthree");
        let hunks = diff_hunks(&old, &new);
        assert_eq!(apply_hunks(&old, &hunks), new);
    }

    #[test]
    fn diff_hunks_handle_deletions_and_insertions() {
        let old = split_lines("one\ntwo\nthree");
        let new = split_lines("one\nthree");
        let hunks = diff_hunks(&old, &new);
        assert_eq!(apply_hunks(&old, &hunks), new);

        let new = split_lines("one\ntwo\n2.5\nthree");
        let hunks = diff_hunks(&old, &new);
        assert_eq!(apply_hunks(&old, &hunks), new);
    }

    #[test]
    fn diff_hunks_are_empty_for_equal_content() {
        let lines = split_lines("same\ncontent");
        assert!(diff_hunks(&lines, &lines).is_empty());
    }

    #[test]
    fn fingerprints_are_stable_and_distinct() {
        assert_eq!(fingerprint("same"), fingerprint("same"));
        assert_ne!(fingerprint("same"), fingerprint("other"));
    }
}
