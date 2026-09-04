//! `impl App`: session lifecycle commands (new/fork/archive/restore/export).

use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use super::super::*;
use crate::app::agent_export::{format_agent_transcript_markdown, write_agent_transcript_export};

use super::format::{agent_connection_state_label, fork_seed};
use super::state::{AgentPaneLayout, PendingFork, SessionDeletionConfirmation};
use super::thread_ui::ThreadUiState;
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
            self.backend.status_message = Some(format!(
                "agent `{agent_id}` unavailable after secure launch configuration resolution"
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

    fn agents_thread_is_idle(&self, index: usize) -> bool {
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
    pub(super) fn agents_fork_session(&mut self, activate_child: bool) {
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
        let parent = &self.agents.threads[active];
        let parent_session_id = parent.session_id.clone();
        let agent_id = parent.agent_id.clone();
        let seed = fork_seed(parent, &self.agents_secret_values());
        self.ensure_agents_host();
        self.start_mcp_servers();
        self.start_session_with_fork(
            agent_id,
            Some(PendingFork { parent_session_id, seed, activate_child }),
        );
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
        self.agents.active_thread = (!self.agents.threads.is_empty())
            .then_some(confirmation.thread_index.min(self.agents.threads.len() - 1));
        self.persist_agent_workspace();
        self.backend.status_message = Some(format!(
            "local transcript deleted for {} ({}); provider session unchanged",
            confirmation.session_name, confirmation.session_id
        ));
    }
}
