//! `impl App`: prompt submission, queue, follow-ups, paused-turn resume.

use std::time::Instant;

use ee_agent_protocol::{ContentBlock, TextContent};

use super::super::*;

use super::constants::AGENT_PROMPT_QUEUE_MAX;
use super::format::{
    PROVIDER_CONFIG_ALIASES, PROVIDER_OWNED_SLASH_COMMANDS, prompt_blocks_with_context,
    queue_command_is_management, split_slash_command,
};
use super::state::AgentPaneLayout;
use super::thread_ui::{AgentContextFile, MessageRenderKind, QueuedPrompt, ThreadUiState};
impl App {
    /// Applies pane-local slash commands before forwarding prompt text to the agent.
    fn submit_agents_local_slash_command(&mut self, draft: &str) -> bool {
        let (Some(command), raw_args) = split_slash_command(draft) else {
            return false;
        };
        let args = raw_args.trim();
        let handled = match command.as_str() {
            "help" if args.is_empty() => {
                self.agents_show_help();
                true
            }
            "status" if args.is_empty() => {
                self.agents_show_status();
                true
            }
            "doctor" if args.is_empty() => {
                self.agents_doctor();
                true
            }
            "memory" => {
                self.agents_memory_command(args);
                true
            }
            "init" if args.is_empty() => {
                self.agents_submit_init_workflow();
                true
            }
            "review" => {
                self.agents_submit_review_workflow(args, false);
                true
            }
            "security-review" => {
                self.agents_submit_review_workflow(args, true);
                true
            }
            "rubber-duck" if self.config.agents.rubber_duck.external_agent_id.is_some() => {
                self.agents_submit_external_rubber_duck(raw_args.trim_end());
                true
            }
            "rubber-duck" => self.agents_require_advertised_provider_command("rubber-duck"),
            "diff" if args.is_empty() => {
                self.open_workspace_git_diff();
                true
            }
            "copy" => {
                self.agents_copy_assistant_response(args);
                true
            }
            "rename" if !args.is_empty() => {
                self.agents_rename_session(args);
                true
            }
            "rename" => {
                self.backend.status_message = Some(String::from("usage: /rename <name>"));
                true
            }
            "sessions" if args.is_empty() => {
                self.open_agents_thread_picker();
                true
            }
            "export" if args.is_empty() => {
                self.agents_export_transcript();
                true
            }
            "new" | "new_thread" => {
                self.agents_new_session(args);
                true
            }
            "archive" => {
                self.agents_archive_command(args);
                true
            }
            "fork" if args.is_empty() => {
                self.agents_fork_session(false);
                true
            }
            "branch" if args.is_empty() => {
                self.agents_fork_session(true);
                true
            }
            "delete" if args.is_empty() => {
                self.request_delete_current_session();
                true
            }
            "delete" => {
                self.backend.status_message = Some(String::from("usage: /delete"));
                true
            }

            "resume" if args.is_empty() => {
                self.resume_paused_turn();
                true
            }
            "discard" if args.is_empty() => {
                self.discard_paused_turn();
                true
            }
            "reconnect" if args.is_empty() => {
                self.agents_reconnect();
                true
            }
            "next" if args.is_empty() => {
                self.agents_switch_thread(1);
                true
            }
            "prev" if args.is_empty() => {
                self.agents_switch_thread(-1);
                true
            }
            "clear" if args.is_empty() => {
                self.agents_clear_scrollback();
                true
            }
            "layout" => {
                if AgentPaneLayout::parse(args).is_some() {
                    self.agents_set_layout(args);
                } else {
                    self.backend.status_message =
                        Some(String::from("usage: /layout right|bottom|full"));
                }
                true
            }
            "thoughts" => {
                if matches!(args, "" | "toggle" | "on" | "off") {
                    self.agents_set_thought_visibility(args);
                } else {
                    self.backend.status_message =
                        Some(String::from("usage: /thoughts on|off|toggle"));
                }
                true
            }
            "config" => {
                let mut parts = args.splitn(2, char::is_whitespace);
                match (parts.next().unwrap_or_default(), parts.next().unwrap_or_default().trim()) {
                    ("", _) => self.agents_list_config_options(),
                    ("set", value)
                        if value
                            .split_once(char::is_whitespace)
                            .is_some_and(|(_, value)| !value.trim().is_empty()) =>
                    {
                        self.agents_set_config_option_command(value)
                    }
                    ("toggle", value)
                        if !value.is_empty() && !value.contains(char::is_whitespace) =>
                    {
                        self.agents_toggle_config_option_command(value)
                    }
                    _ => {
                        self.backend.status_message = Some(String::from(
                            "usage: /config [set <config_id> <value>|toggle <config_id>]",
                        ));
                    }
                }
                true
            }
            "mcp" => {
                if matches!(args, "" | "tools" | "prompts" | "resources" | "close") {
                    self.agents_mcp_command(args);
                } else {
                    self.backend.status_message =
                        Some(String::from("usage: /mcp [tools|prompts|resources|close]"));
                }
                true
            }
            "context" => {
                self.agents_context_command(args);
                true
            }
            "mention" if !args.is_empty() => {
                self.agents_mention_context_file(args);
                true
            }
            "mention" => {
                self.backend.status_message =
                    Some(String::from("usage: /mention <workspace-relative-path>"));
                true
            }
            "add-dir" if !args.is_empty() => {
                self.request_additional_workspace_directory(args);
                true
            }
            "add-dir" => {
                self.backend.status_message = Some(String::from("usage: /add-dir <path>"));
                true
            }
            "tasks" if args.is_empty() => {
                self.agents_list_owned_tasks();
                true
            }
            "ps" if args.is_empty() => {
                self.agents_list_owned_terminals();
                true
            }
            "stop" => {
                self.agents_stop_command(args);
                true
            }
            "steer" if !args.is_empty() => {
                self.agents_steer_command(args);
                true
            }
            "steer" => {
                self.backend.status_message = Some(String::from("usage: /steer <message>"));
                true
            }
            "queue" if queue_command_is_management(args) => {
                self.agents_queue_command(args);
                true
            }
            "queue" if !args.is_empty() => {
                self.agents_queue_prompt_command(args);
                true
            }
            "queue" => {
                self.agents_queue_command(args);
                true
            }
            "details" => {
                self.agents_set_transcript_detail(args);
                true
            }
            "transcript" => {
                self.agents_transcript_command(args);
                true
            }
            "draft" => {
                self.agents_draft_command(args);
                true
            }
            "keys" if args.is_empty() => {
                self.agents_show_key_help();
                true
            }
            "keys" => {
                self.backend.status_message = Some(String::from("usage: /keys"));
                true
            }
            command if PROVIDER_CONFIG_ALIASES.contains(&command) => {
                self.agents_set_provider_config_alias(command, args);
                true
            }
            command if PROVIDER_OWNED_SLASH_COMMANDS.contains(&command) => {
                self.agents_require_advertised_provider_command(command)
            }
            "permissions" => {
                self.agents_permissions_command(args);
                true
            }
            "approval" => {
                match args {
                    "" => match self.active_tool_approval_mode() {
                        Some(mode) => {
                            self.backend.status_message =
                                Some(format!("tool approvals: {}", mode.label()));
                        }
                        None => {
                            self.backend.status_message =
                                Some(String::from("no active agent session"))
                        }
                    },
                    "default" => self.set_active_tool_approval_mode(
                        crate::app::agent_bridge::ToolApprovalMode::Default,
                    ),
                    "autopilot" => self.set_active_tool_approval_mode(
                        crate::app::agent_bridge::ToolApprovalMode::Autopilot,
                    ),
                    "bypass" => self.request_bypass_tool_approvals(),
                    _ => {
                        self.backend.status_message =
                            Some(String::from("usage: /approval [default|autopilot|bypass]"));
                    }
                }
                true
            }
            _ => false,
        };
        if handled {
            self.agents_clear_draft();
        }
        handled
    }

