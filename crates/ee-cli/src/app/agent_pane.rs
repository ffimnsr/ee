//! Irssi-style agents pane: transcript scrollback, status footer, and
//! composer input (Phase 3).
//!
//! The pane is frontend-owned: all agent state arrives as deterministic
//! [`AgentEvent`]s from `ee-agent-host` and is rendered from the local
//! transcript model.  This module never crafts ACP JSON; prompt, permission,
//! and elicitation responses go through host APIs only.
//!
//! The host bridge runs on a dedicated worker thread over a single-threaded
//! tokio runtime so the TUI loop never blocks on subprocess or session I/O.
//! Everything here is gated by the `agents` cargo feature; without it the
//! pane state is absent and `:agents` reports the compile-time disabled
//! message.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::policy::is_protected_relative_path;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ee_agent_host::events::{
    AgentConnectionState, PermissionRequestInfo, RecoverableInfo, ThreadCloseReason, TurnMetrics,
};
use ee_agent_host::{
    AgentError, AgentEvent, AgentManager, AgentManagerConfig, AgentThread, ClientRequestResponse,
    ClientRequestResult, EvidenceRevision, PermissionRequestId, ToolCallState, TurnEvidenceSummary,
};
use ee_agent_protocol::{
    AvailableCommand, ContentBlock, CreateElicitationRequest, CreateElicitationResponse,
    ElicitationAcceptAction, ElicitationAction, ElicitationContentValue, ElicitationMode,
    ElicitationPropertySchema, ElicitationSchema, McpServer, McpServerStdio, PermissionOption,
    PlanEntryPriority, PlanEntryStatus, RequestPermissionOutcome, SelectedPermissionOutcome,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionValue, SessionConfigSelectOptions,
    SessionConfigValueId, SessionId, SessionModeId, SessionUpdate, TextContent, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolKind,
};
use ignore::WalkBuilder;
use ratatui::layout::Rect;
use tokio::runtime::Builder as TokioBuilder;
use tokio::sync::mpsc as tokio_mpsc;
use url::Url;

use super::agent_export::{format_agent_transcript_markdown, write_agent_transcript_export};
use super::*;

// ── Pane geometry constants ──────────────────────────────────────────────────

/// Width of the right-split agents pane.
pub(crate) const AGENTS_PANE_RIGHT_WIDTH: u16 = 48;
/// Height of the bottom-split agents pane.
pub(crate) const AGENTS_PANE_BOTTOM_HEIGHT: u16 = 14;
/// Nick column width inside transcript lines.
pub(crate) const AGENTS_NICK_COL_WIDTH: usize = 10;
/// Maximum transcript items retained per thread.
pub(crate) const AGENTS_TRANSCRIPT_MAX: usize = 1500;
/// Maximum stderr/debug lines retained per thread.
pub(crate) const AGENTS_STDERR_MAX: usize = 200;
/// Lines scrolled per PageUp/PageDown key press.
pub(crate) const AGENTS_SCROLL_PAGE: usize = 10;
/// Maximum explicitly attached context files per agent session.
pub(crate) const AGENT_CONTEXT_MAX_FILES: usize = 8;
/// Maximum bytes captured from one explicitly attached context file.
pub(crate) const AGENT_CONTEXT_MAX_FILE_BYTES: usize = 64 * 1024;
/// Maximum bytes captured from all explicitly attached context files.
pub(crate) const AGENT_CONTEXT_MAX_TOTAL_BYTES: usize = 128 * 1024;
/// Maximum explicit extra workspace roots granted in one Agents TUI process.
const AGENT_ADDITIONAL_ROOT_MAX: usize = 8;
/// Maximum terminal output tail rendered by `/ps` and `/tasks`.
const AGENT_TERMINAL_OUTPUT_TAIL_BYTES: usize = 4 * 1024;
/// Maximum agent-owned terminals targeted by one `/stop all` request.
const AGENT_TERMINAL_STOP_ALL_MAX: usize = 16;

// ── Pane layout ──────────────────────────────────────────────────────────────

/// Where the agents pane sits.  `Closed` keeps the editor layout untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AgentPaneLayout {
    #[default]
    Closed,
    Right,
    Bottom,
    Full,
}

