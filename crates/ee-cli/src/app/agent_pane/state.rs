//! Pane state model: `AgentPaneState`, layout, prompt structs, session lifecycle types.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Instant;

use ee_agent_host::{AgentThread, ExternalCritiqueOutcome, PermissionRequestId};
use ee_agent_protocol::{ContentBlock, PermissionOption, SessionUpdate};
use tokio::sync::watch;

use super::super::*;

use super::elicitation::ElicitationPrompt;
use super::host::AgentHostBridge;
use super::thread_ui::{AgentThreadUi, ExternalEditorRequest, TranscriptItem};

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
    pub(super) fn parse(arg: &str) -> Option<Self> {
        match arg {
            "right" => Some(Self::Right),
            "bottom" => Some(Self::Bottom),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

// ── Permission prompt ────────────────────────────────────────────────────────

/// A pending `session/request_permission` awaiting an explicit choice.
#[derive(Debug)]
pub(crate) struct PermissionPrompt {
    pub(crate) session_id: String,
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
    pub(crate) agent_id: String,
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

// ── Pane state ───────────────────────────────────────────────────────────────

pub(super) type SessionLifecycleResult = Result<Result<AgentThread, String>, String>;

pub(super) struct PendingExternalCritic {
    pub(super) root_session_id: String,
    pub(super) requested_revision: String,
    pub(super) context_limit: usize,
    pub(super) started_at: Instant,
    pub(super) cancel: watch::Sender<bool>,
    pub(super) reply: std_mpsc::Receiver<ExternalCritiqueOutcome>,
}

/// Stable identity for one create/load/resume operation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SessionLifecycleKey {
    pub(crate) agent_id: String,
    /// Known up front for reconnects; `None` for fresh `session/new`.
    pub(crate) session_id: Option<String>,
    pub(crate) operation_id: u64,
}

/// A session lifecycle operation in flight (polled by [`App::pump_agents`]).
#[derive(Debug)]
pub(crate) struct PendingSession {
    pub(crate) reply: std_mpsc::Receiver<Result<AgentThread, String>>,
    pub(super) fork: Option<PendingFork>,
}

/// One fresh ACP session seeded from redacted visible parent messages.
#[derive(Debug)]
pub(super) struct PendingFork {
    pub(super) parent_session_id: String,
    pub(super) seed: Vec<ContentBlock>,
    pub(super) activate_child: bool,
}

/// One client-persisted session within a workspace thread list.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PersistedAgentSession {
    /// Agent server id the session belongs to.
    pub(super) agent_id: String,
    /// Session id as returned by `session/new` (stable across restarts for
    /// durable recovery checkpoints).
    pub(super) session_id: String,
    /// The last submitted prompt text, kept while a turn is recoverable so
    /// the resend path works after a restart.
    pub(super) last_prompt: Option<String>,
    /// User-selected local session name. Absent in records written before Phase 1.
    #[serde(default)]
    pub(super) session_name: Option<String>,
    /// Bounded local transcript fallback. `session/load` replay replaces it when
    /// the agent supplies conversation updates; otherwise reconnect keeps it.
    #[serde(default)]
    pub(super) transcript: Vec<TranscriptItem>,
}

/// Ordered client-side session registry for one canonical workspace.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct PersistedAgentWorkspace {
    /// Versioned separately from editor session state so agent metadata can
    /// evolve without invalidating buffer/tab restoration.
    #[serde(default = "persisted_agent_workspace_version")]
    pub(super) version: u32,
    /// Session selected when this workspace was last closed.
    #[serde(default)]
    pub(super) active_session_id: Option<String>,
    /// Open, non-archived session threads in display order.
    #[serde(default)]
    pub(super) sessions: Vec<PersistedAgentSession>,
}

pub(super) const fn persisted_agent_workspace_version() -> u32 {
    2
}

/// Transitional read format for the former one-session-per-workspace record.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub(super) enum PersistedAgentWorkspaceDocument {
    Legacy(PersistedAgentSession),
    Workspace(PersistedAgentWorkspace),
}

