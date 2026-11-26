//! `impl App`: session lifecycle commands (new/fork/archive/restore/export).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use ee_agent_host::AgentThread;

use super::super::*;
use crate::app::agent_export::{format_agent_transcript_markdown, write_agent_transcript_export};

use super::constants::{AGENT_DISPLAY_NAME, AGENT_PROMPT_HISTORY_MAX};
use super::format::{
    agent_connection_state_label, fork_seed, parse_fork_args, thread_display_name,
};
use super::state::{
    AgentPaneLayout, PendingFork, PersistedAgentSession, SessionDeletionConfirmation,
};
use super::thread_ui::{MessageRenderKind, ThreadUiState, TranscriptItem};
use crate::agent_session_store::SessionArchiveRecord;
impl App {
    /// `:agents_new [agent-id]` — start a fresh session and switch to it.
    pub(super) fn agents_new_session(&mut self, requested_agent_id: &str) {
        if !self.config.agents.enabled {
            self.backend.status_message = Some(self.agents_status_message());
            return;
        }
        let requested_agent_id = requested_agent_id.trim();
        if requested_agent_id.contains(char::is_whitespace) {
            self.backend.status_message = Some(String::from("usage: :agents_new [agent_id]"));
            return;
        }
        let selected = if requested_agent_id.is_empty() {
            self.config
                .agents
                .default_agent
                .as_deref()
                .filter(|id| self.config.agents.servers.contains_key(*id))
                .map(str::to_owned)
                .or_else(|| {
                    (self.config.agents.servers.len() == 1)
                        .then(|| self.config.agents.servers.keys().next().cloned())
                        .flatten()
                })
        } else if self.config.agents.servers.contains_key(requested_agent_id) {
            Some(requested_agent_id.to_owned())
        } else {
            self.backend.status_message = Some(self.unknown_agent_message(requested_agent_id));
            return;
        };

        if let Some(agent_id) = selected {
            self.start_selected_agent_session(agent_id);
        } else if self.config.agents.servers.is_empty() {
            self.backend.status_message =
                Some(String::from("no agent configured (add `[agents.servers.<id>]`)"));
        } else {
            if self.agents.layout == AgentPaneLayout::Closed {
                self.agents.layout = AgentPaneLayout::Full;
            }
            self.enter_agent_focus();
            self.open_agent_server_picker();
        }
    }

    pub(crate) fn start_selected_agent_session(&mut self, agent_id: String) {
        if self.agents.layout == AgentPaneLayout::Closed {
            self.agents.layout = AgentPaneLayout::Full;
        }
        self.enter_agent_focus();
        self.ensure_agents_host();
        self.start_mcp_servers();
        let Some(host) = self.agents.host.as_ref() else {
            return;
        };
        if !host.manager.has_agent(&agent_id) {
            let reason = self
                .agents
                .launch_failures
                .get(&agent_id)
                .map(|message| format!(": {message}"))
                .unwrap_or_default();
            self.backend.status_message = Some(format!(
                "agent `{agent_id}` unavailable after secure launch configuration resolution{reason}"
            ));
            return;
        }
        self.start_session(agent_id);
        self.backend.status_message = Some(String::from("starting new agent session…"));
    }

    fn unknown_agent_message(&self, requested: &str) -> String {
        const MAX_LISTED: usize = 8;
        let ids = self.config.agents.servers.keys().take(MAX_LISTED).cloned().collect::<Vec<_>>();
        let suffix = if self.config.agents.servers.len() > MAX_LISTED { ", …" } else { "" };
        format!("unknown agent `{requested}`; configured: {}{suffix}", ids.join(", "))
    }