impl AgentPaneLayout {
    /// Parses the `:agents_layout` argument.
    fn parse(arg: &str) -> Option<Self> {
        match arg {
            "right" => Some(Self::Right),
            "bottom" => Some(Self::Bottom),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

// ── Transcript model ─────────────────────────────────────────────────────────

/// How a chat message line is attributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageRenderKind {
    User,
    Assistant,
    Thought,
}

/// Stable local identifier for reasoning and tool calls from one agent turn.
pub(crate) type ResponseGroupId = u64;

const AGENT_PROMPT_HISTORY_MAX: usize = 200;
const AGENT_PROMPT_QUEUE_MAX: usize = 16;
const AGENT_REVIEW_CONTEXT_MAX_BYTES: usize = 32 * 1024;

/// One locally queued follow-up. It is never an ACP-side queue: EE dispatches
/// it only after current turn reaches a terminal ready state.
#[derive(Debug, Clone)]
pub(crate) struct QueuedPrompt {
    pub(crate) text: String,
    /// One-turn snapshots travel with their queued prompt and never enter
    /// transcript, export, history, or another session.
    pub(crate) next_prompt_context_files: Vec<AgentContextFile>,
}

/// Frontend handoff request for editing one draft outside the TUI. The terminal
/// owning loop consumes it; this pane never spawns an interactive process.
#[derive(Debug, Clone)]
pub(crate) struct ExternalEditorRequest {
    pub(crate) session_id: Option<String>,
    pub(crate) draft: String,
}

/// One line in the IRC-style scrollback.
#[derive(Debug, Clone)]
pub(crate) enum TranscriptItem {
    /// A chat message with a nick column.
    Message {
        nick: String,
        text: String,
        kind: MessageRenderKind,
        message_id: Option<String>,
        response_group: Option<ResponseGroupId>,
        at: SystemTime,
    },
    /// IRC-style tool notice (`* title [status]`).
    ToolCall {
        id: String,
        title: String,
        status: String,
        detail: String,
        response_group: ResponseGroupId,
        at: SystemTime,
    },
    /// A permission request shown in the transcript.
    Permission { title: String, options: Vec<String>, at: SystemTime },
    /// An elicitation request shown in the transcript.
    Elicitation {
        agent: String,
        message: String,
        url: Option<String>,
        url_host: Option<String>,
        at: SystemTime,
    },
    /// `-!-` system notice.
    System { text: String, at: SystemTime },
    /// `-!-` stderr/debug line.
    Stderr { text: String, at: SystemTime },
}

/// Thread lifecycle state shown in the channel list and footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadUiState {
    Starting,
    Ready,
    Running,
    PausedRecoverable,
    Closed,
    Failed,
}

/// A paused turn awaiting a resume/discard decision.
#[derive(Debug, Clone)]
pub(crate) struct PendingRecovery {
    /// Structured payload from the agent's JSON-RPC error `data`.
    pub(crate) info: RecoverableInfo,
    /// The prompt blocks the paused turn was started with, re-sent verbatim
    /// on Resume.
    pub(crate) prompt: Vec<ContentBlock>,
}

/// Immutable user-selected context snapshot for one agent session.
#[derive(Debug, Clone)]
pub(crate) struct AgentContextFile {
    /// Canonical absolute path, used only for replacement within this session.
    pub(crate) path: PathBuf,
    /// Canonical primary-workspace-relative path shown to user and agent.
    pub(crate) relative_path: String,
    /// Redacted file content captured at attachment time.
    pub(crate) content: String,
}

/// One open agent session thread (IRC channel equivalent).
#[derive(Debug)]
pub(crate) struct AgentThreadUi {
    /// Stable per-pane channel number (used by tests and channel rendering).
    #[allow(dead_code)]
    pub(crate) index: usize,
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    /// Parent session for locally seeded forks. Never sent as an ACP mutation.
    pub(crate) fork_parent_session_id: Option<String>,
    /// Nick shown in the transcript (`you` is reserved for the user).
    pub(crate) nick: String,
    pub(crate) display_name: String,
    pub(crate) state: ThreadUiState,
    /// Unread messages since the thread was last focused.
    pub(crate) unread: usize,
    /// Activity marker: new content arrived while unfocused.
    pub(crate) activity: bool,
    /// Host handle for prompts, cancels, and permission responses.
    pub(crate) host: AgentThread,
    pub(crate) transcript: Vec<TranscriptItem>,
    /// Index of the optimistic `you` message waiting for the agent echo.
    pub(crate) optimistic_message: Option<usize>,
    /// Per-thread composer draft (preserved across switches).
    pub(crate) draft: String,
    /// Submitted plain prompt text. Context snapshots are intentionally absent.
    pub(crate) prompt_history: Vec<String>,
    /// Selected history item while navigating/searching.
    pub(crate) prompt_history_cursor: Option<usize>,
    /// Draft preserved before history navigation so Down can restore it.
    pub(crate) prompt_history_restore_draft: Option<String>,
    /// Local follow-ups waiting for current agent turn to finish.
    pub(crate) queued_prompts: VecDeque<QueuedPrompt>,
    /// Explicit user stash for a long composer draft. Session-local only.
    pub(crate) stashed_draft: Option<String>,
    /// Scrollback mode suppresses response-group UI and shows safe tool detail.
    pub(crate) transcript_raw: bool,
    /// Show sanitized tool summaries for every response group.
    pub(crate) transcript_detail: bool,
    /// Visual-row offset from the top of rendered transcript.
    pub(crate) scroll: usize,
    /// When true new content keeps the view pinned to the newest line.
    pub(crate) stick_to_bottom: bool,
    /// Rendered usage summary (tokens / cost) from the latest update.
    pub(crate) usage: Option<String>,
    /// Stop reason of the last completed turn.
    pub(crate) stop_reason: Option<String>,
    /// When the current turn started (live elapsed rendering while running).
    pub(crate) turn_started_at: Option<Instant>,
    /// Metrics of completed turns, keyed by their response group.
    pub(crate) turn_metrics: BTreeMap<ResponseGroupId, TurnMetrics>,
    /// Metrics of the most recent completed turn (footer rendering).
    pub(crate) last_turn_metrics: Option<TurnMetrics>,
    /// Host-derived completion state; distinct from ACP `stopReason`.
    pub(crate) terminal_evidence: Option<TurnEvidenceSummary>,
    /// Current turn's agent-written paths and aggregate evidence revision.
    /// Remains pane-local; host evidence stores only redacted opaque ids.
    pub(crate) verification_paths: Vec<PathBuf>,
    pub(crate) verification_revision: Option<EvidenceRevision>,
    /// Active local group for the currently streaming agent turn.
    pub(crate) active_response_group: Option<ResponseGroupId>,
    /// Next response-group identifier for this thread.
    pub(crate) next_response_group: ResponseGroupId,
    /// Response group selected for keyboard collapse control.
    pub(crate) selected_response_group: Option<ResponseGroupId>,
    /// Response groups whose reasoning and tools are hidden.
    pub(crate) collapsed_response_groups: BTreeSet<ResponseGroupId>,
    /// Response groups whose tool input/output detail is expanded.
    pub(crate) expanded_tool_details: BTreeSet<ResponseGroupId>,
    /// Latest agent plan snapshot, rendered in a modal instead of scrollback.
    pub(crate) current_plan: Vec<(String, char)>,
    /// Whether the user has opened the current plan modal.
    pub(crate) plan_modal_open: bool,
    /// Last turn error, when any.
    pub(crate) last_error: Option<String>,
    /// Paused turn awaiting resume/discard, when the agent reported a
    /// recoverable interruption.
    pub(crate) pending_recovery: Option<PendingRecovery>,
    /// Prompt blocks of the most recent turn, kept for resume until the turn
    /// reaches a terminal state.
    pub(crate) last_prompt: Option<Vec<ContentBlock>>,
    /// Explicit, bounded file snapshots included with future turns in this session.
    pub(crate) context_files: Vec<AgentContextFile>,
    /// Bounded file snapshots attached to only next submitted prompt.
    /// Never persisted, exported, or copied to child sessions.
    pub(crate) next_prompt_context_files: Vec<AgentContextFile>,
    /// Slash commands currently advertised by the agent.
    pub(crate) available_commands: Vec<AvailableCommand>,
    /// User-selected local name, persisted with the reconnect record.
    pub(crate) session_name: Option<String>,
    /// Session title from `session_info_update`, when present.
    pub(crate) session_title: Option<String>,
    /// Session metadata timestamp from `session_info_update`, when present.
    pub(crate) session_updated_at: Option<String>,
}

impl AgentThreadUi {
    /// Transcript chat messages as `(nick, text)` pairs, in stream order.
    /// (test assertion surface; the renderer walks the raw transcript)
    #[allow(dead_code)]
    pub(crate) fn message_pairs(&self) -> Vec<(String, String)> {
        self.transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Message { nick, text, .. } => Some((nick.clone(), text.clone())),
                _ => None,
            })
            .collect()
    }

    /// System notices in transcript order.
    #[allow(dead_code)]
    pub(crate) fn system_notices(&self) -> Vec<String> {
        self.transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::System { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Tool call notices in transcript order.
    #[allow(dead_code)]
    pub(crate) fn tool_notices(&self) -> Vec<(String, String)> {
        self.transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::ToolCall { title, status, .. } => {
                    Some((title.clone(), status.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Plan markers in transcript order (`content`, `marker`).
    #[allow(dead_code)]
    pub(crate) fn plan_entries(&self) -> Vec<(String, char)> {
        self.current_plan.clone()
    }

    /// Moves within transcript visual rows, clamped to rendered scroll bounds.
    pub(crate) fn scroll_by(&mut self, delta: isize, max_scroll: usize) {
        self.scroll = (self.scroll as isize + delta).clamp(0, max_scroll as isize) as usize;
        // Keep the user's explicit upward-scroll intent even when transcript
        // currently fits viewport; later streamed rows must not pull view down.
        self.stick_to_bottom = delta >= 0 && self.scroll == max_scroll;
    }

    /// Moves to a transcript visual row, clamped to rendered scroll bounds.
    pub(crate) fn scroll_to(&mut self, offset: usize, max_scroll: usize) {
        self.scroll = offset.min(max_scroll);
        self.stick_to_bottom = self.scroll == max_scroll;
    }

    /// Slash command names currently advertised by the agent.
    #[allow(dead_code)]
    pub(crate) fn command_names(&self) -> Vec<String> {
        self.available_commands.iter().map(|command| command.name.clone()).collect()
    }

    /// Appends a message item, merging chunks that continue the same message.
    ///
    /// A `User` chunk arriving while an optimistic `you` message is pending
    /// replaces that optimistic item (the agent echo of the user's prompt).
    fn push_message(
        &mut self,
        nick: &str,
        text: &str,
        kind: MessageRenderKind,
        message_id: Option<String>,
        response_group: Option<ResponseGroupId>,
    ) {
        if text.trim().is_empty() {
            return;
        }
        if kind == MessageRenderKind::User
            && let Some(index) = self.optimistic_message.take()
            && let Some(TranscriptItem::Message { text: target, .. }) =
                self.transcript.get_mut(index)
        {
            *target = text.to_string();
            return;
        }
        if let Some(message_id) = message_id.as_ref() {
            let merges = self.transcript.iter_mut().rev().find(|item| {
                matches!(
                    item,
                    TranscriptItem::Message {
                        kind: existing_kind,
                        message_id: Some(existing_id),
                        response_group: existing_response_group,
                        ..
                    } if *existing_kind == kind
                        && existing_id == message_id
                        && *existing_response_group == response_group
                )
            });
            if let Some(TranscriptItem::Message { text: target, .. }) = merges {
                target.push_str(text);
                return;
            }
        } else if let Some(TranscriptItem::Message {
            kind: existing_kind,
            message_id: None,
            text: target,
            ..
        }) = self.transcript.last_mut()
            && *existing_kind == kind
        {
            target.push_str(text);
            return;
        }
        self.transcript.push(TranscriptItem::Message {
            nick: nick.to_string(),
            text: text.to_string(),
            kind,
            message_id,
            response_group,
            at: SystemTime::now(),
        });
        self.trim_transcript();
    }

    /// Upserts a tool call notice by id.
    fn push_tool_call(
        &mut self,
        id: &str,
        title: &str,
        status: &str,
        detail: &str,
        response_group: ResponseGroupId,
    ) {
        let target = self.transcript.iter_mut().find(
            |item| matches!(item, TranscriptItem::ToolCall { id: existing, .. } if existing == id),
        );
        if let Some(TranscriptItem::ToolCall {
            title: existing_title,
            status: existing_status,
            detail: existing_detail,
            ..
        }) = target
        {
            if !title.is_empty() {
                *existing_title = title.to_string();
            }
            if !status.is_empty() {
                *existing_status = status.to_string();
            }
            if !detail.is_empty() {
                *existing_detail = detail.to_string();
            }
            return;
        }
        self.transcript.push(TranscriptItem::ToolCall {
            id: id.to_string(),
            title: title.to_string(),
            status: status.to_string(),
            detail: detail.to_string(),
            response_group,
            at: SystemTime::now(),
        });
        self.trim_transcript();
    }

    fn ensure_response_group(&mut self) -> ResponseGroupId {
        if let Some(group) = self.active_response_group {
            return group;
        }

        let group = self.next_response_group;
        self.next_response_group += 1;
        self.active_response_group = Some(group);
        self.selected_response_group = Some(group);
        group
    }

    /// Collapses the completed turn's reasoning and tool calls, returning
    /// the group that finished (for attaching per-turn metrics).
    fn finish_response_group(&mut self) -> Option<ResponseGroupId> {
        let group = self.active_response_group.take();
        if let Some(group) = group {
            self.collapsed_response_groups.insert(group);
        }
        group
    }

    /// Records the metrics of the just-finished turn against its response
    /// group, then collapses the group.
    fn record_turn_metrics(&mut self, metrics: TurnMetrics) {
        if let Some(group) = self.finish_response_group() {
            self.turn_metrics.insert(group, metrics.clone());
        }
        self.last_turn_metrics = Some(metrics);
    }

    fn response_group_for_tool_call(&mut self, tool_call_id: &str) -> ResponseGroupId {
        self.transcript
            .iter()
            .find_map(|item| match item {
                TranscriptItem::ToolCall { id, response_group, .. } if id == tool_call_id => {
                    Some(*response_group)
                }
                _ => None,
            })
            .unwrap_or_else(|| self.ensure_response_group())
    }

    pub(crate) fn response_group_ids(&self) -> Vec<ResponseGroupId> {
        self.transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Message {
                    kind: MessageRenderKind::Thought,
                    response_group,
                    ..
                } => *response_group,
                TranscriptItem::ToolCall { response_group, .. } => Some(*response_group),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn response_group_counts(&self, group: ResponseGroupId) -> (usize, usize) {
        self.transcript.iter().fold((0, 0), |(thoughts, tools), item| match item {
            TranscriptItem::Message {
                kind: MessageRenderKind::Thought,
                response_group: Some(item_group),
                ..
            } if *item_group == group => (thoughts + 1, tools),
            TranscriptItem::ToolCall { response_group, .. } if *response_group == group => {
                (thoughts, tools + 1)
            }
            _ => (thoughts, tools),
        })
    }

    pub(crate) fn response_group_for_item(&self, item: &TranscriptItem) -> Option<ResponseGroupId> {
        match item {
            TranscriptItem::Message {
                kind: MessageRenderKind::Thought, response_group, ..
            } => *response_group,
            TranscriptItem::ToolCall { response_group, .. } => Some(*response_group),
            _ => None,
        }
    }

    /// Replaces the plan snapshot wholesale (ACP plans are complete snapshots).
    fn replace_plan(&mut self, entries: Vec<(String, char)>) {
        self.current_plan = entries;
    }

    /// Appends a system notice.
    pub(crate) fn push_system(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptItem::System { text: text.into(), at: SystemTime::now() });
        self.trim_transcript();
    }

    /// Appends a stderr/debug line (bounded).
    fn push_stderr(&mut self, line: impl Into<String>) {
        self.transcript.push(TranscriptItem::Stderr { text: line.into(), at: SystemTime::now() });
        let mut stderr_count = self
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Stderr { .. }))
            .count();
        let mut index = 0;
        while stderr_count > AGENTS_STDERR_MAX {
            if matches!(self.transcript[index], TranscriptItem::Stderr { .. }) {
                self.transcript.remove(index);
                stderr_count -= 1;
            } else {
                index += 1;
            }
        }
        self.trim_transcript();
    }

    fn trim_transcript(&mut self) {
        while self.transcript.len() > AGENTS_TRANSCRIPT_MAX {
            self.transcript.remove(0);
        }
    }

    fn record_prompt_history(&mut self, prompt: &str) {
        self.prompt_history_cursor = None;
        self.prompt_history_restore_draft = None;
        if self.prompt_history.last().is_some_and(|previous| previous == prompt) {
            return;
        }
        self.prompt_history.push(prompt.to_string());
        if self.prompt_history.len() > AGENT_PROMPT_HISTORY_MAX {
            self.prompt_history.remove(0);
        }
    }

    pub(crate) fn response_group_change_summary(&self, group: ResponseGroupId) -> Option<String> {
        let mut paths = Vec::new();
        for item in &self.transcript {
            let TranscriptItem::ToolCall { detail, response_group, .. } = item else { continue };
            if *response_group != group {
                continue;
            }
            for part in detail.split(" · ") {
                let path = part
                    .strip_prefix("content: diff: new file ")
                    .or_else(|| part.strip_prefix("content: diff: "));
                if let Some(path) = path.filter(|path| !path.is_empty())
                    && !paths.iter().any(|existing| existing == path)
                {
                    paths.push(path.to_string());
                }
            }
        }
        (!paths.is_empty()).then(|| {
            let overflow = paths.len().saturating_sub(4);
            let mut summary = format!(
                "changes: {}",
                paths.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
            );
            if overflow > 0 {
                summary.push_str(&format!(" +{overflow}"));
            }
            summary
        })
    }
}

// ── Permission prompt ────────────────────────────────────────────────────────

/// A pending `session/request_permission` awaiting an explicit choice.
#[derive(Debug)]
pub(crate) struct PermissionPrompt {
    pub(crate) thread_index: usize,
    pub(crate) request_id: PermissionRequestId,
    #[allow(dead_code)]
    pub(crate) tool_title: String,
    pub(crate) options: Vec<PermissionOption>,
    pub(crate) selected: usize,
}

/// Local picker shown after submitting `/mode` without an argument.
#[derive(Debug)]
pub(crate) struct ModeSelectionPrompt {
    pub(crate) thread_index: usize,
    pub(crate) options: Vec<String>,
    pub(crate) selected: usize,
}

/// Explicit confirmation required before enabling session-local bypass mode.
#[derive(Debug)]
pub(crate) struct ApprovalModeConfirmation {
    pub(crate) thread_index: usize,
    pub(crate) session_id: String,
}

/// Explicit confirmation before removing local session state. This never deletes provider data.
#[derive(Debug)]
pub(crate) struct SessionDeletionConfirmation {
    pub(crate) thread_index: usize,
    pub(crate) session_id: String,
    pub(crate) session_name: String,
}

/// Explicit confirmation before granting an external workspace root to agent tools.
#[derive(Debug)]
pub(crate) struct AdditionalDirectoryConfirmation {
    pub(crate) path: PathBuf,
}

/// Explicit confirmation before stopping more than one agent-owned terminal.
#[derive(Debug)]
pub(crate) struct TerminalStopConfirmation {
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    pub(crate) terminal_ids: Vec<String>,
}

// ── Elicitation prompt ───────────────────────────────────────────────────────

/// One form field rendered as a TUI widget.
#[derive(Debug, Clone)]
pub(crate) struct ElicitationFieldUi {
    pub(crate) name: String,
    pub(crate) title: String,
    #[allow(dead_code)]
    pub(crate) kind: ElicitationFieldKind,
    pub(crate) value: ElicitationFieldValue,
    /// Set when the schema shape is unsupported; the field is read-only and
    /// the prompt reports it visibly.
    pub(crate) unsupported: Option<String>,
    pub(crate) required: bool,
}

impl ElicitationFieldUi {
    /// Rendered label for the field.
    pub(crate) fn label(&self) -> String {
        if self.title.is_empty() { self.name.clone() } else { self.title.clone() }
    }

    /// Rendered current value for the composer/status area.
    pub(crate) fn display_value(&self) -> String {
        match &self.value {
            ElicitationFieldValue::Text(text) => {
                if text.is_empty() {
                    String::from("(empty)")
                } else {
                    text.clone()
                }
            }
            ElicitationFieldValue::Boolean(value) => {
                if *value {
                    String::from("yes")
                } else {
                    String::from("no")
                }
            }
            ElicitationFieldValue::Enum(selected, options) => {
                options.get(*selected).cloned().unwrap_or_else(|| String::from("(invalid)"))
            }
            ElicitationFieldValue::Number(text) => {
                if text.is_empty() {
                    String::from("(empty)")
                } else {
                    text.clone()
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElicitationFieldKind {
    Text,
    Boolean,
    Enum,
    Number,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ElicitationFieldValue {
    Text(String),
    Boolean(bool),
    Enum(usize, Vec<String>),
    Number(String),
}

/// A pending `elicitation/create` request awaiting user input.
#[derive(Debug)]
pub(crate) struct ElicitationPrompt {
    pub(crate) thread_index: Option<usize>,
    pub(crate) agent_label: String,
    pub(crate) message: String,
    pub(crate) url: Option<String>,
    pub(crate) url_host: Option<String>,
    /// URL-mode `elicitationId` used by `elicitation/complete`.
    pub(crate) completion_id: Option<String>,
    pub(crate) fields: Vec<ElicitationFieldUi>,
    pub(crate) selected_field: usize,
    /// 0 = accept, 1 = decline, 2 = cancel.
    pub(crate) selected_choice: usize,
    /// Response channel; answering resolves the agent's pending request.
    pub(crate) reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    /// Visible rejection reason for unsupported schema shapes or unsafe requests.
    pub(crate) unsupported_reason: Option<String>,
}

impl ElicitationPrompt {
    /// Maximum number of form fields rendered per elicitation (Phase 7
    /// resource limit); larger schemas fail visibly.
    const MAX_ELICITATION_FIELDS: usize = 24;
    /// Maximum JSON nesting depth accepted for a form schema. ACP v1 schemas
    /// are normally shallow; this caps future/extension payloads before UI use.
    const MAX_ELICITATION_SCHEMA_DEPTH: usize = 12;

    /// Builds a prompt from an ACP form-mode request.
    fn from_form(
        thread_index: Option<usize>,
        agent_label: String,
        message: String,
        schema: &ElicitationSchema,
        reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let required: BTreeSet<String> =
            schema.required.clone().unwrap_or_default().into_iter().collect();
        let mut fields = Vec::new();
        let mut unsupported = Vec::new();
        if let Ok(value) = serde_json::to_value(schema)
            && json_depth(&value) > Self::MAX_ELICITATION_SCHEMA_DEPTH
        {
            unsupported
                .push(format!("schema depth exceeds {}", Self::MAX_ELICITATION_SCHEMA_DEPTH));
        }
        let mut over_cap = false;
        for (name, property) in &schema.properties {
            if fields.len() >= Self::MAX_ELICITATION_FIELDS {
                over_cap = true;
                break;
            }
            let field = ElicitationFieldUi::from_property(name, property, &required);
            if let Some(reason) = &field.unsupported {
                unsupported.push(format!("unsupported field {}: {reason}", field.label()));
            }
            fields.push(field);
        }
        if over_cap {
            unsupported.push(format!("too many fields (> {})", Self::MAX_ELICITATION_FIELDS));
        }
        if let Some(reason) = detect_secretive_elicitation_request(&message, &fields) {
            unsupported.push(reason);
        }
        Self {
            thread_index,
            agent_label,
            message,
            url: None,
            url_host: None,
            completion_id: None,
            fields,
            selected_field: 0,
            selected_choice: 0,
            reply,
            unsupported_reason: if unsupported.is_empty() {
                None
            } else {
                Some(unsupported.join(", "))
            },
        }
    }

    /// Builds a prompt from an ACP URL-mode request.
    fn from_url(
        thread_index: Option<usize>,
        agent_label: String,
        message: String,
        completion_id: String,
        url: String,
        reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let url_host =
            Url::parse(&url).ok().and_then(|parsed| parsed.host_str().map(str::to_string));
        Self {
            thread_index,
            agent_label,
            message,
            url: Some(url),
            url_host,
            completion_id: Some(completion_id),
            fields: Vec::new(),
            selected_field: 0,
            selected_choice: 0,
            reply,
            unsupported_reason: None,
        }
    }

    /// Builds the response content map for the current field values.
    fn content_map(&self) -> Result<BTreeMap<String, ElicitationContentValue>, String> {
        if let Some(reason) = &self.unsupported_reason {
            return Err(reason.clone());
        }
        let mut content = BTreeMap::new();
        for field in &self.fields {
            if field.unsupported.is_some() {
                return Err(format!("unsupported field: {}", field.label()));
            }
            if field.required && field.value.is_empty() {
                return Err(format!("required field missing: {}", field.label()));
            }
            let value = field
                .value
                .to_content()
                .ok_or_else(|| format!("invalid value for {}", field.label()))?;
            content.insert(field.name.clone(), value);
        }
        Ok(content)
    }

    fn submit_action(&self, accept: bool) -> ElicitationAction {
        if !accept {
            return ElicitationAction::Cancel;
        }
        match self.selected_choice {
            1 => ElicitationAction::Decline,
            2 => ElicitationAction::Cancel,
            _ => ElicitationAction::Accept(ElicitationAcceptAction::new()),
        }
    }
}

fn detect_secretive_elicitation_request(
    message: &str,
    fields: &[ElicitationFieldUi],
) -> Option<String> {
    let mut blocked = Vec::new();
    for field in fields {
        let label = field.label();
        if looks_sensitive_elicitation_text(&field.name) || looks_sensitive_elicitation_text(&label)
        {
            blocked.push(label);
        }
    }
    if looks_sensitive_elicitation_text(message) {
        blocked.push(String::from("request message"));
    }
    blocked.sort();
    blocked.dedup();
    if blocked.is_empty() {
        None
    } else {
        Some(format!("secret-like elicitation requests are blocked: {}", blocked.join(", ")))
    }
}

fn looks_sensitive_elicitation_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ee_agent_host::redact::is_secret_key(&lower)
        || [
            "password",
            "passcode",
            "secret",
            "token",
            "credential",
            "api key",
            "apikey",
            "private key",
            "access key",
            "auth code",
            "authorization code",
            "otp",
            "one-time code",
        ]
        .into_iter()
        .any(|needle| lower.contains(needle))
}

fn json_depth(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(values) => {
            1 + values.iter().map(json_depth).max().unwrap_or_default()
        }
        serde_json::Value::Object(values) => {
            1 + values.values().map(json_depth).max().unwrap_or_default()
        }
        _ => 1,
    }
}

impl ElicitationFieldValue {
    fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) | Self::Number(text) => text.trim().is_empty(),
            Self::Boolean(_) | Self::Enum(_, _) => false,
        }
    }

    fn to_content(&self) -> Option<ElicitationContentValue> {
        match self {
            Self::Text(text) => Some(ElicitationContentValue::String(text.clone())),
            Self::Number(text) => {
                let text = text.trim();
                if text.parse::<i64>().is_ok() {
                    text.parse::<i64>().ok().map(ElicitationContentValue::Integer)
                } else {
                    text.parse::<f64>().ok().map(ElicitationContentValue::Number)
                }
            }
            Self::Boolean(value) => Some(ElicitationContentValue::Boolean(*value)),
            Self::Enum(selected, options) => {
                options.get(*selected).cloned().map(ElicitationContentValue::String)
            }
        }
    }
}

impl ElicitationFieldUi {
    /// Converts one ACP form property into a UI field; unsupported shapes
    /// are preserved with a visible reason.
    fn from_property(
        name: &str,
        property: &ElicitationPropertySchema,
        required: &BTreeSet<String>,
    ) -> Self {
        let required = required.contains(name);
        let unsupported = |reason: String| Self {
            name: name.to_string(),
            title: String::new(),
            kind: ElicitationFieldKind::Text,
            value: ElicitationFieldValue::Text(String::new()),
            unsupported: Some(reason),
            required,
        };
        match property {
            ElicitationPropertySchema::String(schema) => {
                if let Some(options) = schema.enum_values.clone()
                    && !options.is_empty()
                {
                    let default = options
                        .iter()
                        .position(|option| schema.default.as_deref() == Some(option.as_str()));
                    Self {
                        name: name.to_string(),
                        title: schema.title.clone().unwrap_or_default(),
                        kind: ElicitationFieldKind::Enum,
                        value: ElicitationFieldValue::Enum(default.unwrap_or(0), options),
                        unsupported: None,
                        required,
                    }
                } else {
                    Self {
                        name: name.to_string(),
                        title: schema.title.clone().unwrap_or_default(),
                        kind: ElicitationFieldKind::Text,
                        value: ElicitationFieldValue::Text(
                            schema.default.clone().unwrap_or_default(),
                        ),
                        unsupported: None,
                        required,
                    }
                }
            }
            ElicitationPropertySchema::Boolean(schema) => Self {
                name: name.to_string(),
                title: schema.title.clone().unwrap_or_default(),
                kind: ElicitationFieldKind::Boolean,
                value: ElicitationFieldValue::Boolean(schema.default.unwrap_or(false)),
                unsupported: None,
                required,
            },
            ElicitationPropertySchema::Number(schema) => Self {
                name: name.to_string(),
                title: schema.title.clone().unwrap_or_default(),
                kind: ElicitationFieldKind::Number,
                value: ElicitationFieldValue::Number(
                    schema.default.map(|value| value.to_string()).unwrap_or_default(),
                ),
                unsupported: None,
                required,
            },
            ElicitationPropertySchema::Integer(schema) => Self {
                name: name.to_string(),
                title: schema.title.clone().unwrap_or_default(),
                kind: ElicitationFieldKind::Number,
                value: ElicitationFieldValue::Number(
                    schema.default.map(|value| value.to_string()).unwrap_or_default(),
                ),
                unsupported: None,
                required,
            },
            ElicitationPropertySchema::Array(_) => {
                unsupported(String::from("multi-select arrays are not supported"))
            }
            // Non-exhaustive upstream; unknown property schemas fail visibly.
            _ => unsupported(String::from("unknown property schema")),
        }
    }
}

// ── Host bridge ──────────────────────────────────────────────────────────────

/// Commands the UI enqueues for the host worker thread.
enum HostCommand {
    NewSession {
        agent_id: String,
        roots: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
        /// Stdio `ee --mcp-proxy` fallback entry (proxy mode on); the host
        /// swaps it for an ACP-native entry when the agent supports
        /// MCP-over-ACP (Phase 6b).
        ee_proxy_stdio_fallback: Option<McpServerStdio>,
        reply: std_mpsc::Sender<Result<AgentThread, String>>,
    },
    ReconnectSession {
        agent_id: String,
        session_id: String,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
        reply: std_mpsc::Sender<Result<AgentThread, String>>,
    },
    SendPrompt {
        thread: AgentThread,
        blocks: Vec<ContentBlock>,
    },
    ResumePrompt {
        thread: AgentThread,
        blocks: Vec<ContentBlock>,
    },
    SetMode {
        thread: AgentThread,
        mode_id: SessionModeId,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    SetConfigOption {
        thread: AgentThread,
        config_id: ee_agent_protocol::SessionConfigId,
        value: SessionConfigOptionValue,
        reply: std_mpsc::Sender<Result<String, String>>,
    },
    Cancel {
        thread: AgentThread,
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    Shutdown,
}

/// Runs async host operations on a dedicated worker thread.
///
/// The whole loop runs inside `block_on` and awaits commands over a tokio
/// channel, so the single-threaded runtime keeps driving spawned tasks
/// (connection driver, permission responders, elicitation handler futures)
/// even while no command is queued.  Commands execute sequentially so
/// per-connection request ordering is preserved; every operation carries an
/// internal timeout or cancellation path (host guarantees), so a hung agent
/// can never wedge the worker.
fn host_worker(
    runtime: tokio::runtime::Runtime,
    manager: AgentManager,
    mut rx: tokio_mpsc::UnboundedReceiver<HostCommand>,
) {
    runtime.block_on(async move {
        while let Some(command) = rx.recv().await {
            match command {
                HostCommand::NewSession {
                    agent_id,
                    roots,
                    mcp_servers,
                    ee_proxy_stdio_fallback,
                    reply,
                } => {
                    let result = manager
                        .new_session(&agent_id, roots, mcp_servers, ee_proxy_stdio_fallback)
                        .await;
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                HostCommand::ReconnectSession {
                    agent_id,
                    session_id,
                    cwd,
                    additional_directories,
                    mcp_servers,
                    reply,
                } => {
                    // Prefer `session/load`: it replays the conversation into
                    // the client.  Fall back to `session/resume` (no replay)
                    // only when the agent does not advertise load.
                    let session_id = ee_agent_protocol::SessionId::new(session_id);
                    let result = match manager
                        .load_session(
                            &agent_id,
                            session_id.clone(),
                            cwd.clone(),
                            additional_directories.clone(),
                            mcp_servers.clone(),
                        )
                        .await
                    {
                        Ok(thread) => Ok(thread),
                        Err(AgentError::CapabilityUnsupported { method })
                            if method == "session/load" =>
                        {
                            manager
                                .resume_session(
                                    &agent_id,
                                    session_id,
                                    cwd,
                                    additional_directories,
                                    mcp_servers,
                                )
                                .await
                        }
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                HostCommand::SendPrompt { thread, blocks } => {
                    // Detached: the turn streams through host events, and a
                    // later `Cancel` command must be able to run while the
                    // prompt is still in flight (sequential execution would
                    // deadlock against an agent that waits for the cancel
                    // before answering the prompt).  Terminal turn events
                    // (completed/cancelled/failed) arrive through the event
                    // stream; the host's `send_prompt` owns them.
                    std::mem::drop(tokio::spawn(async move { thread.send_prompt(blocks).await }));
                }
                HostCommand::ResumePrompt { thread, blocks } => {
                    std::mem::drop(tokio::spawn(async move { thread.resume_prompt(blocks).await }));
                }
                HostCommand::SetMode { thread, mode_id, reply } => {
                    let message = format!("mode set: {}", mode_id.0);
                    let result = thread.set_mode(mode_id).await.map(|()| message);
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                HostCommand::SetConfigOption { thread, config_id, value, reply } => {
                    let message = format!("config set: {}", config_id.0);
                    let result = thread.set_config_option(config_id, value).await.map(|()| message);
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                HostCommand::Cancel { thread, reply } => {
                    let result = thread.cancel().await;
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
                HostCommand::Shutdown => break,
            }
        }
        let _ = manager.shutdown().await;
    });
}

/// Owns the lazy host: manager, event receiver, and worker thread.
pub(crate) struct AgentHostBridge {
    pub(crate) manager: AgentManager,
    pub(crate) events: tokio_mpsc::UnboundedReceiver<AgentEvent>,
    commands: tokio_mpsc::UnboundedSender<HostCommand>,
}

impl AgentHostBridge {
    fn new(manager: AgentManager, events: tokio_mpsc::UnboundedReceiver<AgentEvent>) -> Self {
        let (commands_tx, commands_rx) = tokio_mpsc::unbounded_channel();
        let runtime =
            TokioBuilder::new_current_thread().enable_all().build().expect("agents host runtime");
        let worker_manager = manager.clone();
        std::thread::Builder::new()
            .name(String::from("ee-agent-host"))
            .spawn(move || host_worker(runtime, worker_manager, commands_rx))
            .expect("spawn agents host worker");
        Self { manager, events, commands: commands_tx }
    }

    /// Enqueues a new-session request.
    fn request_new_session(
        &self,
        agent_id: String,
        roots: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
        ee_proxy_stdio_fallback: Option<McpServerStdio>,
    ) -> std_mpsc::Receiver<Result<AgentThread, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::NewSession {
            agent_id,
            roots,
            mcp_servers,
            ee_proxy_stdio_fallback,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Enqueues a reconnect request (load, falling back to resume).
    fn request_reconnect(
        &self,
        agent_id: String,
        session_id: String,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
    ) -> std_mpsc::Receiver<Result<AgentThread, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::ReconnectSession {
            agent_id,
            session_id,
            cwd,
            additional_directories,
            mcp_servers,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Enqueues a prompt turn (fire-and-forget; events carry the outcome).
    fn send_prompt(&self, thread: AgentThread, blocks: Vec<ContentBlock>) {
        let _ = self.commands.send(HostCommand::SendPrompt { thread, blocks });
    }

    /// Enqueues a recoverable-turn resume without allocating a new host evidence turn.
    fn resume_prompt(&self, thread: AgentThread, blocks: Vec<ContentBlock>) {
        let _ = self.commands.send(HostCommand::ResumePrompt { thread, blocks });
    }

    /// Enqueues a mode change.
    fn set_mode(
        &self,
        thread: AgentThread,
        mode_id: SessionModeId,
    ) -> std_mpsc::Receiver<Result<String, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::SetMode { thread, mode_id, reply: reply_tx });
        reply_rx
    }

    /// Enqueues a session config option change.
    fn set_config_option(
        &self,
        thread: AgentThread,
        config_id: ee_agent_protocol::SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> std_mpsc::Receiver<Result<String, String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::SetConfigOption {
            thread,
            config_id,
            value,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Enqueues a turn cancellation.
    fn cancel(&self, thread: AgentThread) -> std_mpsc::Receiver<Result<(), String>> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let _ = self.commands.send(HostCommand::Cancel { thread, reply: reply_tx });
        reply_rx
    }
}

impl std::fmt::Debug for AgentHostBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHostBridge").field("manager", &self.manager).finish_non_exhaustive()
    }
}

impl Drop for AgentHostBridge {
    fn drop(&mut self) {
        // Ask the worker to shut the manager down; the worker exits after
        // the current command resolves (all commands are internally bounded).
        let _ = self.commands.send(HostCommand::Shutdown);
    }
}

// ── Pane state ───────────────────────────────────────────────────────────────

/// A session creation in flight (the reply is polled by [`App::pump_agents`]).
#[derive(Debug)]
pub(crate) struct PendingSession {
    pub(crate) agent_id: String,
    /// Known up front for reconnects; `None` for fresh `session/new` (the id
    /// arrives with the reply).
    pub(crate) session_id: Option<String>,
    pub(crate) reply: std_mpsc::Receiver<Result<AgentThread, String>>,
}

/// One fresh ACP session seeded from redacted visible parent messages.
#[derive(Debug)]
struct PendingFork {
    parent_session_id: String,
    seed: Vec<ContentBlock>,
    activate_child: bool,
}

/// One client-persisted session within a workspace thread list.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedAgentSession {
    /// Agent server id the session belongs to.
    agent_id: String,
    /// Session id as returned by `session/new` (stable across restarts for
    /// durable recovery checkpoints).
    session_id: String,
    /// The last submitted prompt text, kept while a turn is recoverable so
    /// the resend path works after a restart.
    last_prompt: Option<String>,
    /// User-selected local session name. Absent in records written before Phase 1.
    #[serde(default)]
    session_name: Option<String>,
}

/// Ordered client-side session registry for one canonical workspace.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct PersistedAgentWorkspace {
    /// Versioned separately from editor session state so agent metadata can
    /// evolve without invalidating buffer/tab restoration.
    #[serde(default = "persisted_agent_workspace_version")]
    version: u32,
    /// Session selected when this workspace was last closed.
    #[serde(default)]
    active_session_id: Option<String>,
    /// Open, non-archived session threads in display order.
    #[serde(default)]
    sessions: Vec<PersistedAgentSession>,
}

const fn persisted_agent_workspace_version() -> u32 {
    1
}

/// Transitional read format for the former one-session-per-workspace record.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum PersistedAgentWorkspaceDocument {
    Legacy(PersistedAgentSession),
    Workspace(PersistedAgentWorkspace),
}

impl PersistedAgentWorkspaceDocument {
    fn into_workspace(self) -> PersistedAgentWorkspace {
        match self {
            Self::Legacy(session) => PersistedAgentWorkspace {
                version: persisted_agent_workspace_version(),
                active_session_id: Some(session.session_id.clone()),
                sessions: vec![session],
            },
            Self::Workspace(workspace) => workspace,
        }
    }
}

/// Sequential startup restoration state. One ACP connection processes loads in
/// order, preserving per-connection request ordering and replay buffering.
#[derive(Debug)]
struct WorkspaceRestore {
    active_session_id: Option<String>,
    sessions: VecDeque<PersistedAgentSession>,
    failed: bool,
}

/// All agents-pane UI state; `Default` is the closed, inert startup state.
pub(crate) struct AgentPaneState {
    pub(crate) layout: AgentPaneLayout,
    pub(crate) threads: Vec<AgentThreadUi>,
    /// Locally hidden threads. Kept in memory so transcript can be restored/exported.
    pub(crate) archived_threads: Vec<AgentThreadUi>,
    pub(crate) active_thread: Option<usize>,
    pub(crate) next_thread_index: usize,
    /// Whether streamed `agent_thought_chunk` messages are shown in transcript.
    pub(crate) show_thoughts: bool,
    pub(crate) pending_session: Option<PendingSession>,
    /// Fresh child session awaiting redacted local transcript seed dispatch.
    pending_fork: Option<PendingFork>,
    /// Workspace threads waiting for sequential ACP session restoration.
    workspace_restore: Option<WorkspaceRestore>,
    /// Composer text typed before a session exists or while session startup fails.
    pub(crate) pending_draft: String,
    /// External editor request consumed only by terminal-owning main loop.
    pending_external_editor: Option<ExternalEditorRequest>,
    pub(crate) pending_cancel: Option<std_mpsc::Receiver<Result<(), String>>>,
    pub(crate) pending_thread_action: Option<std_mpsc::Receiver<Result<String, String>>>,
    pub(crate) permission: Option<PermissionPrompt>,
    pub(crate) mode_selection: Option<ModeSelectionPrompt>,
    pub(crate) approval_mode_confirmation: Option<ApprovalModeConfirmation>,
    pub(crate) session_deletion_confirmation: Option<SessionDeletionConfirmation>,
    pub(crate) additional_directory_confirmation: Option<AdditionalDirectoryConfirmation>,
    pub(crate) terminal_stop_confirmation: Option<TerminalStopConfirmation>,
    /// Explicit user-approved extra roots. Session-local and never persisted.
    pub(crate) additional_workspace_roots: BTreeSet<PathBuf>,
    pub(crate) elicitation: Option<ElicitationPrompt>,
    /// Bridge approval queue (file writes, terminal creates); the front one
    /// is shown and answered first.
    pub(crate) approvals: VecDeque<super::agent_bridge::ApprovalPrompt>,
    pub(crate) error: Option<String>,
    pub(crate) previous_editor_mode: Option<Mode>,
    pub(crate) host: Option<AgentHostBridge>,
    /// Session ids that already emitted `ThreadCreated` (event may beat the
    /// new-session reply; both orders are handled).
    pub(crate) created_sessions: BTreeSet<String>,
    /// Reconnect in flight: `session/update` notifications that arrive before
    /// the reconnect reply (the load replays the conversation while the
    /// thread is not registered yet) are buffered here and applied after
    /// registration.
    pub(crate) pending_replay: HashMap<String, Vec<SessionUpdate>>,
    pub(crate) bridge_tx: std_mpsc::Sender<super::agent_bridge::BridgeUiMessage>,
    pub(crate) bridge_rx: std_mpsc::Receiver<super::agent_bridge::BridgeUiMessage>,
    /// Shared agent terminal registry (spawned here, queried by the host).
    pub(crate) terminals: super::agent_bridge::AgentTerminals,
    /// Recorded agent file operations (future checkpoint/restore source).
    pub(crate) action_log: Vec<super::agent_bridge::ActionLogEntry>,
    /// Session-scoped approval policy (Phase 7).
    pub(crate) approval_policy: super::agent_bridge::ApprovalPolicy,
    /// Lazy service instance. Its bounded cache dies with this pane/session scope.
    pub(crate) web_context_service:
        Option<Arc<ee_agent_host::WebContextService<ee_agent_host::ReqwestWebTransport>>>,
    /// Trusted semantic config used to build `web_context_service`; a mismatch
    /// discards cached remote text and session grants before rebuilding.
    pub(crate) web_context_config_fingerprint: Option<String>,
    /// Session-local approval-dialog behavior. Entries die with their session
    /// and never persist to workspace or user configuration.
    pub(crate) approval_modes: BTreeMap<String, super::agent_bridge::ToolApprovalMode>,
    /// Session-local successful-use counters for persistent rules; rows die
    /// with the session (Phase 2 command trust).
    pub(crate) usage_ledger: crate::policy::UsageLedger,
    /// Phase 6 MCP state: health registry, browsing, and the proxy listener.
    pub(crate) mcp: super::agents_mcp::McpPaneState,
    /// Secret-like resolved agent env values collected when the host config
    /// was built (phase 5); feeds stderr/diagnostics redaction.
    pub(crate) resolved_secret_values: Vec<String>,
    /// Test-only: agent id → fake transport factory (see `tests/agent_pane.rs`).
    #[cfg(test)]
    pub(crate) test_fake_transports: BTreeMap<String, Arc<dyn ee_agent_host::FakeTransportFactory>>,
    /// Test-only: injected secrets store used at launch-time resolution
    /// instead of the real keychain-backed default.
    #[cfg(test)]
    pub(crate) test_secret_store: Option<crate::secrets::SecretStore>,
    /// Test-only: host-local trust store base directory (isolates persistent
    /// grants from real user state).
    #[cfg(test)]
    pub(crate) test_trust_store_base: Option<PathBuf>,
    /// Test-only: session-state file base directory (isolates the persisted
    /// reconnect record from real user state).
    #[cfg(test)]
    pub(crate) test_session_state_base: Option<PathBuf>,
    /// Test-only: export output base directory (isolates transcript files from user state).
    #[cfg(test)]
    pub(crate) test_export_base: Option<PathBuf>,
}

impl Default for AgentPaneState {
    fn default() -> Self {
        let (bridge_tx, bridge_rx) = std_mpsc::channel();
        Self {
            layout: AgentPaneLayout::Closed,
            threads: Vec::new(),
            archived_threads: Vec::new(),
            active_thread: None,
            next_thread_index: 0,
            show_thoughts: true,
            pending_session: None,
            pending_fork: None,
            workspace_restore: None,
            pending_draft: String::new(),
            pending_external_editor: None,
            pending_cancel: None,
            pending_thread_action: None,
            permission: None,
            mode_selection: None,
            approval_mode_confirmation: None,
            session_deletion_confirmation: None,
            additional_directory_confirmation: None,
            terminal_stop_confirmation: None,
            additional_workspace_roots: BTreeSet::new(),
            elicitation: None,
            approvals: VecDeque::new(),
            error: None,
            previous_editor_mode: None,
            host: None,
            created_sessions: BTreeSet::new(),
            pending_replay: HashMap::new(),
            bridge_tx,
            bridge_rx,
            terminals: super::agent_bridge::AgentTerminals::default(),
            action_log: Vec::new(),
            approval_policy: super::agent_bridge::ApprovalPolicy::default(),
            web_context_service: None,
            web_context_config_fingerprint: None,
            approval_modes: BTreeMap::new(),
            usage_ledger: crate::policy::UsageLedger::default(),
            mcp: super::agents_mcp::McpPaneState::default(),
            resolved_secret_values: Vec::new(),
            #[cfg(test)]
            test_fake_transports: BTreeMap::new(),
            #[cfg(test)]
            test_secret_store: None,
            #[cfg(test)]
            test_trust_store_base: None,
            #[cfg(test)]
            test_session_state_base: None,
            #[cfg(test)]
            test_export_base: None,
        }
    }
}

impl std::fmt::Debug for AgentPaneState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentPaneState")
            .field("layout", &self.layout)
            .field("threads", &self.threads.iter().map(|t| &t.display_name).collect::<Vec<_>>())
            .field("active_thread", &self.active_thread)
            .finish_non_exhaustive()
    }
}

// ── App integration ──────────────────────────────────────────────────────────

impl App {
    /// Whether the agents pane owns keyboard focus.
    pub(crate) fn agents_focused(&self) -> bool {
        self.mode == Mode::Agent
    }

    /// Whether the agents pane is currently visible (renderer/command
    /// accessor).
    #[cfg(feature = "agents")]
    pub(crate) fn agents_pane_open(&self) -> bool {
        self.agents.layout != AgentPaneLayout::Closed
    }

    /// Whether the agents pane is currently visible (feature-off stub).
    #[cfg(not(feature = "agents"))]
    pub(crate) fn agents_pane_open(&self) -> bool {
        false
    }

    /// The current pane layout (renderer accessor).
    pub(crate) fn agents_layout(&self) -> AgentPaneLayout {
        self.agents.layout
    }

    /// Drains host events, session replies, cancel replies, and elicitation
    /// requests.  Called from the main loop on every tick; safe to call from
    /// tests.
    pub(crate) fn pump_agents(&mut self) {
        let events = {
            let Some(host) = &mut self.agents.host else {
                return;
            };
            let mut events = Vec::new();
            while let Ok(event) = host.events.try_recv() {
                events.push(event);
            }
            events
        };
        for event in events {
            self.handle_agent_event(event);
        }

        self.pump_session_reply();
        self.pump_cancel_reply();
        self.pump_thread_action_reply();
        self.pump_bridge_requests();
        self.pump_mcp_events();
        self.pump_mcp_replies();
    }

    /// Applies one host event to the pane state.
    fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::ConnectionStateChanged { agent_id, state } => {
                for thread in &mut self.agents.threads {
                    if thread.agent_id != agent_id {
                        continue;
                    }
                    match &state {
                        AgentConnectionState::Starting | AgentConnectionState::Initializing => {
                            if thread.state == ThreadUiState::Failed {
                                thread.state = ThreadUiState::Starting;
                            }
                        }
                        AgentConnectionState::Ready { agent_info, .. } => {
                            if matches!(
                                thread.state,
                                ThreadUiState::Starting | ThreadUiState::Failed
                            ) {
                                thread.state = ThreadUiState::Ready;
                            }
                            if let Some(info) = agent_info.as_ref() {
                                let label = info.title.as_deref().unwrap_or(&info.name);
                                if !label.is_empty() {
                                    thread.nick = label.to_string();
                                }
                            }
                            thread.display_name = thread_display_name(
                                thread.index,
                                &thread.agent_id,
                                thread.session_name.as_deref(),
                                thread.session_title.as_deref(),
                            );
                        }
                        AgentConnectionState::Failed(error) => {
                            thread.state = ThreadUiState::Failed;
                            thread.last_error = Some(error.to_string());
                            thread.push_system(format!("connection failed: {error}"));
                        }
                        AgentConnectionState::Closed(reason) => {
                            thread.state = ThreadUiState::Closed;
                            thread.push_system(format!("connection closed ({reason:?})"));
                        }
                    }
                }
            }
            AgentEvent::ThreadCreated { session_id, .. } => {
                self.agents.created_sessions.insert(session_id.0.to_string());
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
                }
            }
            AgentEvent::ThreadClosed { session_id, reason, .. } => {
                // Session-scoped approval policy and usage counters die with
                // the session; persistent host-local rules remain.
                self.agents.approval_policy.invalidate_session(session_id.0.as_ref());
                self.agents.approval_modes.remove(session_id.0.as_ref());
                if self
                    .agents
                    .approval_mode_confirmation
                    .as_ref()
                    .is_some_and(|confirmation| confirmation.session_id == session_id.0.as_ref())
                {
                    self.agents.approval_mode_confirmation = None;
                }
                self.agents.usage_ledger.invalidate_session(session_id.0.as_ref());
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    let text = match reason {
                        ThreadCloseReason::HostClosed => String::from("session closed"),
                        ThreadCloseReason::ConnectionLost => String::from("connection lost"),
                    };
                    self.agents.threads[index].state = ThreadUiState::Closed;
                    let _ = self.agents.threads[index].finish_response_group();
                    self.agents.threads[index].push_system(text);
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnStarted { session_id, .. } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Running;
                    self.agents.threads[index].verification_paths.clear();
                    self.agents.threads[index].verification_revision = None;
                    self.agents.threads[index].active_response_group = None;
                    self.agents.threads[index].turn_started_at = Some(Instant::now());
                    self.agents.threads[index].push_system(String::from("turn started"));
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnEvidenceUpdated { session_id, summary } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    let thread = &mut self.agents.threads[index];
                    let evidence_ids = summary.evidence_ids.join(", ");
                    thread.terminal_evidence = Some(*summary);
                    let status =
                        thread.terminal_evidence.as_ref().expect("evidence summary just stored");
                    thread.push_system(format!(
                        "verification: {:?}; blocker: {:?}; evidence: {evidence_ids}",
                        status.status, status.blocker
                    ));
                    self.notify_unread(index);
                }
            }
            AgentEvent::SessionUpdate { session_id, update } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.apply_session_update(index, &update);
                    self.notify_unread(index);
                } else if let Some(buffer) =
                    self.agents.pending_replay.get_mut(&session_id.0.to_string())
                {
                    // Reconnect in flight: `session/load` replays the
                    // conversation before the thread is registered; keep the
                    // updates and apply them once the reply lands.
                    buffer.push(*update);
                }
            }
            AgentEvent::TurnCompleted { session_id, stop_reason, metrics } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].optimistic_message = None;
                    self.agents.threads[index].turn_started_at = None;
                    self.agents.threads[index].stop_reason = Some(format!("{stop_reason:?}"));
                    self.agents.threads[index].record_turn_metrics(metrics);
                    self.agents.threads[index].pending_recovery = None;
                    self.agents.threads[index].last_prompt = None;
                    self.agents.threads[index]
                        .push_system(format!("turn completed (stop: {stop_reason:?})"));
                    self.notify_unread(index);
                }
                // The turn is no longer resumable; drop the persisted prompt before
                // optionally recording the newly dispatched queued follow-up.
                self.update_persisted_last_prompt(session_id.0.as_ref(), None);
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.dispatch_next_queued_prompt(index);
                }
            }
            AgentEvent::TurnCancelled { session_id, metrics } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].optimistic_message = None;
                    self.agents.threads[index].turn_started_at = None;
                    self.agents.threads[index].record_turn_metrics(metrics);
                    self.agents.threads[index].pending_recovery = None;
                    self.agents.threads[index].last_prompt = None;
                    self.agents.threads[index].push_system(String::from("turn cancelled"));
                    self.notify_unread(index);
                }
                self.update_persisted_last_prompt(session_id.0.as_ref(), None);
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.dispatch_next_queued_prompt(index);
                }
            }
            AgentEvent::TurnFailed { session_id, error, metrics } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].optimistic_message = None;
                    self.agents.threads[index].turn_started_at = None;
                    self.agents.threads[index].record_turn_metrics(metrics);
                    self.agents.threads[index].last_error = Some(error.to_string());
                    self.agents.threads[index].pending_recovery = None;
                    self.agents.threads[index].push_system(format!("turn failed: {error}"));
                    self.notify_unread(index);
                }
                self.update_persisted_last_prompt(session_id.0.as_ref(), None);
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.dispatch_next_queued_prompt(index);
                }
            }
            AgentEvent::TurnPausedRecoverable { session_id, metrics, recoverable } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    let thread = &mut self.agents.threads[index];
                    thread.state = ThreadUiState::PausedRecoverable;
                    thread.optimistic_message = None;
                    thread.turn_started_at = None;
                    thread.record_turn_metrics(metrics);
                    let info = *recoverable;
                    let notice = format!(
                        "turn paused: {} (resume with :agents_resume, discard with :agents_discard)",
                        info.detail
                    );
                    thread.push_system(notice);
                    // Keep the prompt that started the turn for Resume; it
                    // was captured at submit time.
                    thread.pending_recovery =
                        thread.last_prompt.take().map(|prompt| PendingRecovery { info, prompt });
                    self.notify_unread(index);
                }
            }
            AgentEvent::PermissionRequested { session_id, request } => {
                self.present_permission(&session_id, &request);
            }
            AgentEvent::PermissionResolved { session_id, request_id, outcome } => {
                let notice = match &outcome {
                    RequestPermissionOutcome::Selected(selected) => {
                        format!("permission resolved: {}", selected.option_id.0)
                    }
                    RequestPermissionOutcome::Cancelled => String::from("permission cancelled"),
                    // Non-exhaustive upstream.
                    _ => String::from("permission resolved"),
                };
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].push_system(notice);
                }
                if let Some(prompt) = &self.agents.permission
                    && prompt.request_id == request_id
                {
                    self.agents.permission = None;
                }
            }
            AgentEvent::ElicitationCompleted { elicitation_id } => {
                self.handle_elicitation_completed(elicitation_id.0.as_ref());
            }
            AgentEvent::ClientRequestDispatched { session_id, method } => {
                let notice = format!("client request dispatched: {method}");
                match session_id {
                    Some(session_id) => {
                        if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                            self.agents.threads[index].push_system(notice);
                        }
                    }
                    None => {
                        if let Some(active) = self.agents.active_thread_index() {
                            self.agents.threads[active].push_system(notice);
                        }
                    }
                }
            }
            AgentEvent::StderrLine { agent_id, line } => {
                // Phase 7: configured secret values never reach the debug
                // pane, even when they appear inside stderr text.
                let secrets = self.agents_secret_values();
                let line = ee_agent_host::redact::redact_secret_values(&line, &secrets);
                for thread in &mut self.agents.threads {
                    if thread.agent_id == agent_id {
                        thread.push_stderr(line.clone());
                    }
                }
            }
        }
    }

    /// Secret-like configured values (agent + MCP env/header values whose
    /// keys look secret-like).  Used to redact stderr and diagnostics.
    ///
    /// Agent values are the raw config literals plus the values resolved from
    /// `secret://` references at launch; references themselves are never
    /// collected (their resolved values are, once the launch config exists).
    pub(crate) fn agents_secret_values(&self) -> Vec<String> {
        let mut secrets = Vec::new();
        for server in self.config.agents.servers.values() {
            for (name, value) in &server.env {
                if ee_agent_host::redact::is_secret_key(name)
                    && !crate::secrets::is_secret_reference_text(&value.raw)
                {
                    secrets.push(value.raw.clone());
                }
            }
        }
        for server in self.config.mcp.servers.values() {
            match server {
                crate::config::McpServerSettings::Stdio { env, .. } => {
                    for (name, value) in env {
                        if ee_agent_host::redact::is_secret_key(name) {
                            secrets.push(value.clone());
                        }
                    }
                }
                crate::config::McpServerSettings::StreamableHttp { headers, .. } => {
                    for (name, value) in headers {
                        if ee_agent_host::redact::is_secret_key(name) {
                            secrets.push(value.clone());
                        }
                    }
                }
            }
        }
        secrets.extend(self.agents.resolved_secret_values.iter().cloned());
        secrets.sort();
        secrets.dedup();
        secrets
    }

    /// Reduces one `session/update` into the thread transcript.
    fn apply_session_update(&mut self, thread_index: usize, update: &SessionUpdate) {
        let nick = self.agents.threads[thread_index].nick.clone();
        match update {
            SessionUpdate::UserMessageChunk(chunk) => {
                let text = content_block_text(&chunk.content);
                let message_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
                self.agents.threads[thread_index].push_message(
                    "you",
                    &text,
                    MessageRenderKind::User,
                    message_id,
                    None,
                );
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = content_block_text(&chunk.content);
                let message_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
                let thread = &mut self.agents.threads[thread_index];
                let response_group = thread.ensure_response_group();
                thread.push_message(
                    &nick,
                    &text,
                    MessageRenderKind::Assistant,
                    message_id,
                    Some(response_group),
                );
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = content_block_text(&chunk.content);
                let message_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
                let thread = &mut self.agents.threads[thread_index];
                let response_group = thread.ensure_response_group();
                thread.push_message(
                    "think",
                    &text,
                    MessageRenderKind::Thought,
                    message_id,
                    Some(response_group),
                );
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.sync_tool_call_notice(thread_index, tool_call.tool_call_id.0.as_ref());
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.sync_tool_call_notice(thread_index, update.tool_call_id.0.as_ref());
            }
            SessionUpdate::Plan(plan) => {
                let entries = plan
                    .entries
                    .iter()
                    .map(|entry| {
                        (
                            format!(
                                "[{}] {}",
                                plan_entry_priority_label(entry.priority.clone()),
                                entry.content
                            ),
                            plan_entry_marker(entry.status.clone()),
                        )
                    })
                    .collect();
                self.agents.threads[thread_index].replace_plan(entries);
            }
            SessionUpdate::UsageUpdate(usage) => {
                let mut text = format!("{}k/{}k tokens", usage.used / 1000, usage.size / 1000);
                if let Some(cost) = &usage.cost {
                    text.push_str(&format!(" · cost: {cost:?}"));
                }
                self.agents.threads[thread_index].usage = Some(text);
            }
            SessionUpdate::CurrentModeUpdate(mode) => {
                self.sync_thread_snapshot_fields(thread_index);
                self.agents.threads[thread_index]
                    .push_system(format!("mode: {}", mode.current_mode_id.0));
            }
            SessionUpdate::AvailableCommandsUpdate(commands) => {
                self.sync_thread_snapshot_fields(thread_index);
                let listed = available_commands_summary(&commands.available_commands);
                self.agents.threads[thread_index].push_system(if listed.is_empty() {
                    String::from("commands: none")
                } else {
                    format!("commands: {listed}")
                });
            }
            SessionUpdate::ConfigOptionUpdate(_) => {
                self.sync_thread_snapshot_fields(thread_index);
            }
            SessionUpdate::SessionInfoUpdate(_) => {
                self.sync_thread_snapshot_fields(thread_index);
            }
            // Non-exhaustive upstream; unknown updates carry no rendering.
            _ => {}
        }
    }

    /// Presents a permission request as a prompt plus transcript notice.
    fn present_permission(&mut self, session_id: &SessionId, request: &PermissionRequestInfo) {
        let Some(thread_index) = self.agents.thread_index(session_id.0.as_ref()) else {
            return;
        };
        let options = request.options.clone();
        let titles: Vec<String> = options.iter().map(|option| option.name.clone()).collect();
        let tool_title =
            request.tool_call.fields.title.clone().unwrap_or_else(|| String::from("tool call"));
        self.agents.threads[thread_index].transcript.push(TranscriptItem::Permission {
            title: tool_title.clone(),
            options: titles.clone(),
            at: SystemTime::now(),
        });
        self.agents.permission = Some(PermissionPrompt {
            thread_index,
            request_id: request.request_id,
            tool_title,
            options,
            selected: 0,
        });
        self.notify_unread(thread_index);
    }

    /// Presents an elicitation request as a prompt plus transcript notice.
    pub(super) fn present_elicitation(
        &mut self,
        session_id: Option<SessionId>,
        request: CreateElicitationRequest,
        reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    ) {
        let thread_index =
            session_id.as_ref().and_then(|id| self.agents.thread_index(id.0.as_ref()));
        let agent_label = thread_index
            .and_then(|index| {
                self.agents.threads.get(index).map(|thread| thread.display_name.clone())
            })
            .unwrap_or_else(|| String::from("agent"));
        let prompt = match &request.mode {
            ElicitationMode::Form(mode) => ElicitationPrompt::from_form(
                thread_index,
                agent_label.clone(),
                request.message.clone(),
                &mode.requested_schema,
                reply,
            ),
            ElicitationMode::Url(mode) => ElicitationPrompt::from_url(
                thread_index,
                agent_label.clone(),
                request.message.clone(),
                mode.elicitation_id.0.to_string(),
                mode.url.clone(),
                reply,
            ),
            // Unknown modes fail closed with a typed error and a notice.
            _ => {
                let _ = reply.send(Err(AgentError::invalid_params("unsupported elicitation mode")));
                if let Some(thread_index) = thread_index {
                    self.agents.threads[thread_index]
                        .push_system(String::from("elicitation rejected: unsupported mode"));
                } else if let Some(active) = self.agents.active_thread_index() {
                    self.agents.threads[active]
                        .push_system(String::from("elicitation rejected: unsupported mode"));
                }
                return;
            }
        };
        let url = prompt.url.clone();
        let url_host = prompt.url_host.clone();
        let agent = prompt.agent_label.clone();
        let message = prompt.message.clone();
        if let Some(thread_index) = thread_index {
            self.agents.threads[thread_index].transcript.push(TranscriptItem::Elicitation {
                agent,
                message: message.clone(),
                url: url.clone(),
                url_host,
                at: SystemTime::now(),
            });
            self.notify_unread(thread_index);
        }
        if let Some(reason) = &prompt.unsupported_reason {
            let notice = format!("elicitation: {reason}");
            if let Some(thread_index) = thread_index {
                self.agents.threads[thread_index].push_system(notice);
            }
        }
        self.agents.elicitation = Some(prompt);
    }

    fn push_elicitation_notice(&mut self, thread_index: Option<usize>, text: String) {
        if let Some(thread_index) = thread_index {
            if let Some(thread) = self.agents.threads.get_mut(thread_index) {
                thread.push_system(text);
                self.notify_unread(thread_index);
            }
            return;
        }
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].push_system(text);
        }
    }

    /// Handles agent `elicitation/complete` notifications.
    fn handle_elicitation_completed(&mut self, elicitation_id: &str) {
        let matches_prompt = self
            .agents
            .elicitation
            .as_ref()
            .is_some_and(|prompt| prompt.completion_id.as_deref() == Some(elicitation_id));
        if !matches_prompt {
            let thread_index =
                self.agents.elicitation.as_ref().and_then(|prompt| prompt.thread_index);
            self.push_elicitation_notice(
                thread_index,
                format!("stale elicitation completion ignored: {elicitation_id}"),
            );
            return;
        }

        let prompt = self.agents.elicitation.take().expect("elicitation prompt matched");
        let thread_index = prompt.thread_index;
        let _ = prompt.reply.send(Ok(ClientRequestResponse::CreateElicitation(
            CreateElicitationResponse::new(ElicitationAction::Accept(
                ElicitationAcceptAction::new(),
            )),
        )));
        self.push_elicitation_notice(
            thread_index,
            format!("elicitation completed: {elicitation_id}"),
        );
    }

    /// Records one local proxy web lifecycle row. Detail is compact provenance
    /// metadata only; remote request/query/body never enters transcript state.
    pub(super) fn record_web_lifecycle(
        &mut self,
        id: &str,
        title: &str,
        status: &str,
        detail: &str,
    ) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let group = thread.ensure_response_group();
        thread.push_tool_call(id, title, status, detail, group);
    }

    fn sync_tool_call_notice(&mut self, thread_index: usize, tool_call_id: &str) {
        let Some(thread) = self.agents.threads.get_mut(thread_index) else {
            return;
        };
        let snapshot = thread.host.snapshot();
        let Some(tool_call) = snapshot.tool_calls.get(tool_call_id) else {
            return;
        };
        let response_group = thread.response_group_for_tool_call(&tool_call.tool_call_id);
        thread.push_tool_call(
            &tool_call.tool_call_id,
            &tool_call.title,
            &tool_call_status_label(tool_call.status),
            &tool_call_detail_from_state(tool_call),
            response_group,
        );
    }

    /// Registers a fresh session thread from a new-session reply.
    fn register_session_thread(&mut self, agent_id: &str, thread: AgentThread) {
        let session_id = thread.session_id().0.to_string();
        let snapshot = thread.snapshot();
        // Phase 6b user-visible diagnostics: how the ee proxy was exposed.
        self.agents.mcp.proxy_mode = Some(thread.proxy_mode().to_string());
        let index = self.agents.next_thread_index;
        self.agents.next_thread_index += 1;
        let nick = agent_id.to_string();
        let ready = self.agents.created_sessions.contains(&session_id);
        let session_title =
            snapshot.session_info.as_ref().and_then(|info| info.title.value().cloned());
        let session_updated_at =
            snapshot.session_info.as_ref().and_then(|info| info.updated_at.value().cloned());
        let persisted = self.load_persisted_agent_workspace().and_then(|workspace| {
            workspace.sessions.into_iter().find(|record| record.session_id == session_id)
        });
        let session_name = persisted.as_ref().and_then(|record| record.session_name.clone());
        self.agents.threads.push(AgentThreadUi {
            index,
            agent_id: agent_id.to_string(),
            session_id: session_id.clone(),
            fork_parent_session_id: None,
            nick,
            display_name: thread_display_name(
                index,
                agent_id,
                session_name.as_deref(),
                session_title.as_deref(),
            ),
            state: if ready { ThreadUiState::Ready } else { ThreadUiState::Starting },
            unread: 0,
            activity: false,
            host: thread,
            transcript: Vec::new(),
            optimistic_message: None,
            draft: std::mem::take(&mut self.agents.pending_draft),
            prompt_history: Vec::new(),
            prompt_history_cursor: None,
            prompt_history_restore_draft: None,
            queued_prompts: VecDeque::new(),
            stashed_draft: None,
            transcript_raw: false,
            transcript_detail: false,
            scroll: 0,
            stick_to_bottom: true,
            usage: None,
            stop_reason: None,
            turn_started_at: None,
            turn_metrics: BTreeMap::new(),
            last_turn_metrics: None,
            terminal_evidence: None,
            verification_paths: Vec::new(),
            verification_revision: None,
            active_response_group: None,
            next_response_group: 1,
            selected_response_group: None,
            collapsed_response_groups: BTreeSet::new(),
            expanded_tool_details: BTreeSet::new(),
            current_plan: Vec::new(),
            plan_modal_open: false,
            last_error: None,
            pending_recovery: None,
            last_prompt: None,
            context_files: Vec::new(),
            next_prompt_context_files: Vec::new(),
            available_commands: snapshot.available_commands,
            session_name: session_name.clone(),
            session_title,
            session_updated_at,
        });
        let restored_active = self
            .agents
            .workspace_restore
            .as_ref()
            .and_then(|restore| restore.active_session_id.as_deref())
            == Some(session_id.as_str());
        if self.agents.workspace_restore.is_none() || restored_active {
            self.agents.active_thread = Some(self.agents.threads.len() - 1);
        }
        self.agents.error = None;
        self.agents
            .threads
            .last_mut()
            .expect("thread pushed")
            .push_system(format!("session started ({session_id})"));
        self.persist_agent_workspace();
    }

    /// Bumps unread/activity for a thread unless it is focused.
    fn notify_unread(&mut self, thread_index: usize) {
        if self.agents.active_thread != Some(thread_index)
            && let Some(thread) = self.agents.threads.get_mut(thread_index)
        {
            thread.unread += 1;
            thread.activity = true;
        }
    }

    fn sync_thread_snapshot_fields(&mut self, thread_index: usize) {
        let Some(thread) = self.agents.threads.get_mut(thread_index) else {
            return;
        };
        let snapshot = thread.host.snapshot();
        thread.available_commands = snapshot.available_commands;
        let title = match snapshot.session_info.as_ref() {
            Some(info) => match info.title.as_opt_ref() {
                Some(Some(title)) => Some(title.clone()),
                Some(None) => None,
                None => thread.session_title.clone(),
            },
            None => thread.session_title.clone(),
        };
        let updated_at = match snapshot.session_info.as_ref() {
            Some(info) => match info.updated_at.as_opt_ref() {
                Some(Some(updated_at)) => Some(updated_at.clone()),
                Some(None) => None,
                None => thread.session_updated_at.clone(),
            },
            None => thread.session_updated_at.clone(),
        };
        thread.session_title = title;
        thread.session_updated_at = updated_at;
        thread.display_name = thread_display_name(
            thread.index,
            &thread.agent_id,
            thread.session_name.as_deref(),
            thread.session_title.as_deref(),
        );
    }

    /// Polls a pending new-session reply.
    fn pump_session_reply(&mut self) {
        let result = match &self.agents.pending_session {
            Some(pending) => pending.reply.try_recv(),
            None => return,
        };
        match result {
            Ok(Ok(thread)) => {
                let pending = self.agents.pending_session.take().expect("pending session present");
                let session_id = pending.session_id;
                let agent_id = pending.agent_id;
                if let Some(index) = session_id.as_ref().and_then(|id| self.agents.thread_index(id))
                {
                    // Same-process reconnect: a thread for this session
                    // already exists, so rebind the fresh connection
                    // instead of duplicating the thread.
                    self.agents.threads[index].host = thread;
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].push_system(String::from("session reconnected"));
                    self.sync_thread_snapshot_fields(index);
                    let restored_active = self
                        .agents
                        .workspace_restore
                        .as_ref()
                        .and_then(|restore| restore.active_session_id.as_deref())
                        == session_id.as_deref();
                    if self.agents.workspace_restore.is_none() || restored_active {
                        self.agents.active_thread = Some(index);
                    }
                    self.persist_agent_workspace();
                } else {
                    self.register_session_thread(&agent_id, thread);
                    if let Some(fork) = self.agents.pending_fork.take() {
                        let child = self.agents.threads.len() - 1;
                        self.agents.threads[child].fork_parent_session_id =
                            Some(fork.parent_session_id.clone());
                        self.agents.threads[child].push_system(format!(
                            "seeded local fork from session {}",
                            fork.parent_session_id
                        ));
                        let child_thread = self.agents.threads[child].host.clone();
                        if let Some(host) = &self.agents.host {
                            host.send_prompt(child_thread, fork.seed);
                        }
                        if !fork.activate_child
                            && let Some(parent) = self.agents.thread_index(&fork.parent_session_id)
                        {
                            self.agents.active_thread = Some(parent);
                        }
                        self.persist_agent_workspace();
                    }
                }
                if let Some(session_id) = session_id {
                    // Reconnect: apply the conversation replay updates that
                    // streamed while the thread was not registered yet, then
                    // restore the persisted last prompt for the resend path.
                    if let Some(index) = self
                        .agents
                        .thread_index(&session_id)
                        .zip(self.agents.pending_replay.remove(&session_id))
                    {
                        for update in index.1 {
                            self.apply_session_update(index.0, &update);
                        }
                    }
                    if let Some(text) =
                        self.load_persisted_agent_workspace().and_then(|workspace| {
                            workspace
                                .sessions
                                .into_iter()
                                .find(|record| record.session_id == session_id)
                                .and_then(|record| record.last_prompt)
                        })
                        && let Some(index) = self.agents.thread_index(&session_id)
                    {
                        self.agents.threads[index].last_prompt =
                            Some(vec![ContentBlock::Text(TextContent::new(text))]);
                    }
                }
            }
            Ok(Err(message)) => {
                self.agents.pending_fork = None;
                if let Some(restore) = self.agents.workspace_restore.as_mut() {
                    restore.failed = true;
                }
                let pending = self.agents.pending_session.take().expect("pending session present");
                if let Some(session_id) = pending.session_id {
                    self.agents.pending_replay.remove(&session_id);
                }
                self.agents.error = Some(message.clone());
                let notice = format!("session start failed: {message}");
                if let Some(active) = self.agents.active_thread_index() {
                    self.agents.threads[active].push_system(notice);
                }
                self.backend.status_message = Some(message);
            }
            Err(std_mpsc::TryRecvError::Empty) => {}
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.agents.pending_session = None;
                self.agents.pending_fork = None;
                if let Some(restore) = self.agents.workspace_restore.as_mut() {
                    restore.failed = true;
                }
                self.agents.pending_replay.clear();
                self.agents.error = Some(String::from("agent host stopped"));
            }
        }
        if self.agents.pending_session.is_none() && self.agents.workspace_restore.is_some() {
            self.start_next_workspace_restore();
        }
    }

    /// Polls a pending `:agents_stop` reply.
    fn pump_cancel_reply(&mut self) {
        let result = match &self.agents.pending_cancel {
            Some(reply) => reply.try_recv(),
            None => return,
        };
        match result {
            Ok(Ok(())) => {
                self.agents.pending_cancel = None;
                self.backend.status_message = Some(String::from("turn cancelled"));
            }
            Ok(Err(message)) => {
                self.agents.pending_cancel = None;
                self.backend.status_message = Some(message);
            }
            Err(std_mpsc::TryRecvError::Empty) => {}
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.agents.pending_cancel = None;
            }
        }
    }

    fn pump_thread_action_reply(&mut self) {
        let result = match &self.agents.pending_thread_action {
            Some(reply) => reply.try_recv(),
            None => return,
        };
        match result {
            Ok(Ok(message)) => {
                self.agents.pending_thread_action = None;
                self.backend.status_message = Some(message);
            }
            Ok(Err(message)) => {
                self.agents.pending_thread_action = None;
                self.backend.status_message = Some(message);
            }
            Err(std_mpsc::TryRecvError::Empty) => {}
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.agents.pending_thread_action = None;
            }
        }
    }
}