impl PersistedAgentWorkspaceDocument {
    pub(super) fn into_workspace(self) -> PersistedAgentWorkspace {
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
pub(super) struct WorkspaceRestore {
    pub(super) active_session_id: Option<String>,
    pub(super) sessions: VecDeque<PersistedAgentSession>,
    pub(super) order: BTreeMap<String, usize>,
    pub(super) in_flight: BTreeSet<String>,
    pub(super) failed: bool,
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
    /// In-flight create/load/resume operations keyed by connection, session,
    /// and stable local operation id.
    pub(crate) pending_sessions: BTreeMap<SessionLifecycleKey, PendingSession>,
    next_lifecycle_operation_id: u64,
    /// Workspace threads waiting for bounded parallel ACP session restoration.
    pub(super) workspace_restore: Option<WorkspaceRestore>,
    /// Composer text typed before a session exists or while session startup fails.
    pub(crate) pending_draft: String,
    /// External editor request consumed only by terminal-owning main loop.
    pub(super) pending_external_editor: Option<ExternalEditorRequest>,
    /// In-flight cancellation replies, keyed by originating session id.
    pub(crate) pending_cancels: BTreeMap<String, std_mpsc::Receiver<Result<(), String>>>,

    pub(crate) pending_thread_action: Option<std_mpsc::Receiver<Result<String, String>>>,
    pub(super) pending_external_critic: Option<PendingExternalCritic>,
    pub(super) rubber_duck_calls: BTreeMap<String, usize>,
    /// Pending permission requests, FIFO within each session.
    pub(crate) permissions: BTreeMap<String, VecDeque<PermissionPrompt>>,
    pub(crate) mode_selection: Option<ModeSelectionPrompt>,
    pub(crate) approval_mode_confirmation: Option<ApprovalModeConfirmation>,
    pub(crate) session_deletion_confirmation: Option<SessionDeletionConfirmation>,
    pub(crate) additional_directory_confirmation: Option<AdditionalDirectoryConfirmation>,
    pub(crate) terminal_stop_confirmation: Option<TerminalStopConfirmation>,
    /// Explicit user-approved extra roots. Session-local and never persisted.
    pub(crate) additional_workspace_roots: BTreeSet<PathBuf>,
    /// Pending elicitation requests, FIFO within each session. Empty-string key
    /// represents connection-scoped requests without ACP session identity.
    pub(crate) elicitations: BTreeMap<String, VecDeque<ElicitationPrompt>>,
    /// Bridge approval queue (file writes, terminal creates); the front one
    /// is shown and answered first.
    pub(crate) approvals: VecDeque<crate::app::agent_bridge::ApprovalPrompt>,
    /// Canonical write scopes held from pre-approval until mutation resolves.
    pub(crate) write_leases: crate::app::write_leases::WriteLeaseCoordinator,
    pub(crate) next_write_turn_id: u64,
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
    pub(crate) bridge_tx: std_mpsc::Sender<crate::app::agent_bridge::BridgeUiMessage>,
    pub(crate) bridge_rx: std_mpsc::Receiver<crate::app::agent_bridge::BridgeUiMessage>,
    /// Shared agent terminal registry (spawned here, queried by the host).
    pub(crate) terminals: crate::app::agent_bridge::AgentTerminals,
    /// Recorded agent file operations (future checkpoint/restore source).
    pub(crate) action_log: Vec<crate::app::agent_bridge::ActionLogEntry>,
    /// Session-scoped approval policy (Phase 7).
    pub(crate) approval_policy: crate::app::agent_bridge::ApprovalPolicy,
    /// Lazy service instance. Its bounded cache dies with this pane/session scope.
    pub(crate) web_context_service:
        Option<Arc<ee_agent_host::WebContextService<ee_agent_host::ReqwestWebTransport>>>,
    /// Trusted semantic config used to build `web_context_service`; a mismatch
    /// discards cached remote text and session grants before rebuilding.
    pub(crate) web_context_config_fingerprint: Option<String>,
    /// Session-local approval-dialog behavior. Entries die with their session
    /// and never persist to workspace or user configuration.
    pub(crate) approval_modes: BTreeMap<String, crate::app::agent_bridge::ToolApprovalMode>,
    /// Session-local successful-use counters for persistent rules; rows die
    /// with the session (Phase 2 command trust).
    pub(crate) usage_ledger: crate::policy::UsageLedger,
    /// Effective host-local policy snapshot. Disk changes activate only through
    /// initial load or explicit reload/persistence paths.
    pub(crate) trust_policy: std::cell::RefCell<Option<crate::policy::TrustStoreDocument>>,
    /// Phase 6 MCP state: health registry, browsing, and the proxy listener.
    pub(crate) mcp: crate::app::agents_mcp::McpPaneState,
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
            pending_sessions: BTreeMap::new(),
            next_lifecycle_operation_id: 0,
            workspace_restore: None,
            pending_draft: String::new(),
            pending_external_editor: None,
            pending_cancels: BTreeMap::new(),

            pending_thread_action: None,
            pending_external_critic: None,
            rubber_duck_calls: BTreeMap::new(),
            permissions: BTreeMap::new(),
            mode_selection: None,
            approval_mode_confirmation: None,
            session_deletion_confirmation: None,
            additional_directory_confirmation: None,
            terminal_stop_confirmation: None,
            additional_workspace_roots: BTreeSet::new(),
            elicitations: BTreeMap::new(),
            approvals: VecDeque::new(),
            write_leases: crate::app::write_leases::WriteLeaseCoordinator::default(),
            next_write_turn_id: 0,
            error: None,
            previous_editor_mode: None,
            host: None,
            created_sessions: BTreeSet::new(),
            pending_replay: HashMap::new(),
            bridge_tx,
            bridge_rx,
            terminals: crate::app::agent_bridge::AgentTerminals::default(),
            action_log: Vec::new(),
            approval_policy: crate::app::agent_bridge::ApprovalPolicy::default(),
            web_context_service: None,
            web_context_config_fingerprint: None,
            approval_modes: BTreeMap::new(),
            usage_ledger: crate::policy::UsageLedger::default(),
            trust_policy: std::cell::RefCell::new(None),
            mcp: crate::app::agents_mcp::McpPaneState::default(),
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

impl AgentPaneState {
    pub(super) const CONNECTION_SCOPED_INTERACTION_KEY: &'static str = "";

    pub(super) fn next_lifecycle_key(
        &mut self,
        agent_id: String,
        session_id: Option<String>,
    ) -> SessionLifecycleKey {
        let operation_id = self.next_lifecycle_operation_id;
        self.next_lifecycle_operation_id = self.next_lifecycle_operation_id.wrapping_add(1);
        SessionLifecycleKey { agent_id, session_id, operation_id }
    }

    /// Index of the active thread, if any.
    pub(crate) fn active_thread_index(&self) -> Option<usize> {
        self.active_thread
    }

    pub(crate) fn thread_index(&self, session_id: &str) -> Option<usize> {
        self.threads.iter().position(|thread| thread.session_id == session_id)
    }

    pub(super) fn active_session_id(&self) -> Option<&str> {
        self.active_thread_index()
            .and_then(|index| self.threads.get(index))
            .map(|thread| thread.session_id.as_str())
    }

    pub(crate) fn permission(&self) -> Option<&PermissionPrompt> {
        self.active_session_id()
            .and_then(|session_id| self.permissions.get(session_id))
            .and_then(VecDeque::front)
    }

    pub(super) fn permission_mut(&mut self) -> Option<&mut PermissionPrompt> {
        let session_id = self.active_session_id()?.to_string();
        self.permissions.get_mut(&session_id).and_then(VecDeque::front_mut)
    }

    pub(super) fn take_permission(&mut self) -> Option<PermissionPrompt> {
        let session_id = self.active_session_id()?.to_string();
        let prompt = self.permissions.get_mut(&session_id)?.pop_front();
        if self.permissions.get(&session_id).is_some_and(VecDeque::is_empty) {
            self.permissions.remove(&session_id);
        }
        prompt
    }

    pub(super) fn remove_permission(&mut self, session_id: &str, request_id: PermissionRequestId) {
        let Some(queue) = self.permissions.get_mut(session_id) else {
            return;
        };
        queue.retain(|prompt| prompt.request_id != request_id);
        if queue.is_empty() {
            self.permissions.remove(session_id);
        }
    }

    fn visible_elicitation_key(&self) -> Option<String> {
        if let Some(session_id) = self.active_session_id()
            && self.elicitations.get(session_id).is_some_and(|queue| !queue.is_empty())
        {
            return Some(session_id.to_string());
        }
        self.elicitations
            .get(Self::CONNECTION_SCOPED_INTERACTION_KEY)
            .filter(|queue| !queue.is_empty())
            .map(|_| Self::CONNECTION_SCOPED_INTERACTION_KEY.to_string())
    }

    pub(crate) fn elicitation(&self) -> Option<&ElicitationPrompt> {
        let key = self.visible_elicitation_key()?;
        self.elicitations.get(&key).and_then(VecDeque::front)
    }

    pub(super) fn elicitation_mut(&mut self) -> Option<&mut ElicitationPrompt> {
        let key = self.visible_elicitation_key()?;
        self.elicitations.get_mut(&key).and_then(VecDeque::front_mut)
    }

    pub(super) fn take_elicitation(&mut self) -> Option<ElicitationPrompt> {
        let key = self.visible_elicitation_key()?;
        let prompt = self.elicitations.get_mut(&key)?.pop_front();
        if self.elicitations.get(&key).is_some_and(VecDeque::is_empty) {
            self.elicitations.remove(&key);
        }
        prompt
    }

    pub(super) fn clear_session_interactions(&mut self, session_id: &str) {
        self.permissions.remove(session_id);
        self.elicitations.remove(session_id);
        self.pending_cancels.remove(session_id);
        self.rubber_duck_calls.remove(session_id);
        if self
            .pending_external_critic
            .as_ref()
            .is_some_and(|pending| pending.root_session_id == session_id)
            && let Some(pending) = self.pending_external_critic.take()
        {
            let _ = pending.cancel.send(true);
        }
        self.approvals.retain(|prompt| prompt.session_id != session_id);
        self.write_leases.release_session(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_parses_explicit_values_only() {
        assert_eq!(AgentPaneLayout::parse("right"), Some(AgentPaneLayout::Right));
        assert_eq!(AgentPaneLayout::parse("bottom"), Some(AgentPaneLayout::Bottom));
        assert_eq!(AgentPaneLayout::parse("full"), Some(AgentPaneLayout::Full));
        assert_eq!(AgentPaneLayout::parse("left"), None);
    }
}
