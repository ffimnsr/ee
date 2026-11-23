//! Improver-facing rollout projection.
//!
//! The append-only session log keeps ordered tool records, and the workspace
//! snapshot store keeps per-turn diffs. This module projects them into one
//! bounded, deterministic view that the harnessing context builder can render
//! for an improver session. Bodies, credentials, and raw model output are never
//! part of the view; redaction still runs at the context-builder boundary.

use crate::session_log::SessionLogRecord;
use crate::workspace_snapshot::{DiffChange, TurnDiff};

/// Maximum tool records retained in one rollout view.
pub const MAX_ROLLOUT_TOOLS: usize = 128;
/// Maximum rendered rollout bytes before truncation.
pub const MAX_ROLLOUT_BYTES: usize = 32 * 1024;

/// One observed tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutToolRecord {
    /// Turn number.
    pub turn: u64,
    /// Loop iteration inside the turn.
    pub iteration: usize,
    /// Tool name.
    pub tool: String,
    /// Whether the call succeeded.
    pub success: bool,
    /// Normalized error kind on failure.
    pub error_kind: Option<String>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// One turn's tool activity and stop reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolloutTurn {
    /// Turn number.
    pub turn: u64,
    /// Recorded stop reason, when the turn settled.
    pub stop_reason: Option<String>,
    /// Tool calls in append order.
    pub tools: Vec<RolloutToolRecord>,
}

/// One changed workspace path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutDiffFile {
    /// Display path.
    pub path: String,
    /// `added`, `modified`, or `deleted`.
    pub change: &'static str,
}

/// Bounded improver-facing rollout view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolloutView {
    /// Session the records came from.
    pub session_id: String,
    /// Turns with observed tool activity, ascending.
    pub turns: Vec<RolloutTurn>,
    /// Workspace diff for the run, when a snapshot pair exists.
    pub diff: Vec<RolloutDiffFile>,
    /// Final deliverable body or pointer, already bounded by the caller.
    pub final_deliverable: Option<String>,
}

impl RolloutView {
    /// Whether the view has any renderable trajectory evidence.
    #[must_use]
    pub fn has_trajectory(&self) -> bool {
        !self.turns.is_empty() || !self.diff.is_empty()
    }

    /// Total observed tool calls.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.turns.iter().map(|turn| turn.tools.len()).sum()
    }

    /// Failed tool calls.
    #[must_use]
    pub fn failed_tool_count(&self) -> usize {
        self.turns.iter().flat_map(|turn| turn.tools.iter()).filter(|tool| !tool.success).count()
    }

    /// Renders the view as bounded markdown.
    #[must_use]
    pub fn render(&self, max_bytes: usize) -> String {
        let mut text = format!("# Rollout view: {}\n", self.session_id);
        for turn in &self.turns {
            let stop = turn.stop_reason.as_deref().unwrap_or("unsettled");
            text.push_str(&format!("\n## Turn {} ({stop})\n\n", turn.turn));
            for tool in &turn.tools {
                let status = match (&tool.error_kind, tool.success) {
                    (_, true) => "ok".to_string(),
                    (Some(kind), false) => format!("failed:{kind}"),
                    (None, false) => "failed".to_string(),
                };
                text.push_str(&format!(
                    "- step {} `{}` {status} {}ms\n",
                    tool.iteration, tool.tool, tool.duration_ms
                ));
            }
        }
        if !self.diff.is_empty() {
            text.push_str("\n## Workspace diff\n\n");
            for file in &self.diff {
                text.push_str(&format!("- {} {}\n", file.change, file.path));
            }
        }
        if let Some(deliverable) = &self.final_deliverable {
            text.push_str(&format!("\n## Final deliverable\n\n{deliverable}\n"));
        }
        if text.len() > max_bytes {
            let mut cut = max_bytes.min(text.len());
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("\n… [rollout truncated]\n");
        }
        text
    }
}

