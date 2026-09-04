//! `impl App`: key handling, permission/elicitation confirmation, selection moves.

use crate::policy::is_protected_relative_path;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ee_agent_host::ClientRequestResponse;
use ee_agent_protocol::{
    CreateElicitationResponse, ElicitationAcceptAction, ElicitationAction,
    RequestPermissionOutcome, SelectedPermissionOutcome,
};
use ignore::WalkBuilder;
use ratatui::layout::Rect;

use super::super::*;

use super::constants::AGENTS_SCROLL_PAGE;
use super::elicitation::ElicitationFieldValue;
impl App {
    /// Confirms the selected permission option.
    fn confirm_permission(&mut self) {
        let Some(prompt) = self.agents.take_permission() else {
            return;
        };
        let request_id = prompt.request_id;
        let Some(option) = prompt.options.get(prompt.selected).cloned() else {
            self.agents
                .permissions
                .entry(prompt.session_id.clone())
                .or_default()
                .push_front(prompt);
            return;
        };
        let Some(thread_index) = self.agents.thread_index(&prompt.session_id) else {
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
        self.refresh_thread_runtime_state(&prompt.session_id);
    }

    /// Applies the selected local mode and closes the composer picker.
    fn confirm_mode_selection(&mut self) {
        let Some(prompt) = self.agents.mode_selection.take() else {
            return;
        };
        let Some(mode) = prompt.options.get(prompt.selected).cloned() else {
            self.agents.mode_selection = Some(prompt);
            return;
        };
        self.agents_set_mode(prompt.thread_index, &mode);
    }

    /// Resolves pending elicitation with accept, decline, or cancel semantics.
    fn confirm_elicitation(&mut self, action: ElicitationAction) {
        let Some(prompt) = self.agents.take_elicitation() else {
            return;
        };
        let session_id = prompt.session_id.clone();
        let response = match action {
            ElicitationAction::Accept(_) => {
                if prompt.url.is_some() {
                    Ok(ClientRequestResponse::CreateElicitation(CreateElicitationResponse::new(
                        ElicitationAction::Accept(ElicitationAcceptAction::new()),
                    )))
                } else {
                    match prompt.content_map() {
                        Ok(content) => Ok(ClientRequestResponse::CreateElicitation(
                            CreateElicitationResponse::new(ElicitationAction::Accept(
                                ElicitationAcceptAction::new().content(content),
                            )),
                        )),
                        Err(error) => {
                            let message = format!("elicitation blocked locally: {error}");
                            self.agents.error = Some(message.clone());
                            self.backend.status_message = Some(message);
                            let key = prompt.session_id.clone().unwrap_or_default();
                            self.agents.elicitations.entry(key).or_default().push_front(prompt);
                            return;
                        }
                    }
                }
            }
            ElicitationAction::Decline => Ok(ClientRequestResponse::CreateElicitation(
                CreateElicitationResponse::new(ElicitationAction::Decline),
            )),
            ElicitationAction::Cancel => Ok(ClientRequestResponse::CreateElicitation(
                CreateElicitationResponse::new(ElicitationAction::Cancel),
            )),
            _ => Ok(ClientRequestResponse::CreateElicitation(CreateElicitationResponse::new(
                ElicitationAction::Cancel,
            ))),
        };
        let _ = prompt.reply.send(response);
        if let Some(thread_index) =
            session_id.as_deref().and_then(|id| self.agents.thread_index(id))
            && let Some(thread) = self.agents.threads.get_mut(thread_index)
        {
            let notice = match action {
                ElicitationAction::Accept(_) => "elicitation answered",
                ElicitationAction::Decline => "elicitation declined",
                ElicitationAction::Cancel => "elicitation cancelled",
                _ => "elicitation cancelled",
            };
            thread.push_system(notice);
        }
        if let Some(session_id) = session_id {
            self.refresh_thread_runtime_state(&session_id);
        }
    }

    /// Appends text to the active thread's draft, or to the startup draft before a session exists.
    pub(crate) fn agents_append_draft(&mut self, text: &str) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft.push_str(text);
        } else {
            self.agents.pending_draft.push_str(text);
        }
    }

    /// Dispatches explicit `mode = "agent"` keymap actions before built-in
    /// composer keys. Other editor actions never leak into Agents TUI.
    pub(crate) fn handle_agent_keybinding_action(&mut self, action: crate::keymap::Action) -> bool {
        match action {
            crate::keymap::Action::AgentHistoryPrevious => self.agents_navigate_prompt_history(-1),
            crate::keymap::Action::AgentHistoryNext => self.agents_navigate_prompt_history(1),
            crate::keymap::Action::AgentHistorySearchReverse => {
                self.agents_reverse_prompt_history_search()
            }
            crate::keymap::Action::AgentDraftStash => self.agents_stash_draft(),
            crate::keymap::Action::AgentDraftRestore => self.agents_restore_draft(),
            crate::keymap::Action::AgentDraftExternalEdit => self.request_agent_external_editor(),
            crate::keymap::Action::AgentToggleTranscriptDetails => {
                self.agents_set_transcript_detail("toggle")
            }
            crate::keymap::Action::AgentToggleTranscriptRaw => {
                self.agents_transcript_command("toggle")
            }
            _ => return false,
        }
        true
    }

    /// Key handling while `Mode::Agent` is active.
    pub(crate) fn handle_agent_key(&mut self, key: KeyEvent) {
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

        // Plan modal: Esc closes the overlay without modifying transcript.
        if self
            .agents
            .active_thread_index()
            .and_then(|index| self.agents.threads.get(index))
            .is_some_and(|thread| thread.plan_modal_open)
            && key.code == KeyCode::Esc
        {
            if let Some(index) = self.agents.active_thread_index()
                && let Some(thread) = self.agents.threads.get_mut(index)
            {
                thread.plan_modal_open = false;
                self.backend.status_message = Some(String::from("plan closed"));
            }
            return;
        }

        // Local transcript deletion is irreversible in this process. Provider data remains untouched.
        if self.agents.session_deletion_confirmation.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_delete_current_session();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.session_deletion_confirmation = None;
                    self.backend.status_message =
                        Some(String::from("local session deletion cancelled"));
                    return;
                }
                _ => return,
            }
        }

        if self.agents.additional_directory_confirmation.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_additional_workspace_directory();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.additional_directory_confirmation = None;
                    self.backend.status_message =
                        Some(String::from("additional workspace root cancelled"));
                    return;
                }
                _ => return,
            }
        }

        if self.agents.terminal_stop_confirmation.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_stop_owned_terminals();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.terminal_stop_confirmation = None;
                    self.backend.status_message = Some(String::from("terminal stop cancelled"));
                    return;
                }
                _ => return,
            }
        }

        // Bypass mode needs an explicit confirmation because it removes approval
        // dialogs for every validated bridge tool call in this session.
        if self.agents.approval_mode_confirmation.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_bypass_tool_approvals();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.approval_mode_confirmation = None;
                    self.backend.status_message =
                        Some(String::from("bypass tool approvals cancelled"));
                    return;
                }
                _ => return,
            }
        }

        // Bridge approvals render above permissions in the composer. Up/down selects
        // a visible option row; left/right and tab remain aliases for compatibility.
        if self.agents.approvals.front().is_some() {
            if self
                .agents
                .approvals
                .front()
                .is_some_and(crate::app::agent_bridge::ApprovalPrompt::is_confirming_rule)
            {
                match key.code {
                    KeyCode::Enter => {
                        let choice = self
                            .agents
                            .approvals
                            .front()
                            .and_then(
                                crate::app::agent_bridge::ApprovalPrompt::confirming_allow_choice,
                            )
                            .unwrap_or(crate::app::agent_bridge::ApprovalChoice::DenyPersistent);
                        self.confirm_bridge_approval(choice);
                    }
                    KeyCode::Esc => self.cancel_rule_confirmation(),
                    _ => {}
                }
                return;
            }
            match key.code {
                KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
                    self.move_approval_selection(-1);
                    return;
                }
                KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                    self.move_approval_selection(1);
                    return;
                }
                KeyCode::Enter => {
                    let choice = self
                        .agents
                        .approvals
                        .front()
                        .and_then(|prompt| prompt.options.get(prompt.selected).map(|(_, c)| *c))
                        .unwrap_or(crate::app::agent_bridge::ApprovalChoice::AllowOnce);
                    self.confirm_bridge_approval(choice);
                    return;
                }
                KeyCode::Esc => {
                    self.confirm_bridge_approval(
                        crate::app::agent_bridge::ApprovalChoice::DenyOnce,
                    );
                    return;
                }
                _ => {}
            }
        }

        // Mode selection expands in the composer just like bridge approvals.
        if self.agents.mode_selection.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
                    self.move_mode_selection(-1);
                    return;
                }
                KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                    self.move_mode_selection(1);
                    return;
                }
                KeyCode::Enter => {
                    self.confirm_mode_selection();
                    return;
                }
                KeyCode::Esc => {
                    self.agents.mode_selection = None;
                    return;
                }
                _ => return,
            }
        }

        // Permission selection: ←/→/Tab move, Enter confirms.
        if self.agents.permission().is_some() {
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

        // Elicitation widgets: ↑/↓ move fields, ←/→/Tab change values or URL choice,
        // Enter submits current choice, Ctrl-D declines, Esc cancels.
        if self.agents.elicitation().is_some() {
            match key.code {
                KeyCode::Up => {
                    if let Some(prompt) = self.agents.elicitation_mut() {
                        prompt.selected_field = prompt.selected_field.saturating_sub(1);
                    }
                    return;
                }
                KeyCode::Down | KeyCode::Tab => {
                    if let Some(prompt) = self.agents.elicitation_mut() {
                        if prompt.url.is_some() {
                            prompt.selected_choice = (prompt.selected_choice + 1) % 3;
                        } else {
                            let count = prompt.fields.len().max(1);
                            prompt.selected_field = (prompt.selected_field + 1) % count;
                        }
                    }
                    return;
                }
                KeyCode::BackTab => {
                    if let Some(prompt) = self.agents.elicitation_mut() {
                        if prompt.url.is_some() {
                            prompt.selected_choice = (prompt.selected_choice + 2) % 3;
                        } else {
                            let count = prompt.fields.len().max(1);
                            prompt.selected_field = (prompt.selected_field + count - 1) % count;
                        }
                    }
                    return;
                }
                KeyCode::Left | KeyCode::Right => {
                    if let Some(prompt) = self.agents.elicitation_mut() {
                        if prompt.url.is_some() {
                            let delta = if key.code == KeyCode::Left { 2 } else { 1 };
                            prompt.selected_choice = (prompt.selected_choice + delta) % 3;
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
                    let action = self
                        .agents
                        .elicitation()
                        .map(|prompt| {
                            if prompt.url.is_some() {
                                prompt.submit_action(true)
                            } else {
                                ElicitationAction::Accept(ElicitationAcceptAction::new())
                            }
                        })
                        .unwrap_or(ElicitationAction::Cancel);
                    self.confirm_elicitation(action);
                    return;
                }
                KeyCode::Esc => {
                    self.confirm_elicitation(ElicitationAction::Cancel);
                    return;
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.confirm_elicitation(ElicitationAction::Decline);
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
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match c {
                        'n' => self.agents_switch_thread(1),
                        'p' => self.agents_switch_thread(-1),
                        't' => self.open_agents_thread_picker(),
                        'g' => self.agents_toggle_plan(),
                        'r' if key.modifiers.contains(KeyModifiers::SHIFT)
                            || self
                                .agents
                                .active_thread_index()
                                .and_then(|index| self.agents.threads.get(index))
                                .is_some_and(|thread| thread.selected_response_group.is_some()) =>
                        {
                            self.agents_toggle_selected_response_group()
                        }
                        'r' => self.agents_reverse_prompt_history_search(),
                        'e' if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            self.request_agent_external_editor()
                        }
                        'e' => self.agents_toggle_selected_tool_details(),
                        's' => self.agents_stash_draft(),
                        'o' => self.agents_restore_draft(),
                        'u' => self.agents_clear_draft(),
                        _ => {}
                    }
                } else if !key.modifiers.contains(KeyModifiers::ALT) {
                    self.agents_append_draft(&c.to_string());
                }
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.agents_select_response_group(-1);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.agents_select_response_group(1);
            }
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.agents_append_draft("\n");
                } else {
                    self.submit_prompt();
                }
            }
            KeyCode::Tab
                if !self.cycle_slash_command(1) && !self.agents_complete_mention_path() =>
            {
                self.agents_append_draft("\t");
            }
            KeyCode::BackTab => {
                let _ = self.cycle_slash_command(-1);
            }
            KeyCode::Backspace => self.agents_draft_backspace(),
            KeyCode::Up => self.agents_navigate_prompt_history(-1),
            KeyCode::Down => self.agents_navigate_prompt_history(1),
            KeyCode::Esc => {}
            KeyCode::PageUp => self.agents_scroll(-(AGENTS_SCROLL_PAGE as isize)),
            KeyCode::PageDown => self.agents_scroll(AGENTS_SCROLL_PAGE as isize),
            KeyCode::Home => self.agents_scroll_to(0),
            KeyCode::End => self.agents_scroll_to_bottom(),
            _ => {}
        }
    }

    /// Toggles the active thread's plan modal (Ctrl-G).
    fn agents_toggle_plan(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            if !thread.current_plan.is_empty() {
                thread.plan_modal_open = !thread.plan_modal_open;
            }
        }
    }

    /// Selects a response group containing reasoning or tool calls.
    fn agents_select_response_group(&mut self, delta: isize) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let groups = thread.response_group_ids();
        let Some(current) = thread.selected_response_group else {
            thread.selected_response_group = groups.last().copied();
            return;
        };
        let Some(index) = groups.iter().position(|group| *group == current) else {
            thread.selected_response_group = groups.last().copied();
            return;
        };
        thread.selected_response_group =
            Some(groups[(index as isize + delta).rem_euclid(groups.len() as isize) as usize]);
    }

    /// Toggles reasoning and tool visibility for the selected response group.
    fn agents_toggle_selected_response_group(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let Some(group) = thread.selected_response_group else {
            return;
        };
        if !thread.response_group_ids().contains(&group) {
            return;
        }
        if !thread.collapsed_response_groups.insert(group) {
            thread.collapsed_response_groups.remove(&group);
        }
    }

    /// Toggles tool input/output detail for the selected response group (Ctrl-E).
    fn agents_toggle_selected_tool_details(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let Some(group) = thread.selected_response_group else {
            return;
        };
        if !thread.response_group_ids().contains(&group) {
            return;
        }
        if !thread.expanded_tool_details.insert(group) {
            thread.expanded_tool_details.remove(&group);
        }
    }

    /// Returns active transcript's maximum visual-row offset for current terminal layout.
    fn agents_transcript_scroll_max(&self) -> usize {
        let fallback = self
            .agents
            .active_thread_index()
            .map(|active| self.agents.threads[active].transcript.len().saturating_sub(1))
            .unwrap_or(0);
        let Ok((width, height)) = crossterm::terminal::size() else {
            return fallback;
        };
        let area = Rect { x: 0, y: 0, width, height };
        let Some(pane) = crate::ui::agents_pane_rect_for(area, self) else {
            return fallback;
        };
        crate::ui::agents_transcript_scroll_max(self, pane)
    }

    /// Moves the local mode selection by `delta`.
    fn move_mode_selection(&mut self, delta: isize) {
        if let Some(prompt) = &mut self.agents.mode_selection {
            let count = prompt.options.len().max(1) as isize;
            prompt.selected = (prompt.selected as isize + delta).rem_euclid(count) as usize;
        }
    }

    /// Moves the permission option selection by `delta`.
    fn move_permission_selection(&mut self, delta: isize) {
        if let Some(prompt) = self.agents.permission_mut() {
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

    /// Scrolls the active transcript by visual rows (`delta`: negative = up).
    pub(crate) fn agents_scroll(&mut self, delta: isize) {
        let max_scroll = self.agents_transcript_scroll_max();
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].scroll_by(delta, max_scroll);
        }
    }

    /// Jumps to a fixed visual-row offset.
    fn agents_scroll_to(&mut self, offset: usize) {
        let max_scroll = self.agents_transcript_scroll_max();
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].scroll_to(offset, max_scroll);
        }
    }

    /// Pins transcript to newest rendered row.
    fn agents_scroll_to_bottom(&mut self) {
        let max_scroll = self.agents_transcript_scroll_max();
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].scroll_to(max_scroll, max_scroll);
        }
    }

    /// Clears the composer draft (Ctrl-U).
    pub(super) fn agents_clear_draft(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.draft.clear();
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
        } else {
            self.agents.pending_draft.clear();
        }
    }

    fn agents_navigate_prompt_history(&mut self, delta: isize) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        if thread.prompt_history.is_empty() {
            self.backend.status_message = Some(String::from("prompt history is empty"));
            return;
        }
        let next = match thread.prompt_history_cursor {
            None if delta < 0 => {
                thread.prompt_history_restore_draft = Some(thread.draft.clone());
                Some(thread.prompt_history.len() - 1)
            }
            None => None,
            Some(current) => {
                let candidate = current as isize + delta;
                if candidate < 0 {
                    Some(0)
                } else if candidate >= thread.prompt_history.len() as isize {
                    thread.draft = thread.prompt_history_restore_draft.take().unwrap_or_default();
                    None
                } else {
                    Some(candidate as usize)
                }
            }
        };
        thread.prompt_history_cursor = next;
        if let Some(index) = next {
            thread.draft = thread.prompt_history[index].clone();
        }
    }

    fn agents_reverse_prompt_history_search(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            return;
        };
        let thread = &mut self.agents.threads[active];
        let query = thread.draft.clone();
        let upper_bound = thread.prompt_history_cursor.unwrap_or(thread.prompt_history.len());
        let found = thread.prompt_history[..upper_bound]
            .iter()
            .rposition(|entry| query.is_empty() || entry.contains(&query));
        match found {
            Some(index) => {
                if thread.prompt_history_cursor.is_none() {
                    thread.prompt_history_restore_draft = Some(query);
                }
                thread.prompt_history_cursor = Some(index);
                thread.draft = thread.prompt_history[index].clone();
                self.backend.status_message =
                    Some(format!("history search: {}/{}", index + 1, thread.prompt_history.len()));
            }
            None => {
                self.backend.status_message = Some(String::from("no earlier prompt-history match"))
            }
        }
    }

    /// Completes one trailing `@workspace/path` token when exactly one safe
    /// workspace file matches. Completion never reads or attaches file content.
    fn agents_complete_mention_path(&mut self) -> bool {
        const MENTION_COMPLETION_SCAN_MAX: usize = 1_024;
        let draft = self
            .agents
            .active_thread_index()
            .and_then(|active| self.agents.threads.get(active).map(|thread| thread.draft.clone()))
            .unwrap_or_else(|| self.agents.pending_draft.clone());
        let token_start =
            draft.rfind(char::is_whitespace).map_or(0, |index| index.saturating_add(1));
        let Some(partial) = draft.get(token_start..).and_then(|token| token.strip_prefix('@'))
        else {
            return false;
        };
        if partial.is_empty() || partial.contains(['\\', '\n', '\r']) {
            return false;
        }
        let Ok(root) = std::fs::canonicalize(&self.working_dir) else {
            return false;
        };
        let mut matches = Vec::new();
        for entry in WalkBuilder::new(&root)
            .max_depth(Some(12))
            .standard_filters(true)
            .build()
            .filter_map(Result::ok)
            .take(MENTION_COMPLETION_SCAN_MAX)
        {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&root) else {
                continue;
            };
            let relative = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if relative.is_empty()
                || !relative.starts_with(partial)
                || is_protected_relative_path(&relative)
                || self.is_secret_store_path(entry.path())
            {
                continue;
            }
            matches.push(relative);
            if matches.len() > 1 {
                break;
            }
        }
        let Some(completed) = (matches.len() == 1).then(|| matches.remove(0)) else {
            if matches.len() > 1 {
                self.backend.status_message = Some(String::from("mention completion is ambiguous"));
            }
            return false;
        };
        let replacement = format!("@{completed}");
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].draft.replace_range(token_start.., &replacement);
        } else {
            self.agents.pending_draft.replace_range(token_start.., &replacement);
        }
        self.backend.status_message = Some(format!("mention path completed: {completed}"));
        true
    }

    fn agents_draft_backspace(&mut self) {
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &mut self.agents.threads[active];
            thread.prompt_history_cursor = None;
            thread.prompt_history_restore_draft = None;
            thread.draft.pop();
        } else {
            self.agents.pending_draft.pop();
        }
    }

    fn agents_elicitation_type(&mut self, c: char) {
        if let Some(prompt) = self.agents.elicitation_mut()
            && let Some(field) = prompt.fields.get_mut(prompt.selected_field)
        {
            match &mut field.value {
                ElicitationFieldValue::Text(text) if c != '\n' => {
                    text.push(c);
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
        if let Some(prompt) = self.agents.elicitation_mut()
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
