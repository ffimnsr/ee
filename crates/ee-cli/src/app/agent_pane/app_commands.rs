//! `impl App`: agents command dispatch, pane open/close, stop, queue, drafts.

use super::super::*;

use super::constants::{AGENT_TERMINAL_OUTPUT_TAIL_BYTES, AGENT_TERMINAL_STOP_ALL_MAX};
use super::format::owned_terminal_summary_line;
use super::state::{AgentPaneLayout, TerminalStopConfirmation};
use super::thread_ui::{ExternalEditorRequest, ThreadUiState};
impl App {
    /// Full agents command dispatch (feature enabled).  Returns `true` when
    /// the pane keeps keyboard focus (the caller then skips the trailing
    /// `enter_normal_mode`).
    pub(crate) fn dispatch_agents_command_impl(&mut self, head: &str, tail: &str) -> bool {
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
                self.agents_new_session(tail);
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
            && self.agents.pending_sessions.is_empty()
            && self.agents.workspace_restore.is_none()
            && self.start_workspace_restore();
        if self.agents.active_thread.is_none()
            && self.agents.pending_sessions.is_empty()
            && self.agents.workspace_restore.is_none()
        {
            let Some(agent_id) = self.default_agent_id() else {
                if self.config.agents.servers.is_empty() {
                    let message = String::from("no agent configured (add `[agents.servers.<id>]`)");
                    self.agents.error = Some(message.clone());
                    self.backend.status_message = Some(message);
                } else {
                    self.open_agent_server_picker();
                }
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
        if self.agents.pending_external_critic.as_ref().is_some_and(|pending| {
            pending.root_session_id == self.agents.threads[active].session_id
        }) {
            if let Some(pending) = &self.agents.pending_external_critic {
                let _ = pending.cancel.send(true);
            }
            self.backend.status_message = Some(String::from("cancelling external rubber duck…"));
            return;
        }
        let Some(host) = &self.agents.host else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = self.agents.threads[active].host.clone();
        if !thread.is_turn_running() {
            self.backend.status_message = Some(String::from("no running turn to stop"));
            return;
        }
        let session_id = self.agents.threads[active].session_id.clone();
        if self.agents.pending_cancels.contains_key(&session_id) {
            self.backend.status_message = Some(String::from("cancellation already pending"));
            return;
        }
        let reply = host.cancel(thread);
        self.agents.threads[active].state = ThreadUiState::Cancelling;
        self.agents.pending_cancels.insert(session_id, reply);
        self.backend.status_message = Some(String::from("cancelling turn…"));
    }

    fn active_terminal_owner(&self) -> Option<crate::app::agent_bridge::TerminalOwner> {
        let active = self.agents.active_thread_index()?;
        let thread = self.agents.threads.get(active)?;
        Some(crate::app::agent_bridge::TerminalOwner {
            agent_id: thread.agent_id.clone(),
            session_id: thread.session_id.clone(),
        })
    }

    pub(super) fn agents_list_owned_terminals(&mut self) {
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

    pub(super) fn agents_list_owned_tasks(&mut self) {
        self.agents_list_owned_terminals();
        let suffix =
            "subagent tasks: unavailable; current ACP host advertises no task-list capability";
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].push_system(suffix);
        }
        let current = self.backend.status_message.take().unwrap_or_default();
        self.backend.status_message = Some(format!("{current}\n{suffix}"));
    }

    fn stop_owned_terminals(
        &mut self,
        owner: &crate::app::agent_bridge::TerminalOwner,
        ids: &[String],
    ) {
        let mut results = Vec::new();
        for terminal_id in ids {
            let result = match self.agents.terminals.stop_owned(owner, terminal_id) {
                Ok(crate::app::agent_bridge::OwnedTerminalStop::StopRequested) => {
                    format!("terminal stop requested: {terminal_id} (direct child only)")
                }
                Ok(crate::app::agent_bridge::OwnedTerminalStop::AlreadyExited) => {
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

    pub(super) fn agents_queue_command(&mut self, args: &str) {
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
    pub(super) fn agents_queue_prompt_command(&mut self, args: &str) {
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
    pub(super) fn agents_steer_command(&mut self, args: &str) {
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
                    let session_id = self.agents.threads[active].session_id.clone();
                    self.agents
                        .pending_cancels
                        .entry(session_id)
                        .or_insert_with(|| host.cancel(thread));
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

    pub(super) fn agents_set_transcript_detail(&mut self, args: &str) {
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

    pub(super) fn agents_transcript_command(&mut self, args: &str) {
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

    pub(super) fn agents_draft_command(&mut self, args: &str) {
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

    pub(super) fn agents_stash_draft(&mut self) {
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

    pub(super) fn agents_restore_draft(&mut self) {
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

    pub(super) fn request_agent_external_editor(&mut self) {
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

    pub(super) fn agents_show_key_help(&mut self) {
        self.backend.status_message = Some(String::from(
            "Agents keys: ↑/↓ history · Ctrl-R reverse history search · Ctrl-Shift-R response collapse · Enter send/queue · Alt-Enter newline · Ctrl-U clear draft · Ctrl-S stash · Ctrl-O restore · Ctrl-Shift-E external edit · Ctrl-G plan · Ctrl-E selected tool detail · Ctrl-←/→ response group · PgUp/PgDn/Home/End scroll · Tab slash/@ completion. Configure mode=agent bindings in [keymap].",
        ));
    }

    pub(super) fn agents_stop_command(&mut self, args: &str) {
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

    pub(super) fn confirm_stop_owned_terminals(&mut self) {
        let Some(confirmation) = self.agents.terminal_stop_confirmation.take() else {
            return;
        };
        let owner = crate::app::agent_bridge::TerminalOwner {
            agent_id: confirmation.agent_id,
            session_id: confirmation.session_id,
        };
        self.stop_owned_terminals(&owner, &confirmation.terminal_ids);
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
}
