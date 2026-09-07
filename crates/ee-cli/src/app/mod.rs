pub(crate) use std::collections::HashMap;
pub(crate) use std::io;
pub(crate) use std::path::Path;
pub(crate) use std::path::PathBuf;
pub(crate) use std::time::{Duration, Instant};

pub(crate) use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
pub(crate) use ratatui::layout::Rect;
pub(crate) use serde_json::json;
pub(crate) use xi_core_lib::plugin_rpc::{CodeActionDescriptor, SelectionRange};
pub(crate) use xi_core_lib::rpc::LineReplacement;

pub(crate) use crate::backend::{CompletionSuggestion, PendingUiAction};
pub(crate) use crate::buffer::BufferManager;
pub(crate) use crate::folds::FoldStore;
pub(crate) use crate::git::{self, GitBufferCache, GitBufferStatus};
pub(crate) use crate::keymap::{Action, BindingKey, SequenceNode};
pub(crate) use crate::picker::PickerState;
pub(crate) use crate::quickfix::{QfEntry, QfList};
pub(crate) use crate::registers::{BlockInsert, LastChange, RegisterName, RegisterStore};
pub(crate) use crate::text::{byte_col_to_display_col, previous_char_boundary};
pub(crate) use crate::window::{SplitDir, TabManager, ViewDirection};

#[cfg(feature = "agents")]
mod agent_bridge;
#[cfg(feature = "agents")]
mod agent_export;
#[cfg(feature = "agents")]
mod agent_filesystem;
#[cfg(feature = "agents")]
mod agent_knowledge;
#[cfg(feature = "agents")]
mod agent_pane;
mod agents;
#[cfg(feature = "agents")]
pub(crate) mod agents_mcp;
mod commands;
mod dispatch;
mod editing;
mod fold_navigation;
mod folding;
mod key_sequence;
mod marks;
mod motion;
mod operators;
mod parsing;
mod pickers;
mod quickfix;
mod registers;
mod shell;
mod source_control;
mod state;
mod substitute;
mod swift_motion;
mod terminal;
#[cfg(feature = "agents")]
mod trust_manager;
mod visual;
mod vlf;
mod windows;
#[cfg(feature = "agents")]
mod write_leases;
const VLF_SOURCE_CONTROL_DISABLED_REASON: &str = "requires whole-buffer diff/blame scans";

/// Background source-control refresh is skipped for non-VLF buffers larger than this
/// line threshold. For constrained-sized buffers the full clone + diff would block the UI;
/// use an explicit git command (`]c`, `[c`, or `:GitDiff`) to refresh on demand.
const CONSTRAINED_GIT_REFRESH_MAX_LINES: usize = 50_000;

pub(crate) use parsing::{
    line_col_for_offset, parse_ex_range, parse_substitute_cmd, smart_case_sensitive,
    text_obj_bracket, text_obj_quote, text_obj_tag, text_obj_word,
};
pub(crate) use state::{
    App, HoverPopup, Mode, Operator, PendingCharFind, PrivilegedSavePending, RepeatableMotion,
    SubstitutePending, SwiftMotionState, SwiftMotionTarget, Viewport,
};

// Served to the bin's `tests/` modules; the lib test build compiles this
// module tree without those modules, so the lint fires there.
#[cfg(all(feature = "agents", test))]
#[allow(unused_imports)]
pub(crate) use crate::policy::{
    EXECUTE_GRANT_MAX_USES as PERSISTENT_TERMINAL_MAX_USES,
    WRITE_GRANT_MAX_USES as PERSISTENT_WRITE_MAX_USES,
};
#[cfg(all(feature = "agents", test))]
#[allow(unused_imports)]
pub(crate) use agent_bridge::{
    ActionLogEntry, AgentTerminals, ApprovalChoice, OwnedTerminalStop,
    PERSISTENT_TERMINAL_OPTION_LABEL, PreparedWrite, TerminalOwner, ToolApprovalMode,
    WriteExpectation, WriteReplyKind,
};

#[cfg(feature = "agents")]
pub(crate) use crate::text::wrap_text;
#[cfg(feature = "agents")]
pub(crate) use agent_pane::{
    AGENTS_NICK_COL_WIDTH, AGENTS_PANE_BOTTOM_HEIGHT, AGENTS_PANE_RIGHT_WIDTH, AgentPaneLayout,
    AgentThreadUi, MessageRenderKind, ThreadUiState, TranscriptItem, format_duration,
    turn_metrics_label,
};

const SWIFT_MOTION_LABELS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
const SWIFT_MOTION_MAX_TARGETS: usize = SWIFT_MOTION_LABELS.len() * SWIFT_MOTION_LABELS.len();

fn normalize_pasted_line_endings(text: String) -> String {
    if !text.contains('\r') {
        return text;
    }

    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(ch);
        }
    }
    normalized
}

fn parse_surround_pair(spec: &str) -> Option<(String, String)> {
    match spec {
        "(" | ")" | "b" => Some(("(".to_owned(), ")".to_owned())),
        "[" | "]" => Some(("[".to_owned(), "]".to_owned())),
        "{" | "}" | "B" => Some(("{".to_owned(), "}".to_owned())),
        "<" | ">" => Some(("<".to_owned(), ">".to_owned())),
        "\"" => Some(("\"".to_owned(), "\"".to_owned())),
        "'" => Some(("'".to_owned(), "'".to_owned())),
        "`" => Some(("`".to_owned(), "`".to_owned())),
        _ => {
            let mut chars = spec.chars();
            let open = chars.next()?;
            let close = chars.next()?;
            if chars.next().is_some() { None } else { Some((open.to_string(), close.to_string())) }
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum ShellSelectionMode {
    Replace,
    IgnoreOutput,
    InsertBefore,
    InsertAfter,
    KeepByStatus,
}

pub(super) fn format_git_hunk_replacement(
    old_lines: &[&str],
    new_start: usize,
    new_count: usize,
    total_lines: usize,
) -> String {
    if old_lines.is_empty() {
        return String::new();
    }

    let joined = old_lines.join("\n");
    let has_following_line = new_start + new_count < total_lines;
    if new_count == 0 {
        if total_lines == 0 {
            joined
        } else if has_following_line {
            format!("{joined}\n")
        } else {
            format!("\n{joined}")
        }
    } else if has_following_line {
        format!("{joined}\n")
    } else {
        joined
    }
}