    pub(super) fn open_agent_server_picker(&mut self) {
        let default = self.config.agents.default_agent.as_deref();
        let items = self
            .config
            .agents
            .servers
            .iter()
            .enumerate()
            .map(|(index, (id, server))| {
                let label = server.label.as_deref().unwrap_or(id);
                let default_marker = if default == Some(id.as_str()) { " · default" } else { "" };
                let state = self
                    .agents
                    .host
                    .as_ref()
                    .and_then(|host| host.manager.connection_state(id))
                    .map_or("not started", agent_connection_state_label);
                crate::picker::PickerItem {
                    label: if label == id { id.clone() } else { format!("{label} ({id})") },
                    detail: Some(format!("{state}{default_marker}")),
                    path: None,
                    buf_id: None,
                    line: None,
                    col: None,
                    choice_index: Some(index),
                }
            })
            .collect();
        self.open_picker(crate::picker::PickerState::new_agent_servers(items));
        self.backend.status_message = Some(String::from("select agent for new session"));
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

    /// Private per-session evidence audit log directory. Kept separate from
    /// transcript exports so append-only logs never collide with create-new
    /// exports.
    pub(super) fn agent_evidence_dir(&self) -> io::Result<PathBuf> {
        #[cfg(test)]
        let resolved: io::Result<PathBuf> = {
            // Tests without an explicit export base must fail closed instead
            // of writing evidence logs into the real user state directory.
            let Some(base) = &self.agents.test_export_base else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "test evidence logs require test_export_base",
                ));
            };
            Ok(base.join("agent-evidence"))
        };
        #[cfg(not(test))]
        let resolved: io::Result<PathBuf> = {
            crate::logs::state_dir().map(|dir| dir.join("agent-evidence")).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "platform state directory is unavailable")
            })
        };
        resolved
    }

    /// Exports current local transcript, including redacted tool payloads, to private Markdown.
    pub(super) fn agents_export_transcript(&mut self) {
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

    pub(super) fn agents_open_exported_transcript(&mut self) {
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
        thread.clear_transcript_state();
        self.persist_agent_workspace();
        self.backend.status_message =
            Some(String::from("visible scrollback cleared; provider conversation remains intact"));
    }

    pub(super) fn agents_thread_is_idle(&self, index: usize) -> bool {
        let Some(thread) = self.agents.threads.get(index) else {
            return false;
        };
        thread.state == ThreadUiState::Ready
            && !thread.host.is_turn_running()
            && thread.pending_recovery.is_none()
            && !self.agents.permissions.contains_key(&thread.session_id)
            && !self.agents.elicitations.contains_key(&thread.session_id)
            && !self.agents.pending_cancels.contains_key(&thread.session_id)
            && self.agents.mode_selection.as_ref().is_none_or(|prompt| prompt.thread_index != index)
            && self
                .agents
                .approval_mode_confirmation
                .as_ref()
                .is_none_or(|confirmation| confirmation.session_id != thread.session_id)
            && self
                .agents
                .session_deletion_confirmation
                .as_ref()
                .is_none_or(|confirmation| confirmation.session_id != thread.session_id)
            && self.agents.approvals.is_empty()
    }

    /// Starts a fresh provider session seeded with redacted visible parent messages.
    /// This is deliberately not presented as an ACP/provider-side clone.
    /// An explicit `requested_agent_id` forks under a different configured
    /// agent; empty keeps the parent's agent (previous behavior). An optional
    /// 1-based response position (most-recent-first, `/copy` numbering) bounds
    /// the seed at that assistant response.
    pub(super) fn agents_fork_session(&mut self, activate_child: bool, args: &str) {
        let (requested_agent_id, position) = match parse_fork_args(args) {
            Ok(parsed) => parsed,
            Err(()) => {
                self.backend.status_message =
                    Some(String::from("usage: /fork [agent_id] [response number]"));
                return;
            }
        };
        let cutoff_group = match position {
            None => None,
            Some(position) => {
                let Some(active) = self.agents.active_thread_index() else {
                    self.backend.status_message =
                        Some(String::from("no active agent session to fork"));
                    return;
                };
                match self.agents.threads[active]
                    .completed_assistant_response_groups()
                    .into_iter()
                    .rev()
                    .nth(position - 1)
                {
                    Some(group) => Some(group),
                    None => {
                        self.backend.status_message =
                            Some(String::from("no completed assistant response to fork from"));
                        return;
                    }
                }
            }
        };
        self.agents_fork_session_with_cutoff(activate_child, &requested_agent_id, cutoff_group);
    }

    /// Keymap action `agent_fork_selected_response`: fork from the selected
    /// final agent response, or from the response group selected with
    /// Ctrl+Left/Ctrl+Right when no final response is selected.
    pub(super) fn agents_fork_selected_response(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session to fork"));
            return;
        };
        let thread = &self.agents.threads[active];
        let Some(group) = thread.selected_final_response.or(thread.selected_response_group) else {
            self.backend.status_message = Some(String::from(
                "no response selected; Ctrl+Up/Ctrl+Down selects a final response, Ctrl+Left/Ctrl+Right selects a group",
            ));
            return;
        };
        if !thread.has_response_group(group) {
            self.backend.status_message = Some(String::from("selected response no longer exists"));
            return;
        }
        self.agents_fork_session_with_cutoff(false, "", Some(group));
    }

    fn agents_fork_session_with_cutoff(
        &mut self,
        activate_child: bool,
        requested_agent_id: &str,
        cutoff_group: Option<ResponseGroupId>,
    ) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session to fork"));
            return;
        };
        if !self.agents_thread_is_idle(active)
            || self.agents.pending_sessions.values().any(|pending| pending.fork.is_some())
        {
            self.backend.status_message = Some(String::from(
                "session must be idle before fork; stop and resolve pending work first",
            ));
            return;
        }
        let requested_agent_id = requested_agent_id.trim();
        if requested_agent_id.contains(char::is_whitespace) {
            self.backend.status_message =
                Some(String::from("usage: /fork [agent_id] [response number]"));
            return;
        }
        let parent = &self.agents.threads[active];
        let parent_session_id = parent.session_id.clone();
        let seed = fork_seed(parent, &self.agents_secret_values(), cutoff_group);
        let agent_id = if requested_agent_id.is_empty() {
            parent.agent_id.clone()
        } else if self.config.agents.servers.contains_key(requested_agent_id) {
            requested_agent_id.to_owned()
        } else {
            self.backend.status_message = Some(self.unknown_agent_message(requested_agent_id));
            return;
        };
        self.ensure_agents_host();
        self.start_mcp_servers();
        if !requested_agent_id.is_empty()
            && let Some(host) = self.agents.host.as_ref()
            && !host.manager.has_agent(&agent_id)
        {
            self.backend.status_message = Some(format!(
                "agent `{agent_id}` unavailable after secure launch configuration resolution"
            ));
            return;
        }
        self.start_session_with_fork(
            agent_id.clone(),
            Some(PendingFork { parent_session_id, seed, activate_child }),
        );
        self.backend.status_message = Some(if requested_agent_id.is_empty() {
            String::from(if activate_child {
                "starting seeded branch session…"
            } else {
                "starting seeded fork session…"
            })
        } else {
            format!(
                "starting seeded {} with {agent_id}…",
                if activate_child { "branch" } else { "fork" }
            )
        });
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
        let archive_fallback = match self.workspace_session_archive() {
            Some(archive) => {
                let record = SessionArchiveRecord {
                    agent_id: thread.agent_id.clone(),
                    session_id: thread.session_id.clone(),
                    session_name: thread.session_name.clone(),
                    archived: true,
                };
                match archive.save(&record, &thread.transcript) {
                    Ok(()) => Vec::new(),
                    Err(error) => {
                        eprintln!(
                            "ee: agent transcript archive failed for {}: {error}",
                            thread.session_id
                        );
                        thread.transcript.clone()
                    }
                }
            }
            None => thread.transcript.clone(),
        };
        self.agents.archived_persisted.retain(|record| record.session_id != session_id);
        self.agents.archived_persisted.push(PersistedAgentSession {
            agent_id: thread.agent_id.clone(),
            session_id: session_id.clone(),
            last_prompt: None,
            session_name: thread.session_name.clone(),
            transcript: archive_fallback,
        });
        self.agents.archived_threads.push(thread);
        self.agents.active_thread = if self.agents.threads.is_empty() {
            None
        } else {
            Some(active.min(self.agents.threads.len() - 1))
        };
        self.persist_agent_workspace();
        self.backend.status_message = Some(format!(
            "session archived locally: {label}; restore with /archive restore {}",
            self.agents.archived_threads.len()
        ));
    }

    fn agents_list_archived_sessions(&mut self) {
        let mut labels = self
            .agents
            .archived_threads
            .iter()
            .map(|thread| {
                let label = thread
                    .session_name
                    .as_deref()
                    .or(thread.session_title.as_deref())
                    .unwrap_or(&thread.session_id);
                format!("{label} ({})", thread.session_id)
            })
            .collect::<Vec<_>>();
        labels.extend(
            self.agents
                .archived_persisted
                .iter()
                .filter(|record| {
                    !self
                        .agents
                        .archived_threads
                        .iter()
                        .any(|thread| thread.session_id == record.session_id)
                })
                .map(|record| {
                    let label = record.session_name.as_deref().unwrap_or(&record.session_id);
                    format!("{label} ({})", record.session_id)
                }),
        );
        let entries = labels
            .into_iter()
            .enumerate()
            .map(|(index, label)| format!("{}: {label}", index + 1))
            .collect::<Vec<_>>();
        let listing = if entries.is_empty() {
            String::from("archived sessions: none")
        } else {
            format!(
                "archived sessions: {}; /archive restore <N> restores locally",
                entries.join(" · ")
            )
        };
        self.backend.status_message = Some(listing);
    }

    fn agents_restore_archived_session(&mut self, raw_index: &str) {
        let Ok(index) = raw_index.parse::<usize>() else {
            self.backend.status_message =
                Some(String::from("usage: /archive restore <positive number>"));
            return;
        };
        let Some(index) = index.checked_sub(1) else {
            self.backend.status_message = Some(String::from("archived session not found"));
            return;
        };
        if index < self.agents.archived_threads.len() {
            let thread = self.agents.archived_threads.remove(index);
            let label = thread.display_name.clone();
            let session_id = thread.session_id.clone();
            self.agents.archived_persisted.retain(|record| record.session_id != session_id);
            self.agents.threads.push(thread);
            self.agents.active_thread = Some(self.agents.threads.len() - 1);
            self.persist_agent_workspace();
            self.backend.status_message = Some(format!("session restored locally: {label}"));
            return;
        }

        let persisted_index = index - self.agents.archived_threads.len();
        let visible = self
            .agents
            .archived_persisted
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                !self
                    .agents
                    .archived_threads
                    .iter()
                    .any(|thread| thread.session_id == record.session_id)
            })
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        let Some(position) = visible.get(persisted_index).copied() else {
            self.backend.status_message = Some(String::from("archived session not found"));
            return;
        };
        let record = self.agents.archived_persisted[position].clone();
        let label = record.session_name.clone().unwrap_or_else(|| record.session_id.clone());
        self.request_persisted_agent_reconnect(record);
        self.backend.status_message =
            Some(format!("reconnecting archived session: {label}; transcript restores on load"));
    }

    pub(super) fn agents_archive_command(&mut self, args: &str) {
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

    pub(super) fn request_delete_current_session(&mut self) {
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
            agent_id: thread.agent_id.clone(),
            session_id: thread.session_id.clone(),
            session_name,
        });
        self.backend.status_message = Some(String::from("confirm local session deletion"));
    }

    pub(super) fn confirm_delete_current_session(&mut self) {
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
        self.agents.clear_session_interactions(&confirmation.session_id);
        let removed = self.agents.threads.remove(confirmation.thread_index);
        // Service cache is pane-owned today; clear all entries when any session
        // closes rather than retain cross-session external content.
        if let Some(service) = &self.agents.web_context_service {
            service.clear_cache();
        }
        // Proxy connections currently share pane ownership. Clear both route
        // scopes when any owning agent session closes; never retain host grants.
        for route in [
            crate::app::agents_mcp::ProxyRoute::Stdio,
            crate::app::agents_mcp::ProxyRoute::AcpNative,
        ] {
            self.agents
                .approval_policy
                .invalidate_session(&format!("proxy-network:{}", route.transport_identity()));
        }
        self.agents.approval_modes.remove(&removed.session_id);
        self.agents.agent_permissions.invalidate_session(&removed.session_id);
        self.agents.archived_persisted.retain(|record| record.session_id != removed.session_id);
        if let Some(archive) = self.workspace_session_archive()
            && let Err(error) = archive.delete_session(&removed.session_id)
        {
            eprintln!(
                "ee: deleting the agent transcript journal for {} failed: {error}",
                removed.session_id
            );
        }
        self.agents.active_thread = if self.agents.threads.is_empty() {
            None
        } else {
            Some(confirmation.thread_index.min(self.agents.threads.len() - 1))
        };
        self.persist_agent_workspace();
        self.backend.status_message = Some(format!(
            "local transcript deleted for {} ({}); provider session unchanged",
            confirmation.session_name, confirmation.session_id
        ));
    }
    /// Registers a fresh session thread from a new-session reply.
    pub(super) fn register_session_thread(&mut self, agent_id: &str, thread: AgentThread) {
        let session_id = thread.session_id().0.to_string();
        let snapshot = thread.snapshot();
        // Phase 6b user-visible diagnostics: how the ee proxy was exposed.
        self.agents.mcp.proxy_mode = Some(thread.proxy_mode().to_string());
        let index = self.agents.next_thread_index;
        self.agents.next_thread_index += 1;
        let nick = AGENT_DISPLAY_NAME.to_owned();
        let ready = self.agents.created_sessions.contains(&session_id);
        let session_title =
            snapshot.session_info.as_ref().and_then(|info| info.title.value().cloned());
        let session_updated_at =
            snapshot.session_info.as_ref().and_then(|info| info.updated_at.value().cloned());
        let persisted = self.load_persisted_agent_workspace().and_then(|workspace| {
            workspace.sessions.into_iter().find(|record| record.session_id == session_id)
        });
        let archived_record = self
            .agents
            .archived_persisted
            .iter()
            .find(|record| record.session_id == session_id)
            .cloned();
        let archive_transcript = self
            .workspace_session_archive()
            .and_then(|archive| archive.load_transcript(&session_id).ok().flatten());
        let fallback = persisted.as_ref().or(archived_record.as_ref());
        let session_name = fallback.and_then(|record| record.session_name.clone());
        let transcript = match archive_transcript {
            Some(transcript) => transcript,
            None => {
                let legacy = fallback.map(|record| record.transcript.clone()).unwrap_or_default();
                if !legacy.is_empty()
                    && let Some(archive) = self.workspace_session_archive()
                {
                    let archive_record = SessionArchiveRecord {
                        agent_id: agent_id.to_string(),
                        session_id: session_id.clone(),
                        session_name: session_name.clone(),
                        archived: archived_record.is_some(),
                    };
                    if let Err(error) = archive.save(&archive_record, &legacy) {
                        eprintln!(
                            "ee: agent transcript migration failed for {session_id}: {error}"
                        );
                    }
                }
                legacy
            }
        };
        if let Some(position) =
            self.agents.archived_persisted.iter().position(|record| record.session_id == session_id)
        {
            self.agents.archived_persisted.remove(position);
        }
        let transcript_archive_loaded = !transcript.is_empty();
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
        let restored = persisted.is_some() || archived_record.is_some();
        self.agents.threads.push(AgentThreadUi {
            index,
            agent_id: agent_id.to_string(),
            session_id: session_id.clone(),
            fork_parent_session_id: None,
            nick,
            display_name: thread_display_name(
                index,
                session_name.as_deref(),
                session_title.as_deref(),
            ),
            state: if ready { ThreadUiState::Ready } else { ThreadUiState::Starting },
            unread: 0,
            activity: false,
            host: thread,
            transcript,
            transcript_archive_loaded,
            optimistic_message: None,
            draft: std::mem::take(&mut self.agents.pending_draft),
            draft_cursor: 0,
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
            current_model: None,
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
            selected_final_response: None,
            collapsed_response_groups: BTreeSet::new(),
            collapse_incoming_response_groups: restored,
            expanded_tool_details: BTreeSet::new(),
            tool_detail_overrides: BTreeMap::new(),
            current_plan: Vec::new(),
            plan_modal_open: false,
            prompt_editor_snapshot: None,
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
        let restored_thread = self.agents.threads.last_mut().expect("thread pushed");
        if restored {
            restored_thread.collapse_restored_response_groups();
        }
        restored_thread.push_system(format!(
            "session {} ({session_id})",
            if restored { "restored" } else { "started" }
        ));
        self.persist_agent_workspace();
    }
}