    /// Submits the active thread's draft as a prompt turn.
    pub(super) fn submit_prompt(&mut self) {
        let active = self.agents.active_thread_index();
        let active_has_modal = active.is_some_and(|index| {
            self.agents.permission().is_some()
                || self.agents.elicitation().is_some()
                || self
                    .agents
                    .mode_selection
                    .as_ref()
                    .is_some_and(|prompt| prompt.thread_index == index)
                || self.agents.approval_mode_confirmation.as_ref().is_some_and(|confirmation| {
                    self.agents.threads[index].session_id == confirmation.session_id
                })
                || self
                    .agents
                    .session_deletion_confirmation
                    .as_ref()
                    .is_some_and(|confirmation| confirmation.thread_index == index)
        });
        if active_has_modal || !self.agents.approvals.is_empty() {
            return;
        }
        let draft = active
            .map(|index| self.agents.threads[index].draft.clone())
            .unwrap_or_else(|| self.agents.pending_draft.clone());
        if draft.trim().is_empty() {
            return;
        }
        if self.submit_agents_exit_command(&draft) {
            return;
        }
        if self.submit_agents_local_slash_command(&draft) {
            return;
        }
        let Some(active) = active else {
            self.submit_without_session();
            return;
        };
        if self.agents.pending_external_critic.as_ref().is_some_and(|pending| {
            pending.root_session_id == self.agents.threads[active].session_id
        }) {
            self.backend.status_message = Some(String::from(
                "external rubber duck is running; use /stop to cancel before another root turn",
            ));
            return;
        }
        let (command, args) = split_slash_command(&draft);
        if let Some("mode") = command.as_deref() {
            self.agents.threads[active].draft.clear();
            self.agents_set_mode(active, &args);
            return;
        }
        let prompt_text = draft.trim_end().to_string();
        if self.agents.threads[active].state == ThreadUiState::Running {
            self.enqueue_agent_follow_up(active, prompt_text);
            return;
        }
        if !matches!(self.agents.threads[active].state, ThreadUiState::Ready) {
            self.agents.error = Some(match self.agents.threads[active].state {
                ThreadUiState::PausedRecoverable => {
                    String::from("a turn is paused and recoverable; use /resume or /discard")
                }
                _ => String::from("agent session is not ready; cannot send prompt"),
            });
            return;
        }
        self.send_ready_agent_prompt(active, prompt_text);
    }