impl AgentPaneState {
    /// Index of the active thread, if any.
    pub(crate) fn active_thread_index(&self) -> Option<usize> {
        self.active_thread
    }

    pub(crate) fn thread_index(&self, session_id: &str) -> Option<usize> {
        self.threads.iter().position(|thread| thread.session_id == session_id)
    }
}

// ── Rendering helpers ────────────────────────────────────────────────────────

/// Extracts display text from a content block (non-text blocks get markers).
fn content_block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(_) => String::from("[image]"),
        ContentBlock::Audio(_) => String::from("[audio]"),
        ContentBlock::ResourceLink(link) => format!("[resource: {}]", link.uri),
        ContentBlock::Resource(resource) => {
            let uri = match &resource.resource {
                ee_agent_protocol::EmbeddedResourceResource::TextResourceContents(contents) => {
                    contents.uri.clone()
                }
                ee_agent_protocol::EmbeddedResourceResource::BlobResourceContents(contents) => {
                    contents.uri.clone()
                }
                // Non-exhaustive upstream.
                _ => String::from("(unknown)"),
            };
            format!("[resource: {uri}]")
        }
        // Non-exhaustive upstream; unknown blocks render as a marker.
        _ => String::from("[content]"),
    }
}

fn tool_call_status_label(status: ToolCallStatus) -> String {
    match status {
        ToolCallStatus::Pending => String::from("pending"),
        ToolCallStatus::InProgress => String::from("in_progress"),
        ToolCallStatus::Completed => String::from("completed"),
        ToolCallStatus::Failed => String::from("failed"),
        // Non-exhaustive upstream.
        _ => String::from("?"),
    }
}

