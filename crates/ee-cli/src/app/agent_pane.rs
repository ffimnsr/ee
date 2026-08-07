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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ee_agent_host::events::{
    AgentConnectionState, PermissionRequestInfo, ThreadCloseReason, TurnMetrics,
};
use ee_agent_host::{
    AgentError, AgentEvent, AgentManager, AgentManagerConfig, AgentThread, ClientRequestResponse,
    ClientRequestResult, PermissionRequestId, ToolCallState,
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
use tokio::runtime::Builder as TokioBuilder;
use tokio::sync::mpsc as tokio_mpsc;
use url::Url;

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
    /// A bridge approval request (file write / terminal create).
    Approval { title: String, detail: String, options: Vec<String>, at: SystemTime },
    /// `-!-` system notice.
    System(String),
    /// `-!-` stderr/debug line.
    Stderr(String),
}

/// Thread lifecycle state shown in the channel list and footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadUiState {
    Starting,
    Ready,
    Running,
    Closed,
    Failed,
}

/// One open agent session thread (IRC channel equivalent).
#[derive(Debug)]
pub(crate) struct AgentThreadUi {
    /// Stable per-pane channel number (used by tests and channel rendering).
    #[allow(dead_code)]
    pub(crate) index: usize,
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
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
    /// Scroll offset from the top of the transcript.
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
    /// Active local group for the currently streaming agent turn.
    pub(crate) active_response_group: Option<ResponseGroupId>,
    /// Next response-group identifier for this thread.
    pub(crate) next_response_group: ResponseGroupId,
    /// Response group selected for keyboard collapse control.
    pub(crate) selected_response_group: Option<ResponseGroupId>,
    /// Response groups whose reasoning and tools are hidden.
    pub(crate) collapsed_response_groups: BTreeSet<ResponseGroupId>,
    /// Latest agent plan snapshot, rendered in a modal instead of scrollback.
    pub(crate) current_plan: Vec<(String, char)>,
    /// Whether the user has opened the current plan modal.
    pub(crate) plan_modal_open: bool,
    /// Last turn error, when any.
    pub(crate) last_error: Option<String>,
    /// Slash commands currently advertised by the agent.
    pub(crate) available_commands: Vec<AvailableCommand>,
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
                TranscriptItem::System(text) => Some(text.clone()),
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
        self.transcript.push(TranscriptItem::System(text.into()));
        self.trim_transcript();
    }

    /// Appends a stderr/debug line (bounded).
    fn push_stderr(&mut self, line: impl Into<String>) {
        self.transcript.push(TranscriptItem::Stderr(line.into()));
        let mut stderr_count =
            self.transcript.iter().filter(|i| matches!(i, TranscriptItem::Stderr(_))).count();
        let mut index = 0;
        while stderr_count > AGENTS_STDERR_MAX {
            if matches!(self.transcript[index], TranscriptItem::Stderr(_)) {
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
    SendPrompt {
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

    /// Enqueues a prompt turn (fire-and-forget; events carry the outcome).
    fn send_prompt(&self, thread: AgentThread, blocks: Vec<ContentBlock>) {
        let _ = self.commands.send(HostCommand::SendPrompt { thread, blocks });
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
    pub(crate) reply: std_mpsc::Receiver<Result<AgentThread, String>>,
}

/// All agents-pane UI state; `Default` is the closed, inert startup state.
pub(crate) struct AgentPaneState {
    pub(crate) layout: AgentPaneLayout,
    pub(crate) threads: Vec<AgentThreadUi>,
    pub(crate) active_thread: Option<usize>,
    pub(crate) next_thread_index: usize,
    /// Whether streamed `agent_thought_chunk` messages are shown in transcript.
    pub(crate) show_thoughts: bool,
    pub(crate) pending_session: Option<PendingSession>,
    /// Composer text typed before a session exists or while session startup fails.
    pub(crate) pending_draft: String,
    pub(crate) pending_cancel: Option<std_mpsc::Receiver<Result<(), String>>>,
    pub(crate) pending_thread_action: Option<std_mpsc::Receiver<Result<String, String>>>,
    pub(crate) permission: Option<PermissionPrompt>,
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
    pub(crate) bridge_tx: std_mpsc::Sender<super::agent_bridge::BridgeUiMessage>,
    pub(crate) bridge_rx: std_mpsc::Receiver<super::agent_bridge::BridgeUiMessage>,
    /// Shared agent terminal registry (spawned here, queried by the host).
    pub(crate) terminals: super::agent_bridge::AgentTerminals,
    /// Recorded agent file operations (future checkpoint/restore source).
    pub(crate) action_log: Vec<super::agent_bridge::ActionLogEntry>,
    /// Session-scoped approval policy (Phase 7).
    pub(crate) approval_policy: super::agent_bridge::ApprovalPolicy,
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
}

impl Default for AgentPaneState {
    fn default() -> Self {
        let (bridge_tx, bridge_rx) = std_mpsc::channel();
        Self {
            layout: AgentPaneLayout::Closed,
            threads: Vec::new(),
            active_thread: None,
            next_thread_index: 0,
            show_thoughts: true,
            pending_session: None,
            pending_draft: String::new(),
            pending_cancel: None,
            pending_thread_action: None,
            permission: None,
            elicitation: None,
            approvals: VecDeque::new(),
            error: None,
            previous_editor_mode: None,
            host: None,
            created_sessions: BTreeSet::new(),
            bridge_tx,
            bridge_rx,
            terminals: super::agent_bridge::AgentTerminals::default(),
            action_log: Vec::new(),
            approval_policy: super::agent_bridge::ApprovalPolicy::default(),
            usage_ledger: crate::policy::UsageLedger::default(),
            mcp: super::agents_mcp::McpPaneState::default(),
            resolved_secret_values: Vec::new(),
            #[cfg(test)]
            test_fake_transports: BTreeMap::new(),
            #[cfg(test)]
            test_secret_store: None,
            #[cfg(test)]
            test_trust_store_base: None,
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
                            thread.state = ThreadUiState::Ready;
                            if let Some(info) = agent_info.as_ref() {
                                let label = info.title.as_deref().unwrap_or(&info.name);
                                if !label.is_empty() {
                                    thread.nick = label.to_string();
                                }
                            }
                            thread.display_name = thread_display_name(
                                thread.index,
                                &thread.agent_id,
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
            AgentEvent::TurnStarted { session_id } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Running;
                    self.agents.threads[index].active_response_group = None;
                    self.agents.threads[index].turn_started_at = Some(Instant::now());
                    self.agents.threads[index].push_system(String::from("turn started"));
                    self.notify_unread(index);
                }
            }
            AgentEvent::SessionUpdate { session_id, update } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.apply_session_update(index, &update);
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnCompleted { session_id, stop_reason, metrics } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].optimistic_message = None;
                    self.agents.threads[index].turn_started_at = None;
                    self.agents.threads[index].stop_reason = Some(format!("{stop_reason:?}"));
                    self.agents.threads[index].record_turn_metrics(metrics);
                    self.agents.threads[index]
                        .push_system(format!("turn completed (stop: {stop_reason:?})"));
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnCancelled { session_id, metrics } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].optimistic_message = None;
                    self.agents.threads[index].turn_started_at = None;
                    self.agents.threads[index].record_turn_metrics(metrics);
                    self.agents.threads[index].push_system(String::from("turn cancelled"));
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnFailed { session_id, error, metrics } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].optimistic_message = None;
                    self.agents.threads[index].turn_started_at = None;
                    self.agents.threads[index].record_turn_metrics(metrics);
                    self.agents.threads[index].last_error = Some(error.to_string());
                    self.agents.threads[index].push_system(format!("turn failed: {error}"));
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
        self.agents.threads.push(AgentThreadUi {
            index,
            agent_id: agent_id.to_string(),
            session_id: session_id.clone(),
            nick,
            display_name: thread_display_name(index, agent_id, session_title.as_deref()),
            state: if ready { ThreadUiState::Ready } else { ThreadUiState::Starting },
            unread: 0,
            activity: false,
            host: thread,
            transcript: Vec::new(),
            optimistic_message: None,
            draft: std::mem::take(&mut self.agents.pending_draft),
            scroll: 0,
            stick_to_bottom: true,
            usage: None,
            stop_reason: None,
            turn_started_at: None,
            turn_metrics: BTreeMap::new(),
            last_turn_metrics: None,
            active_response_group: None,
            next_response_group: 1,
            selected_response_group: None,
            collapsed_response_groups: BTreeSet::new(),
            current_plan: Vec::new(),
            plan_modal_open: false,
            last_error: None,
            available_commands: snapshot.available_commands,
            session_title,
            session_updated_at,
        });
        self.agents.active_thread = Some(self.agents.threads.len() - 1);
        self.agents.error = None;
        self.agents
            .threads
            .last_mut()
            .expect("thread pushed")
            .push_system(format!("session started ({session_id})"));
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
        thread.display_name =
            thread_display_name(thread.index, &thread.agent_id, thread.session_title.as_deref());
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
                self.register_session_thread(&pending.agent_id, thread);
            }
            Ok(Err(message)) => {
                self.agents.pending_session = None;
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
                self.agents.error = Some(String::from("agent host stopped"));
            }
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

fn thread_display_name(index: usize, agent_id: &str, session_title: Option<&str>) -> String {
    match session_title.filter(|title| !title.trim().is_empty()) {
        Some(title) => format!("{}.{}", index + 1, title),
        None => format!("{}.{}", index + 1, agent_id),
    }
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

fn cycle_select_value(
    options: &SessionConfigSelectOptions,
    current: &SessionConfigValueId,
    delta: isize,
) -> Option<SessionConfigValueId> {
    let values = match options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            options.iter().map(|option| option.value.clone()).collect::<Vec<_>>()
        }
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(|option| option.value.clone()))
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
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

const LOCAL_AGENT_SLASH_COMMANDS: &[&str] = &["quit", "quit_full", "new_thread"];

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
    matches!(name.as_deref(), Some("quit_full")) && rest.trim().is_empty()
}

fn is_agents_new_slash_command(draft: &str) -> bool {
    let (name, rest) = split_slash_command(draft);
    matches!(name.as_deref(), Some("new_thread")) && rest.trim().is_empty()
}

/// Deterministic display-width wrapping used by the transcript renderer.
///
/// Breaks at whitespace when possible and hard-breaks overlong words; the
/// output never exceeds `width` display columns.
pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let paragraph = paragraph.strip_suffix('\r').unwrap_or(paragraph);
        let mut wrapped = wrap_text_paragraph(paragraph, width);
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            lines.append(&mut wrapped);
        }
    }
    lines
}

