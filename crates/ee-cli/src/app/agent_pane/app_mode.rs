//! `impl App`: mode/config option commands, tool-approval mode switches.

use ee_agent_protocol::{SessionConfigKind, SessionConfigOptionValue, SessionModeId};

use super::super::*;

use super::format::{
    agent_mode_ids, agent_slash_command_names, config_option_summary, cycle_select_value,
    is_agents_quit_full_slash_command, is_agents_quit_slash_command, is_mode_config_option,
    parse_config_option_value, slash_command_draft, split_slash_command,
};
use super::state::{ApprovalModeConfirmation, ModeSelectionPrompt};
impl App {
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

    fn open_mode_selection(&mut self, thread_index: usize) {
        let thread = &self.agents.threads[thread_index];
        let options = agent_mode_ids(thread);
        let selected = thread
            .host
            .snapshot()
            .current_mode
            .as_ref()
            .and_then(|current| options.iter().position(|mode| mode == current.0.as_ref()))
            .unwrap_or_default();
        self.agents.mode_selection = Some(ModeSelectionPrompt { thread_index, options, selected });
    }

    pub(super) fn agents_set_mode(&mut self, thread_index: usize, raw_mode: &str) {
        let raw_mode = raw_mode.trim();
        if raw_mode.is_empty() {
            self.open_mode_selection(thread_index);
            return;
        }

        let snapshot = self.agents.threads[thread_index].host.snapshot();
        if let Some(mode_option) =
            snapshot.config_options.iter().find(|option| is_mode_config_option(option))
        {
            match parse_config_option_value(mode_option, raw_mode) {
                Ok(value) => {
                    self.queue_thread_config_option_change(
                        thread_index,
                        mode_option.id.clone(),
                        value,
                    );
                }
                Err(error) => self.backend.status_message = Some(error),
            }
            return;
        }

        let Some(modes) = self.agents.threads[thread_index].host.advertised_modes() else {
            self.open_mode_selection(thread_index);
            return;
        };
        let Some(mode) =
            modes.available_modes.iter().find(|mode| mode.id.0.eq_ignore_ascii_case(raw_mode))
        else {
            let available = modes
                .available_modes
                .iter()
                .map(|mode| mode.id.0.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            self.backend.status_message = Some(if available.is_empty() {
                String::from("agent advertised no modes")
            } else {
                format!("unknown mode: {raw_mode}; available: {available}")
            });
            return;
        };
        self.queue_thread_mode_change(thread_index, mode.id.clone());
    }

    pub(super) fn agents_cycle_mode(&mut self, delta: isize) {
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

    pub(super) fn agents_list_config_options(&mut self) {
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

    /// Mutates only an ACP configuration option explicitly advertised by the
    /// active provider session. Alias names deliberately match option ids; EE
    /// never guesses provider model, effort, or personality semantics.
    pub(super) fn agents_set_provider_config_alias(&mut self, alias: &str, raw_value: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let options = self.agents.threads[active].host.config_options();
        let Some(option) = options.into_iter().find(|option| option.id.0.as_ref() == alias) else {
            self.backend.status_message = Some(format!(
                "provider config /{alias} unavailable: agent did not advertise config option {alias}; use /config"
            ));
            return;
        };

        if raw_value.is_empty() {
            if let SessionConfigKind::Boolean(current) = &option.kind {
                self.queue_thread_config_option_change(
                    active,
                    option.id.clone(),
                    SessionConfigOptionValue::boolean(!current.current_value),
                );
            } else {
                self.backend.status_message = Some(format!(
                    "provider config /{alias} currently {}; usage: /{alias} <value>",
                    config_option_summary(&option)
                ));
            }
            return;
        }

        let value = match parse_config_option_value(&option, raw_value) {
            Ok(value) => value,
            Err(message) => {
                self.backend.status_message = Some(message);
                return;
            }
        };
        self.queue_thread_config_option_change(active, option.id.clone(), value);
    }

    /// Returns true when a known provider-owned command must be consumed
    /// locally. Advertised provider commands return false and continue through
    /// normal ACP prompt forwarding unchanged.
    pub(super) fn agents_require_advertised_provider_command(&mut self, command: &str) -> bool {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return true;
        };
        if self.agents.threads[active]
            .available_commands
            .iter()
            .any(|available| available.name == command)
        {
            return false;
        }
        self.backend.status_message = Some(format!(
            "provider command /{command} unavailable: agent did not advertise it; use /help for provider commands"
        ));
        true
    }

    pub(super) fn agents_set_config_option_command(&mut self, tail: &str) {
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

    pub(super) fn agents_toggle_config_option_command(&mut self, tail: &str) {
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

    pub(super) fn cycle_slash_command(&mut self, delta: isize) -> bool {
        let Some(active) = self.agents.active_thread_index() else {
            return false;
        };
        let external_rubber_duck = self.external_rubber_duck_available();
        let thread = &mut self.agents.threads[active];
        let draft = thread.draft.clone();
        let (current_name, rest) = split_slash_command(&draft);
        // Preserve user-entered arguments while cycling slash commands.
        if !draft.trim_start().starts_with('/') {
            return false;
        }

        let command_name = current_name.as_deref().unwrap_or_default();
        let command_names =
            agent_slash_command_names(&thread.available_commands, external_rubber_duck);
        let current_index = command_names.iter().position(|name| *name == command_name);
        let matching_indices: Vec<usize> = if current_index.is_some() {
            (0..command_names.len()).collect()
        } else {
            // Prefer agent-advertised commands for ambiguous prefixes. Local
            // commands remain available as a fallback without stealing an
            // agent's command such as `/edit` from local `/export`.
            let advertised_matches: Vec<usize> = thread
                .available_commands
                .iter()
                .filter(|command| command.name.starts_with(command_name))
                .filter_map(|command| command_names.iter().position(|name| *name == command.name))
                .collect();
            if advertised_matches.is_empty() {
                command_names
                    .iter()
                    .enumerate()
                    .filter_map(|(index, name)| name.starts_with(command_name).then_some(index))
                    .collect()
            } else {
                advertised_matches
            }
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

    /// Applies pane-local exit commands without requiring an agent session.
    pub(super) fn submit_agents_exit_command(&mut self, draft: &str) -> bool {
        if is_agents_quit_slash_command(draft) {
            self.agents_clear_draft();
            self.close_agents_pane();
            return true;
        }
        if is_agents_quit_full_slash_command(draft) {
            self.agents_clear_draft();
            self.should_quit = true;
            return true;
        }
        false
    }

    pub(super) fn active_tool_approval_mode(
        &self,
    ) -> Option<crate::app::agent_bridge::ToolApprovalMode> {
        let active = self.agents.active_thread_index()?;
        let session_id = &self.agents.threads[active].session_id;
        Some(self.agents.approval_modes.get(session_id).copied().unwrap_or_default())
    }

    pub(super) fn set_active_tool_approval_mode(
        &mut self,
        mode: crate::app::agent_bridge::ToolApprovalMode,
    ) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let session_id = self.agents.threads[active].session_id.clone();
        if mode == crate::app::agent_bridge::ToolApprovalMode::Default {
            self.agents.approval_modes.remove(&session_id);
        } else {
            self.agents.approval_modes.insert(session_id, mode);
        }
        let summary = format!("tool approvals: {}", mode.label());
        self.agents.threads[active].push_system(summary.clone());
        self.backend.status_message = Some(summary);
    }

    pub(super) fn request_bypass_tool_approvals(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        self.agents.approval_mode_confirmation = Some(ApprovalModeConfirmation {
            thread_index: active,
            session_id: self.agents.threads[active].session_id.clone(),
        });
        self.backend.status_message = Some(String::from("confirm bypass tool approvals"));
    }

    pub(super) fn confirm_bypass_tool_approvals(&mut self) {
        let Some(confirmation) = self.agents.approval_mode_confirmation.take() else {
            return;
        };
        let Some(thread) = self.agents.threads.get(confirmation.thread_index) else {
            self.backend.status_message =
                Some(String::from("agent session closed before bypass confirmation"));
            return;
        };
        if thread.session_id != confirmation.session_id {
            self.backend.status_message =
                Some(String::from("agent session changed before bypass confirmation"));
            return;
        }
        self.set_active_tool_approval_mode(crate::app::agent_bridge::ToolApprovalMode::Bypass);
    }
}
