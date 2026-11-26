//! `impl App`: agent event handling, session updates, thread registration.

use std::collections::VecDeque;
use std::time::{Instant, SystemTime};

use ee_agent_host::events::{AgentConnectionState, PermissionRequestInfo, ThreadCloseReason};
use ee_agent_host::{AgentError, AgentEvent, ClientRequestResponse, ClientRequestResult};
use ee_agent_protocol::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationMode, FINAL_RESPONSE_MESSAGE_ID_PREFIX, PermissionOption,
    PermissionOptionKind, RequestPermissionOutcome, SelectedPermissionOutcome, SessionId,
    SessionUpdate,
};

use super::super::*;
use super::pump;

use crate::app::agent_bridge::{AgentPermissionPreview, V2CommandPermission};
use crate::config::AgentAlignmentMode;

use super::constants::AGENT_DISPLAY_NAME;
use super::elicitation::ElicitationPrompt;
use super::format::{
    client_request_label, content_block_text, plan_entry_marker, plan_entry_priority_label,
    thread_display_name, tool_call_detail_from_state, tool_call_status_label,
};
use super::state::{AgentPaneLayout, AgentPaneState, PermissionPrompt};
use super::thread_ui::{MessageRenderKind, PendingRecovery, ThreadUiState, TranscriptItem};

// ── App integration ──────────────────────────────────────────────────────────

pub(super) fn session_update_replays_transcript(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
    )
}