fn tool_kind_label(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        ToolKind::Other => "other",
        _ => "other",
    }
}

fn tool_call_content_summary(content: &ToolCallContent) -> String {
    match content {
        ToolCallContent::Content(content) => content_block_text(&content.content),
        ToolCallContent::Diff(diff) => {
            let path = diff.path.display();
            if diff.old_text.is_some() {
                format!("diff: {path}")
            } else {
                format!("diff: new file {path}")
            }
        }
        ToolCallContent::Terminal(terminal) => {
            format!("terminal: {}", terminal.terminal_id.0)
        }
        _ => String::from("content: [unknown]"),
    }
}

fn tool_call_location_summary(location: &ToolCallLocation) -> String {
    match location.line {
        Some(line) => format!("{}:{line}", location.path.display()),
        None => location.path.display().to_string(),
    }
}

fn tool_call_detail_from_state(tool_call: &ToolCallState) -> String {
    if tool_call.kind == ToolKind::Fetch {
        // ACP may carry remote request/response bytes in generic content or raw
        // fields. Keep lifecycle visible without treating that content as local
        // transcript, planner, or export data.
        return String::from(
            "kind: fetch · external content: untrusted · remote payload withheld · use source provenance from tool result",
        );
    }
    let mut sections = vec![format!("kind: {}", tool_kind_label(tool_call.kind))];

    if !tool_call.content.is_empty() {
        let content = tool_call
            .content
            .iter()
            .map(tool_call_content_summary)
            .filter(|item| !item.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
        if !content.is_empty() {
            sections.push(format!("content: {content}"));
        }
    }

    if !tool_call.locations.is_empty() {
        let locations = tool_call
            .locations
            .iter()
            .map(tool_call_location_summary)
            .collect::<Vec<_>>()
            .join(", ");
        sections.push(format!("locations: {locations}"));
    }

    match (tool_call.raw_input.is_some(), tool_call.raw_output.is_some()) {
        (true, true) => sections.push(String::from("diagnostics: raw input/output captured")),
        (true, false) => sections.push(String::from("diagnostics: raw input captured")),
        (false, true) => sections.push(String::from("diagnostics: raw output captured")),
        (false, false) => {}
    }

    sections.join(" · ")
}

fn plan_entry_marker(status: PlanEntryStatus) -> char {
    match status {
        PlanEntryStatus::Pending => '-',
        PlanEntryStatus::InProgress => '>',
        PlanEntryStatus::Completed => 'x',
        // Non-exhaustive upstream.
        _ => '!',
    }
}

fn plan_entry_priority_label(priority: PlanEntryPriority) -> &'static str {
    match priority {
        PlanEntryPriority::High => "high",
        PlanEntryPriority::Medium => "medium",
        PlanEntryPriority::Low => "low",
        _ => "?",
    }
}

fn thread_display_name(
    index: usize,
    agent_id: &str,
    session_name: Option<&str>,
    session_title: Option<&str>,
) -> String {
    let label =
        session_name.or(session_title).filter(|title| !title.trim().is_empty()).unwrap_or(agent_id);
    format!("{}.{}", index + 1, label)
}

/// Formats a duration compactly: `1.2s`, `45s`, or `3m 12s`.
pub(crate) fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs_f64();
    if total < 60.0 {
        return format!("{total:.1}s");
    }
    let minutes = total as u64 / 60;
    let seconds = total as u64 % 60;
    format!("{minutes}m {seconds}s")
}

/// Formats a token count with thousands separators (`8,431`).
pub(crate) fn format_tokens(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Renders one completed turn's metrics: `12.4s` or
/// `12.4s · 8,431 tokens (6,120 in / 2,311 out)`; unknown token usage is
/// never shown as zero.
pub(crate) fn turn_metrics_label(metrics: &TurnMetrics) -> String {
    let mut label = format_duration(metrics.elapsed);
    if let Some(usage) = &metrics.tokens {
        label.push_str(&format!(
            " · {} tokens ({} in / {} out)",
            format_tokens(usage.total_tokens),
            format_tokens(usage.input_tokens),
            format_tokens(usage.output_tokens),
        ));
    }
    label
}

fn is_mode_config_option(option: &SessionConfigOption) -> bool {
    matches!(option.category, Some(ee_agent_protocol::SessionConfigOptionCategory::Mode))
}

fn select_option_values(options: &SessionConfigSelectOptions) -> Vec<SessionConfigValueId> {
    match options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            options.iter().map(|option| option.value.clone()).collect()
        }
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(|option| option.value.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

fn cycle_select_value(
    options: &SessionConfigSelectOptions,
    current: &SessionConfigValueId,
    delta: isize,
) -> Option<SessionConfigValueId> {
    let values = select_option_values(options);
    if values.is_empty() {
        return None;
    }
    let current_index = values.iter().position(|value| value == current).unwrap_or_default();
    let next_index = (current_index as isize + delta).rem_euclid(values.len() as isize) as usize;
    values.get(next_index).cloned()
}

fn config_option_summary(option: &SessionConfigOption) -> String {
    match &option.kind {
        SessionConfigKind::Select(select) => format!("{}={}", option.id.0, select.current_value.0),
        SessionConfigKind::Boolean(current) => {
            format!("{}={}", option.id.0, if current.current_value { "on" } else { "off" })
        }
        _ => format!("{}=?", option.id.0),
    }
}

fn parse_config_option_value(
    option: &SessionConfigOption,
    raw_value: &str,
) -> Result<SessionConfigOptionValue, String> {
    match &option.kind {
        SessionConfigKind::Select(select) => {
            let value = SessionConfigValueId::new(raw_value);
            let exists = match &select.options {
                SessionConfigSelectOptions::Ungrouped(options) => {
                    options.iter().any(|option| option.value == value)
                }
                SessionConfigSelectOptions::Grouped(groups) => groups
                    .iter()
                    .flat_map(|group| group.options.iter())
                    .any(|option| option.value == value),
                _ => false,
            };
            if exists {
                Ok(SessionConfigOptionValue::value_id(value))
            } else {
                Err(format!("invalid value for {}: {raw_value}", option.id.0))
            }
        }
        SessionConfigKind::Boolean(_) => parse_bool(raw_value)
            .map(SessionConfigOptionValue::boolean)
            .ok_or_else(|| format!("invalid boolean for {}: {raw_value}", option.id.0)),
        _ => Err(format!("unsupported config option kind: {}", option.id.0)),
    }
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn split_slash_command(draft: &str) -> (Option<String>, String) {
    let trimmed = draft.trim_start();
    if !trimmed.starts_with('/') {
        return (None, String::new());
    }
    let without_slash = &trimmed[1..];
    let mut parts = without_slash.splitn(2, char::is_whitespace);
    let name = parts.next().filter(|part| !part.is_empty()).map(str::to_string);
    let rest = parts.next().unwrap_or_default().to_string();
    (name, rest)
}

fn queue_command_is_management(args: &str) -> bool {
    matches!(
        args.split_whitespace().next(),
        None | Some("list" | "edit" | "move" | "remove" | "clear")
    )
}

fn prompt_blocks_with_context(
    prompt_text: &str,
    context_files: &[AgentContextFile],
    next_prompt_context_files: &[AgentContextFile],
) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::Text(TextContent::new(prompt_text))];
    for (scope, files) in [
        ("User-selected context file", context_files),
        ("One-turn user mention", next_prompt_context_files),
    ] {
        blocks.extend(files.iter().map(|file| {
            ContentBlock::Text(TextContent::new(format!(
                "{scope}: `{}`\n--- file snapshot ---\n{}\n--- end file snapshot ---",
                file.relative_path, file.content
            )))
        }));
    }
    blocks
}

fn agent_mode_ids(thread: &AgentThreadUi) -> Vec<String> {
    let snapshot = thread.host.snapshot();
    if let Some(mode_option) =
        snapshot.config_options.iter().find(|option| is_mode_config_option(option))
        && let SessionConfigKind::Select(select) = &mode_option.kind
    {
        return select_option_values(&select.options)
            .into_iter()
            .map(|value| value.0.to_string())
            .collect();
    }
    thread
        .host
        .advertised_modes()
        .map(|modes| modes.available_modes.into_iter().map(|mode| mode.id.0.to_string()).collect())
        .unwrap_or_default()
}

const LOCAL_AGENT_SLASH_COMMANDS: &[&str] = &[
    "quit",
    "q",
    "quit_full",
    "qf",
    "help",
    "status",
    "doctor",
    "init",
    "review",
    "security-review",
    "diff",
    "copy",
    "rename",
    "new",
    "new_thread",
    "archive",
    "delete",
    "fork",
    "branch",
    "sessions",
    "export",
    "stop",
    "steer",
    "resume",
    "discard",
    "reconnect",
    "next",
    "prev",
    "clear",
    "layout",
    "thoughts",
    "config",
    "mcp",
    "approval",
    "context",
    "mention",
    "add-dir",
    "tasks",
    "ps",
    "mode",
    "queue",
    "details",
    "transcript",
    "draft",
    "keys",
];

/// Provider-owned configuration aliases accepted only when the active session
/// explicitly advertises an ACP option with the matching id.
const PROVIDER_CONFIG_ALIASES: &[&str] = &["model", "effort", "fast", "personality"];

/// Provider-owned workflow commands. EE does not emulate these; an unadvertised
/// command stops locally with guidance instead of becoming an ordinary prompt.
const PROVIDER_OWNED_SLASH_COMMANDS: &[&str] = &[
    "compact",
    "subtask",
    "background",
    "side",
    "btw",
    "permissions",
    "skills",
    "plugins",
    "hooks",
    "usage",
    "billing",
    "cloud",
    "remote-control",
    "web-search",
    "app",
];

const LOCAL_AGENT_SLASH_HELP: &[(&str, &str)] = &[
    ("/help", "show local commands and provider commands"),
    ("/status", "show local session state"),
    ("/doctor", "read-only Agents TUI health report"),
    ("/init", "ask agent to preview safe AGENTS.md scaffold"),
    ("/review [target]", "send bounded EE evidence for agent-generated review"),
    ("/security-review [target]", "send bounded EE evidence for security review"),
    ("/diff", "open bounded workspace diff"),
    ("/copy [N]", "copy Nth completed assistant response"),
    ("/rename <name>", "set persisted local session name"),
    ("/new, /new_thread", "start a fresh provider session"),
    ("/clear", "clear visible local scrollback; provider context stays"),
    ("/archive", "hide idle local session; use /archive list|restore <N>"),
    ("/delete", "confirm local transcript deletion; provider session stays"),
    ("/fork", "create non-active seeded session from redacted visible transcript"),
    ("/branch", "create and switch to seeded session"),
    ("/sessions", "switch session"),
    ("/export", "write redacted Markdown transcript"),
    ("/stop [terminal-id|all]", "cancel turn or stop owned direct-child terminal"),
    ("/steer <message>", "cancel active turn; run steer message next"),
    ("/resume | /discard", "resolve paused turn"),
    ("/reconnect", "reconnect persisted session"),
    ("/next | /prev", "cycle active sessions"),
    ("/layout", "right|bottom|full"),
    ("/thoughts", "on|off|toggle"),
    ("/config", "show or change advertised options"),
    ("/mcp", "show local MCP state"),
    ("/approval", "default|autopilot|bypass; bypass keeps validation"),
    ("/context", "list|status|add|remove|clear session snapshots"),
    ("/mention <path>", "attach redacted file snapshot to next prompt only"),
    ("/add-dir <path>", "confirm extra root for capable agent sessions"),
    ("/tasks | /ps", "list owned background terminals; subagent tasks when supported"),
    ("/mode", "select agent-advertised mode"),
    ("/queue <message>", "run message after current turn; /queue list manages follow-ups"),
    ("/details", "on|off|toggle sanitized transcript tool detail"),
    ("/transcript", "raw|grouped|toggle|open|export local transcript"),
    ("/draft", "restore|clear long prompt draft; Ctrl-S stash, Ctrl-Shift-E edit"),
    ("/keys", "show Agents TUI keyboard shortcuts"),
    ("/quit, /q", "close Agents pane"),
    ("/quit_full, /qf", "exit EE"),
];

fn owned_terminal_summary_line(
    summary: &super::agent_bridge::OwnedTerminalSummary,
    secrets: &[String],
) -> String {
    let command = if summary.args.is_empty() {
        summary.command.clone()
    } else {
        format!("{} {}", summary.command, summary.args.join(" "))
    };
    let state = if summary.running { "running" } else { "exited" };
    let cwd = summary
        .cwd
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| String::from("(default)"));
    let tail = ee_agent_host::redact::redact_secret_values(&summary.output_tail, secrets);
    let truncation = if summary.output_truncated { " truncated" } else { "" };
    format!(
        "{} {state} {} · cwd:{} · output:{} bytes{truncation}{}",
        summary.terminal_id,
        command,
        cwd,
        summary.output_total_bytes,
        if tail.trim().is_empty() { String::new() } else { format!("\n  tail: {tail}") },
    )
}

fn thread_state_label(state: ThreadUiState) -> &'static str {
    match state {
        ThreadUiState::Starting => "starting",
        ThreadUiState::Ready => "ready",
        ThreadUiState::Running => "running",
        ThreadUiState::PausedRecoverable => "paused",
        ThreadUiState::Closed => "closed",
        ThreadUiState::Failed => "failed",
    }
}

fn fork_seed(thread: &AgentThreadUi, secrets: &[String]) -> Vec<ContentBlock> {
    const FORK_SEED_MAX_BYTES: usize = 48 * 1024;
    let mut transcript = String::from(
        "This is a locally seeded fork. Treat following redacted visible messages as prior context; it is not provider-side session cloning.\n\n",
    );
    for item in &thread.transcript {
        let TranscriptItem::Message { nick, text, kind, .. } = item else { continue };
        let role = match kind {
            MessageRenderKind::User => "User",
            MessageRenderKind::Assistant => "Assistant",
            MessageRenderKind::Thought => continue,
        };
        transcript.push_str(&format!("## {role} ({nick})\n{text}\n\n"));
    }
    let mut transcript = ee_agent_host::redact::redact_secret_values(&transcript, secrets);
    if transcript.len() > FORK_SEED_MAX_BYTES {
        let mut end = FORK_SEED_MAX_BYTES;
        while !transcript.is_char_boundary(end) {
            end -= 1;
        }
        transcript.truncate(end);
        transcript.push_str("\n\n[local fork seed truncated]\n");
    }
    vec![ContentBlock::Text(TextContent::new(transcript))]
}

fn sanitize_session_name(raw: &str) -> Option<String> {
    const MAX_SESSION_NAME_CHARS: usize = 80;
    let mut name = raw
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{00AD}'
                        | '\u{034F}'
                        | '\u{061C}'
                        | '\u{115F}'..='\u{1160}'
                        | '\u{17B4}'..='\u{17B5}'
                        | '\u{180E}'
                        | '\u{200B}'..='\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2060}'..='\u{206F}'
                        | '\u{3164}'
                        | '\u{FE00}'..='\u{FE0F}'
                        | '\u{FEFF}'
                        | '\u{FFA0}'
                )
        })
        .collect::<String>();
    name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then(|| name.chars().take(MAX_SESSION_NAME_CHARS).collect())
}

