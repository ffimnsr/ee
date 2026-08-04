//! Irssi-style agents pane: thread/channel list, transcript scrollback,
//! status footer, and composer input (Phase 3).
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
use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ee_agent_host::events::{AgentConnectionState, PermissionRequestInfo, ThreadCloseReason};
use ee_agent_host::{
    AgentError, AgentEvent, AgentManager, AgentManagerConfig, AgentThread, ClientRequestResponse,
    ClientRequestResult, PermissionRequestId,
};
use ee_agent_protocol::{
    ContentBlock, CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationContentValue, ElicitationMode, ElicitationPropertySchema,
    ElicitationSchema, McpServer, McpServerStdio, PermissionOption, PlanEntryStatus,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionId, SessionUpdate, TextContent,
    ToolCallStatus,
};
use tokio::runtime::Builder as TokioBuilder;
use tokio::sync::mpsc as tokio_mpsc;

use super::*;

// ── Pane geometry constants ──────────────────────────────────────────────────

/// Width of the IRC channel/thread list column.
pub(crate) const AGENTS_CHANNEL_COL_WIDTH: u16 = 12;
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

/// One line in the IRC-style scrollback.
#[derive(Debug, Clone)]
pub(crate) enum TranscriptItem {
    /// A chat message with a nick column.
    Message {
        nick: String,
        text: String,
        kind: MessageRenderKind,
        message_id: Option<String>,
        at: SystemTime,
    },
    /// IRC-style tool notice (`* title [status]`).
    ToolCall { id: String, title: String, status: String, detail: String, at: SystemTime },
    /// Plan block (replaced wholesale by `plan` updates).
    Plan { entries: Vec<(String, char)>, at: SystemTime },
    /// A permission request shown in the transcript.
    Permission { title: String, options: Vec<String>, at: SystemTime },
    /// An elicitation request shown in the transcript.
    Elicitation { message: String, url: Option<String>, at: SystemTime },
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

impl ThreadUiState {
    pub(crate) fn marker(self) -> char {
        match self {
            Self::Starting => '?',
            Self::Ready => ' ',
            Self::Running => '*',
            Self::Closed => '·',
            Self::Failed => '!',
        }
    }
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
    /// Last turn error, when any.
    pub(crate) last_error: Option<String>,
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
        self.transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Plan { entries, .. } => Some(entries.clone()),
                _ => None,
            })
            .flatten()
            .collect()
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
    ) {
        if kind == MessageRenderKind::User
            && let Some(index) = self.optimistic_message.take()
            && let Some(TranscriptItem::Message { text: target, .. }) =
                self.transcript.get_mut(index)
        {
            *target = text.to_string();
            return;
        }
        let merges = self.transcript.iter_mut().rev().find(|item| {
            matches!(
                item,
                TranscriptItem::Message { kind: existing_kind, message_id: existing_id, .. }
                    if *existing_kind == kind && *existing_id == message_id
            )
        });
        if let Some(TranscriptItem::Message { text: target, .. }) = merges {
            target.push_str(text);
            return;
        }
        self.transcript.push(TranscriptItem::Message {
            nick: nick.to_string(),
            text: text.to_string(),
            kind,
            message_id,
            at: SystemTime::now(),
        });
        self.trim_transcript();
    }

    /// Upserts a tool call notice by id.
    fn push_tool_call(&mut self, id: &str, title: &str, status: &str, detail: &str) {
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
            at: SystemTime::now(),
        });
        self.trim_transcript();
    }

    /// Replaces the plan block wholesale (ACP plans are complete snapshots).
    fn replace_plan(&mut self, entries: Vec<(String, char)>) {
        let target = self
            .transcript
            .iter_mut()
            .rev()
            .find(|item| matches!(item, TranscriptItem::Plan { .. }));
        if let Some(item) = target {
            *item = TranscriptItem::Plan { entries, at: SystemTime::now() };
            return;
        }
        self.transcript.push(TranscriptItem::Plan { entries, at: SystemTime::now() });
        self.trim_transcript();
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
    pub(crate) message: String,
    pub(crate) url: Option<String>,
    pub(crate) fields: Vec<ElicitationFieldUi>,
    pub(crate) selected_field: usize,
    /// Selected choice for URL elicitations (0 = accept/open, 1 = decline).
    pub(crate) selected_choice: usize,
    /// Response channel; answering resolves the agent's pending request.
    pub(crate) reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    /// Visible rejection reason for unsupported schema shapes.
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
            if field.unsupported.is_some() {
                unsupported.push(field.label());
            }
            fields.push(field);
        }
        if over_cap {
            unsupported.push(format!("too many fields (> {} )", Self::MAX_ELICITATION_FIELDS));
        }
        Self {
            thread_index,
            message,
            url: None,
            fields,
            selected_field: 0,
            selected_choice: 0,
            reply,
            unsupported_reason: if unsupported.is_empty() {
                None
            } else {
                Some(format!(
                    "unsupported form fields (decline to cancel): {}",
                    unsupported.join(", ")
                ))
            },
        }
    }

    /// Builds a prompt from an ACP URL-mode request.
    fn from_url(
        thread_index: Option<usize>,
        message: String,
        url: String,
        reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        Self {
            thread_index,
            message,
            url: Some(url),
            fields: Vec::new(),
            selected_field: 0,
            selected_choice: 0,
            reply,
            unsupported_reason: None,
        }
    }

    /// Builds the response content map for the current field values.
    fn content_map(&self) -> Option<BTreeMap<String, ElicitationContentValue>> {
        let mut content = BTreeMap::new();
        for field in &self.fields {
            if field.unsupported.is_some() {
                return None;
            }
            if field.required && field.value.is_empty() {
                return None;
            }
            content.insert(field.name.clone(), field.value.to_content()?);
        }
        Some(content)
    }
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
    pub(crate) pending_session: Option<PendingSession>,
    pub(crate) pending_cancel: Option<std_mpsc::Receiver<Result<(), String>>>,
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
    /// Phase 6 MCP state: health registry, browsing, and the proxy listener.
    pub(crate) mcp: super::agents_mcp::McpPaneState,
    /// Test-only: agent id → fake transport factory (see `tests/agent_pane.rs`).
    #[cfg(test)]
    pub(crate) test_fake_transports: BTreeMap<String, Arc<dyn ee_agent_host::FakeTransportFactory>>,
}

