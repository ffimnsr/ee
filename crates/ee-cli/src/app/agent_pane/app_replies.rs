//! `impl App`: session/cancel/thread-action/external-critic reply pumps.

use std::sync::mpsc as std_mpsc;

use ee_agent_host::{AgentThread, ExternalCritiqueOutcome};
use ee_agent_protocol::{ContentBlock, TextContent};

use super::super::*;

use super::app_events::session_update_replays_transcript;
use super::format::external_critic_cost_micros;
use super::state::{PendingSession, SessionLifecycleKey, SessionLifecycleResult};
use super::thread_ui::ThreadUiState;
impl App {
    /// Polls every pending lifecycle reply without serializing unrelated sessions.
    pub(super) fn pump_session_reply(&mut self) {
        let completed: Vec<(SessionLifecycleKey, SessionLifecycleResult)> = self
            .agents
            .pending_sessions
            .iter()
            .filter_map(|(key, pending)| match pending.reply.try_recv() {
                Ok(result) => Some((key.clone(), Ok(result))),
                Err(std_mpsc::TryRecvError::Empty) => None,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    Some((key.clone(), Err(String::from("agent host stopped"))))
                }
            })
            .collect();
        for (key, result) in completed {
            let pending = self
                .agents
                .pending_sessions
                .remove(&key)
                .expect("completed lifecycle operation present");
            self.finish_session_reply(key, pending, result);
        }
        self.start_next_workspace_restore();
    }

    fn finish_session_reply(
        &mut self,
        key: SessionLifecycleKey,
        mut pending: PendingSession,
        result: Result<Result<AgentThread, String>, String>,
    ) {
        let session_id = key.session_id.clone();
        let agent_id = key.agent_id.clone();
        match result {
            Ok(Ok(thread)) => {
                let returned_session_id = thread.session_id().0.to_string();
                if session_id.is_none() && self.agents.thread_index(&returned_session_id).is_some()
                {
                    self.record_session_lifecycle_failure(
                        &key,
                        format!("duplicate session id returned: {returned_session_id}"),
                    );
                    return;
                }
                if let Some(index) = session_id.as_ref().and_then(|id| self.agents.thread_index(id))
                {
                    // Same-process reconnect: a thread for this session
                    // already exists, so rebind the fresh connection
                    // instead of duplicating the thread.
                    self.agents.threads[index].host = thread;
                    self.agents.threads[index].state = ThreadUiState::Ready;
                    self.agents.threads[index].push_system(String::from("session reconnected"));
                    self.sync_thread_snapshot_fields(index);
                    if self.agents.workspace_restore.is_none() {
                        self.agents.active_thread = Some(index);
                    }
                    self.persist_agent_workspace();
                } else {
                    self.register_session_thread(&agent_id, thread);
                    if let Some(fork) = pending.fork.take() {
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
                if let Some(ref session_id) = session_id {
                    // Reconnect: apply the conversation replay updates that
                    // streamed while the thread was not registered yet, then
                    // restore the persisted last prompt for the resend path.
                    if let Some(index) = self
                        .agents
                        .thread_index(session_id)
                        .zip(self.agents.pending_replay.remove(session_id))
                    {
                        if index.1.iter().any(session_update_replays_transcript) {
                            self.agents.threads[index.0].clear_transcript_state();
                        }
                        for update in index.1 {
                            self.apply_session_update(index.0, &update);
                        }
                    }
                    if let Some(text) =
                        self.load_persisted_agent_workspace().and_then(|workspace| {
                            workspace
                                .sessions
                                .into_iter()
                                .find(|record| record.session_id == *session_id)
                                .and_then(|record| record.last_prompt)
                        })
                        && let Some(index) = self.agents.thread_index(session_id)
                    {
                        self.agents.threads[index].last_prompt =
                            Some(vec![ContentBlock::Text(TextContent::new(text))]);
                    }
                }
            }
            Ok(Err(message)) | Err(message) => self.record_session_lifecycle_failure(&key, message),
        }
        if let Some(session_id) = session_id
            && let Some(restore) = self.agents.workspace_restore.as_mut()
        {
            restore.in_flight.remove(&session_id);
        }
    }

    fn record_session_lifecycle_failure(&mut self, key: &SessionLifecycleKey, message: String) {
        if let Some(session_id) = &key.session_id {
            self.agents.pending_replay.remove(session_id);
            if let Some(restore) = self.agents.workspace_restore.as_mut() {
                restore.in_flight.remove(session_id);
                restore.failed = true;
            }
        }
        let attributed = match &key.session_id {
            Some(session_id) => format!("session {session_id} lifecycle failed: {message}"),
            None => format!("agent {} session start failed: {message}", key.agent_id),
        };
        self.agents.error = Some(attributed.clone());
        self.backend.status_message = Some(attributed);
    }

    /// Polls all pending cancellation replies without letting one session
    /// replace or delay another session's result.
    pub(super) fn pump_cancel_reply(&mut self) {
        let completed: Vec<(String, Result<(), String>)> = self
            .agents
            .pending_cancels
            .iter()
            .filter_map(|(session_id, reply)| match reply.try_recv() {
                Ok(result) => Some((session_id.clone(), result)),
                Err(std_mpsc::TryRecvError::Empty) => None,
                Err(std_mpsc::TryRecvError::Disconnected) => Some((
                    session_id.clone(),
                    Err(String::from("agent cancellation channel closed")),
                )),
            })
            .collect();
        for (session_id, result) in completed {
            self.agents.pending_cancels.remove(&session_id);
            let message = match result {
                Ok(()) => String::from("turn cancelled"),
                Err(message) => message,
            };
            if let Some(index) = self.agents.thread_index(&session_id) {
                self.refresh_thread_runtime_state(&session_id);
                self.agents.threads[index].push_system(message.clone());
                if self.agents.active_thread_index() == Some(index) {
                    self.backend.status_message = Some(message);
                }
            }
        }
    }

    pub(super) fn refresh_thread_runtime_state(&mut self, session_id: &str) {
        let Some(index) = self.agents.thread_index(session_id) else {
            return;
        };
        if matches!(
            self.agents.threads[index].state,
            ThreadUiState::Closed | ThreadUiState::Failed | ThreadUiState::PausedRecoverable
        ) {
            return;
        }
        self.agents.threads[index].state = if self.agents.pending_cancels.contains_key(session_id) {
            ThreadUiState::Cancelling
        } else if self.agents.permissions.get(session_id).is_some_and(|queue| !queue.is_empty()) {
            ThreadUiState::AwaitingPermission
        } else if self.agents.elicitations.get(session_id).is_some_and(|queue| !queue.is_empty()) {
            ThreadUiState::AwaitingElicitation
        } else if self.agents.threads[index].host.is_turn_running() {
            ThreadUiState::Running
        } else {
            ThreadUiState::Ready
        };
    }

    pub(super) fn pump_thread_action_reply(&mut self) {
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

    pub(super) fn pump_external_critic_reply(&mut self) {
        let result = match self.agents.pending_external_critic.as_ref() {
            Some(pending) => pending.reply.try_recv(),
            None => return,
        };
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(std_mpsc::TryRecvError::Empty) => return,
            Err(std_mpsc::TryRecvError::Disconnected) => ExternalCritiqueOutcome::Failed {
                reason: String::from("external critic worker stopped"),
            },
        };
        let pending = self.agents.pending_external_critic.take().expect("pending critic exists");
        let Some(thread_index) = self.agents.thread_index(&pending.root_session_id) else {
            return;
        };
        let elapsed_ms =
            u64::try_from(pending.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let revision_current = self
            .external_critic_context(pending.context_limit)
            .is_ok_and(|(_, _, revision)| revision == pending.requested_revision);
        if !revision_current && matches!(outcome, ExternalCritiqueOutcome::Completed(_)) {
            let message = String::from(
                "external rubber duck quarantined: workspace evidence changed during critique",
            );
            self.agents.threads[thread_index].push_system(message.clone());
            self.backend.status_message = Some(message);
            return;
        }

        match outcome {
            ExternalCritiqueOutcome::Completed(completed) => {
                let counts = ee_agent_host::finding_counts(&completed.report);
                let report_json = completed
                    .report
                    .to_json()
                    .unwrap_or_else(|_| String::from(r#"{"schema_version":1,"findings":[]}"#));
                let attribution = &completed.attribution;
                let cost = external_critic_cost_micros(attribution.session_usage.as_ref())
                    .map_or_else(|| String::from("unknown"), |value| format!("{value} micro-USD"));
                let identity = match (
                    attribution.implementation_name.as_deref(),
                    attribution.implementation_version.as_deref(),
                ) {
                    (Some(name), Some(version)) => format!("{name} {version}"),
                    (Some(name), None) => name.to_string(),
                    _ => String::from("implementation metadata unavailable"),
                };
                let notice = format!(
                    "external rubber duck completed via {} ({identity}); findings: {} blocking, {} non-blocking, {} suggestions; latency: {elapsed_ms}ms; estimated cost: {cost}",
                    attribution.critic_agent_id,
                    counts.blocking,
                    counts.non_blocking,
                    counts.suggestions,
                );
                self.agents.threads[thread_index].push_system(notice.clone());
                if let Some(warning) = &attribution.warning {
                    self.agents.threads[thread_index].push_system(format!("warning: {warning}"));
                }
                if self.agents.threads[thread_index].state != ThreadUiState::Ready {
                    self.backend.status_message =
                        Some(format!("{notice}; root session unavailable for synthesis"));
                    return;
                }
                let instruction = format!(
                    "EE verified external rubber-duck evidence follows. It is critique evidence only, not validation, approval, completion evidence, or permission to mutate. You remain sole decision owner. Produce one bounded synthesis for user: state accepted, rejected with evidence, or deferred with reason for material findings; explain any plan change. Do not expose hidden reasoning or claim critic opinion proves completion.\n\n<verified_external_critique>\n{report_json}\n</verified_external_critique>"
                );
                self.send_agent_prompt_blocks(
                    thread_index,
                    String::from(
                        "EE verified external rubber duck returned; root synthesis requested.",
                    ),
                    vec![ContentBlock::Text(TextContent::new(instruction))],
                    None,
                );
                self.backend.status_message = Some(notice);
            }
            ExternalCritiqueOutcome::Unavailable(reason) => {
                let message = format!("external rubber duck skipped: {reason:?}");
                self.agents.threads[thread_index].push_system(message.clone());
                self.backend.status_message = Some(message);
            }
            ExternalCritiqueOutcome::Quarantined { reason, .. } => {
                let message = format!(
                    "external rubber duck quarantined after {elapsed_ms}ms: {}",
                    reason.chars().take(256).collect::<String>()
                );
                self.agents.threads[thread_index].push_system(message.clone());
                self.backend.status_message = Some(message);
            }
            ExternalCritiqueOutcome::Cancelled => {
                let message = String::from("external rubber duck cancelled");
                self.agents.threads[thread_index].push_system(message.clone());
                self.backend.status_message = Some(message);
            }
            ExternalCritiqueOutcome::TimedOut => {
                let message = format!("external rubber duck timed out after {elapsed_ms}ms");
                self.agents.threads[thread_index].push_system(message.clone());
                self.backend.status_message = Some(message);
            }
            ExternalCritiqueOutcome::Failed { reason } => {
                let message = format!(
                    "external rubber duck failed: {}",
                    reason.chars().take(256).collect::<String>()
                );
                self.agents.threads[thread_index].push_system(message.clone());
                self.backend.status_message = Some(message);
            }
        }
    }
}