/// Lists local and agent-advertised slash commands without duplicate names.
pub(crate) fn agent_slash_command_names(commands: &[AvailableCommand]) -> Vec<&str> {
    let mut names = LOCAL_AGENT_SLASH_COMMANDS.to_vec();
    for command in commands {
        let name = command.name.as_str();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Renders the advertised command list for the transcript notice:
/// `/name — description` when a description is advertised, `/name` otherwise.
fn available_commands_summary(commands: &[AvailableCommand]) -> String {
    commands
        .iter()
        .map(|command| {
            if command.description.is_empty() {
                format!("/{}", command.name)
            } else {
                format!("/{} — {}", command.name, command.description)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Draft text for a cycled command, preserving any trailing user text.
fn slash_command_draft(command_name: &str, rest: &str) -> String {
    if rest.trim().is_empty() {
        format!("/{command_name}")
    } else {
        format!("/{command_name} {}", rest.trim_start())
    }
}

fn is_agents_quit_slash_command(draft: &str) -> bool {
    let (name, rest) = split_slash_command(draft);
    matches!(name.as_deref(), Some("q" | "quit")) && rest.trim().is_empty()
}

fn is_agents_quit_full_slash_command(draft: &str) -> bool {
    let (name, rest) = split_slash_command(draft);
    matches!(name.as_deref(), Some("qf" | "quit_full")) && rest.trim().is_empty()
}

// ── App command and key wiring ───────────────────────────────────────────────

impl App {
    /// Full agents command dispatch (feature enabled).  Returns `true` when
    /// the pane keeps keyboard focus (the caller then skips the trailing
    /// `enter_normal_mode`).
    pub(super) fn dispatch_agents_command_impl(&mut self, head: &str, tail: &str) -> bool {
        match head {
            "agents" => self.open_agents_pane(),
            "agents_close" => {
                self.close_agents_pane();
                true
            }
            "agents_stop" => {
                self.agents_stop_turn();
                true
            }
            "agents_resume" => {
                self.resume_paused_turn();
                true
            }
            "agents_discard" => {
                self.discard_paused_turn();
                true
            }
            "agents_reconnect" => {
                self.agents_reconnect();
                true
            }
            "agents_new" => {
                self.agents_new_session();
                true
            }
            "agents_threads" => {
                self.open_agents_thread_picker();
                true
            }
            "agents_next" => {
                self.agents_switch_thread(1);
                true
            }
            "agents_prev" => {
                self.agents_switch_thread(-1);
                true
            }
            "agents_mode_next" => {
                self.agents_cycle_mode(1);
                true
            }
            "agents_mode_prev" => {
                self.agents_cycle_mode(-1);
                true
            }
            "agents_config" => {
                self.agents_list_config_options();
                true
            }
            "agents_config_set" => {
                self.agents_set_config_option_command(tail);
                true
            }
            "agents_config_toggle" => {
                self.agents_toggle_config_option_command(tail);
                true
            }
            "agents_clear" => {
                self.agents_clear_scrollback();
                true
            }
            "agents_layout" => {
                self.agents_set_layout(tail);
                true
            }
            "agents_thoughts" => {
                self.agents_set_thought_visibility(tail);
                true
            }
            "agents_mcp" => {
                self.agents_mcp_command(tail);
                true
            }
            _ => {
                self.backend.status_message = Some(format!("unknown agents command: {head}"));
                false
            }
        }
    }

    /// `:agents` — open the pane and start the default agent lazily.
    /// Returns `true` when the pane opened (keeps keyboard focus).
    pub(super) fn open_agents_pane(&mut self) -> bool {
        if !self.config.agents.enabled {
            self.backend.status_message = Some(self.agents_status_message());
            return false;
        }
        let opening = self.agents.layout == AgentPaneLayout::Closed;
        if opening {
            self.agents.layout = AgentPaneLayout::Full;
        }
        self.enter_agent_focus();
        self.ensure_agents_host();
        // MCP health/prompt browsing start lazily when the pane opens.
        self.start_mcp_servers();
        let restoring = self.agents.active_thread.is_none()
            && self.agents.pending_session.is_none()
            && self.agents.workspace_restore.is_none()
            && self.start_workspace_restore();
        if self.agents.active_thread.is_none()
            && self.agents.pending_session.is_none()
            && self.agents.workspace_restore.is_none()
        {
            let Some(agent_id) = self.default_agent_id() else {
                let message =
                    String::from("no agent configured (set `agents.default_agent` or add servers)");
                self.agents.error = Some(message.clone());
                self.backend.status_message = Some(message);
                return true;
            };
            self.start_session(agent_id);
        }
        if opening {
            self.backend.status_message = Some(if restoring {
                String::from("agents pane opened (restoring workspace sessions…)")
            } else if self.agents.active_thread.is_some() {
                String::from("agents pane opened")
            } else {
                String::from("agents pane opened (starting session…)")
            });
        }
        true
    }

    /// `:agents_close` — hide the pane without killing the session.
    /// Restores the previous editor mode (focus return).
    pub(super) fn close_agents_pane(&mut self) {
        if self.agents.layout == AgentPaneLayout::Closed {
            return;
        }
        self.agents.layout = AgentPaneLayout::Closed;
        // Restore focus even when the command ran from the pane's `:`
        // command line (mode is `CommandLine` at that point, not `Agent`).
        if let Some(previous) = self.agents.previous_editor_mode.take() {
            self.mode = previous;
        } else if self.agents_focused() {
            self.mode = Mode::Normal;
        }
        self.backend.status_message = Some("agents pane closed (session kept running)".to_string());
    }

    /// `:agents_stop` — cancel the running turn on the active thread.
    pub(super) fn agents_stop_turn(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let Some(host) = &self.agents.host else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = self.agents.threads[active].host.clone();
        if !thread.is_turn_running() {
            self.backend.status_message = Some(String::from("no running turn to stop"));
            return;
        }
        let reply = host.cancel(thread);
        self.agents.pending_cancel = Some(reply);
        self.backend.status_message = Some(String::from("cancelling turn…"));
    }

    fn active_terminal_owner(&self) -> Option<super::agent_bridge::TerminalOwner> {
        let active = self.agents.active_thread_index()?;
        let thread = self.agents.threads.get(active)?;
        Some(super::agent_bridge::TerminalOwner {
            agent_id: thread.agent_id.clone(),
            session_id: thread.session_id.clone(),
        })
    }

    fn agents_list_owned_terminals(&mut self) {
        let Some(owner) = self.active_terminal_owner() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let secrets = self.agents_secret_values();
        let terminals = self.agents.terminals.list_owned(&owner, AGENT_TERMINAL_OUTPUT_TAIL_BYTES);
        let summary = if terminals.is_empty() {
            String::from("owned terminals: none")
        } else {
            format!(
                "owned terminals:\n{}",
                terminals
                    .iter()
                    .map(|terminal| owned_terminal_summary_line(terminal, &secrets))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].push_system(summary.clone());
        }
        self.backend.status_message = Some(summary);
    }

    fn agents_list_owned_tasks(&mut self) {
        self.agents_list_owned_terminals();
        let suffix =
            "subagent tasks: unavailable; current ACP host advertises no task-list capability";
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].push_system(suffix);
        }
        let current = self.backend.status_message.take().unwrap_or_default();
        self.backend.status_message = Some(format!("{current}\n{suffix}"));
    }

    fn stop_owned_terminals(&mut self, owner: &super::agent_bridge::TerminalOwner, ids: &[String]) {
        let mut results = Vec::new();
        for terminal_id in ids {
            let result = match self.agents.terminals.stop_owned(owner, terminal_id) {
                Ok(super::agent_bridge::OwnedTerminalStop::StopRequested) => {
                    format!("terminal stop requested: {terminal_id} (direct child only)")
                }
                Ok(super::agent_bridge::OwnedTerminalStop::AlreadyExited) => {
                    format!("terminal already exited: {terminal_id}")
                }
                Err(error) => format!("terminal stop rejected for {terminal_id}: {error}"),
            };
            results.push(result);
        }
        let summary = results.join(" · ");
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].push_system(summary.clone());
        }
        self.backend.status_message = Some(summary);
    }

    fn agents_queue_command(&mut self, args: &str) {
        let secrets = self.agents_secret_values();
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        let mut parts = args.splitn(3, char::is_whitespace);
        match (parts.next().unwrap_or_default(), parts.next(), parts.next()) {
            ("" | "list", None, None) => {
                let entries = thread
                    .queued_prompts
                    .iter()
                    .enumerate()
                    .map(|(index, prompt)| {
                        format!(
                            "{}: {}",
                            index + 1,
                            ee_agent_host::redact::redact_secret_values(&prompt.text, &secrets)
                        )
                    })
                    .collect::<Vec<_>>();
                self.backend.status_message = Some(if entries.is_empty() {
                    String::from("queued follow-ups: none")
                } else {
                    format!(
                        "queued follow-ups (dispatch after current turn): {}",
                        entries.join(" · ")
                    )
                });
            }
            ("remove", Some(raw_index), None) => match raw_index.parse::<usize>() {
                Ok(index) if index > 0 && index <= thread.queued_prompts.len() => {
                    thread.queued_prompts.remove(index - 1);
                    self.backend.status_message = Some(format!("queued follow-up {index} removed"));
                }
                _ => self.backend.status_message = Some(String::from("usage: /queue remove <N>")),
            },
            ("edit", Some(raw_index), Some(text)) if !text.trim().is_empty() => {
                match raw_index.parse::<usize>() {
                    Ok(index) if index > 0 && index <= thread.queued_prompts.len() => {
                        thread.queued_prompts[index - 1].text = text.trim_end().to_string();
                        self.backend.status_message =
                            Some(format!("queued follow-up {index} updated"));
                    }
                    _ => {
                        self.backend.status_message =
                            Some(String::from("usage: /queue edit <N> <prompt>"))
                    }
                }
            }
            ("move", Some(raw_index), Some(raw_target)) => {
                let parsed = raw_index.parse::<usize>().ok();
                let target = raw_target.trim().parse::<usize>().ok();
                match (parsed, target) {
                    (Some(index), Some(target))
                        if index > 0
                            && index <= thread.queued_prompts.len()
                            && target > 0
                            && target <= thread.queued_prompts.len() =>
                    {
                        let prompt =
                            thread.queued_prompts.remove(index - 1).expect("validated queue index");
                        thread.queued_prompts.insert(target - 1, prompt);
                        self.backend.status_message =
                            Some(format!("queued follow-up {index} moved to {target}"));
                    }
                    _ => {
                        self.backend.status_message =
                            Some(String::from("usage: /queue move <N> <position>"))
                    }
                }
            }
            ("clear", None, None) => {
                let count = thread.queued_prompts.len();
                thread.queued_prompts.clear();
                self.backend.status_message = Some(format!("cleared {count} queued follow-ups"));
            }
            _ => {
                self.backend.status_message = Some(String::from(
                    "usage: /queue <message> | /queue [list|edit <N> <prompt>|move <N> <position>|remove <N>|clear]",
                ));
            }
        }
    }

    /// Queues an explicit follow-up while a turn runs, or sends it immediately once ready.
    fn agents_queue_prompt_command(&mut self, args: &str) {
        let prompt_text = args.trim_end().to_string();
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        match self.agents.threads[active].state {
            ThreadUiState::Running => self.enqueue_agent_follow_up(active, prompt_text),
            ThreadUiState::Ready => {
                self.send_ready_agent_prompt(active, prompt_text);
                self.backend.status_message =
                    Some(String::from("queued prompt dispatched immediately"));
            }
            ThreadUiState::PausedRecoverable => {
                self.backend.status_message =
                    Some(String::from("a turn is paused; use /resume or /discard before queueing"));
            }
            _ => {
                self.backend.status_message =
                    Some(String::from("agent session is not ready; cannot queue prompt"));
            }
        }
    }

    /// Cancels a running ACP turn and dispatches this steer message before older follow-ups.
    fn agents_steer_command(&mut self, args: &str) {
        let prompt_text = args.trim_end().to_string();
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        match self.agents.threads[active].state {
            ThreadUiState::Running => {
                let Some(queued_count) = self.queue_agent_prompt(active, prompt_text, true) else {
                    return;
                };
                let thread = self.agents.threads[active].host.clone();
                if thread.is_turn_running() {
                    let host = self.agents.host.as_ref().expect("host present");
                    self.agents.pending_cancel = Some(host.cancel(thread));
                    self.backend.status_message = Some(format!(
                        "steering active turn; cancelling now, steer message dispatches next ({queued_count} queued)"
                    ));
                } else {
                    self.backend.status_message = Some(format!(
                        "steer message queued first ({queued_count} queued); current turn is starting"
                    ));
                }
            }
            ThreadUiState::Ready => {
                self.send_ready_agent_prompt(active, prompt_text);
                self.backend.status_message =
                    Some(String::from("steer message dispatched immediately"));
            }
            ThreadUiState::PausedRecoverable => {
                self.backend.status_message =
                    Some(String::from("a turn is paused; use /resume or /discard before steering"));
            }
            _ => {
                self.backend.status_message =
                    Some(String::from("agent session is not ready; cannot steer"));
            }
        }
    }

    fn agents_set_transcript_detail(&mut self, args: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        match args {
            "" | "toggle" => thread.transcript_detail = !thread.transcript_detail,
            "on" => thread.transcript_detail = true,
            "off" => thread.transcript_detail = false,
            _ => {
                self.backend.status_message = Some(String::from("usage: /details [on|off|toggle]"));
                return;
            }
        }
        self.backend.status_message = Some(format!(
            "sanitized transcript tool details {}",
            if thread.transcript_detail { "shown" } else { "hidden" }
        ));
    }

    fn agents_transcript_command(&mut self, args: &str) {
        match args {
            "open" => self.agents_open_exported_transcript(),
            "export" => self.agents_export_transcript(),
            "" | "toggle" | "raw" | "grouped" => {
                let Some(active) = self.agents.active_thread_index() else {
                    self.backend.status_message = Some(String::from("no active agent session"));
                    return;
                };
                let thread = &mut self.agents.threads[active];
                match args {
                    "raw" => thread.transcript_raw = true,
                    "grouped" => thread.transcript_raw = false,
                    "" | "toggle" => thread.transcript_raw = !thread.transcript_raw,
                    _ => unreachable!(),
                }
                self.backend.status_message = Some(format!(
                    "transcript view: {} (safe local scrollback)",
                    if thread.transcript_raw { "raw" } else { "grouped" }
                ));
            }
            _ => {
                self.backend.status_message =
                    Some(String::from("usage: /transcript [raw|grouped|toggle|open|export]"));
            }
        }
    }

    fn agents_draft_command(&mut self, args: &str) {
        if matches!(args, "edit" | "stash") {
            self.backend.status_message = Some(String::from(
                "use Ctrl-S to stash current draft or Ctrl-Shift-E to edit it externally",
            ));
            return;
        }
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        match args {
            "restore" => self.agents_restore_draft(),
            "clear" => {
                self.agents.threads[active].stashed_draft = None;
                self.backend.status_message = Some(String::from("stashed draft cleared"));
            }
            _ => {
                self.backend.status_message = Some(String::from(
                    "usage: /draft restore|clear (Ctrl-S stash, Ctrl-Shift-E edit)",
                ))
            }
        }
    }

    fn agents_stash_draft(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        if thread.draft.trim().is_empty() {
            self.backend.status_message = Some(String::from("draft is empty"));
            return;
        }
        thread.stashed_draft = Some(std::mem::take(&mut thread.draft));
        thread.prompt_history_cursor = None;
        thread.prompt_history_restore_draft = None;
        self.backend.status_message = Some(String::from("draft stashed locally"));
    }

    fn agents_restore_draft(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        match thread.stashed_draft.take() {
            Some(draft) => {
                thread.draft = draft;
                thread.prompt_history_cursor = None;
                thread.prompt_history_restore_draft = None;
                self.backend.status_message = Some(String::from("draft restored locally"));
            }
            None => self.backend.status_message = Some(String::from("no stashed draft")),
        }
    }

    fn request_agent_external_editor(&mut self) {
        if self.agents.pending_external_editor.is_some() {
            self.backend.status_message =
                Some(String::from("external draft editor is already pending"));
            return;
        }
        let request = match self.agents.active_thread_index() {
            Some(active) => {
                let thread = &self.agents.threads[active];
                ExternalEditorRequest {
                    session_id: Some(thread.session_id.clone()),
                    draft: thread.draft.clone(),
                }
            }
            None => {
                ExternalEditorRequest { session_id: None, draft: self.agents.pending_draft.clone() }
            }
        };
        self.agents.pending_external_editor = Some(request);
        self.backend.status_message =
            Some(String::from("opening external draft editor; prompt will not send automatically"));
    }

    /// Consumed by terminal-owning loop after input dispatch. Never call from an
    /// ACP worker: interactive child processes need foreground terminal access.
    pub(crate) fn take_agent_external_editor_request(&mut self) -> Option<ExternalEditorRequest> {
        self.agents.pending_external_editor.take()
    }

    /// Replaces only draft target captured before handoff. A switched/deleted
    /// session cannot receive another session's prompt text.
    pub(crate) fn apply_agent_external_editor_result(
        &mut self,
        request: ExternalEditorRequest,
        result: Result<String, String>,
    ) {
        match result {
            Ok(draft) => {
                let applied = match request.session_id {
                    Some(session_id) => self
                        .agents
                        .thread_index(&session_id)
                        .and_then(|index| self.agents.threads.get_mut(index))
                        .map(|thread| {
                            thread.draft = draft;
                            thread.prompt_history_cursor = None;
                            thread.prompt_history_restore_draft = None;
                        })
                        .is_some(),
                    None => {
                        self.agents.pending_draft = draft;
                        true
                    }
                };
                self.backend.status_message = Some(if applied {
                    String::from(
                        "draft updated from external editor; review then press Enter to send",
                    )
                } else {
                    String::from(
                        "external draft editor finished, but original session no longer exists",
                    )
                });
            }
            Err(error) => {
                self.backend.status_message =
                    Some(format!("external draft editor unavailable: {error}"));
            }
        }
    }

    fn agents_show_key_help(&mut self) {
        self.backend.status_message = Some(String::from(
            "Agents keys: ↑/↓ history · Ctrl-R reverse history search · Ctrl-Shift-R response collapse · Enter send/queue · Alt-Enter newline · Ctrl-U clear draft · Ctrl-S stash · Ctrl-O restore · Ctrl-Shift-E external edit · Ctrl-G plan · Ctrl-E selected tool detail · Ctrl-←/→ response group · PgUp/PgDn/Home/End scroll · Tab slash/@ completion. Configure mode=agent bindings in [keymap].",
        ));
    }

    fn agents_stop_command(&mut self, args: &str) {
        if args.is_empty() {
            self.agents_stop_turn();
            return;
        }
        let Some(owner) = self.active_terminal_owner() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        if args == "all" {
            let ids = self
                .agents
                .terminals
                .list_owned(&owner, 0)
                .into_iter()
                .filter(|terminal| terminal.running)
                .map(|terminal| terminal.terminal_id)
                .take(AGENT_TERMINAL_STOP_ALL_MAX)
                .collect::<Vec<_>>();
            match ids.len() {
                0 => {
                    self.backend.status_message =
                        Some(String::from("no owned running terminals to stop"))
                }
                1 => self.stop_owned_terminals(&owner, &ids),
                _ => {
                    self.agents.terminal_stop_confirmation = Some(TerminalStopConfirmation {
                        agent_id: owner.agent_id,
                        session_id: owner.session_id,
                        terminal_ids: ids,
                    });
                    self.backend.status_message =
                        Some(String::from("confirm stopping owned terminals"));
                }
            }
            return;
        }
        if args.contains(char::is_whitespace) {
            self.backend.status_message = Some(String::from("usage: /stop [terminal-id|all]"));
            return;
        }
        self.stop_owned_terminals(&owner, &[args.to_string()]);
    }

    fn confirm_stop_owned_terminals(&mut self) {
        let Some(confirmation) = self.agents.terminal_stop_confirmation.take() else {
            return;
        };
        let owner = super::agent_bridge::TerminalOwner {
            agent_id: confirmation.agent_id,
            session_id: confirmation.session_id,
        };
        self.stop_owned_terminals(&owner, &confirmation.terminal_ids);
    }

    /// `:agents_new` — start a fresh session and switch to it.
    pub(super) fn agents_new_session(&mut self) {
        if !self.config.agents.enabled {
            self.backend.status_message = Some(self.agents_status_message());
            return;
        }
        if self.agents.pending_session.is_some() {
            self.backend.status_message = Some(String::from("agent session is already starting"));
            return;
        }
        if self.agents.layout == AgentPaneLayout::Closed {
            self.agents.layout = AgentPaneLayout::Full;
        }
        self.enter_agent_focus();
        self.ensure_agents_host();
        self.start_mcp_servers();
        let Some(agent_id) = self.default_agent_id() else {
            self.backend.status_message = Some(String::from(
                "no agent configured (set `agents.default_agent` or add servers)",
            ));
            return;
        };
        self.start_session(agent_id);
        self.backend.status_message = Some(String::from("starting new agent session…"));
    }

    /// `:agents_next` / `:agents_prev` — switch threads.
    pub(super) fn agents_switch_thread(&mut self, delta: isize) {
        let count = self.agents.threads.len();
        if count == 0 {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        }
        let current = self.agents.active_thread.unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(count as isize) as usize;
        self.focus_thread(next);
    }

    fn agent_export_dir(&self) -> io::Result<PathBuf> {
        #[cfg(test)]
        if let Some(base) = &self.agents.test_export_base {
            return Ok(base.join("agent-exports"));
        }
        crate::logs::state_dir().map(|state_dir| state_dir.join("agent-exports")).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "platform state directory is unavailable")
        })
    }

    /// Exports current local transcript, including redacted tool payloads, to private Markdown.
    fn agents_export_transcript(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session to export"));
            return;
        };
        let secrets = self.agents_secret_values();
        let (session_id, markdown) = {
            let thread = &self.agents.threads[active];
            (
                thread.session_id.clone(),
                format_agent_transcript_markdown(thread, SystemTime::now(), &secrets),
            )
        };
        let result = self.agent_export_dir().and_then(|directory| {
            write_agent_transcript_export(&directory, &session_id, &markdown)
        });
        match result {
            Ok(path) => {
                self.agents.threads[active]
                    .push_system(format!("transcript exported: {}", path.display()));
                self.backend.status_message =
                    Some(format!("agent transcript exported: {}", path.display()));
            }
            Err(error) => {
                self.backend.status_message =
                    Some(format!("agent transcript export failed: {error}"));
            }
        }
    }

    fn agents_open_exported_transcript(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session to export"));
            return;
        };
        let secrets = self.agents_secret_values();
        let (session_id, markdown) = {
            let thread = &self.agents.threads[active];
            (
                thread.session_id.clone(),
                format_agent_transcript_markdown(thread, SystemTime::now(), &secrets),
            )
        };
        let result = self.agent_export_dir().and_then(|directory| {
            write_agent_transcript_export(&directory, &session_id, &markdown)
        });
        match result {
            Ok(path) => match self.backend.open_buffer(Some(path.clone())) {
                Ok(buffer_id) => {
                    if let Err(error) = self.backend.switch_to_id(buffer_id) {
                        self.backend.status_message = Some(format!(
                            "transcript exported but could not select buffer: {error}"
                        ));
                    } else {
                        self.agents.threads[active]
                            .push_system(format!("transcript opened: {}", path.display()));
                        self.backend.status_message = Some(format!(
                            "transcript opened in editor: {}; close Agents pane to review",
                            path.display()
                        ));
                    }
                }
                Err(error) => {
                    self.backend.status_message = Some(format!(
                        "transcript exported but could not open in editor: {} ({error})",
                        path.display()
                    ));
                }
            },
            Err(error) => {
                self.backend.status_message =
                    Some(format!("agent transcript export failed: {error}"));
            }
        }
    }

    /// `:agents_clear` — clear the active thread's local scrollback.
    pub(super) fn agents_clear_scrollback(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        if thread.state == ThreadUiState::Running {
            self.backend.status_message =
                Some(String::from("cannot clear scrollback while a turn is running"));
            return;
        }
        thread.transcript.clear();
        thread.optimistic_message = None;
        thread.scroll = 0;
        thread.stick_to_bottom = true;
        self.backend.status_message =
            Some(String::from("visible scrollback cleared; provider conversation remains intact"));
    }

    fn agents_thread_is_idle(&self, index: usize) -> bool {
        self.agents.threads.get(index).is_some_and(|thread| {
            thread.state == ThreadUiState::Ready
                && !thread.host.is_turn_running()
                && thread.pending_recovery.is_none()
        }) && self.agents.permission.is_none()
            && self.agents.mode_selection.is_none()
            && self.agents.approval_mode_confirmation.is_none()
            && self.agents.session_deletion_confirmation.is_none()
            && self.agents.elicitation.is_none()
            && self.agents.approvals.is_empty()
    }

    /// Starts a fresh provider session seeded with redacted visible parent messages.
    /// This is deliberately not presented as an ACP/provider-side clone.
    fn agents_fork_session(&mut self, activate_child: bool) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session to fork"));
            return;
        };
        if !self.agents_thread_is_idle(active) || self.agents.pending_session.is_some() {
            self.backend.status_message = Some(String::from(
                "session must be idle before fork; stop and resolve pending work first",
            ));
            return;
        }
        let parent = &self.agents.threads[active];
        let parent_session_id = parent.session_id.clone();
        let agent_id = parent.agent_id.clone();
        let seed = fork_seed(parent, &self.agents_secret_values());
        self.ensure_agents_host();
        self.start_mcp_servers();
        self.start_session(agent_id);
        self.agents.pending_fork = Some(PendingFork { parent_session_id, seed, activate_child });
        self.backend.status_message = Some(String::from(if activate_child {
            "starting seeded branch session…"
        } else {
            "starting seeded fork session…"
        }));
    }

    fn agents_archive_current_session(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        if !self.agents_thread_is_idle(active) {
            self.backend.status_message = Some(String::from(
                "session must be idle before archive; stop and resolve pending work first",
            ));
            return;
        }
        let thread = self.agents.threads.remove(active);
        let session_id = thread.session_id.clone();
        let label = thread
            .session_name
            .as_deref()
            .or(thread.session_title.as_deref())
            .unwrap_or(&session_id)
            .to_string();
        self.agents.archived_threads.push(thread);
        self.agents.active_thread =
            (!self.agents.threads.is_empty()).then_some(active.min(self.agents.threads.len() - 1));
        self.persist_agent_workspace();
        self.backend.status_message = Some(format!(
            "session archived locally: {label}; restore with /archive restore {}",
            self.agents.archived_threads.len()
        ));
    }

    fn agents_list_archived_sessions(&mut self) {
        let listing = if self.agents.archived_threads.is_empty() {
            String::from("archived sessions: none")
        } else {
            let entries = self
                .agents
                .archived_threads
                .iter()
                .enumerate()
                .map(|(index, thread)| {
                    let label = thread
                        .session_name
                        .as_deref()
                        .or(thread.session_title.as_deref())
                        .unwrap_or(&thread.session_id);
                    format!("{}: {} ({})", index + 1, label, thread.session_id)
                })
                .collect::<Vec<_>>()
                .join(" · ");
            format!("archived sessions: {entries}; /archive restore <N> restores locally")
        };
        self.backend.status_message = Some(listing);
    }

    fn agents_restore_archived_session(&mut self, raw_index: &str) {
        let Ok(index) = raw_index.parse::<usize>() else {
            self.backend.status_message =
                Some(String::from("usage: /archive restore <positive number>"));
            return;
        };
        let Some(index) =
            index.checked_sub(1).filter(|index| *index < self.agents.archived_threads.len())
        else {
            self.backend.status_message = Some(String::from("archived session not found"));
            return;
        };
        let thread = self.agents.archived_threads.remove(index);
        let label = thread.display_name.clone();
        self.agents.threads.push(thread);
        self.agents.active_thread = Some(self.agents.threads.len() - 1);
        self.persist_agent_workspace();
        self.backend.status_message = Some(format!("session restored locally: {label}"));
    }

    fn agents_archive_command(&mut self, args: &str) {
        let mut parts = args.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (None, _, _) => self.agents_archive_current_session(),
            (Some("list"), None, _) => self.agents_list_archived_sessions(),
            (Some("restore"), Some(index), None) => self.agents_restore_archived_session(index),
            _ => {
                self.backend.status_message =
                    Some(String::from("usage: /archive [list|restore <N>]"))
            }
        }
    }

    fn request_delete_current_session(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        if !self.agents_thread_is_idle(active) {
            self.backend.status_message = Some(String::from(
                "session must be idle before delete; stop and resolve pending work first",
            ));
            return;
        }
        let thread = &self.agents.threads[active];
        let session_name = thread
            .session_name
            .as_deref()
            .or(thread.session_title.as_deref())
            .unwrap_or("unnamed")
            .to_string();
        self.agents.session_deletion_confirmation = Some(SessionDeletionConfirmation {
            thread_index: active,
            session_id: thread.session_id.clone(),
            session_name,
        });
        self.backend.status_message = Some(String::from("confirm local session deletion"));
    }

    fn confirm_delete_current_session(&mut self) {
        let Some(confirmation) = self.agents.session_deletion_confirmation.take() else {
            return;
        };
        let Some(thread) = self.agents.threads.get(confirmation.thread_index) else {
            self.backend.status_message =
                Some(String::from("session changed before delete confirmation"));
            return;
        };
        if thread.session_id != confirmation.session_id {
            self.backend.status_message =
                Some(String::from("session changed before delete confirmation"));
            return;
        }
        let removed = self.agents.threads.remove(confirmation.thread_index);
        // Service cache is pane-owned today; clear all entries when any session
        // closes rather than retain cross-session external content.
        if let Some(service) = &self.agents.web_context_service {
            service.clear_cache();
        }
        // Proxy connections currently share pane ownership. Clear both route
        // scopes when any owning agent session closes; never retain host grants.
        for route in
            [super::agents_mcp::ProxyRoute::Stdio, super::agents_mcp::ProxyRoute::AcpNative]
        {
            self.agents
                .approval_policy
                .invalidate_session(&format!("proxy-network:{}", route.transport_identity()));
        }
        self.agents.approval_modes.remove(&removed.session_id);
        self.agents.active_thread = (!self.agents.threads.is_empty())
            .then_some(confirmation.thread_index.min(self.agents.threads.len() - 1));
        self.persist_agent_workspace();
        self.backend.status_message = Some(format!(
            "local transcript deleted for {} ({}); provider session unchanged",
            confirmation.session_name, confirmation.session_id
        ));
    }

    /// `:agents_layout <right|bottom|full>`.
    pub(super) fn agents_set_layout(&mut self, tail: &str) {
        if !self.config.agents.enabled {
            self.backend.status_message = Some(self.agents_status_message());
            return;
        }
        let Some(layout) = AgentPaneLayout::parse(tail.trim()) else {
            self.backend.status_message =
                Some(String::from("usage: :agents_layout right|bottom|full"));
            return;
        };
        let was_closed = self.agents.layout == AgentPaneLayout::Closed;
        self.agents.layout = layout;
        if was_closed {
            self.open_agents_pane();
        }
        self.backend.status_message = Some(format!("agents layout: {layout:?}"));
    }

    /// `:agents_thoughts <on|off|toggle>`.
    pub(super) fn agents_set_thought_visibility(&mut self, tail: &str) {
        if !self.config.agents.enabled {
            self.backend.status_message = Some(self.agents_status_message());
            return;
        }
        let show = match tail.trim() {
            "" | "toggle" => !self.agents.show_thoughts,
            "on" => true,
            "off" => false,
            _ => {
                self.backend.status_message =
                    Some(String::from("usage: :agents_thoughts on|off|toggle"));
                return;
            }
        };
        self.agents.show_thoughts = show;
        self.backend.status_message =
            Some(format!("agent thoughts {}", if show { "visible" } else { "hidden" }));
    }

    /// Phase 7 shutdown orchestration, called before the app exits:
    /// cancels running turns, resolves pending approvals/elicitations as
    /// cancelled, kills agent-owned terminals, and stops MCP servers and
    /// agent subprocesses.  Every step is internally bounded (host request
    /// timeouts), so a hung agent or MCP server cannot delay exit.
    pub(crate) fn shutdown_agents(&mut self) {
        // 1. Cancel running turns (also resolves their pending permissions).
        if let Some(host) = &self.agents.host {
            for thread in &self.agents.threads {
                if thread.host.is_turn_running() {
                    let _ = host.cancel(thread.host.clone());
                }
            }
        }
        // 2. Resolve pending approvals and elicitations as cancelled:
        //    dropping the reply senders makes the host resolve them.
        self.agents.approvals.clear();
        self.agents.mode_selection = None;
        self.agents.approval_mode_confirmation = None;
        self.agents.elicitation = None;
        self.agents.permission = None;
        // 3. Kill agent-owned terminals.
        self.agents.terminals.kill_all();
        // 4. Stop MCP servers and the proxy listener.
        self.shutdown_mcp();
        // 5. Stop ACP agent subprocesses (worker → manager shutdown).
        if let Some(host) = self.agents.host.take() {
            drop(host);
        }
        self.agents.threads.clear();
        self.agents.archived_threads.clear();
        self.agents.web_context_service = None;
        self.agents.web_context_config_fingerprint = None;
        self.agents.pending_fork = None;
        self.agents.approval_policy = super::agent_bridge::ApprovalPolicy::default();
        self.agents.approval_modes.clear();
        self.agents.usage_ledger = crate::policy::UsageLedger::default();
    }

    /// `:agents_threads` — open modal thread picker.
    pub(super) fn open_agents_thread_picker(&mut self) {
        if self.agents.threads.is_empty() {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        }
        let items = self
            .agents
            .threads
            .iter()
            .enumerate()
            .map(|(index, thread)| {
                let state = match thread.state {
                    ThreadUiState::Starting => "starting",
                    ThreadUiState::Ready => "ready",
                    ThreadUiState::Running => "running",
                    ThreadUiState::PausedRecoverable => "paused (recoverable)",
                    ThreadUiState::Closed => "closed",
                    ThreadUiState::Failed => "failed",
                };
                let unread = if thread.unread > 0 {
                    format!(" · unread:{}", thread.unread)
                } else {
                    String::new()
                };
                crate::picker::PickerItem {
                    label: thread.display_name.clone(),
                    detail: Some(format!("{state}{unread} · {}", thread.session_id)),
                    path: None,
                    buf_id: None,
                    line: None,
                    col: None,
                    choice_index: Some(index),
                }
            })
            .collect();
        self.open_picker(crate::picker::PickerState::new_agent_threads(items));
        if let Some(picker) = self.picker.as_mut()
            && let Some(active) = self.agents.active_thread
            && let Some(selected) = picker.filtered.iter().position(|index| *index == active)
        {
            picker.selected = selected;
        }
    }

    /// Focuses thread `index`, resetting its unread state.
    pub(crate) fn focus_thread(&mut self, index: usize) {
        self.agents.active_thread = Some(index);
        if let Some(thread) = self.agents.threads.get_mut(index) {
            thread.unread = 0;
            thread.activity = false;
        }
        self.persist_agent_workspace();
        self.enter_agent_focus();
    }

    /// Enters agents focus, remembering the editor mode to return to.
    ///
    /// The mode seen here is usually `CommandLine` (commands run from `:`);
    /// the mode to restore on close is the one held before the pane opened,
    /// captured on first focus and never overwritten.
    pub(super) fn enter_agent_focus(&mut self) {
        if self.agents_focused() {
            return;
        }
        if self.agents.previous_editor_mode.is_none() {
            let previous = match self.mode {
                Mode::CommandLine => self.command_mode_origin.unwrap_or(Mode::Normal),
                mode => mode,
            };
            if previous != Mode::Agent {
                self.agents.previous_editor_mode = Some(previous);
            }
        }
        self.mode = Mode::Agent;
    }

    /// The agent id used for `:agents` / `:agents_new`.
    fn default_agent_id(&self) -> Option<String> {
        let host = self.agents.host.as_ref()?;
        host.manager.resolve_default_agent(self.config.agents.default_agent.as_deref())
    }

    /// Creates the host bridge on first use (lazy).
    ///
    /// Secret references in agent env values are resolved here, immediately
    /// before `AgentProcessConfig` creation; a server whose references fail
    /// to resolve is skipped and never spawned.
    fn ensure_agents_host(&mut self) {
        if self.agents.host.is_some() {
            return;
        }
        let servers: Vec<(String, crate::config::AgentServerSettings)> = self
            .config
            .agents
            .servers
            .iter()
            .map(|(id, server)| (id.clone(), server.clone()))
            .collect();
        let mut config = AgentManagerConfig::default();
        let mut secret_store: Option<crate::secrets::SecretStore> = None;
        for (id, server) in servers {
            let env = if crate::secrets::resolve::agent_env_has_references(&server.env) {
                if secret_store.is_none() {
                    secret_store = self.build_agents_secret_store();
                }
                let Some(store) = &secret_store else {
                    eprintln!(
                        "ee: warning: agent `{id}` launch aborted: secrets store unavailable"
                    );
                    continue;
                };
                match crate::secrets::resolve::resolve_agent_env(store, &server) {
                    Ok(env) => env,
                    Err(err) => {
                        eprintln!("ee: warning: agent `{id}` launch aborted: {err}");
                        continue;
                    }
                }
            } else {
                server.env.iter().map(|(key, value)| (key.clone(), value.raw.clone())).collect()
            };
            // Collect secret-like final values for stderr/diagnostics
            // redaction (phase 5).
            for (name, value) in &env {
                if ee_agent_host::redact::is_secret_key(name) {
                    self.agents.resolved_secret_values.push(value.clone());
                }
            }
            config.agents.insert(
                id.clone(),
                ee_agent_host::AgentProcessConfig {
                    command: server.command.clone(),
                    args: server.args.clone(),
                    env,
                    cwd: server.cwd.clone(),
                },
            );
        }
        // Always host policy-governed editor MCP for ACP-native agents. Explicit
        // proxy configuration remains required only for stdio fallback.
        config.ee_proxy_enabled = true;
        #[cfg(test)]
        for (id, factory) in &self.agents.test_fake_transports {
            config.fake_transports.insert(id.clone(), factory.clone());
        }
        let (events_tx, events_rx) = tokio_mpsc::unbounded_channel();
        let handler: Arc<dyn ee_agent_host::ClientRequestHandler> =
            Arc::new(super::agent_bridge::BridgeUiHandler::new(
                self.agents.bridge_tx.clone(),
                self.agents.terminals.clone(),
            ));
        let manager = AgentManager::new(config, handler, events_tx);
        self.agents.host = Some(AgentHostBridge::new(manager, events_rx));
    }

    /// Builds the secrets store used by lazy agent-launch and web-search reference resolution.
    /// Tests inject a fake store; production uses the real default.
    pub(super) fn build_agents_secret_store(&mut self) -> Option<crate::secrets::SecretStore> {
        #[cfg(test)]
        {
            self.agents.test_secret_store.take()
        }
        #[cfg(not(test))]
        {
            match crate::secrets::SecretStore::default() {
                Ok(store) => Some(store),
                Err(err) => {
                    eprintln!("ee: warning: secrets store unavailable: {err}");
                    None
                }
            }
        }
    }

    /// Requests a new session for `agent_id` (async; reply pumped later).
    fn start_session(&mut self, agent_id: String) {
        let Some(host) = &self.agents.host else {
            return;
        };
        let roots = self.agents_workspace_roots();
        let mcp_servers = super::agents_mcp::mcp_forward_entries(&self.config.mcp);
        let ee_proxy_stdio_fallback =
            self.agents.mcp.proxy.as_ref().map(super::agents_mcp::proxy_stdio_fallback_entry);
        let reply =
            host.request_new_session(agent_id.clone(), roots, mcp_servers, ee_proxy_stdio_fallback);
        self.agents.pending_session = Some(PendingSession { agent_id, session_id: None, reply });
    }

    /// Reconnects selected session from this workspace's persisted thread list.
    pub(super) fn agents_reconnect(&mut self) {
        if self.agents.pending_session.is_some() {
            self.agents.error =
                Some(String::from("a session start or reconnect is already in progress"));
            return;
        }
        let Some(workspace) = self.load_persisted_agent_workspace() else {
            self.agents.error =
                Some(String::from("no persisted agent session for this workspace to reconnect"));
            return;
        };
        let selected_id = self
            .agents
            .active_thread_index()
            .and_then(|index| self.agents.threads.get(index))
            .map(|thread| thread.session_id.as_str())
            .or(workspace.active_session_id.as_deref());
        let Some(record) = selected_id
            .and_then(|session_id| {
                workspace.sessions.iter().find(|record| record.session_id == session_id)
            })
            .or_else(|| workspace.sessions.first())
            .cloned()
        else {
            self.agents.error =
                Some(String::from("no persisted agent session for this workspace to reconnect"));
            return;
        };
        self.request_persisted_agent_reconnect(record);
    }

    /// Starts restoring every persisted workspace thread. Returns `true` when
    /// at least one session was queued instead of creating a fresh session.
    fn start_workspace_restore(&mut self) -> bool {
        let Some(workspace) = self.load_persisted_agent_workspace() else {
            return false;
        };
        if workspace.sessions.is_empty() {
            return false;
        }
        self.agents.workspace_restore = Some(WorkspaceRestore {
            active_session_id: workspace.active_session_id,
            sessions: workspace.sessions.into(),
            failed: false,
        });
        self.start_next_workspace_restore();
        true
    }

    /// Sends one queued restoration request. ACP connection ordering requires
    /// that only one `session/load` or `session/resume` is active at a time.
    fn start_next_workspace_restore(&mut self) {
        let next =
            self.agents.workspace_restore.as_mut().and_then(|restore| restore.sessions.pop_front());
        if let Some(record) = next {
            self.request_persisted_agent_reconnect(record);
            return;
        }
        let Some(restore) = self.agents.workspace_restore.take() else {
            return;
        };
        if self.agents.active_thread.is_none() {
            self.agents.active_thread = restore
                .active_session_id
                .as_deref()
                .and_then(|session_id| self.agents.thread_index(session_id))
                .or_else(|| (!self.agents.threads.is_empty()).then_some(0));
        }
        if !restore.failed {
            self.persist_agent_workspace();
        }
    }

    /// Enqueues one reconnect through the existing load-then-resume pipeline.
    fn request_persisted_agent_reconnect(&mut self, record: PersistedAgentSession) {
        self.ensure_agents_host();
        let Some(host) = &self.agents.host else {
            return;
        };
        // `session/load` replays the conversation while the reply is still
        // in flight; those updates are buffered until the thread exists.
        self.agents.pending_replay.insert(record.session_id.clone(), Vec::new());
        let roots = self.agents_workspace_roots();
        let cwd = roots.first().cloned().unwrap_or_else(|| self.working_dir.clone());
        let additional_directories = roots.iter().skip(1).cloned().collect();
        let mcp_servers = super::agents_mcp::mcp_forward_entries(&self.config.mcp);
        let reply = host.request_reconnect(
            record.agent_id.clone(),
            record.session_id.clone(),
            cwd,
            additional_directories,
            mcp_servers,
        );
        self.agents.pending_session = Some(PendingSession {
            agent_id: record.agent_id.clone(),
            session_id: Some(record.session_id.clone()),
            reply,
        });
        self.backend.status_message =
            Some(format!("reconnecting session {}...", record.session_id));
    }

    /// The path of the client-persisted agent session record (per-workspace
    /// entries under the platform state directory).
    fn agents_session_record_path(&self) -> Option<std::path::PathBuf> {
        #[cfg(test)]
        if let Some(base) = self.agents.test_session_state_base.as_deref() {
            return Some(base.join("agent-sessions.json"));
        }
        crate::logs::state_dir().map(|dir| dir.join("agent-sessions.json"))
    }

    /// Loads persisted ordered threads for the primary workspace. Legacy
    /// one-session documents remain readable and migrate on their next write.
    fn load_persisted_agent_workspace(&self) -> Option<PersistedAgentWorkspace> {
        let path = self.agents_session_record_path()?;
        let documents: HashMap<String, serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        let document = documents.get(&self.primary_workspace_identity().as_string())?.clone();
        serde_json::from_value::<PersistedAgentWorkspaceDocument>(document)
            .ok()
            .map(PersistedAgentWorkspaceDocument::into_workspace)
    }

    /// Writes one complete workspace thread registry atomically at the
    /// document level, preserving records for every other workspace.
    fn save_persisted_agent_workspace(&self, workspace: Option<&PersistedAgentWorkspace>) {
        let Some(path) = self.agents_session_record_path() else {
            return;
        };
        let mut documents: HashMap<String, serde_json::Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let key = self.primary_workspace_identity().as_string();
        match workspace {
            Some(workspace) => match serde_json::to_value(workspace) {
                Ok(value) => {
                    documents.insert(key, value);
                }
                Err(_) => return,
            },
            None => {
                documents.remove(&key);
            }
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&documents) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Persists current open-thread ordering, local names, selected thread,
    /// and per-thread recoverable prompts. Archived/deleted local threads are
    /// intentionally omitted so they do not reopen after process restart.
    fn persist_agent_workspace(&self) {
        // Keep queued restore records intact until every ACP load completes.
        // Otherwise registering the first restored thread would erase peers
        // that have not yet been attempted.
        if self.agents.workspace_restore.is_some() {
            return;
        }
        let existing = self.load_persisted_agent_workspace().unwrap_or_default();
        let sessions = self
            .agents
            .threads
            .iter()
            .map(|thread| PersistedAgentSession {
                agent_id: thread.agent_id.clone(),
                session_id: thread.session_id.clone(),
                last_prompt: existing
                    .sessions
                    .iter()
                    .find(|record| record.session_id == thread.session_id)
                    .and_then(|record| record.last_prompt.clone()),
                session_name: thread.session_name.clone(),
            })
            .collect();
        let active_session_id = self
            .agents
            .workspace_restore
            .as_ref()
            .and_then(|restore| restore.active_session_id.clone())
            .or_else(|| {
                self.agents
                    .active_thread_index()
                    .and_then(|index| self.agents.threads.get(index))
                    .map(|thread| thread.session_id.clone())
            });
        self.save_persisted_agent_workspace(Some(&PersistedAgentWorkspace {
            version: persisted_agent_workspace_version(),
            active_session_id,
            sessions,
        }));
    }

    /// Updates a persisted thread's recoverable prompt without disturbing
    /// other workspace sessions.
    fn update_persisted_last_prompt(&self, session_id: &str, last_prompt: Option<&str>) {
        let Some(mut workspace) = self.load_persisted_agent_workspace() else {
            return;
        };
        let Some(record) =
            workspace.sessions.iter_mut().find(|record| record.session_id == session_id)
        else {
            return;
        };
        record.last_prompt = last_prompt.map(str::to_string);
        self.save_persisted_agent_workspace(Some(&workspace));
    }

    /// Absolute workspace roots forwarded as ACP session context.
    pub(super) fn agents_workspace_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.working_dir.clone()];
        for buf in self.backend.all_bufs() {
            if let Some(path) = &buf.path
                && let Some(parent) = path.parent()
            {
                roots.push(parent.to_path_buf());
            }
        }
        roots.extend(self.agents.additional_workspace_roots.iter().cloned());
        roots.sort();
        roots.dedup();
        roots
    }

    fn queue_thread_mode_change(&mut self, thread_index: usize, mode_id: SessionModeId) {
        let Some(host) = &self.agents.host else {
            self.backend.status_message = Some(String::from("agent host not ready"));
            return;
        };
        let reply = host.set_mode(self.agents.threads[thread_index].host.clone(), mode_id.clone());
        self.agents.pending_thread_action = Some(reply);
        self.backend.status_message = Some(format!("setting mode: {}", mode_id.0));
    }

    fn queue_thread_config_option_change(
        &mut self,
        thread_index: usize,
        config_id: ee_agent_protocol::SessionConfigId,
        value: SessionConfigOptionValue,
    ) {
        let Some(host) = &self.agents.host else {
            self.backend.status_message = Some(String::from("agent host not ready"));
            return;
        };
        let reply = host.set_config_option(
            self.agents.threads[thread_index].host.clone(),
            config_id.clone(),
            value,
        );
        self.agents.pending_thread_action = Some(reply);
        self.backend.status_message = Some(format!("setting config: {}", config_id.0));
    }

    fn open_mode_selection(&mut self, thread_index: usize) {
        let thread = &self.agents.threads[thread_index];
        let options = agent_mode_ids(thread);
        let selected = thread
            .host
            .snapshot()
            .current_mode
            .as_ref()
            .and_then(|current| options.iter().position(|mode| mode == current.0.as_ref()))
            .unwrap_or_default();
        self.agents.mode_selection = Some(ModeSelectionPrompt { thread_index, options, selected });
    }

    fn agents_set_mode(&mut self, thread_index: usize, raw_mode: &str) {
        let raw_mode = raw_mode.trim();
        if raw_mode.is_empty() {
            self.open_mode_selection(thread_index);
            return;
        }

        let snapshot = self.agents.threads[thread_index].host.snapshot();
        if let Some(mode_option) =
            snapshot.config_options.iter().find(|option| is_mode_config_option(option))
        {
            match parse_config_option_value(mode_option, raw_mode) {
                Ok(value) => {
                    self.queue_thread_config_option_change(
                        thread_index,
                        mode_option.id.clone(),
                        value,
                    );
                }
                Err(error) => self.backend.status_message = Some(error),
            }
            return;
        }

        let Some(modes) = self.agents.threads[thread_index].host.advertised_modes() else {
            self.open_mode_selection(thread_index);
            return;
        };
        let Some(mode) =
            modes.available_modes.iter().find(|mode| mode.id.0.eq_ignore_ascii_case(raw_mode))
        else {
            let available = modes
                .available_modes
                .iter()
                .map(|mode| mode.id.0.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            self.backend.status_message = Some(if available.is_empty() {
                String::from("agent advertised no modes")
            } else {
                format!("unknown mode: {raw_mode}; available: {available}")
            });
            return;
        };
        self.queue_thread_mode_change(thread_index, mode.id.clone());
    }

    fn agents_cycle_mode(&mut self, delta: isize) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let snapshot = self.agents.threads[active].host.snapshot();
        if let Some(mode_option) =
            snapshot.config_options.iter().find(|option| is_mode_config_option(option))
            && let SessionConfigKind::Select(select) = &mode_option.kind
            && let Some(next) = cycle_select_value(&select.options, &select.current_value, delta)
        {
            self.queue_thread_mode_change(active, SessionModeId::new(next.0.clone()));
            return;
        }
        if let Some(modes) = self.agents.threads[active].host.advertised_modes() {
            if modes.available_modes.is_empty() {
                self.backend.status_message = Some(String::from("agent advertised no modes"));
                return;
            }
            let current = modes.current_mode_id;
            let current_index = modes
                .available_modes
                .iter()
                .position(|mode| mode.id == current)
                .unwrap_or_default();
            let next_index = (current_index as isize + delta)
                .rem_euclid(modes.available_modes.len() as isize)
                as usize;
            self.queue_thread_mode_change(active, modes.available_modes[next_index].id.clone());
            return;
        }
        self.backend.status_message = Some(String::from("agent session has no advertised modes"));
    }

    fn agents_list_config_options(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let options = self.agents.threads[active].host.config_options();
        if options.is_empty() {
            self.backend.status_message =
                Some(String::from("no session config options advertised"));
            return;
        }
        let summary = options.iter().map(config_option_summary).collect::<Vec<_>>().join(" · ");
        self.backend.status_message = Some(summary);
    }

    /// Mutates only an ACP configuration option explicitly advertised by the
    /// active provider session. Alias names deliberately match option ids; EE
    /// never guesses provider model, effort, or personality semantics.
    fn agents_set_provider_config_alias(&mut self, alias: &str, raw_value: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let options = self.agents.threads[active].host.config_options();
        let Some(option) = options.into_iter().find(|option| option.id.0.as_ref() == alias) else {
            self.backend.status_message = Some(format!(
                "provider config /{alias} unavailable: agent did not advertise config option {alias}; use /config"
            ));
            return;
        };

        if raw_value.is_empty() {
            if let SessionConfigKind::Boolean(current) = &option.kind {
                self.queue_thread_config_option_change(
                    active,
                    option.id.clone(),
                    SessionConfigOptionValue::boolean(!current.current_value),
                );
            } else {
                self.backend.status_message = Some(format!(
                    "provider config /{alias} currently {}; usage: /{alias} <value>",
                    config_option_summary(&option)
                ));
            }
            return;
        }

        let value = match parse_config_option_value(&option, raw_value) {
            Ok(value) => value,
            Err(message) => {
                self.backend.status_message = Some(message);
                return;
            }
        };
        self.queue_thread_config_option_change(active, option.id.clone(), value);
    }

    /// Returns true when a known provider-owned command must be consumed
    /// locally. Advertised provider commands return false and continue through
    /// normal ACP prompt forwarding unchanged.
    fn agents_require_advertised_provider_command(&mut self, command: &str) -> bool {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return true;
        };
        if self.agents.threads[active]
            .available_commands
            .iter()
            .any(|available| available.name == command)
        {
            return false;
        }
        self.backend.status_message = Some(format!(
            "provider command /{command} unavailable: agent did not advertise it; use /help for provider commands"
        ));
        true
    }

    fn agents_set_config_option_command(&mut self, tail: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let mut parts = tail.trim().splitn(2, char::is_whitespace);
        let Some(config_id) = parts.next().filter(|part| !part.is_empty()) else {
            self.backend.status_message =
                Some(String::from("usage: :agents_config_set <config_id> <value>"));
            return;
        };
        let Some(raw_value) = parts.next().map(str::trim).filter(|part| !part.is_empty()) else {
            self.backend.status_message =
                Some(String::from("usage: :agents_config_set <config_id> <value>"));
            return;
        };
        let options = self.agents.threads[active].host.config_options();
        let Some(option) = options.into_iter().find(|option| option.id.0.as_ref() == config_id)
        else {
            self.backend.status_message = Some(format!("unknown config option: {config_id}"));
            return;
        };
        let value = match parse_config_option_value(&option, raw_value) {
            Ok(value) => value,
            Err(message) => {
                self.backend.status_message = Some(message);
                return;
            }
        };
        self.queue_thread_config_option_change(active, option.id.clone(), value);
    }

    fn agents_toggle_config_option_command(&mut self, tail: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let config_id = tail.trim();
        if config_id.is_empty() {
            self.backend.status_message =
                Some(String::from("usage: :agents_config_toggle <config_id>"));
            return;
        }
        let options = self.agents.threads[active].host.config_options();
        let Some(option) = options.into_iter().find(|option| option.id.0.as_ref() == config_id)
        else {
            self.backend.status_message = Some(format!("unknown config option: {config_id}"));
            return;
        };
        let SessionConfigKind::Boolean(current) = option.kind else {
            self.backend.status_message = Some(format!("config option {config_id} is not boolean"));
            return;
        };
        self.queue_thread_config_option_change(
            active,
            option.id.clone(),
            SessionConfigOptionValue::boolean(!current.current_value),
        );
    }

    fn cycle_slash_command(&mut self, delta: isize) -> bool {
        let Some(active) = self.agents.active_thread_index() else {
            return false;
        };
        let thread = &mut self.agents.threads[active];
        let draft = thread.draft.clone();
        let (current_name, rest) = split_slash_command(&draft);
        // Preserve user-entered arguments while cycling slash commands.
        if !draft.trim_start().starts_with('/') {
            return false;
        }

        let command_name = current_name.as_deref().unwrap_or_default();
        let command_names = agent_slash_command_names(&thread.available_commands);
        let current_index = command_names.iter().position(|name| *name == command_name);
        let matching_indices: Vec<usize> = if current_index.is_some() {
            (0..command_names.len()).collect()
        } else {
            // Prefer agent-advertised commands for ambiguous prefixes. Local
            // commands remain available as a fallback without stealing an
            // agent's command such as `/edit` from local `/export`.
            let advertised_matches: Vec<usize> = thread
                .available_commands
                .iter()
                .filter(|command| command.name.starts_with(command_name))
                .filter_map(|command| command_names.iter().position(|name| *name == command.name))
                .collect();
            if advertised_matches.is_empty() {
                command_names
                    .iter()
                    .enumerate()
                    .filter_map(|(index, name)| name.starts_with(command_name).then_some(index))
                    .collect()
            } else {
                advertised_matches
            }
        };
        let Some(next_index) = (!matching_indices.is_empty()).then(|| {
            let position = current_index.and_then(|index| {
                matching_indices.iter().position(|candidate| *candidate == index)
            });
            let next_position = match position {
                Some(position) => {
                    (position as isize + delta).rem_euclid(matching_indices.len() as isize) as usize
                }
                None if delta >= 0 => 0,
                None => matching_indices.len() - 1,
            };
            matching_indices[next_position]
        }) else {
            return false;
        };
        thread.draft = slash_command_draft(command_names[next_index], &rest);
        true
    }

    /// Applies pane-local exit commands without requiring an agent session.
    fn submit_agents_exit_command(&mut self, draft: &str) -> bool {
        if is_agents_quit_slash_command(draft) {
            self.agents_clear_draft();
            self.close_agents_pane();
            return true;
        }
        if is_agents_quit_full_slash_command(draft) {
            self.agents_clear_draft();
            self.should_quit = true;
            return true;
        }
        false
    }

    fn active_tool_approval_mode(&self) -> Option<super::agent_bridge::ToolApprovalMode> {
        let active = self.agents.active_thread_index()?;
        let session_id = &self.agents.threads[active].session_id;
        Some(self.agents.approval_modes.get(session_id).copied().unwrap_or_default())
    }

    fn set_active_tool_approval_mode(&mut self, mode: super::agent_bridge::ToolApprovalMode) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let session_id = self.agents.threads[active].session_id.clone();
        if mode == super::agent_bridge::ToolApprovalMode::Default {
            self.agents.approval_modes.remove(&session_id);
        } else {
            self.agents.approval_modes.insert(session_id, mode);
        }
        let summary = format!("tool approvals: {}", mode.label());
        self.agents.threads[active].push_system(summary.clone());
        self.backend.status_message = Some(summary);
    }

    fn request_bypass_tool_approvals(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        self.agents.approval_mode_confirmation = Some(ApprovalModeConfirmation {
            thread_index: active,
            session_id: self.agents.threads[active].session_id.clone(),
        });
        self.backend.status_message = Some(String::from("confirm bypass tool approvals"));
    }

    fn confirm_bypass_tool_approvals(&mut self) {
        let Some(confirmation) = self.agents.approval_mode_confirmation.take() else {
            return;
        };
        let Some(thread) = self.agents.threads.get(confirmation.thread_index) else {
            self.backend.status_message =
                Some(String::from("agent session closed before bypass confirmation"));
            return;
        };
        if thread.session_id != confirmation.session_id {
            self.backend.status_message =
                Some(String::from("agent session changed before bypass confirmation"));
            return;
        }
        self.set_active_tool_approval_mode(super::agent_bridge::ToolApprovalMode::Bypass);
    }

    fn agents_show_help(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let secrets = self.agents_secret_values();
        let provider_commands = self.agents.threads[active]
            .available_commands
            .iter()
            .map(|command| {
                let description =
                    ee_agent_host::redact::redact_secret_values(&command.description, &secrets);
                if description.is_empty() {
                    format!("/{}", command.name)
                } else {
                    format!("/{} — {description}", command.name)
                }
            })
            .collect::<Vec<_>>();
        let local = LOCAL_AGENT_SLASH_HELP
            .iter()
            .map(|(command, usage)| format!("{command} — {usage}"))
            .collect::<Vec<_>>()
            .join("\n");
        let provider_config = PROVIDER_CONFIG_ALIASES
            .iter()
            .copied()
            .filter(|alias| {
                self.agents.threads[active]
                    .host
                    .config_options()
                    .iter()
                    .any(|option| option.id.0.as_ref() == *alias)
            })
            .map(|alias| format!("/{alias} [value] — advertised provider config"))
            .collect::<Vec<_>>()
            .join("\n");
        let provider = if provider_commands.is_empty() {
            String::from("(none advertised)")
        } else {
            provider_commands.join("\n")
        };
        let provider_config = if provider_config.is_empty() {
            String::from("(none advertised; use /config to inspect ACP options)")
        } else {
            provider_config
        };
        self.agents.threads[active].push_system(format!(
            "Local slash commands:\n{local}\n\nCapability-gated provider config:\n{provider_config}\n\nProvider-owned slash commands (sent to agent):\n{provider}"
        ));
        self.backend.status_message = Some(String::from("slash help added to transcript"));
    }

    fn agents_show_status(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &self.agents.threads[active];
        let snapshot = thread.host.snapshot();
        let mode = snapshot
            .current_mode
            .map_or_else(|| String::from("unavailable"), |mode| mode.0.to_string());
        let approval =
            self.agents.approval_modes.get(&thread.session_id).copied().unwrap_or_default().label();
        let context_bytes =
            thread.context_files.iter().map(|file| file.content.len()).sum::<usize>();
        let name = thread
            .session_name
            .as_deref()
            .or(thread.session_title.as_deref())
            .unwrap_or("(unnamed)");
        let summary = format!(
            "session:{} name:{} agent:{} connection:{} mode:{} approval:{} context:{}/{} bytes mcp:{} configured provider-commands:{}",
            thread.session_id,
            name,
            thread.agent_id,
            thread_state_label(thread.state),
            mode,
            approval,
            thread.context_files.len(),
            context_bytes,
            self.config.mcp.servers.len(),
            thread.available_commands.len(),
        );
        self.agents.threads[active].push_system(summary.clone());
        self.backend.status_message = Some(summary);
    }

    /// Read-only local diagnostics. It never reconnects, resets, repairs, or starts services.
    fn agents_doctor(&mut self) {
        let secrets = self.agents_secret_values();
        let workspace = std::fs::canonicalize(&self.working_dir)
            .unwrap_or_else(|_| self.working_dir.clone())
            .display()
            .to_string();
        let configured_agents = self
            .config
            .agents
            .servers
            .iter()
            .map(|(id, server)| {
                let command = if server.args.is_empty() {
                    server.command.clone()
                } else {
                    format!("{} {}", server.command, server.args.join(" "))
                };
                format!("{id}: {}", ee_agent_host::redact::redact_secret_values(&command, &secrets))
            })
            .collect::<Vec<_>>();
        let persisted = self.load_persisted_agent_workspace();
        let mut lines = vec![
            String::from("Agents TUI doctor (read-only)"),
            format!("feature: {}", self.agents_status_message()),
            format!(
                "workspace: {}",
                ee_agent_host::redact::redact_secret_values(&workspace, &secrets)
            ),
            if configured_agents.is_empty() {
                String::from("configured agent command: none")
            } else {
                format!("configured agent command: {}", configured_agents.join("; "))
            },
            format!(
                "MCP configuration: {} server(s), proxy:{}",
                self.config.mcp.servers.len(),
                if self.config.mcp.proxy.enabled { "enabled" } else { "disabled" }
            ),
            match persisted {
                Some(workspace) => format!(
                    "session storage: {} workspace thread(s), active:{}",
                    workspace.sessions.len(),
                    workspace.active_session_id.as_deref().unwrap_or("none")
                ),
                None => String::from("session storage: no persisted workspace threads"),
            },
            String::from(
                "redaction: secret-like JSON keys and configured secret values are redacted; context snapshots stay session-local",
            ),
        ];
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &self.agents.threads[active];
            let snapshot = thread.host.snapshot();
            let mode = snapshot
                .current_mode
                .map_or_else(|| String::from("unavailable"), |mode| mode.0.to_string());
            lines.push(format!(
                "ACP session: {} agent:{} connection:{} mode:{} advertised-commands:{} config-options:{}",
                thread.session_id,
                thread.agent_id,
                thread_state_label(thread.state),
                mode,
                thread.available_commands.len(),
                snapshot.config_options.len(),
            ));
        } else {
            lines.push(String::from("ACP session: no active session; handshake unavailable"));
        }
        if let Some(error) = &self.agents.error {
            lines.push(format!(
                "agent error: {}",
                ee_agent_host::redact::redact_secret_values(error, &secrets)
            ));
        }
        lines.extend(
            self.mcp_health_lines()
                .into_iter()
                .map(|line| ee_agent_host::redact::redact_secret_values(&line, &secrets)),
        );
        let report = lines.join("\n");
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].push_system(report.clone());
        }
        self.backend.status_message =
            Some(String::from("Agents TUI doctor report added to transcript"));
    }

    /// Asks active agent to inspect existing instructions and optionally propose a safe scaffold.
    fn agents_submit_init_workflow(&mut self) {
        self.agents_send_local_workflow(
            String::from("EE local /init request sent; agent response is provider-generated."),
            String::from(
                "EE-local /init workflow. Inspect existing project instructions with `ee_project_instructions` before proposing changes. If an AGENTS.md or equivalent already exists, show a concise preview/diff only; do not overwrite it. If no project instruction exists, offer a compact AGENTS.md scaffold tailored to this workspace. Create it only through `ee_create_text_file`, which must receive normal file-write approval. Never use a shell write, overwrite, or bypass approval. Clearly label advice as agent-generated, not an EE-native initialization engine.",
            ),
        );
    }

    /// Sends bounded, redacted local review evidence to current agent without persisting or rendering body.
    fn agents_submit_review_workflow(&mut self, target: &str, security: bool) {
        let target = target.trim();
        if target.len() > 1024 || target.chars().any(char::is_control) {
            self.backend.status_message =
                Some(String::from("review target must be printable and at most 1024 bytes"));
            return;
        }
        let secrets = self.agents_secret_values();
        let target = if target.is_empty() {
            String::from("current workspace changes")
        } else {
            ee_agent_host::redact::redact_secret_values(target, &secrets)
        };
        let evidence = match self.proxy_review_context() {
            Ok(value) => value,
            Err(error) => {
                self.backend.status_message =
                    Some(format!("local review evidence unavailable: {error}"));
                return;
            }
        };
        let evidence = ee_agent_host::redact::redact_json(&evidence);
        let mut evidence = match serde_json::to_string_pretty(&evidence) {
            Ok(value) => ee_agent_host::redact::redact_secret_values(&value, &secrets),
            Err(error) => {
                self.backend.status_message =
                    Some(format!("local review evidence unavailable: {error}"));
                return;
            }
        };
        if evidence.len() > AGENT_REVIEW_CONTEXT_MAX_BYTES {
            let mut end = AGENT_REVIEW_CONTEXT_MAX_BYTES;
            while !evidence.is_char_boundary(end) {
                end -= 1;
            }
            evidence.truncate(end);
            evidence.push_str("\n[EE local review evidence truncated]");
        }
        let focus = if security {
            "Review for security defects: authorization and approval boundaries, workspace/path containment, command and terminal ownership, secret exposure, MCP capability gates, unsafe input handling, and fail-open behavior. Do not use network access. Do not inspect protected paths or request raw secret values."
        } else {
            "Review for correctness, regressions, missing tests, diagnostics, and risky changes. Use bounded diff drill-down only for relevant changed files."
        };
        let kind = if security { "security review" } else { "code review" };
        let instruction = format!(
            "EE-local {kind} workflow for target: {target}.\n{focus}\nThis evidence comes from EE changed-file, diagnostics, symbol, and test-metadata tools. It is bounded and redacted; treat omissions as unknown. Do not claim a native provider review engine.\n\nBounded EE evidence:\n{evidence}"
        );
        self.agents_send_local_workflow(
            format!("EE local {kind} request sent; result will be agent-generated."),
            instruction,
        );
    }

    /// Dispatches workflow prompt only for ready session. Local evidence never enters persistence or transcript body.
    fn agents_send_local_workflow(&mut self, display_text: String, instruction: String) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message =
                Some(String::from("no active agent session; start one with /new"));
            return;
        };
        match self.agents.threads[active].state {
            ThreadUiState::Ready => {}
            ThreadUiState::Running => {
                self.backend.status_message = Some(String::from(
                    "agent turn is running; wait for it to finish before starting local workflow",
                ));
                return;
            }
            ThreadUiState::PausedRecoverable => {
                self.backend.status_message =
                    Some(String::from("a turn is paused; use /resume or /discard first"));
                return;
            }
            _ => {
                self.backend.status_message =
                    Some(String::from("agent session is not ready; cannot start local workflow"));
                return;
            }
        }
        let blocks = {
            let thread = &self.agents.threads[active];
            prompt_blocks_with_context(&instruction, &thread.context_files, &[])
        };
        self.send_agent_prompt_blocks(active, display_text, blocks, None);
    }

    fn agents_copy_assistant_response(&mut self, args: &str) {
        let position = match args {
            "" => 1,
            value => match value.parse::<usize>() {
                Ok(position) if position > 0 => position,
                _ => {
                    self.backend.status_message =
                        Some(String::from("usage: /copy [positive response number]"));
                    return;
                }
            },
        };
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &self.agents.threads[active];
        let mut groups = BTreeSet::new();
        for item in &thread.transcript {
            if let TranscriptItem::Message {
                kind: MessageRenderKind::Assistant,
                response_group: Some(group),
                ..
            } = item
                && thread.active_response_group != Some(*group)
            {
                groups.insert(*group);
            }
        }
        let Some(group) = groups.into_iter().rev().nth(position - 1) else {
            self.backend.status_message =
                Some(String::from("no completed assistant response to copy"));
            return;
        };
        let text = thread
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Message {
                    text,
                    kind: MessageRenderKind::Assistant,
                    response_group: Some(item_group),
                    ..
                } if *item_group == group => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        if text.trim().is_empty() {
            self.backend.status_message =
                Some(String::from("no completed assistant response to copy"));
            return;
        }
        let text = ee_agent_host::redact::redact_secret_values(&text, &self.agents_secret_values());
        match crate::registers::write_system_clipboard(&text) {
            Ok(()) => {
                self.backend.status_message = Some(format!("copied assistant response {position}"));
            }
            Err(error) => self.backend.status_message = Some(error),
        }
    }

    fn agents_rename_session(&mut self, raw_name: &str) {
        let Some(name) = sanitize_session_name(raw_name) else {
            self.backend.status_message =
                Some(String::from("session name must contain visible text"));
            return;
        };
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        thread.session_name = Some(name.clone());
        thread.display_name = thread_display_name(
            thread.index,
            &thread.agent_id,
            thread.session_name.as_deref(),
            thread.session_title.as_deref(),
        );
        thread.push_system(format!("session renamed: {name}"));
        self.persist_agent_workspace();
        self.backend.status_message = Some(format!("session renamed: {name}"));
    }

    fn agents_list_context_files(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let files = &self.agents.threads[active].context_files;
        self.backend.status_message = Some(if files.is_empty() {
            String::from("context files: none")
        } else {
            format!(
                "context files: {}",
                files
                    .iter()
                    .map(|file| format!("{} ({} bytes)", file.relative_path, file.content.len()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    }

    fn agents_add_context_file(&mut self, path: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let (canonical, relative_path, content) =
            match self.agent_context_file_snapshot(Path::new(path), AGENT_CONTEXT_MAX_FILE_BYTES) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.backend.status_message = Some(error);
                    return;
                }
            };
        let content_len = content.len();
        let thread = &mut self.agents.threads[active];
        let existing = thread.context_files.iter().position(|file| file.path == canonical);
        let total = thread
            .context_files
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != existing)
            .map(|(_, file)| file.content.len())
            .sum::<usize>();
        if total.saturating_add(content_len) > AGENT_CONTEXT_MAX_TOTAL_BYTES {
            self.backend.status_message = Some(format!(
                "context files exceed {AGENT_CONTEXT_MAX_TOTAL_BYTES} byte total limit"
            ));
            return;
        }
        let attached_path = relative_path.clone();
        let context_file = AgentContextFile { path: canonical, relative_path, content };
        if let Some(existing) = existing {
            thread.context_files[existing] = context_file;
        } else {
            if thread.context_files.len() >= AGENT_CONTEXT_MAX_FILES {
                self.backend.status_message =
                    Some(format!("context file limit reached ({AGENT_CONTEXT_MAX_FILES})"));
                return;
            }
            thread.context_files.push(context_file);
        }
        let notice = format!("context attached: {attached_path} ({content_len} bytes)");
        thread.push_system(notice.clone());
        self.backend.status_message = Some(notice);
    }

    fn agents_remove_context_file(&mut self, path: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        let Some(index) = thread.context_files.iter().position(|file| file.relative_path == path)
        else {
            self.backend.status_message = Some(format!("context file not attached: {path}"));
            return;
        };
        let removed = thread.context_files.remove(index);
        let notice = format!("context removed: {}", removed.relative_path);
        thread.push_system(notice.clone());
        self.backend.status_message = Some(notice);
    }

    fn agents_clear_context_files(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        let count = thread.context_files.len();
        thread.context_files.clear();
        let notice = format!("context files cleared ({count})");
        thread.push_system(notice.clone());
        self.backend.status_message = Some(notice);
    }

    fn agents_context_status(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &self.agents.threads[active];
        let session_bytes =
            thread.context_files.iter().map(|file| file.content.len()).sum::<usize>();
        let mention_bytes =
            thread.next_prompt_context_files.iter().map(|file| file.content.len()).sum::<usize>();
        let paths = thread
            .context_files
            .iter()
            .map(|file| format!("{} ({} bytes)", file.relative_path, file.content.len()))
            .collect::<Vec<_>>();
        let mentions = thread
            .next_prompt_context_files
            .iter()
            .map(|file| format!("{} ({} bytes)", file.relative_path, file.content.len()))
            .collect::<Vec<_>>();
        let summary = format!(
            "context scope=session-only; selected:[{}]; one-turn mentions:[{}]; totals:{} session + {} one-turn / {} bytes; caps:{} files, {} bytes/file, {} bytes total",
            if paths.is_empty() { String::from("none") } else { paths.join(", ") },
            if mentions.is_empty() { String::from("none") } else { mentions.join(", ") },
            session_bytes,
            mention_bytes,
            AGENT_CONTEXT_MAX_TOTAL_BYTES,
            AGENT_CONTEXT_MAX_FILES,
            AGENT_CONTEXT_MAX_FILE_BYTES,
            AGENT_CONTEXT_MAX_TOTAL_BYTES,
        );
        self.agents.threads[active].push_system(summary.clone());
        self.backend.status_message = Some(summary);
    }

    /// Adds a bounded, redacted snapshot to exactly the next submitted prompt.
    fn agents_mention_context_file(&mut self, path: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let (canonical, relative_path, content) =
            match self.agent_context_file_snapshot(Path::new(path), AGENT_CONTEXT_MAX_FILE_BYTES) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.backend.status_message = Some(error);
                    return;
                }
            };
        let content_len = content.len();
        let thread = &mut self.agents.threads[active];
        let existing =
            thread.next_prompt_context_files.iter().position(|file| file.path == canonical);
        let total = thread
            .context_files
            .iter()
            .chain(thread.next_prompt_context_files.iter())
            .enumerate()
            .filter(|(index, _)| {
                *index < thread.context_files.len()
                    || Some(*index - thread.context_files.len()) != existing
            })
            .map(|(_, file)| file.content.len())
            .sum::<usize>();
        if total.saturating_add(content_len) > AGENT_CONTEXT_MAX_TOTAL_BYTES {
            self.backend.status_message = Some(format!(
                "context snapshots exceed {AGENT_CONTEXT_MAX_TOTAL_BYTES} byte total limit"
            ));
            return;
        }
        if existing.is_none()
            && thread.context_files.len() + thread.next_prompt_context_files.len()
                >= AGENT_CONTEXT_MAX_FILES
        {
            self.backend.status_message =
                Some(format!("context file limit reached ({AGENT_CONTEXT_MAX_FILES})"));
            return;
        }
        let mention =
            AgentContextFile { path: canonical, relative_path: relative_path.clone(), content };
        if let Some(existing) = existing {
            thread.next_prompt_context_files[existing] = mention;
        } else {
            thread.next_prompt_context_files.push(mention);
        }
        let notice =
            format!("mention attached for next prompt only: {relative_path} ({content_len} bytes)");
        thread.push_system(notice.clone());
        self.backend.status_message = Some(notice);
    }

    fn request_additional_workspace_directory(&mut self, raw_path: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        if !self.agents.threads[active].host.supports_additional_directories() {
            self.backend.status_message =
                Some(String::from("agent does not advertise additional-directory capability"));
            return;
        }
        let path = Path::new(raw_path);
        let candidate =
            if path.is_absolute() { path.to_path_buf() } else { self.working_dir.join(path) };
        let canonical = match std::fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(error) => {
                self.backend.status_message = Some(format!(
                    "cannot access additional directory {}: {error}",
                    candidate.display()
                ));
                return;
            }
        };
        if !canonical.is_dir() {
            self.backend.status_message =
                Some(format!("additional root is not a directory: {}", canonical.display()));
            return;
        }
        if self.is_secret_store_path(&canonical) {
            self.backend.status_message = Some(String::from("additional root is protected"));
            return;
        }
        if canonical
            == std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone())
        {
            self.backend.status_message =
                Some(String::from("directory is already primary workspace root"));
            return;
        }
        if self.agents.additional_workspace_roots.contains(&canonical) {
            self.backend.status_message = Some(format!(
                "additional root already trusted for this session: {}",
                canonical.display()
            ));
            return;
        }
        if self.agents.additional_workspace_roots.len() >= AGENT_ADDITIONAL_ROOT_MAX {
            self.backend.status_message =
                Some(format!("additional root limit reached ({AGENT_ADDITIONAL_ROOT_MAX})"));
            return;
        }
        self.agents.additional_directory_confirmation =
            Some(AdditionalDirectoryConfirmation { path: canonical.clone() });
        self.backend.status_message =
            Some(format!("confirm additional workspace root: {}", canonical.display()));
    }

    fn confirm_additional_workspace_directory(&mut self) {
        let Some(confirmation) = self.agents.additional_directory_confirmation.take() else {
            return;
        };
        self.agents.additional_workspace_roots.insert(confirmation.path.clone());
        self.backend.status_message = Some(format!(
            "additional root trusted for this Agents TUI session: {}; current provider session unchanged; /new uses it when supported",
            confirmation.path.display()
        ));
    }

    fn agents_context_command(&mut self, args: &str) {
        let mut parts = args.splitn(2, char::is_whitespace);
        match (parts.next().unwrap_or_default(), parts.next().unwrap_or_default().trim()) {
            ("", _) => self.agents_list_context_files(),
            ("status", "") => self.agents_context_status(),
            ("add", path) if !path.is_empty() => self.agents_add_context_file(path),
            ("remove", path) if !path.is_empty() => self.agents_remove_context_file(path),
            ("clear", "") => self.agents_clear_context_files(),
            _ => {
                self.backend.status_message =
                    Some(String::from("usage: /context [status|add <path>|remove <path>|clear]"));
            }
        }
    }

    /// Applies pane-local slash commands before forwarding prompt text to the agent.
    fn submit_agents_local_slash_command(&mut self, draft: &str) -> bool {
        let (Some(command), args) = split_slash_command(draft) else {
            return false;
        };
        let args = args.trim();
        let handled = match command.as_str() {
            "help" if args.is_empty() => {
                self.agents_show_help();
                true
            }
            "status" if args.is_empty() => {
                self.agents_show_status();
                true
            }
            "doctor" if args.is_empty() => {
                self.agents_doctor();
                true
            }
            "init" if args.is_empty() => {
                self.agents_submit_init_workflow();
                true
            }
            "review" => {
                self.agents_submit_review_workflow(args, false);
                true
            }
            "security-review" => {
                self.agents_submit_review_workflow(args, true);
                true
            }
            "diff" if args.is_empty() => {
                self.open_workspace_git_diff();
                true
            }
            "copy" => {
                self.agents_copy_assistant_response(args);
                true
            }
            "rename" if !args.is_empty() => {
                self.agents_rename_session(args);
                true
            }
            "rename" => {
                self.backend.status_message = Some(String::from("usage: /rename <name>"));
                true
            }
            "sessions" if args.is_empty() => {
                self.open_agents_thread_picker();
                true
            }
            "export" if args.is_empty() => {
                self.agents_export_transcript();
                true
            }
            "new" | "new_thread" if args.is_empty() => {
                self.agents_new_session();
                true
            }
            "archive" => {
                self.agents_archive_command(args);
                true
            }
            "fork" if args.is_empty() => {
                self.agents_fork_session(false);
                true
            }
            "branch" if args.is_empty() => {
                self.agents_fork_session(true);
                true
            }
            "delete" if args.is_empty() => {
                self.request_delete_current_session();
                true
            }
            "delete" => {
                self.backend.status_message = Some(String::from("usage: /delete"));
                true
            }

            "resume" if args.is_empty() => {
                self.resume_paused_turn();
                true
            }
            "discard" if args.is_empty() => {
                self.discard_paused_turn();
                true
            }
            "reconnect" if args.is_empty() => {
                self.agents_reconnect();
                true
            }
            "next" if args.is_empty() => {
                self.agents_switch_thread(1);
                true
            }
            "prev" if args.is_empty() => {
                self.agents_switch_thread(-1);
                true
            }
            "clear" if args.is_empty() => {
                self.agents_clear_scrollback();
                true
            }
            "layout" => {
                if AgentPaneLayout::parse(args).is_some() {
                    self.agents_set_layout(args);
                } else {
                    self.backend.status_message =
                        Some(String::from("usage: /layout right|bottom|full"));
                }
                true
            }
            "thoughts" => {
                if matches!(args, "" | "toggle" | "on" | "off") {
                    self.agents_set_thought_visibility(args);
                } else {
                    self.backend.status_message =
                        Some(String::from("usage: /thoughts on|off|toggle"));
                }
                true
            }
            "config" => {
                let mut parts = args.splitn(2, char::is_whitespace);
                match (parts.next().unwrap_or_default(), parts.next().unwrap_or_default().trim()) {
                    ("", _) => self.agents_list_config_options(),
                    ("set", value)
                        if value
                            .split_once(char::is_whitespace)
                            .is_some_and(|(_, value)| !value.trim().is_empty()) =>
                    {
                        self.agents_set_config_option_command(value)
                    }
                    ("toggle", value)
                        if !value.is_empty() && !value.contains(char::is_whitespace) =>
                    {
                        self.agents_toggle_config_option_command(value)
                    }
                    _ => {
                        self.backend.status_message = Some(String::from(
                            "usage: /config [set <config_id> <value>|toggle <config_id>]",
                        ));
                    }
                }
                true
            }
            "mcp" => {
                if matches!(args, "" | "tools" | "prompts" | "resources" | "close") {
                    self.agents_mcp_command(args);
                } else {
                    self.backend.status_message =
                        Some(String::from("usage: /mcp [tools|prompts|resources|close]"));
                }
                true
            }
            "context" => {
                self.agents_context_command(args);
                true
            }
            "mention" if !args.is_empty() => {
                self.agents_mention_context_file(args);
                true
            }
            "mention" => {
                self.backend.status_message =
                    Some(String::from("usage: /mention <workspace-relative-path>"));
                true
            }
            "add-dir" if !args.is_empty() => {
                self.request_additional_workspace_directory(args);
                true
            }
            "add-dir" => {
                self.backend.status_message = Some(String::from("usage: /add-dir <path>"));
                true
            }
            "tasks" if args.is_empty() => {
                self.agents_list_owned_tasks();
                true
            }
            "ps" if args.is_empty() => {
                self.agents_list_owned_terminals();
                true
            }
            "stop" => {
                self.agents_stop_command(args);
                true
            }
            "steer" if !args.is_empty() => {
                self.agents_steer_command(args);
                true
            }
            "steer" => {
                self.backend.status_message = Some(String::from("usage: /steer <message>"));
                true
            }
            "queue" if queue_command_is_management(args) => {
                self.agents_queue_command(args);
                true
            }
            "queue" if !args.is_empty() => {
                self.agents_queue_prompt_command(args);
                true
            }
            "queue" => {
                self.agents_queue_command(args);
                true
            }
            "details" => {
                self.agents_set_transcript_detail(args);
                true
            }
            "transcript" => {
                self.agents_transcript_command(args);
                true
            }
            "draft" => {
                self.agents_draft_command(args);
                true
            }
            "keys" if args.is_empty() => {
                self.agents_show_key_help();
                true
            }
            "keys" => {
                self.backend.status_message = Some(String::from("usage: /keys"));
                true
            }
            command if PROVIDER_CONFIG_ALIASES.contains(&command) => {
                self.agents_set_provider_config_alias(command, args);
                true
            }
            command if PROVIDER_OWNED_SLASH_COMMANDS.contains(&command) => {
                self.agents_require_advertised_provider_command(command)
            }
            "approval" => {
                match args {
                    "" => match self.active_tool_approval_mode() {
                        Some(mode) => {
                            self.backend.status_message =
                                Some(format!("tool approvals: {}", mode.label()));
                        }
                        None => {
                            self.backend.status_message =
                                Some(String::from("no active agent session"))
                        }
                    },
                    "default" => self.set_active_tool_approval_mode(
                        super::agent_bridge::ToolApprovalMode::Default,
                    ),
                    "autopilot" => self.set_active_tool_approval_mode(
                        super::agent_bridge::ToolApprovalMode::Autopilot,
                    ),
                    "bypass" => self.request_bypass_tool_approvals(),
                    _ => {
                        self.backend.status_message =
                            Some(String::from("usage: /approval [default|autopilot|bypass]"));
                    }
                }
                true
            }
            _ => false,
        };
        if handled {
            self.agents_clear_draft();
        }
        handled
    }

    /// Submits the active thread's draft as a prompt turn.
    fn submit_prompt(&mut self) {
        if self.agents.permission.is_some()
            || self.agents.mode_selection.is_some()
            || self.agents.approval_mode_confirmation.is_some()
            || self.agents.elicitation.is_some()
            || !self.agents.approvals.is_empty()
        {
            return;
        }
        let active = self.agents.active_thread_index();
        let draft = active
            .map(|index| self.agents.threads[index].draft.clone())
            .unwrap_or_else(|| self.agents.pending_draft.clone());
        if draft.trim().is_empty() {
            return;
        }
        if self.submit_agents_exit_command(&draft) {
            return;
        }
        if self.submit_agents_local_slash_command(&draft) {
            return;
        }
        let Some(active) = active else {
            self.submit_without_session();
            return;
        };
        let (command, args) = split_slash_command(&draft);
        if let Some("mode") = command.as_deref() {
            self.agents.threads[active].draft.clear();
            self.agents_set_mode(active, &args);
            return;
        }
        let prompt_text = draft.trim_end().to_string();
        if self.agents.threads[active].state == ThreadUiState::Running {
            self.enqueue_agent_follow_up(active, prompt_text);
            return;
        }
        if !matches!(self.agents.threads[active].state, ThreadUiState::Ready) {
            self.agents.error = Some(match self.agents.threads[active].state {
                ThreadUiState::PausedRecoverable => {
                    String::from("a turn is paused and recoverable; use /resume or /discard")
                }
                _ => String::from("agent session is not ready; cannot send prompt"),
            });
            return;
        }
        self.send_ready_agent_prompt(active, prompt_text);
    }

    fn send_ready_agent_prompt(&mut self, active: usize, prompt_text: String) {
        let next_context = {
            let thread = &mut self.agents.threads[active];
            thread.draft.clear();
            thread.record_prompt_history(&prompt_text);
            std::mem::take(&mut thread.next_prompt_context_files)
        };
        self.send_agent_prompt(active, prompt_text, next_context);
    }

    fn enqueue_agent_follow_up(&mut self, active: usize, prompt_text: String) {
        let Some(queued_count) = self.queue_agent_prompt(active, prompt_text, false) else {
            return;
        };
        self.backend.status_message = Some(format!(
            "follow-up queued ({queued_count}/{AGENT_PROMPT_QUEUE_MAX}); /stop cancels current turn, /queue edits pending prompts"
        ));
    }

    fn queue_agent_prompt(
        &mut self,
        active: usize,
        prompt_text: String,
        priority: bool,
    ) -> Option<usize> {
        let thread = &mut self.agents.threads[active];
        if thread.queued_prompts.len() >= AGENT_PROMPT_QUEUE_MAX {
            self.backend.status_message = Some(format!(
                "queued follow-up limit reached ({AGENT_PROMPT_QUEUE_MAX}); edit or remove entries with /queue"
            ));
            return None;
        }
        let next_prompt_context_files = std::mem::take(&mut thread.next_prompt_context_files);
        thread.draft.clear();
        thread.record_prompt_history(&prompt_text);
        let prompt = QueuedPrompt { text: prompt_text, next_prompt_context_files };
        if priority {
            thread.queued_prompts.push_front(prompt);
        } else {
            thread.queued_prompts.push_back(prompt);
        }
        Some(thread.queued_prompts.len())
    }

    fn dispatch_next_queued_prompt(&mut self, active: usize) {
        let Some(queued) = self
            .agents
            .threads
            .get_mut(active)
            .and_then(|thread| thread.queued_prompts.pop_front())
        else {
            return;
        };
        let remaining = self.agents.threads[active].queued_prompts.len();
        self.send_agent_prompt(active, queued.text, queued.next_prompt_context_files);
        self.backend.status_message =
            Some(format!("dispatching queued follow-up ({remaining} remaining)"));
    }

    fn send_agent_prompt(
        &mut self,
        active: usize,
        prompt_text: String,
        next_prompt_context_files: Vec<AgentContextFile>,
    ) {
        let blocks = {
            let thread = &self.agents.threads[active];
            prompt_blocks_with_context(
                &prompt_text,
                &thread.context_files,
                &next_prompt_context_files,
            )
        };
        self.send_agent_prompt_blocks(active, prompt_text.clone(), blocks, Some(&prompt_text));
    }

    /// Sends a local workflow prompt without persisting its bounded evidence or rendering it verbatim.
    fn send_agent_prompt_blocks(
        &mut self,
        active: usize,
        display_text: String,
        blocks: Vec<ContentBlock>,
        persisted_prompt: Option<&str>,
    ) {
        if let Some(prompt) = persisted_prompt
            && let Some(thread) = self.agents.threads.get(active)
        {
            self.update_persisted_last_prompt(&thread.session_id, Some(prompt));
        }
        let thread_handle = {
            let thread = &mut self.agents.threads[active];
            thread.state = ThreadUiState::Running;
            thread.turn_started_at = Some(Instant::now());
            thread.active_response_group = None;
            thread.push_message("you", &display_text, MessageRenderKind::User, None, None);
            thread.optimistic_message = Some(thread.transcript.len().saturating_sub(1));
            thread.last_prompt = Some(blocks.clone());
            thread.host.clone()
        };
        let host = self.agents.host.as_ref().expect("host present");
        host.send_prompt(thread_handle, blocks);
    }

    /// Resumes the paused turn: re-sends the exact prompt blocks that started
    /// it, so the agent continues from its checkpoint.
    fn resume_paused_turn(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let Some(blocks) =
            self.agents.threads[active].pending_recovery.clone().map(|pending| pending.prompt)
        else {
            self.agents.error = Some(String::from("no paused turn to resume"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        thread.state = ThreadUiState::Running;
        thread.turn_started_at = Some(Instant::now());
        thread.push_system(String::from("resuming paused turn"));
        let host = self.agents.host.as_ref().expect("host present");
        host.resume_prompt(thread.host.clone(), blocks);
    }

    /// Discards the paused turn: tells the agent to drop its checkpoint and
    /// returns the thread to ready.
    fn discard_paused_turn(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        if thread.pending_recovery.is_none() {
            self.agents.error = Some(String::from("no paused turn to discard"));
            return;
        }
        thread.state = ThreadUiState::Running;
        thread.push_system(String::from("discarding paused turn"));
        let host = self.agents.host.as_ref().expect("host present");
        let blocks = vec![ContentBlock::Text(TextContent::new(String::from("/discard")))];
        host.send_prompt(thread.host.clone(), blocks);
    }

    fn submit_without_session(&mut self) {
        self.ensure_agents_host();
        let Some(agent_id) = self.default_agent_id() else {
            let message = String::from(
                "no agent configured; add [agents.servers.<id>] in .ee.toml, then run /new_thread",
            );
            self.agents.error = Some(message.clone());
            self.backend.status_message = Some(message);
            return;
        };
        if self.agents.pending_session.is_none() {
            self.start_session(agent_id);
            self.backend.status_message = Some(String::from(
                "starting agent session; prompt will send after session is ready",
            ));
        }
    }

    /// Confirms the selected permission option.
    fn confirm_permission(&mut self) {
        let Some(prompt) = &self.agents.permission else {
            return;
        };
        let thread_index = prompt.thread_index;
        let request_id = prompt.request_id;
        let Some(option) = prompt.options.get(prompt.selected).cloned() else {
            return;
        };
        let thread = self.agents.threads[thread_index].host.clone();
        let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            option.option_id.clone(),
        ));
        let resolved = thread.respond_permission(request_id, outcome);
        if let Some(thread) = self.agents.threads.get_mut(thread_index) {
            thread.push_system(format!(
                "approval: {} ({})",
                option.name,
                if resolved { "sent" } else { "stale" }
            ));
        }
        self.agents.permission = None;
    }

    /// Applies the selected local mode and closes the composer picker.
    fn confirm_mode_selection(&mut self) {
        let Some(prompt) = self.agents.mode_selection.take() else {
            return;
        };
        let Some(mode) = prompt.options.get(prompt.selected).cloned() else {
            self.agents.mode_selection = Some(prompt);
            return;
        };
        self.agents_set_mode(prompt.thread_index, &mode);
    }

    /// Resolves pending elicitation with accept, decline, or cancel semantics.
    fn confirm_elicitation(&mut self, action: ElicitationAction) {
        let Some(prompt) = self.agents.elicitation.take() else {
            return;
        };
        let thread_index = prompt.thread_index;
        let response = match action {
            ElicitationAction::Accept(_) => {
                if prompt.url.is_some() {
                    Ok(ClientRequestResponse::CreateElicitation(CreateElicitationResponse::new(
                        ElicitationAction::Accept(ElicitationAcceptAction::new()),
                    )))
                } else {
                    match prompt.content_map() {
                        Ok(content) => Ok(ClientRequestResponse::CreateElicitation(
                            CreateElicitationResponse::new(ElicitationAction::Accept(
                                ElicitationAcceptAction::new().content(content),
                            )),
                        )),
                        Err(error) => {
                            let message = format!("elicitation blocked locally: {error}");
                            self.agents.error = Some(message.clone());
                            self.backend.status_message = Some(message);
                            self.agents.elicitation = Some(prompt);
                            return;
                        }
                    }
                }
            }
            ElicitationAction::Decline => Ok(ClientRequestResponse::CreateElicitation(
                CreateElicitationResponse::new(ElicitationAction::Decline),
            )),
            ElicitationAction::Cancel => Ok(ClientRequestResponse::CreateElicitation(
                CreateElicitationResponse::new(ElicitationAction::Cancel),
            )),
            _ => Ok(ClientRequestResponse::CreateElicitation(CreateElicitationResponse::new(
                ElicitationAction::Cancel,
            ))),
        };
        let _ = prompt.reply.send(response);
        if let Some(thread_index) = thread_index
            && let Some(thread) = self.agents.threads.get_mut(thread_index)
        {
            let notice = match action {
                ElicitationAction::Accept(_) => "elicitation answered",
                ElicitationAction::Decline => "elicitation declined",
                ElicitationAction::Cancel => "elicitation cancelled",
                _ => "elicitation cancelled",
            };
            thread.push_system(notice);
        }
    }

    /// Appends text to the active thread's draft, or to the startup draft before a session exists.
    pub(super) fn agents_append_draft(&mut self, text: &str) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft.push_str(text);
        } else {
            self.agents.pending_draft.push_str(text);
        }
    }

    /// Dispatches explicit `mode = "agent"` keymap actions before built-in
    /// composer keys. Other editor actions never leak into Agents TUI.
    pub(super) fn handle_agent_keybinding_action(&mut self, action: crate::keymap::Action) -> bool {
        match action {
            crate::keymap::Action::AgentHistoryPrevious => self.agents_navigate_prompt_history(-1),
            crate::keymap::Action::AgentHistoryNext => self.agents_navigate_prompt_history(1),
            crate::keymap::Action::AgentHistorySearchReverse => {
                self.agents_reverse_prompt_history_search()
            }
            crate::keymap::Action::AgentDraftStash => self.agents_stash_draft(),
            crate::keymap::Action::AgentDraftRestore => self.agents_restore_draft(),
            crate::keymap::Action::AgentDraftExternalEdit => self.request_agent_external_editor(),
            crate::keymap::Action::AgentToggleTranscriptDetails => {
                self.agents_set_transcript_detail("toggle")
            }
            crate::keymap::Action::AgentToggleTranscriptRaw => {
                self.agents_transcript_command("toggle")
            }
            _ => return false,
        }
        true
    }

    /// Key handling while `Mode::Agent` is active.
    pub(super) fn handle_agent_key(&mut self, key: KeyEvent) {
        // MCP browse picker: ↑/↓ move, Enter inserts, Esc closes.
        if self.agents.mcp.browse.is_some() {
            match key.code {
                KeyCode::Up => {
                    self.agents_mcp_select(-1);
                    return;
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.agents_mcp_select(1);
                    return;
                }
                KeyCode::Enter => {
                    self.agents_mcp_confirm();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.mcp.browse = None;
                    self.backend.status_message = Some(String::from("mcp browse closed"));
                    return;
                }
                _ => return,
            }
        }

        // Plan modal: Esc closes the overlay without modifying transcript.
        if self
            .agents
            .active_thread_index()
            .and_then(|index| self.agents.threads.get(index))
            .is_some_and(|thread| thread.plan_modal_open)
            && key.code == KeyCode::Esc
        {
            if let Some(index) = self.agents.active_thread_index()
                && let Some(thread) = self.agents.threads.get_mut(index)
            {
                thread.plan_modal_open = false;
                self.backend.status_message = Some(String::from("plan closed"));
            }
            return;
        }

        // Local transcript deletion is irreversible in this process. Provider data remains untouched.
        if self.agents.session_deletion_confirmation.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_delete_current_session();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.session_deletion_confirmation = None;
                    self.backend.status_message =
                        Some(String::from("local session deletion cancelled"));
                    return;
                }
                _ => return,
            }
        }

        if self.agents.additional_directory_confirmation.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_additional_workspace_directory();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.additional_directory_confirmation = None;
                    self.backend.status_message =
                        Some(String::from("additional workspace root cancelled"));
                    return;
                }
                _ => return,
            }
        }

        if self.agents.terminal_stop_confirmation.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_stop_owned_terminals();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.terminal_stop_confirmation = None;
                    self.backend.status_message = Some(String::from("terminal stop cancelled"));
                    return;
                }
                _ => return,
            }
        }

        // Bypass mode needs an explicit confirmation because it removes approval
        // dialogs for every validated bridge tool call in this session.
        if self.agents.approval_mode_confirmation.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_bypass_tool_approvals();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.approval_mode_confirmation = None;
                    self.backend.status_message =
                        Some(String::from("bypass tool approvals cancelled"));
                    return;
                }
                _ => return,
            }
        }

        // Bridge approvals render above permissions in the composer. Up/down selects
        // a visible option row; left/right and tab remain aliases for compatibility.
        if self.agents.approvals.front().is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
                    self.move_approval_selection(-1);
                    return;
                }
                KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                    self.move_approval_selection(1);
                    return;
                }
                KeyCode::Enter => {
                    let choice = self
                        .agents
                        .approvals
                        .front()
                        .and_then(|prompt| prompt.options.get(prompt.selected).map(|(_, c)| *c))
                        .unwrap_or(super::agent_bridge::ApprovalChoice::AllowOnce);
                    self.confirm_bridge_approval(choice);
                    return;
                }
                KeyCode::Esc => {
                    self.confirm_bridge_approval(super::agent_bridge::ApprovalChoice::DenyOnce);
                    return;
                }
                _ => {}
            }
        }

        // Mode selection expands in the composer just like bridge approvals.
        if self.agents.mode_selection.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
                    self.move_mode_selection(-1);
                    return;
                }
                KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                    self.move_mode_selection(1);
                    return;
                }
                KeyCode::Enter => {
                    self.confirm_mode_selection();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.mode_selection = None;
                    return;
                }
                _ => return,
            }
        }

        // Permission selection: ←/→/Tab move, Enter confirms.
        if self.agents.permission.is_some() {
            match key.code {
                KeyCode::Left | KeyCode::Tab | KeyCode::BackTab => {
                    self.move_permission_selection(-1);
                    return;
                }
                KeyCode::Right => {
                    self.move_permission_selection(1);
                    return;
                }
                KeyCode::Enter => {
                    self.confirm_permission();
                    return;
                }
                KeyCode::Esc => {
                    self.return_to_editor();
                    return;
                }
                _ => {}
            }
        }

        // Elicitation widgets: ↑/↓ move fields, ←/→/Tab change values or URL choice,
        // Enter submits current choice, Ctrl-D declines, Esc cancels.
        if self.agents.elicitation.is_some() {
            match key.code {
                KeyCode::Up => {
                    if let Some(prompt) = &mut self.agents.elicitation {
                        prompt.selected_field = prompt.selected_field.saturating_sub(1);
                    }
                    return;
                }
                KeyCode::Down | KeyCode::Tab => {
                    if let Some(prompt) = &mut self.agents.elicitation {
                        if prompt.url.is_some() {
                            prompt.selected_choice = (prompt.selected_choice + 1) % 3;
                        } else {
                            let count = prompt.fields.len().max(1);
                            prompt.selected_field = (prompt.selected_field + 1) % count;
                        }
                    }
                    return;
                }
                KeyCode::BackTab => {
                    if let Some(prompt) = &mut self.agents.elicitation {
                        if prompt.url.is_some() {
                            prompt.selected_choice = (prompt.selected_choice + 2) % 3;
                        } else {
                            let count = prompt.fields.len().max(1);
                            prompt.selected_field = (prompt.selected_field + count - 1) % count;
                        }
                    }
                    return;
                }
                KeyCode::Left | KeyCode::Right => {
                    if let Some(prompt) = &mut self.agents.elicitation {
                        if prompt.url.is_some() {
                            let delta = if key.code == KeyCode::Left { 2 } else { 1 };
                            prompt.selected_choice = (prompt.selected_choice + delta) % 3;
                        } else {
                            prompt.step_elicitation_field(if key.code == KeyCode::Left {
                                -1
                            } else {
                                1
                            });
                        }
                    }
                    return;
                }
                KeyCode::Enter => {
                    let action = self
                        .agents
                        .elicitation
                        .as_ref()
                        .map(|prompt| {
                            if prompt.url.is_some() {
                                prompt.submit_action(true)
                            } else {
                                ElicitationAction::Accept(ElicitationAcceptAction::new())
                            }
                        })
                        .unwrap_or(ElicitationAction::Cancel);
                    self.confirm_elicitation(action);
                    return;
                }
                KeyCode::Esc => {
                    self.confirm_elicitation(ElicitationAction::Cancel);
                    return;
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.confirm_elicitation(ElicitationAction::Decline);
                    return;
                }
                KeyCode::Char(c) => {
                    self.agents_elicitation_type(c);
                    return;
                }
                KeyCode::Backspace => {
                    self.agents_elicitation_backspace();
                    return;
                }
                _ => return,
            }
        }

        match key.code {
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match c {
                        'n' => self.agents_switch_thread(1),
                        'p' => self.agents_switch_thread(-1),
                        't' => self.open_agents_thread_picker(),
                        'g' => self.agents_toggle_plan(),
                        'r' if key.modifiers.contains(KeyModifiers::SHIFT)
                            || self
                                .agents
                                .active_thread_index()
                                .and_then(|index| self.agents.threads.get(index))
                                .is_some_and(|thread| thread.selected_response_group.is_some()) =>
                        {
                            self.agents_toggle_selected_response_group()
                        }
                        'r' => self.agents_reverse_prompt_history_search(),
                        'e' if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            self.request_agent_external_editor()
                        }
                        'e' => self.agents_toggle_selected_tool_details(),
                        's' => self.agents_stash_draft(),
                        'o' => self.agents_restore_draft(),
                        'u' => self.agents_clear_draft(),
                        _ => {}
                    }
                } else if !key.modifiers.contains(KeyModifiers::ALT) {
                    self.agents_append_draft(&c.to_string());
                }
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.agents_select_response_group(-1);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.agents_select_response_group(1);
            }
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.agents_append_draft("\n");
                } else {
                    self.submit_prompt();
                }
            }
            KeyCode::Tab
                if !self.cycle_slash_command(1) && !self.agents_complete_mention_path() =>
            {
                self.agents_append_draft("\t");
            }
            KeyCode::BackTab => {
                let _ = self.cycle_slash_command(-1);
            }
            KeyCode::Backspace => self.agents_draft_backspace(),
            KeyCode::Up => self.agents_navigate_prompt_history(-1),
            KeyCode::Down => self.agents_navigate_prompt_history(1),
            KeyCode::Esc => {}
            KeyCode::PageUp => self.agents_scroll(-(AGENTS_SCROLL_PAGE as isize)),
            KeyCode::PageDown => self.agents_scroll(AGENTS_SCROLL_PAGE as isize),
            KeyCode::Home => self.agents_scroll_to(0),
            KeyCode::End => self.agents_scroll_to_bottom(),
            _ => {}
        }
    }

    /// Toggles the active thread's plan modal (Ctrl-G).
    fn agents_toggle_plan(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            if !thread.current_plan.is_empty() {
                thread.plan_modal_open = !thread.plan_modal_open;
            }
        }
    }

    /// Selects a response group containing reasoning or tool calls.
    fn agents_select_response_group(&mut self, delta: isize) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let groups = thread.response_group_ids();
        let Some(current) = thread.selected_response_group else {
            thread.selected_response_group = groups.last().copied();
            return;
        };
        let Some(index) = groups.iter().position(|group| *group == current) else {
            thread.selected_response_group = groups.last().copied();
            return;
        };
        thread.selected_response_group =
            Some(groups[(index as isize + delta).rem_euclid(groups.len() as isize) as usize]);
    }

    /// Toggles reasoning and tool visibility for the selected response group.
    fn agents_toggle_selected_response_group(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let Some(group) = thread.selected_response_group else {
            return;
        };
        if !thread.response_group_ids().contains(&group) {
            return;
        }
        if !thread.collapsed_response_groups.insert(group) {
            thread.collapsed_response_groups.remove(&group);
        }
    }

    /// Toggles tool input/output detail for the selected response group (Ctrl-E).
    fn agents_toggle_selected_tool_details(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let Some(group) = thread.selected_response_group else {
            return;
        };
        if !thread.response_group_ids().contains(&group) {
            return;
        }
        if !thread.expanded_tool_details.insert(group) {
            thread.expanded_tool_details.remove(&group);
        }
    }

    /// Returns active transcript's maximum visual-row offset for current terminal layout.
    fn agents_transcript_scroll_max(&self) -> usize {
        let fallback = self
            .agents
            .active_thread_index()
            .map(|active| self.agents.threads[active].transcript.len().saturating_sub(1))
            .unwrap_or(0);
        let Ok((width, height)) = crossterm::terminal::size() else {
            return fallback;
        };
        let area = Rect { x: 0, y: 0, width, height };
        let Some(pane) = crate::ui::agents_pane_rect_for(area, self) else {
            return fallback;
        };
        crate::ui::agents_transcript_scroll_max(self, pane)
    }

    /// Moves the local mode selection by `delta`.
    fn move_mode_selection(&mut self, delta: isize) {
        if let Some(prompt) = &mut self.agents.mode_selection {
            let count = prompt.options.len().max(1) as isize;
            prompt.selected = (prompt.selected as isize + delta).rem_euclid(count) as usize;
        }
    }

    /// Moves the permission option selection by `delta`.
    fn move_permission_selection(&mut self, delta: isize) {
        if let Some(prompt) = &mut self.agents.permission {
            let count = prompt.options.len().max(1) as isize;
            prompt.selected = (prompt.selected as isize + delta).rem_euclid(count) as usize;
        }
    }

    /// Moves the front approval option selection by `delta`.
    fn move_approval_selection(&mut self, delta: isize) {
        if let Some(prompt) = self.agents.approvals.front_mut() {
            let count = prompt.options.len().max(1) as isize;
            prompt.selected = (prompt.selected as isize + delta).rem_euclid(count) as usize;
        }
    }

    /// Scrolls the active transcript by visual rows (`delta`: negative = up).
    pub(super) fn agents_scroll(&mut self, delta: isize) {
        let max_scroll = self.agents_transcript_scroll_max();
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].scroll_by(delta, max_scroll);
        }
    }

    /// Jumps to a fixed visual-row offset.
    fn agents_scroll_to(&mut self, offset: usize) {
        let max_scroll = self.agents_transcript_scroll_max();
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].scroll_to(offset, max_scroll);
        }
    }

    /// Pins transcript to newest rendered row.
    fn agents_scroll_to_bottom(&mut self) {
        let max_scroll = self.agents_transcript_scroll_max();
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].scroll_to(max_scroll, max_scroll);
        }
    }

    /// Clears the composer draft (Ctrl-U).
    fn agents_clear_draft(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.draft.clear();
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
        } else {
            self.agents.pending_draft.clear();
        }
    }

    fn agents_navigate_prompt_history(&mut self, delta: isize) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        if thread.prompt_history.is_empty() {
            self.backend.status_message = Some(String::from("prompt history is empty"));
            return;
        }
        let next = match thread.prompt_history_cursor {
            None if delta < 0 => {
                thread.prompt_history_restore_draft = Some(thread.draft.clone());
                Some(thread.prompt_history.len() - 1)
            }
            None => None,
            Some(current) => {
                let candidate = current as isize + delta;
                if candidate < 0 {
                    Some(0)
                } else if candidate >= thread.prompt_history.len() as isize {
                    thread.draft = thread.prompt_history_restore_draft.take().unwrap_or_default();
                    None
                } else {
                    Some(candidate as usize)
                }
            }
        };
        thread.prompt_history_cursor = next;
        if let Some(index) = next {
            thread.draft = thread.prompt_history[index].clone();
        }
    }

    fn agents_reverse_prompt_history_search(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let query = thread.draft.clone();
        let upper_bound = thread.prompt_history_cursor.unwrap_or(thread.prompt_history.len());
        let found = thread.prompt_history[..upper_bound]
            .iter()
            .rposition(|entry| query.is_empty() || entry.contains(&query));
        match found {
            Some(index) => {
                if thread.prompt_history_cursor.is_none() {
                    thread.prompt_history_restore_draft = Some(query);
                }
                thread.prompt_history_cursor = Some(index);
                thread.draft = thread.prompt_history[index].clone();
                self.backend.status_message =
                    Some(format!("history search: {}/{}", index + 1, thread.prompt_history.len()));
            }
            None => {
                self.backend.status_message = Some(String::from("no earlier prompt-history match"))
            }
        }
    }

    /// Completes one trailing `@workspace/path` token when exactly one safe
    /// workspace file matches. Completion never reads or attaches file content.
    fn agents_complete_mention_path(&mut self) -> bool {
        const MENTION_COMPLETION_SCAN_MAX: usize = 1_024;
        let draft = self
            .agents
            .active_thread_index()
            .and_then(|active| self.agents.threads.get(active).map(|thread| thread.draft.clone()))
            .unwrap_or_else(|| self.agents.pending_draft.clone());
        let token_start =
            draft.rfind(char::is_whitespace).map_or(0, |index| index.saturating_add(1));
        let Some(partial) = draft.get(token_start..).and_then(|token| token.strip_prefix('@'))
        else {
            return false;
        };
        if partial.is_empty() || partial.contains(['\\', '\n', '\r']) {
            return false;
        }
        let Ok(root) = std::fs::canonicalize(&self.working_dir) else {
            return false;
        };
        let mut matches = Vec::new();
        for entry in WalkBuilder::new(&root)
            .max_depth(Some(12))
            .standard_filters(true)
            .build()
            .filter_map(Result::ok)
            .take(MENTION_COMPLETION_SCAN_MAX)
        {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&root) else {
                continue;
            };
            let relative = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if relative.is_empty()
                || !relative.starts_with(partial)
                || is_protected_relative_path(&relative)
                || self.is_secret_store_path(entry.path())
            {
                continue;
            }
            matches.push(relative);
            if matches.len() > 1 {
                break;
            }
        }
        let Some(completed) = (matches.len() == 1).then(|| matches.remove(0)) else {
            if matches.len() > 1 {
                self.backend.status_message = Some(String::from("mention completion is ambiguous"));
            }
            return false;
        };
        let replacement = format!("@{completed}");
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].draft.replace_range(token_start.., &replacement);
        } else {
            self.agents.pending_draft.replace_range(token_start.., &replacement);
        }
        self.backend.status_message = Some(format!("mention path completed: {completed}"));
        true
    }

    fn agents_draft_backspace(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft.pop();
        } else {
            self.agents.pending_draft.pop();
        }
    }

    fn agents_elicitation_type(&mut self, c: char) {
        if let Some(prompt) = &mut self.agents.elicitation
            && let Some(field) = prompt.fields.get_mut(prompt.selected_field)
        {
            match &mut field.value {
                ElicitationFieldValue::Text(text) if c != '\n' => {
                    text.push(c);
                }
                ElicitationFieldValue::Number(text)
                    if c.is_ascii_digit() || c == '.' || c == '-' =>
                {
                    text.push(c);
                }
                _ => {}
            }
        }
    }

    fn agents_elicitation_backspace(&mut self) {
        if let Some(prompt) = &mut self.agents.elicitation
            && let Some(field) = prompt.fields.get_mut(prompt.selected_field)
        {
            match &mut field.value {
                ElicitationFieldValue::Text(text) | ElicitationFieldValue::Number(text) => {
                    text.pop();
                }
                _ => {}
            }
        }
    }

    /// Returns focus to the previous editor mode without closing the pane.
    fn return_to_editor(&mut self) {
        if self.agents_focused() {
            self.mode = self.agents.previous_editor_mode.take().unwrap_or(Mode::Normal);
        }
    }
}