impl Default for AgentPaneState {
    fn default() -> Self {
        let (bridge_tx, bridge_rx) = std_mpsc::channel();
        Self {
            layout: AgentPaneLayout::Closed,
            threads: Vec::new(),
            active_thread: None,
            next_thread_index: 0,
            pending_session: None,
            pending_cancel: None,
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
            mcp: super::agents_mcp::McpPaneState::default(),
            #[cfg(test)]
            test_fake_transports: BTreeMap::new(),
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
                            if let Some(info) = agent_info.as_ref()
                                && !info.name.is_empty()
                            {
                                thread.nick = info.name.clone();
                            }
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
                // Session-scoped approval policy dies with the session.
                self.agents.approval_policy.invalidate_session(session_id.0.as_ref());
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    let text = match reason {
                        ThreadCloseReason::HostClosed => String::from("session closed"),
                        ThreadCloseReason::ConnectionLost => String::from("connection lost"),
                    };
                    self.agents.threads[index].state = ThreadUiState::Closed;
                    self.agents.threads[index].push_system(text);
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnStarted { session_id } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Running;
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
            AgentEvent::TurnCompleted { session_id, stop_reason } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].stop_reason = Some(format!("{stop_reason:?}"));
                    self.agents.threads[index]
                        .push_system(format!("turn completed (stop: {stop_reason:?})"));
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnCancelled { session_id } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].push_system(String::from("turn cancelled"));
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnFailed { session_id, error } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Ready;
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
    pub(crate) fn agents_secret_values(&self) -> Vec<String> {
        let mut secrets = Vec::new();
        for server in self.config.agents.servers.values() {
            for (name, value) in &server.env {
                if ee_agent_host::redact::is_secret_key(name) {
                    secrets.push(value.clone());
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
                );
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = content_block_text(&chunk.content);
                let message_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
                self.agents.threads[thread_index].push_message(
                    &nick,
                    &text,
                    MessageRenderKind::Assistant,
                    message_id,
                );
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = content_block_text(&chunk.content);
                let message_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
                self.agents.threads[thread_index].push_message(
                    "thought",
                    &text,
                    MessageRenderKind::Thought,
                    message_id,
                );
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.agents.threads[thread_index].push_tool_call(
                    &tool_call.tool_call_id.0,
                    &tool_call.title,
                    &tool_call_status_label(tool_call.status),
                    &tool_call_detail(tool_call.raw_input.as_ref()),
                );
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let fields = &update.fields;
                let title = fields.title.clone().unwrap_or_default();
                let status = fields.status.map(tool_call_status_label).unwrap_or_default();
                let detail = fields
                    .raw_input
                    .as_ref()
                    .map(|raw| tool_call_detail(Some(raw)))
                    .unwrap_or_default();
                self.agents.threads[thread_index].push_tool_call(
                    &update.tool_call_id.0,
                    &title,
                    &status,
                    &detail,
                );
            }
            SessionUpdate::Plan(plan) => {
                let entries = plan
                    .entries
                    .iter()
                    .map(|entry| (entry.content.clone(), plan_entry_marker(entry.status.clone())))
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
                self.agents.threads[thread_index]
                    .push_system(format!("mode: {}", mode.current_mode_id.0));
            }
            SessionUpdate::AvailableCommandsUpdate(_)
            | SessionUpdate::ConfigOptionUpdate(_)
            | SessionUpdate::SessionInfoUpdate(_) => {
                // Stored in the host snapshot; nothing new to render.
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
        let prompt = match &request.mode {
            ElicitationMode::Form(mode) => ElicitationPrompt::from_form(
                thread_index,
                request.message.clone(),
                &mode.requested_schema,
                reply,
            ),
            ElicitationMode::Url(mode) => ElicitationPrompt::from_url(
                thread_index,
                request.message.clone(),
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
        let message = prompt.message.clone();
        if let Some(thread_index) = thread_index {
            self.agents.threads[thread_index].transcript.push(TranscriptItem::Elicitation {
                message: message.clone(),
                url: url.clone(),
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

    /// Registers a fresh session thread from a new-session reply.
    fn register_session_thread(&mut self, agent_id: &str, thread: AgentThread) {
        let session_id = thread.session_id().0.to_string();
        // Phase 6b user-visible diagnostics: how the ee proxy was exposed.
        self.agents.mcp.proxy_mode = Some(thread.proxy_mode().to_string());
        let index = self.agents.next_thread_index;
        self.agents.next_thread_index += 1;
        let nick = agent_id.to_string();
        let ready = self.agents.created_sessions.contains(&session_id);
        self.agents.threads.push(AgentThreadUi {
            index,
            agent_id: agent_id.to_string(),
            session_id: session_id.clone(),
            nick,
            display_name: format!("{}.{}", index + 1, agent_id),
            state: if ready { ThreadUiState::Ready } else { ThreadUiState::Starting },
            unread: 0,
            activity: false,
            host: thread,
            transcript: Vec::new(),
            optimistic_message: None,
            draft: String::new(),
            scroll: 0,
            stick_to_bottom: true,
            usage: None,
            stop_reason: None,
            last_error: None,
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
        ToolCallStatus::InProgress => String::from("running"),
        ToolCallStatus::Completed => String::from("done"),
        ToolCallStatus::Failed => String::from("failed"),
        // Non-exhaustive upstream.
        _ => String::from("?"),
    }
}

fn tool_call_detail(raw: Option<&serde_json::Value>) -> String {
    match raw {
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| String::from("{}")),
        None => String::new(),
    }
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

/// Deterministic display-width wrapping used by the transcript renderer.
///
/// Breaks at whitespace when possible and hard-breaks overlong words; the
/// output never exceeds `width` display columns.
pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
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
            "agents_next" => {
                self.agents_switch_thread(1);
                true
            }
            "agents_prev" => {
                self.agents_switch_thread(-1);
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
            self.agents.layout = AgentPaneLayout::Right;
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
        if self.agents.layout == AgentPaneLayout::Closed {
            self.agents.layout = AgentPaneLayout::Right;
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

    /// `:agents_next` / `:agents_prev` — switch threads like IRC channels.
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
    }

    /// Focuses thread `index`, resetting its unread state.
    fn focus_thread(&mut self, index: usize) {
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
    fn ensure_agents_host(&mut self) {
        if self.agents.host.is_some() {
            return;
        }
        let mut config = AgentManagerConfig::default();
        for (id, server) in &self.config.agents.servers {
            config.agents.insert(
                id.clone(),
                ee_agent_host::AgentProcessConfig {
                    command: server.command.clone(),
                    args: server.args.clone(),
                    env: server.env.clone(),
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

    /// Submits the active thread's draft as a prompt turn.
    fn submit_prompt(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
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
        thread.push_message("you", &prompt_text, MessageRenderKind::User, None);
        thread.optimistic_message = Some(thread.transcript.len().saturating_sub(1));
        let host = self.agents.host.as_ref().expect("host present");
        let blocks = vec![ContentBlock::Text(TextContent::new(prompt_text))];
        host.send_prompt(thread.host.clone(), blocks);
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

    /// Submits or declines the pending elicitation.
    fn confirm_elicitation(&mut self, accept: bool) {
        let Some(prompt) = self.agents.elicitation.take() else {
            return;
        };
        let thread_index = prompt.thread_index;
        let response = if accept {
            match prompt.content_map() {
                Some(content) => {
                    let action = ElicitationAcceptAction::new().content(content);
                    Ok(ClientRequestResponse::CreateElicitation(CreateElicitationResponse::new(
                        ElicitationAction::Accept(action),
                    )))
                }
                None => {
                    self.agents.error = Some(String::from(
                        "elicitation form is incomplete or unsupported; declined",
                    ));
                    Err(AgentError::invalid_params("elicitation declined by user"))
                }
            }
        } else {
            Err(AgentError::invalid_params("elicitation declined by user"))
        };
        let _ = prompt.reply.send(response);
        if let Some(thread_index) = thread_index
            && let Some(thread) = self.agents.threads.get_mut(thread_index)
        {
            thread.push_system(if accept {
                "elicitation answered"
            } else {
                "elicitation declined"
            });
        }
    }

    /// Appends text to the active thread's draft.
    pub(super) fn agents_append_draft(&mut self, text: &str) {
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].draft.push_str(text);
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

        // Bridge approvals (file write / terminal create): ←/→/Tab move,
        // Enter allows with the selected policy choice, Esc denies once.
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

        // Elicitation widgets: ↑/↓ move fields, ←/→/Tab change values,
        // Enter submits, Esc declines.
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
                        let count = prompt.fields.len().max(1);
                        prompt.selected_field = (prompt.selected_field + 1) % count;
                    }
                    return;
                }
                KeyCode::BackTab => {
                    if let Some(prompt) = &mut self.agents.elicitation {
                        let count = prompt.fields.len().max(1);
                        prompt.selected_field = (prompt.selected_field + count - 1) % count;
                    }
                    return;
                }
                KeyCode::Left | KeyCode::Right => {
                    if let Some(prompt) = &mut self.agents.elicitation {
                        if prompt.url.is_some() {
                            // URL elicitation: open/decline choice.
                            prompt.selected_choice = (prompt.selected_choice + 1) % 2;
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
                    self.confirm_elicitation(true);
                    return;
                }
                KeyCode::Esc => {
                    self.confirm_elicitation(false);
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
            KeyCode::Char(':') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                // `:` opens the ex command line from the agents pane, like in
                // normal mode (so `:agents_stop` etc. work while focused).
                self.command_mode_origin = Some(self.mode);
                self.command_buffer.clear();
                self.mode = Mode::CommandLine;
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match c {
                        'n' => self.agents_switch_thread(1),
                        'p' => self.agents_switch_thread(-1),
                        'u' => self.agents_clear_draft(),
                        _ => {}
                    }
                } else if !key.modifiers.contains(KeyModifiers::ALT) {
                    self.agents_append_draft(&c.to_string());
                }
            }
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.agents_append_draft("\n");
                } else {
                    self.submit_prompt();
                }
            }
            KeyCode::Backspace => self.agents_draft_backspace(),
            KeyCode::Esc => {
                let running = self
                    .agents
                    .active_thread_index()
                    .and_then(|index| self.agents.threads.get(index))
                    .is_some_and(|thread| thread.state == ThreadUiState::Running);
                if running {
                    self.agents_stop_turn();
                } else {
                    self.return_to_editor();
                }
            }
            KeyCode::PageUp => self.agents_scroll(-(AGENTS_SCROLL_PAGE as isize)),
            KeyCode::PageDown => self.agents_scroll(AGENTS_SCROLL_PAGE as isize),
            KeyCode::Home => self.agents_scroll_to(0),
            KeyCode::End => self.agents_scroll_to_bottom(),
            _ => {}
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
        }
    }

    fn agents_draft_backspace(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.draft.pop();
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
}
