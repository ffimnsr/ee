//! `impl App`: agent event handling, session updates, thread registration.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Instant, SystemTime};

use ee_agent_host::events::{AgentConnectionState, PermissionRequestInfo, ThreadCloseReason};
use ee_agent_host::{
    AgentError, AgentEvent, AgentThread, ClientRequestResponse, ClientRequestResult,
};
use ee_agent_protocol::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationMode, RequestPermissionOutcome, SessionId, SessionUpdate,
};

use super::super::*;
use super::pump;

use super::constants::AGENT_PROMPT_HISTORY_MAX;
use super::elicitation::ElicitationPrompt;
use super::format::{
    content_block_text, plan_entry_marker, plan_entry_priority_label, thread_display_name,
    tool_call_detail_from_state, tool_call_status_label,
};
use super::state::{AgentPaneLayout, AgentPaneState, PermissionPrompt};
use super::thread_ui::{
    AgentThreadUi, MessageRenderKind, PendingRecovery, ThreadUiState, TranscriptItem,
};

// ── App integration ──────────────────────────────────────────────────────────

pub(super) fn session_update_replays_transcript(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
    )
}

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

    /// Applies one host event to the pane state.
    pub(super) fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::ConnectionStateChanged { agent_id, state } => {
                if matches!(
                    state,
                    AgentConnectionState::Failed(_) | AgentConnectionState::Closed(_)
                ) {
                    self.agents.write_leases.release_connection(&agent_id);
                }
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
                            thread.push_system(format!(
                                "agent `{agent_id}` connection failed: {error}"
                            ));
                        }
                        AgentConnectionState::Closed(reason) => {
                            thread.state = ThreadUiState::Closed;
                            thread.push_system(format!(
                                "agent `{agent_id}` connection closed ({reason:?})"
                            ));
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
                self.agents.clear_session_interactions(session_id.0.as_ref());
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
                    self.agents.threads[index].state =
                        pump::state_after_turn_activity(self.agents.threads[index].state);
                    self.agents.threads[index].verification_paths.clear();
                    self.agents.threads[index].verification_revision = None;
                    self.agents.threads[index].active_response_group = None;
                    self.agents.threads[index].turn_started_at = Some(Instant::now());
                    self.agents.threads[index].push_system(String::from("turn started"));
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnQueued { session_id, position } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Queued;
                    self.agents.threads[index]
                        .push_system(format!("provider prompt queued (position {position})"));
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnDispatched { session_id } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state =
                        pump::state_after_turn_activity(self.agents.threads[index].state);
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
                self.persist_agent_workspace();
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
                self.persist_agent_workspace();
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.dispatch_next_queued_prompt(index);
                }
            }
            AgentEvent::TurnFailed { session_id, error, metrics } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Failed;
                    self.agents.threads[index].optimistic_message = None;
                    self.agents.threads[index].turn_started_at = None;
                    self.agents.threads[index].record_turn_metrics(metrics);
                    self.agents.threads[index].last_error = Some(error.to_string());
                    self.agents.threads[index].pending_recovery = None;
                    let agent_id = self.agents.threads[index].agent_id.clone();
                    self.agents.threads[index]
                        .push_system(format!("agent `{agent_id}` turn failed: {error}"));
                    self.notify_unread(index);
                }
                self.update_persisted_last_prompt(session_id.0.as_ref(), None);
                self.persist_agent_workspace();
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
                        "agent `{}` turn paused: {} (resume with :agents_resume, discard with :agents_discard)",
                        thread.agent_id, info.detail
                    );
                    thread.push_system(notice);
                    // Keep the prompt that started the turn for Resume; it
                    // was captured at submit time.
                    thread.pending_recovery =
                        thread.last_prompt.take().map(|prompt| PendingRecovery { info, prompt });
                    self.notify_unread(index);
                    self.persist_agent_workspace();
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
                self.agents.remove_permission(session_id.0.as_ref(), request_id);
                self.refresh_thread_runtime_state(session_id.0.as_ref());
            }
            AgentEvent::ElicitationCompleted { agent_id, session_id, elicitation_id } => {
                self.handle_elicitation_completed(
                    &agent_id,
                    session_id.as_ref().map(|id| id.0.as_ref()),
                    elicitation_id.0.as_ref(),
                );
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
    pub(super) fn apply_session_update(&mut self, thread_index: usize, update: &SessionUpdate) {
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
            SessionUpdate::AvailableCommandsUpdate(_) => {
                self.sync_thread_snapshot_fields(thread_index);
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
        self.agents.permissions.entry(session_id.0.to_string()).or_default().push_back(
            PermissionPrompt {
                session_id: session_id.0.to_string(),
                request_id: request.request_id,
                tool_title,
                options,
                selected: 0,
            },
        );
        self.agents.threads[thread_index].state = ThreadUiState::AwaitingPermission;
        self.notify_unread(thread_index);
    }

    /// Presents an elicitation request as a prompt plus transcript notice.
    pub(crate) fn present_elicitation(
        &mut self,
        session_id: Option<SessionId>,
        request: CreateElicitationRequest,
        reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    ) {
        let session_id = session_id.map(|id| id.0.to_string());
        let thread_index = session_id.as_deref().and_then(|id| self.agents.thread_index(id));
        let (agent_id, agent_label) = thread_index
            .and_then(|index| self.agents.threads.get(index))
            .map(|thread| (thread.agent_id.clone(), thread.display_name.clone()))
            .or_else(|| {
                self.agents
                    .active_thread_index()
                    .and_then(|index| self.agents.threads.get(index))
                    .map(|thread| (thread.agent_id.clone(), thread.display_name.clone()))
            })
            .unwrap_or_else(|| (String::new(), String::from("agent")));
        let prompt = match &request.mode {
            ElicitationMode::Form(mode) => ElicitationPrompt::from_form(
                session_id.clone(),
                agent_id.clone(),
                agent_label.clone(),
                request.message.clone(),
                &mode.requested_schema,
                reply,
            ),
            ElicitationMode::Url(mode) => ElicitationPrompt::from_url(
                session_id.clone(),
                agent_id.clone(),
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
            self.agents.threads[thread_index].state = ThreadUiState::AwaitingElicitation;
            self.notify_unread(thread_index);
        }
        if let Some(reason) = &prompt.unsupported_reason {
            let notice = format!("elicitation: {reason}");
            if let Some(thread_index) = thread_index {
                self.agents.threads[thread_index].push_system(notice);
            }
        }
        let key = session_id
            .clone()
            .unwrap_or_else(|| AgentPaneState::CONNECTION_SCOPED_INTERACTION_KEY.to_string());
        self.agents.elicitations.entry(key).or_default().push_back(prompt);
    }

    fn push_elicitation_notice(&mut self, session_id: Option<&str>, text: String) {
        if let Some(session_id) = session_id {
            if let Some(thread_index) = self.agents.thread_index(session_id) {
                self.agents.threads[thread_index].push_system(text);
                self.notify_unread(thread_index);
            }
            return;
        }
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].push_system(text);
        }
    }

    /// Handles agent `elicitation/complete` notifications.
    fn handle_elicitation_completed(
        &mut self,
        agent_id: &str,
        session_id: Option<&str>,
        elicitation_id: &str,
    ) {
        let key =
            session_id.unwrap_or(AgentPaneState::CONNECTION_SCOPED_INTERACTION_KEY).to_string();
        let prompt = self.agents.elicitations.get_mut(&key).and_then(|queue| {
            let position = queue.iter().position(|prompt| {
                prompt.agent_id == agent_id
                    && prompt.session_id.as_deref() == session_id
                    && prompt.completion_id.as_deref() == Some(elicitation_id)
            })?;
            queue.remove(position)
        });
        let Some(prompt) = prompt else {
            self.push_elicitation_notice(
                session_id,
                format!("stale elicitation completion ignored: {elicitation_id}"),
            );
            return;
        };
        if self.agents.elicitations.get(&key).is_some_and(VecDeque::is_empty) {
            self.agents.elicitations.remove(&key);
        }
        if let Some(session_id) = session_id {
            self.refresh_thread_runtime_state(session_id);
        }

        let prompt_session_id = prompt.session_id.clone();
        let _ = prompt.reply.send(Ok(ClientRequestResponse::CreateElicitation(
            CreateElicitationResponse::new(ElicitationAction::Accept(
                ElicitationAcceptAction::new(),
            )),
        )));
        self.push_elicitation_notice(
            prompt_session_id.as_deref(),
            format!("elicitation completed: {elicitation_id}"),
        );
    }

    /// Records one local proxy web lifecycle row. Detail is compact provenance
    /// metadata only; remote request/query/body never enters transcript state.
    pub(crate) fn record_web_lifecycle(
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
    pub(super) fn register_session_thread(&mut self, agent_id: &str, thread: AgentThread) {
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
        let transcript =
            persisted.as_ref().map(|record| record.transcript.clone()).unwrap_or_default();
        let prompt_history = transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Message { text, kind: MessageRenderKind::User, .. } => {
                    Some(text.clone())
                }
                _ => None,
            })
            .rev()
            .take(AGENT_PROMPT_HISTORY_MAX)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let next_response_group = transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Message { response_group, .. } => *response_group,
                TranscriptItem::ToolCall { response_group, .. } => Some(*response_group),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let restored = persisted.is_some();
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
            transcript,
            optimistic_message: None,
            draft: std::mem::take(&mut self.agents.pending_draft),
            prompt_history,
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
            next_response_group,
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
        if self.agents.workspace_restore.is_none() {
            self.agents.active_thread = Some(self.agents.threads.len() - 1);
        }
        self.agents.error = None;
        self.agents.threads.last_mut().expect("thread pushed").push_system(format!(
            "session {} ({session_id})",
            if restored { "restored" } else { "started" }
        ));
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

    pub(super) fn sync_thread_snapshot_fields(&mut self, thread_index: usize) {
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
}