impl ElicitationPrompt {
    /// Steps the selected field's value by `delta` (enum/boolean cycling).
    fn step_elicitation_field(&mut self, delta: isize) {
        let Some(field) = self.fields.get_mut(self.selected_field) else {
            return;
        };
        match &mut field.value {
            ElicitationFieldValue::Boolean(value) => *value = !*value,
            ElicitationFieldValue::Enum(selected, options) => {
                let count = options.len().max(1) as isize;
                *selected = (*selected as isize + delta).rem_euclid(count) as usize;
            }
            ElicitationFieldValue::Text(_) | ElicitationFieldValue::Number(_) => {
                if self.fields.len() > 1 {
                    // Left/Right on a text field moves to the adjacent field.
                    let count = self.fields.len() as isize;
                    self.selected_field =
                        (self.selected_field as isize + delta).rem_euclid(count) as usize;
                }
            }
        }
    }
}

// ── Unit tests (pure helpers) ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_markers_match_issue_contract() {
        assert_eq!(plan_entry_marker(PlanEntryStatus::Pending), '-');
        assert_eq!(plan_entry_marker(PlanEntryStatus::InProgress), '>');
        assert_eq!(plan_entry_marker(PlanEntryStatus::Completed), 'x');
    }

    #[test]
    fn layout_parses_explicit_values_only() {
        assert_eq!(AgentPaneLayout::parse("right"), Some(AgentPaneLayout::Right));
        assert_eq!(AgentPaneLayout::parse("bottom"), Some(AgentPaneLayout::Bottom));
        assert_eq!(AgentPaneLayout::parse("full"), Some(AgentPaneLayout::Full));
        assert_eq!(AgentPaneLayout::parse("left"), None);
    }

    #[test]
    fn queue_management_forms_do_not_consume_prompt_messages() {
        assert!(queue_command_is_management(""));
        assert!(queue_command_is_management("list"));
        assert!(queue_command_is_management("edit 1 revised prompt"));
        assert!(queue_command_is_management("move 2 1"));
        assert!(queue_command_is_management("remove 1"));
        assert!(queue_command_is_management("clear"));
        assert!(!queue_command_is_management("review changed files"));
    }

    fn compact_command() -> AvailableCommand {
        AvailableCommand::new("compact", "Summarize the session history").input(
            ee_agent_protocol::AvailableCommandInput::Unstructured(
                ee_agent_protocol::UnstructuredCommandInput::new("optional instructions"),
            ),
        )
    }

    #[test]
    fn split_slash_command_parses_only_leading_slashes() {
        assert_eq!(split_slash_command("hello world"), (None, String::new()));
        assert_eq!(split_slash_command(""), (None, String::new()));
        assert_eq!(split_slash_command("/compact"), (Some(String::from("compact")), String::new()));
        assert_eq!(
            split_slash_command("/compact focus on auth"),
            (Some(String::from("compact")), String::from("focus on auth"))
        );
        assert_eq!(
            split_slash_command("/compactness"),
            (Some(String::from("compactness")), String::new())
        );
    }

    #[test]
    fn available_commands_summary_includes_descriptions() {
        let commands = vec![compact_command(), AvailableCommand::new("plan", "Create a plan")];
        let summary = available_commands_summary(&commands);
        assert!(summary.contains("/compact — Summarize the session history"), "{summary}");
        assert!(summary.contains("/plan — Create a plan"), "{summary}");
        assert_eq!(available_commands_summary(&[]), "");
        assert_eq!(available_commands_summary(&[AvailableCommand::new("bare", "")]), "/bare");
    }

    #[test]
    fn slash_command_draft_inserts_only_the_command_name() {
        assert_eq!(slash_command_draft("compact", ""), "/compact");
        assert_eq!(slash_command_draft("compact", "  keep API v2 "), "/compact keep API v2 ");
    }

    #[test]
    fn agent_slash_command_names_include_local_and_advertised_commands() {
        assert_eq!(
            agent_slash_command_names(&[compact_command(), AvailableCommand::new("quit", "")]),
            vec![
                "quit",
                "q",
                "quit_full",
                "qf",
                "help",
                "status",
                "doctor",
                "init",
                "review",
                "security-review",
                "diff",
                "copy",
                "rename",
                "new",
                "new_thread",
                "archive",
                "delete",
                "fork",
                "branch",
                "sessions",
                "export",
                "stop",
                "steer",
                "resume",
                "discard",
                "reconnect",
                "next",
                "prev",
                "clear",
                "layout",
                "thoughts",
                "config",
                "mcp",
                "approval",
                "context",
                "mention",
                "add-dir",
                "tasks",
                "ps",
                "mode",
                "queue",
                "details",
                "transcript",
                "draft",
                "keys",
                "compact",
            ]
        );
    }

    #[test]
    fn local_slash_command_aliases_are_recognized() {
        assert!(is_agents_quit_slash_command("/q"));
        assert!(is_agents_quit_slash_command("/quit"));
        assert!(is_agents_quit_full_slash_command("/qf"));
        assert!(is_agents_quit_full_slash_command("/quit_full"));
        assert!(!is_agents_quit_full_slash_command("/qf now"));
    }
}