    pub(super) fn send_ready_agent_prompt(&mut self, active: usize, prompt_text: String) {
        let next_context = {
            let thread = &mut self.agents.threads[active];
            thread.draft.clear();
            thread.record_prompt_history(&prompt_text);
            std::mem::take(&mut thread.next_prompt_context_files)
        };
        self.send_agent_prompt(active, prompt_text, next_context);
    }

    pub(super) fn enqueue_agent_follow_up(&mut self, active: usize, prompt_text: String) {
        let Some(queued_count) = self.queue_agent_prompt(active, prompt_text, false) else {
            return;
        };
        self.backend.status_message = Some(format!(
            "follow-up queued ({queued_count}/{AGENT_PROMPT_QUEUE_MAX}); /stop cancels current turn, /queue edits pending prompts"
        ));
    }

    pub(super) fn queue_agent_prompt(
        &mut self,
        active: usize,
        prompt_text: String,
        priority: bool,
    ) -> Option<usize> {
        let thread = &mut self.agents.threads[active];
        if thread.queued_prompts.len() >= AGENT_PROMPT_QUEUE_MAX {
            self.backend.status_message = Some(format!(
                "queued follow-up limit reached ({AGENT_PROMPT_QUEUE_MAX}); edit or remove entries with /queue"
            ));
            return None;
        }
        let next_prompt_context_files = std::mem::take(&mut thread.next_prompt_context_files);
        thread.draft.clear();
        thread.record_prompt_history(&prompt_text);
        let prompt = QueuedPrompt { text: prompt_text, next_prompt_context_files };
        if priority {
            thread.queued_prompts.push_front(prompt);
        } else {
            thread.queued_prompts.push_back(prompt);
        }
        Some(thread.queued_prompts.len())
    }