fn format_evidence_timestamp(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl App {
    /// Appends one line to the session's private evidence audit log. Turn
    /// lifecycle markers and verification summaries are debugging aids, not
    /// user-facing chat content.
    fn append_evidence_log(&self, session_id: &str, line: &str) {
        if let Ok(directory) = self.agent_evidence_dir()
            && let Err(error) =
                crate::app::agent_export::append_agent_evidence_log(&directory, session_id, line)
        {
            tracing::warn!(?error, "agent evidence log append failed");
        }
    }

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
                        AgentConnectionState::Ready { .. } => {
                            if matches!(
                                thread.state,
                                ThreadUiState::Starting | ThreadUiState::Failed
                            ) {
                                thread.state = ThreadUiState::Ready;
                            }
                            // Keep the pane label stable across providers: the
                            // configured server id is a routing key, not a name.
                            thread.nick = AGENT_DISPLAY_NAME.to_owned();
                            thread.display_name = thread_display_name(
                                thread.index,
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
                self.agents.agent_permissions.invalidate_session(session_id.0.as_ref());
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
            AgentEvent::TurnStarted { session_id, turn } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state =
                        pump::state_after_turn_activity(self.agents.threads[index].state);
                    self.agents.threads[index].verification_paths.clear();
                    self.agents.threads[index].verification_revision = None;
                    self.agents.threads[index].active_response_group = None;
                    self.agents.threads[index].selected_final_response = None;
                    self.agents.threads[index].collapse_incoming_response_groups = false;
                    self.agents.threads[index].turn_started_at = Some(Instant::now());
                    // Lifecycle markers are debugging aids, not chat content;
                    // they live in the private evidence audit log.
                    self.append_evidence_log(
                        session_id.0.as_ref(),
                        &format!(
                            "{} turn:{} started",
                            format_evidence_timestamp(SystemTime::now()),
                            turn.turn_id()
                        ),
                    );
                    self.notify_unread(index);
                }
            }
            AgentEvent::TurnQueued { session_id, position } => {
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].state = ThreadUiState::Queued;
                    self.agents.threads[index].selected_final_response = None;
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
                // Verification summaries are host diagnostics, not user-facing
                // chat content: keep them out of the thread transcript and
                // append them to the private per-session evidence audit log.
                // Intermediate observations log only their new evidence ids;
                // the terminal summary carries the complete id list.
                let previous_ids = self
                    .agents
                    .thread_index(session_id.0.as_ref())
                    .and_then(|index| self.agents.threads[index].terminal_evidence.clone())
                    .map(|previous| previous.evidence_ids)
                    .unwrap_or_default();
                let new_ids: Vec<String> = summary
                    .evidence_ids
                    .iter()
                    .filter(|id| !previous_ids.contains(id))
                    .cloned()
                    .collect();
                let timestamp = format_evidence_timestamp(SystemTime::now());
                let line = if summary.terminal {
                    format!(
                        "{timestamp} turn:{} verification: {:?}; blocker: {:?}; follow_up: {:?}; evidence: {}",
                        summary.key.turn_id(),
                        summary.status,
                        summary.blocker,
                        summary.safe_follow_up,
                        summary.evidence_ids.join(", "),
                    )
                } else {
                    format!(
                        "{timestamp} turn:{} verification: {:?}; blocker: {:?}; follow_up: {:?}; new evidence: {}",
                        summary.key.turn_id(),
                        summary.status,
                        summary.blocker,
                        summary.safe_follow_up,
                        new_ids.join(", "),
                    )
                };
                if let Some(index) = self.agents.thread_index(session_id.0.as_ref()) {
                    self.agents.threads[index].terminal_evidence = Some(*summary);
                    self.notify_unread(index);
                    self.append_evidence_log(session_id.0.as_ref(), &line);
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
                    // Lifecycle markers are debugging aids, not chat content;
                    // they live in the private evidence audit log.
                    self.append_evidence_log(
                        session_id.0.as_ref(),
                        &format!(
                            "{} turn completed (stop: {stop_reason:?})",
                            format_evidence_timestamp(SystemTime::now())
                        ),
                    );
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
                    // A rejected request is not a dead session: the agent can
                    // still accept the next turn. Stay `Failed` only when the
                    // connection itself is gone, otherwise return to `Ready`
                    // so the composer is usable again.
                    let agent_id = self.agents.threads[index].agent_id.clone();
                    let connection_ready = self
                        .agents
                        .host
                        .as_ref()
                        .and_then(|host| host.manager.connection_state(&agent_id))
                        .is_some_and(|state| matches!(state, AgentConnectionState::Ready { .. }));
                    self.agents.threads[index].state =
                        if connection_ready { ThreadUiState::Ready } else { ThreadUiState::Failed };
                    self.agents.threads[index].optimistic_message = None;
                    self.agents.threads[index].turn_started_at = None;
                    self.agents.threads[index].record_turn_metrics(metrics);
                    self.agents.threads[index].last_error = Some(error.to_string());
                    self.agents.threads[index].pending_recovery = None;
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
            AgentEvent::ClientRequestDispatched { session_id, method, target } => {
                let notice = client_request_label(&method, target.as_deref());
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
            AgentEvent::PluginStatusChanged { agent_id, report } => {
                let active = report.plugins.iter().filter(|plugin| plugin.active).count();
                let notice = format!(
                    "agent plugins: {active}/{} active, {} diagnostic(s)",
                    report.plugins.len(),
                    report.diagnostics
                );
                for thread in &mut self.agents.threads {
                    if thread.agent_id == agent_id {
                        thread.push_system(notice.clone());
                    }
                }
            }
            // Rules already settled the request; no UI prompt exists to clear.
            AgentEvent::PermissionAutoResolved { .. } => {}
        }
    }

    /// Reduces one `session/update` into the thread transcript.
    pub(super) fn apply_session_update(&mut self, thread_index: usize, update: &SessionUpdate) {
        if self.agents.threads[thread_index].transcript_archive_loaded
            && session_update_replays_transcript(update)
        {
            // A provider load replay is a bounded request view. The durable
            // journal remains authoritative until the user starts a new turn.
            return;
        }
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
                if message_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with(FINAL_RESPONSE_MESSAGE_ID_PREFIX))
                {
                    // Providers append their evidence-derived completion summary
                    // on the assistant text surface under a reserved id. It is
                    // host diagnostics, not model prose: keep it out of the
                    // transcript like `TurnEvidenceUpdated` and retain it only
                    // in the private evidence audit log.
                    let session_id = self.agents.threads[thread_index].session_id.clone();
                    self.append_evidence_log(
                        &session_id,
                        &format!(
                            "{} final response: {}",
                            format_evidence_timestamp(SystemTime::now()),
                            text.trim()
                        ),
                    );
                    return;
                }
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
        // ACP v2 command subjects carry the raw command the agent intends to
        // execute. Classify it against the same workspace safe-read policy used
        // for v1 terminal requests; auto-answer only when the policy is
        // unambiguous.
        if let Some(command) = &request.command
            && let Some(thread_index) = self.agents.thread_index(session_id.0.as_ref())
        {
            let decision = self.v2_command_permission(&command.command, &command.cwd);
            let outcome = match decision {
                V2CommandPermission::Allow => {
                    select_permission_option(&request.options, &[PermissionOptionKind::AllowOnce])
                }
                V2CommandPermission::Deny => Some(
                    select_permission_option(
                        &request.options,
                        &[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways],
                    )
                    .unwrap_or(RequestPermissionOutcome::Cancelled),
                ),
                V2CommandPermission::Ask => None,
            };
            if let Some(outcome) = outcome
                && self.agents.threads[thread_index]
                    .host
                    .respond_permission(request.request_id, outcome)
            {
                let notice = match decision {
                    V2CommandPermission::Allow => {
                        String::from("trusted built-in read-only command (ACP v2)")
                    }
                    V2CommandPermission::Deny => String::from(
                        "command denied by built-in safeguard or trust policy (ACP v2)",
                    ),
                    V2CommandPermission::Ask => unreachable!("auto outcome only for allow/deny"),
                };
                self.agents.threads[thread_index].push_system(notice.clone());
                self.backend.status_message = Some(notice);
                return;
            }
        }
        let options = request.options.clone();
        let titles: Vec<String> = options.iter().map(|option| option.name.clone()).collect();
        let tool_title =
            request.tool_call.fields.title.clone().unwrap_or_else(|| String::from("tool call"));
        // Alignment (Phases 1–3): remember the ee-validated operation
        // identity (if the payload normalizes) so a matching bridge prompt
        // can reuse the user's decision instead of asking twice, and render
        // what ee itself checked.
        let agent_permission_key = self.agent_operation_key_for_tool_call(&request.tool_call);
        let agent_permission_class = self.agent_operation_class_for_tool_call(&request.tool_call);
        let preview = match agent_permission_key.as_ref() {
            Some(key) => AgentPermissionPreview::Verified(key.verified_summary()),
            None if self.config.agents.approval.alignment == AgentAlignmentMode::Heuristic => {
                AgentPermissionPreview::Heuristic { class: agent_permission_class }
            }
            None => AgentPermissionPreview::Unverifiable,
        };
        // The request can reach the pane before the new-session reply has
        // registered its thread (the host emits it from its own task). Queue
        // it regardless of registration order: dropping it would strand the
        // agent waiting on an approval the user never sees.
        if let Some(thread_index) = self.agents.thread_index(session_id.0.as_ref()) {
            self.agents.threads[thread_index].transcript.push(TranscriptItem::Permission {
                title: tool_title.clone(),
                options: titles.clone(),
                preview: preview.clone(),
                at: SystemTime::now(),
            });
            self.agents.threads[thread_index].state = ThreadUiState::AwaitingPermission;
            self.notify_unread(thread_index);
        }
        self.agents.permissions.entry(session_id.0.to_string()).or_default().push_back(
            PermissionPrompt {
                session_id: session_id.0.to_string(),
                request_id: request.request_id,
                tool_title,
                options,
                selected: 0,
                agent_permission_key,
                agent_permission_class,
                preview,
            },
        );
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
            thread.session_name.as_deref(),
            thread.session_title.as_deref(),
        );
    }
}

/// Selects an offered permission option by kind. Never fabricates an option
/// the agent did not send.
fn select_permission_option(
    options: &[PermissionOption],
    kinds: &[PermissionOptionKind],
) -> Option<RequestPermissionOutcome> {
    options.iter().find(|option| kinds.contains(&option.kind)).map(|option| {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(id.to_string(), id.to_string(), kind)
    }

    #[test]
    fn auto_outcome_never_selects_allow_always_or_missing_kinds() {
        assert_eq!(
            select_permission_option(
                &[option("always", PermissionOptionKind::AllowAlways)],
                &[PermissionOptionKind::AllowOnce]
            ),
            None,
            "AllowAlways must never be auto-selected"
        );
        assert_eq!(
            select_permission_option(
                &[
                    option("once", PermissionOptionKind::AllowOnce),
                    option("always", PermissionOptionKind::AllowAlways),
                ],
                &[PermissionOptionKind::AllowOnce]
            ),
            Some(RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("once")))
        );
        assert_eq!(
            select_permission_option(
                &[option("reject", PermissionOptionKind::RejectOnce)],
                &[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]
            ),
            Some(RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("reject")))
        );
    }
}