fn wrap_text_paragraph(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = unicode_width::UnicodeWidthStr::width(word);
        if current_width + 1 + word_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if word_width > width {
            // Hard-break an overlong word across multiple lines.
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            let mut rest = word;
            while unicode_width::UnicodeWidthStr::width(rest) > width {
                let mut cut = width;
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                lines.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            if !rest.is_empty() {
                current = rest.to_string();
                current_width = unicode_width::UnicodeWidthStr::width(rest);
            }
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
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
        if self.agents.active_thread.is_none() && self.agents.pending_session.is_none() {
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
            self.backend.status_message = Some(if self.agents.active_thread.is_some() {
                "agents pane opened".to_string()
            } else {
                "agents pane opened (starting session…)".to_string()
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
        self.backend.status_message = Some(String::from("agents scrollback cleared"));
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
        self.agents.approval_policy = super::agent_bridge::ApprovalPolicy::default();
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
        config.ee_proxy_enabled = self.config.mcp.proxy.enabled;
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

    /// Builds the secrets store used for launch-time reference resolution.
    /// Tests inject a fake store; production uses the real default.
    fn build_agents_secret_store(&mut self) -> Option<crate::secrets::SecretStore> {
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
        self.agents.pending_session = Some(PendingSession { agent_id, reply });
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
        if thread.available_commands.is_empty() {
            return false;
        }
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
            command_names
                .iter()
                .enumerate()
                .filter_map(|(index, name)| name.starts_with(command_name).then_some(index))
                .collect()
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

    /// Submits the active thread's draft as a prompt turn.
    fn submit_prompt(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.submit_without_session();
            return;
        };
        if self.agents.permission.is_some()
            || self.agents.elicitation.is_some()
            || !self.agents.approvals.is_empty()
        {
            return;
        }
        let draft = self.agents.threads[active].draft.clone();
        if draft.trim().is_empty() {
            return;
        }
        if is_agents_quit_slash_command(&draft) {
            self.agents.threads[active].draft.clear();
            self.close_agents_pane();
            return;
        }
        if is_agents_quit_full_slash_command(&draft) {
            self.agents.threads[active].draft.clear();
            self.should_quit = true;
            return;
        }
        if is_agents_new_slash_command(&draft) {
            self.agents.threads[active].draft.clear();
            self.agents_new_session();
            return;
        }
        let thread = &mut self.agents.threads[active];
        if thread.state == ThreadUiState::Running {
            self.agents.error = Some(String::from("a turn is already running"));
            return;
        }
        if !matches!(thread.state, ThreadUiState::Ready) {
            self.agents.error =
                Some(String::from("agent session is not ready; cannot send prompt"));
            return;
        }
        thread.draft.clear();
        let prompt_text = draft.trim_end().to_string();
        thread.push_message("you", &prompt_text, MessageRenderKind::User, None, None);
        thread.optimistic_message = Some(thread.transcript.len().saturating_sub(1));
        let host = self.agents.host.as_ref().expect("host present");
        let blocks = vec![ContentBlock::Text(TextContent::new(prompt_text))];
        host.send_prompt(thread.host.clone(), blocks);
    }

    fn submit_without_session(&mut self) {
        self.ensure_agents_host();
        let Some(agent_id) = self.default_agent_id() else {
            let message = String::from(
                "no agent configured; add [agents.servers.<id>] in .ee.toml, then run :agents_new",
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
            self.agents.threads[active].draft.push_str(text);
        } else {
            self.agents.pending_draft.push_str(text);
        }
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

        // Bridge approvals render above permissions in the composer; keep key priority identical.
        // ←/→/Tab move, Enter allows with the selected policy choice, Esc denies once.
        if self.agents.approvals.front().is_some() {
            match key.code {
                KeyCode::Left | KeyCode::Tab | KeyCode::BackTab => {
                    self.move_approval_selection(-1);
                    return;
                }
                KeyCode::Right => {
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
                        'r' => self.agents_toggle_selected_response_group(),
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
            KeyCode::Tab => {
                if !self.cycle_slash_command(1) {
                    self.agents_append_draft("\t");
                }
            }
            KeyCode::BackTab => {
                let _ = self.cycle_slash_command(-1);
            }
            KeyCode::Backspace => self.agents_draft_backspace(),
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

    /// Scrolls the active transcript by `delta` lines (negative = up).
    pub(super) fn agents_scroll(&mut self, delta: isize) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let max = thread.transcript.len().saturating_sub(1);
        thread.scroll = (thread.scroll as isize + delta).clamp(0, max as isize) as usize;
        thread.stick_to_bottom = thread.scroll == max || thread.transcript.is_empty();
    }

    /// Jumps to a fixed transcript offset.
    fn agents_scroll_to(&mut self, offset: usize) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.scroll = offset.min(thread.transcript.len().saturating_sub(1));
            thread.stick_to_bottom = false;
        }
    }

    /// Pins the transcript to the newest line.
    fn agents_scroll_to_bottom(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.scroll = thread.transcript.len().saturating_sub(1);
            thread.stick_to_bottom = true;
        }
    }

    /// Clears the composer draft (Ctrl-U).
    fn agents_clear_draft(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].draft.clear();
        } else {
            self.agents.pending_draft.clear();
        }
    }

    fn agents_draft_backspace(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
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
                ElicitationFieldValue::Text(text) => {
                    if c != '\n' {
                        text.push(c);
                    }
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
    fn wrap_text_breaks_at_whitespace_and_hard_breaks_long_words() {
        let lines = wrap_text("alpha beta gamma delta", 10);
        assert_eq!(lines, vec!["alpha beta", "gamma", "delta"]);

        let long = wrap_text("supercalifragilistic", 8);
        assert!(long.iter().all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 8));
        assert_eq!(long.join(""), "supercalifragilistic");
    }

    #[test]
    fn wrap_text_handles_narrow_widths() {
        let lines = wrap_text("ab cd", 4);
        assert_eq!(lines, vec!["ab", "cd"]);
        assert!(!wrap_text("x", 4).is_empty());
    }

    #[test]
    fn wrap_text_preserves_explicit_newlines() {
        let lines = wrap_text("alpha beta\ngamma\n\ndelta", 20);
        assert_eq!(lines, vec!["alpha beta", "gamma", "", "delta"]);
    }

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
        assert_eq!(AgentPaneLayout::parse(""), None);
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
            vec!["quit", "quit_full", "new_thread", "compact"]
        );
    }
}