    pub(super) fn dispatch_next_queued_prompt(&mut self, active: usize) {
        let Some(queued) = self
            .agents
            .threads
            .get_mut(active)
            .and_then(|thread| thread.queued_prompts.pop_front())
        else {
            return;
        };
        let remaining = self.agents.threads[active].queued_prompts.len();
        self.send_agent_prompt(active, queued.text, queued.next_prompt_context_files);
        self.backend.status_message =
            Some(format!("dispatching queued follow-up ({remaining} remaining)"));
    }

    fn send_agent_prompt(
        &mut self,
        active: usize,
        prompt_text: String,
        next_prompt_context_files: Vec<AgentContextFile>,
    ) {
        let blocks = {
            let thread = &self.agents.threads[active];
            prompt_blocks_with_context(
                &prompt_text,
                &thread.context_files,
                &next_prompt_context_files,
            )
        };
        self.send_agent_prompt_blocks(active, prompt_text.clone(), blocks, Some(&prompt_text));
    }

    /// Sends a local workflow prompt without persisting its bounded evidence or rendering it verbatim.
    pub(super) fn send_agent_prompt_blocks(
        &mut self,
        active: usize,
        display_text: String,
        blocks: Vec<ContentBlock>,
        persisted_prompt: Option<&str>,
    ) {
        if let Some(prompt) = persisted_prompt
            && let Some(thread) = self.agents.threads.get(active)
        {
            self.update_persisted_last_prompt(&thread.session_id, Some(prompt));
        }
        let thread_handle = {
            let thread = &mut self.agents.threads[active];
            thread.state = ThreadUiState::Running;
            thread.turn_started_at = Some(Instant::now());
            thread.active_response_group = None;
            thread.push_message("you", &display_text, MessageRenderKind::User, None, None);
            thread.optimistic_message = Some(thread.transcript.len().saturating_sub(1));
            thread.last_prompt = Some(blocks.clone());
            thread.host.clone()
        };
        self.persist_agent_workspace();
        let host = self.agents.host.as_ref().expect("host present");
        host.send_prompt(thread_handle, blocks);
    }

    /// Resumes the paused turn: re-sends the exact prompt blocks that started
    /// it, so the agent continues from its checkpoint.
    pub(super) fn resume_paused_turn(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let Some(blocks) =
            self.agents.threads[active].pending_recovery.clone().map(|pending| pending.prompt)
        else {
            self.agents.error = Some(String::from("no paused turn to resume"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        thread.state = ThreadUiState::Running;
        thread.turn_started_at = Some(Instant::now());
        thread.push_system(String::from("resuming paused turn"));
        let host = self.agents.host.as_ref().expect("host present");
        host.resume_prompt(thread.host.clone(), blocks);
    }

    /// Discards the paused turn: tells the agent to drop its checkpoint and
    /// returns the thread to ready.
    pub(super) fn discard_paused_turn(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        if thread.pending_recovery.is_none() {
            self.agents.error = Some(String::from("no paused turn to discard"));
            return;
        }
        thread.state = ThreadUiState::Running;
        thread.push_system(String::from("discarding paused turn"));
        let host = self.agents.host.as_ref().expect("host present");
        let blocks = vec![ContentBlock::Text(TextContent::new(String::from("/discard")))];
        host.send_prompt(thread.host.clone(), blocks);
    }

    fn submit_without_session(&mut self) {
        self.ensure_agents_host();
        let Some(agent_id) = self.default_agent_id() else {
            let message = String::from(
                "no agent configured; add [agents.servers.<id>] in .ee.toml, then run /new_thread",
            );
            self.agents.error = Some(message.clone());
            self.backend.status_message = Some(message);
            return;
        };
        if !self
            .agents
            .pending_sessions
            .keys()
            .any(|key| key.agent_id == agent_id && key.session_id.is_none())
        {
            self.start_session(agent_id);
            self.backend.status_message = Some(String::from(
                "starting agent session; prompt will send after session is ready",
            ));
        }
    }
}
