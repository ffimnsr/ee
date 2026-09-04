//! Thread transcript model: `AgentThreadUi`, `TranscriptItem`, queued prompts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use ee_agent_host::events::{RecoverableInfo, TurnMetrics};
use ee_agent_host::{AgentThread, EvidenceRevision, TurnEvidenceSummary};
use ee_agent_protocol::{AvailableCommand, ContentBlock};

use super::constants::{AGENT_PROMPT_HISTORY_MAX, AGENTS_STDERR_MAX, AGENTS_TRANSCRIPT_MAX};

// ── Transcript model ─────────────────────────────────────────────────────────

/// How a chat message line is attributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum MessageRenderKind {
    User,
    Assistant,
    Thought,
}

/// Stable local identifier for reasoning and tool calls from one agent turn.
pub(crate) type ResponseGroupId = u64;

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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    Queued,
    Running,
    AwaitingPermission,
    AwaitingElicitation,
    Cancelling,
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
    pub(super) fn push_message(
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
    pub(super) fn push_tool_call(
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

    pub(super) fn ensure_response_group(&mut self) -> ResponseGroupId {
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
    pub(super) fn finish_response_group(&mut self) -> Option<ResponseGroupId> {
        let group = self.active_response_group.take();
        if let Some(group) = group {
            self.collapsed_response_groups.insert(group);
        }
        group
    }

    /// Records the metrics of the just-finished turn against its response
    /// group, then collapses the group.
    pub(super) fn record_turn_metrics(&mut self, metrics: TurnMetrics) {
        if let Some(group) = self.finish_response_group() {
            self.turn_metrics.insert(group, metrics.clone());
        }
        self.last_turn_metrics = Some(metrics);
    }

    pub(super) fn response_group_for_tool_call(&mut self, tool_call_id: &str) -> ResponseGroupId {
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
    pub(super) fn replace_plan(&mut self, entries: Vec<(String, char)>) {
        self.current_plan = entries;
    }

    /// Appends a system notice.
    pub(crate) fn push_system(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptItem::System { text: text.into(), at: SystemTime::now() });
        self.trim_transcript();
    }

    /// Appends a stderr/debug line (bounded).
    pub(super) fn push_stderr(&mut self, line: impl Into<String>) {
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

    pub(super) fn clear_transcript_state(&mut self) {
        self.transcript.clear();
        self.optimistic_message = None;
        self.scroll = 0;
        self.stick_to_bottom = true;
        self.active_response_group = None;
        self.next_response_group = 1;
        self.selected_response_group = None;
        self.collapsed_response_groups.clear();
        self.expanded_tool_details.clear();
        self.turn_metrics.clear();
        self.last_turn_metrics = None;
    }

    pub(super) fn record_prompt_history(&mut self, prompt: &str) {
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