/// Builds a rollout view from session records and a turn diff.
///
/// Non-tool records only contribute turn stop reasons; message bodies are never
/// read. The tool list is capped at [`MAX_ROLLOUT_TOOLS`].
#[must_use]
pub fn build(
    records: &[SessionLogRecord],
    diff: Option<&TurnDiff>,
    final_deliverable: Option<&str>,
) -> RolloutView {
    let session_id =
        records.first().map(|record| record.session_id().to_string()).unwrap_or_default();
    let mut turns: Vec<RolloutTurn> = Vec::new();
    let mut tool_count = 0usize;
    for record in records {
        match record {
            SessionLogRecord::Tool(tool) => {
                if tool_count >= MAX_ROLLOUT_TOOLS {
                    continue;
                }
                let index = match turns.iter().position(|turn| turn.turn == tool.turn) {
                    Some(index) => index,
                    None => {
                        turns.push(RolloutTurn { turn: tool.turn, ..RolloutTurn::default() });
                        turns.len() - 1
                    }
                };
                turns[index].tools.push(RolloutToolRecord {
                    turn: tool.turn,
                    iteration: tool.iteration,
                    tool: tool.tool_name.clone(),
                    success: tool.success,
                    error_kind: tool.error_kind.clone(),
                    duration_ms: tool.duration_ms,
                });
                tool_count += 1;
            }
            SessionLogRecord::TurnStopped { stop_reason, .. } => {
                // The stop record has no turn number; attach it to the highest
                // observed turn, which is the turn that just settled.
                if let Some(turn) = turns.iter_mut().max_by_key(|turn| turn.turn) {
                    turn.stop_reason = Some(stop_reason.clone());
                }
            }
            _ => {}
        }
    }
    turns.sort_by_key(|turn| turn.turn);
    let diff = diff
        .map(|diff| {
            diff.files
                .iter()
                .map(|file| RolloutDiffFile {
                    path: file.path.clone(),
                    change: match file.change {
                        DiffChange::Added => "added",
                        DiffChange::Modified => "modified",
                        DiffChange::Deleted => "deleted",
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    RolloutView {
        session_id,
        turns,
        diff,
        final_deliverable: final_deliverable.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::ToolRecord;

    fn tool(seq: u64, turn: u64, iteration: usize, name: &str, success: bool) -> SessionLogRecord {
        SessionLogRecord::Tool(ToolRecord {
            seq,
            session_id: "s-1".to_string(),
            tool_name: name.to_string(),
            success,
            error_kind: (!success).then(|| "backend".to_string()),
            duration_ms: 7,
            turn,
            iteration,
        })
    }

    fn stopped(seq: u64) -> SessionLogRecord {
        SessionLogRecord::TurnStopped {
            seq,
            session_id: "s-1".to_string(),
            stop_reason: "end_turn".to_string(),
        }
    }

    #[test]
    fn records_group_into_turns_and_render() {
        let records =
            vec![tool(1, 1, 1, "read_file", true), tool(2, 1, 2, "write_file", false), stopped(3)];
        let view = build(&records, None, Some("wrote parser.rs"));
        assert_eq!(view.session_id, "s-1");
        assert_eq!(view.turns.len(), 1);
        assert_eq!(view.turns[0].stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(view.tool_count(), 2);
        assert_eq!(view.failed_tool_count(), 1);
        assert!(view.has_trajectory());
        let text = view.render(MAX_ROLLOUT_BYTES);
        assert!(text.contains("## Turn 1 (end_turn)"));
        assert!(text.contains("`write_file` failed:backend"));
        assert!(text.contains("## Final deliverable"));
        assert!(text.contains("wrote parser.rs"));
    }

    #[test]
    fn render_is_bounded() {
        let mut records = Vec::new();
        for seq in 0..MAX_ROLLOUT_TOOLS {
            records.push(tool(seq as u64, 1, seq, "ee_search_text", true));
        }
        let view = build(&records, None, None);
        assert_eq!(view.tool_count(), MAX_ROLLOUT_TOOLS);
        let text = view.render(256);
        assert!(text.len() <= 256 + 32);
        assert!(text.contains("[rollout truncated]"));
    }

    #[test]
    fn empty_records_are_not_a_trajectory() {
        let view = build(&[], None, None);
        assert!(!view.has_trajectory());
    }
}
