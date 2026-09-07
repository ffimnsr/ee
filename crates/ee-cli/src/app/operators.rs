//! `impl App` methods: operators domain.
use super::*;

impl App {
    pub(super) fn handle_default(&mut self, key: KeyEvent) {
        // Cancel operator-pending on Escape
        if key.code == KeyCode::Esc && self.mode == Mode::OperatorPending {
            self.enter_normal_mode();
            self.input_state.reset();
            return;
        }

        let ch = match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                c
            }
            KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return;
            }
            _ => return,
        };

        match self.mode {
            Mode::OperatorPending => {
                self.handle_operator_pending(ch);
            }
            Mode::Insert => {
                let s = ch.to_string();
                self.insert_buffer.push(ch);
                if self.try_vlf_insert_text(&s) {
                    return;
                }
                let _ = self.backend.send_edit("insert", json!({ "chars": s }));
            }
            Mode::CommandLine => {
                // Any typed char resets history navigation.
                self.history_idx = None;
                self.command_buffer.push(ch);
            }
            Mode::Normal => {
                if let Some(find) = self.input_state.pending_find.take() {
                    let count = self.input_state.count();
                    self.input_state.reset();
                    for _ in 0..count {
                        self.jump_to_char(ch, find.forward, find.inclusive);
                    }
                    return;
                }
                if ch == '0' {
                    if self.input_state.count_digits.is_empty() {
                        if !self.handle_vlf_navigation("move_to_left_end_of_line", 1) {
                            let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                        }
                        self.input_state.reset();
                    } else {
                        self.input_state.count_digits.push(0);
                    }
                    return;
                }
                if let Some(digit) = ch.to_digit(10) {
                    self.input_state.count_digits.push(digit as u8);
                }
            }
            Mode::Search => {
                self.command_buffer.push(ch);
                let chars = self.command_buffer.clone();
                let case_sensitive = smart_case_sensitive(&chars);
                let _ = self.backend.send_edit(
                    "find",
                    json!({
                        "chars": chars,
                        "case_sensitive": case_sensitive,
                        "regex": false,
                        "whole_words": false
                    }),
                );
            }
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                self.handle_visual_char(ch);
            }
            // Context-specific pseudo-modes are intercepted before `handle_default`.
            Mode::Picker | Mode::Quickfix | Mode::LocationList => {}
            // SubstituteConfirm/PrivilegeConfirm only accept dedicated keys.
            Mode::SubstituteConfirm | Mode::PrivilegeConfirm => {}
            // Agents pane keys are intercepted before `handle_default`.
            Mode::Agent => {}
        }
    }
    pub(super) fn handle_mouse_event(&mut self, m: MouseEvent) {
        let Ok((width, height)) = crossterm::terminal::size() else {
            return;
        };
        self.handle_mouse_event_in_area(m, Rect { x: 0, y: 0, width, height });
    }
    pub(crate) fn handle_mouse_event_in_area(&mut self, m: MouseEvent, area: Rect) {
        #[cfg(feature = "agents")]
        if let Some(pane) = crate::ui::agents_pane_rect_for(area, self)
            && pane.contains(ratatui::layout::Position { x: m.column, y: m.row })
        {
            match m.kind {
                MouseEventKind::ScrollUp => {
                    self.agents_scroll(-1);
                }
                MouseEventKind::ScrollDown => {
                    self.agents_scroll(1);
                }
                _ => {}
            }
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => {
                let _ = self.backend.send_edit("scroll_up", json!([]));
            }
            MouseEventKind::ScrollDown => {
                let _ = self.backend.send_edit("scroll_down", json!([]));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((row, col)) = crate::ui::hit_test_buffer_cell(area, self, m.column, m.row)
                else {
                    return;
                };
                let line_count = self.backend.line_count();
                if row < line_count {
                    let byte_col = if let Some(line) = self.backend.get_line(row) {
                        crate::text::display_col_to_byte(line, col)
                    } else {
                        0
                    };
                    let _ = self.backend.send_edit(
                        "gesture",
                        json!({
                            "line": row as u64,
                            "col": byte_col as u64,
                            "ty": {
                                "select": {
                                    "granularity": "point",
                                    "multi": false
                                }
                            }
                        }),
                    );
                    // Exit any special mode on click.
                    if !matches!(self.mode, Mode::Normal | Mode::Insert) {
                        self.enter_normal_mode();
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((row, col)) = crate::ui::hit_test_buffer_cell(area, self, m.column, m.row)
                else {
                    return;
                };
                let line_count = self.backend.line_count();
                if row < line_count {
                    let byte_col = if let Some(line) = self.backend.get_line(row) {
                        crate::text::display_col_to_byte(line, col)
                    } else {
                        0
                    };
                    let _ = self.backend.send_edit(
                        "gesture",
                        json!({
                            "line": row as u64,
                            "col": byte_col as u64,
                            "ty": "drag"
                        }),
                    );
                }
            }
            _ => {}
        }
    }
    pub(super) fn handle_paste(&mut self, text: String) {
        let text = normalize_pasted_line_endings(text);
        match self.mode {
            Mode::Insert => {
                self.insert_buffer.push_str(&text);
                if self.try_vlf_insert_text(&text) {
                    return;
                }
                let _ = self.backend.send_edit("paste", json!({ "chars": text }));
            }
            Mode::CommandLine | Mode::Search => {
                // Paste into the command/search buffer.
                self.command_buffer.push_str(&text);
                if self.mode == Mode::Search {
                    let chars = self.command_buffer.clone();
                    let case_sensitive = smart_case_sensitive(&chars);
                    let _ = self.backend.send_edit(
                        "find",
                        json!({
                            "chars": chars,
                            "case_sensitive": case_sensitive,
                            "regex": false,
                            "whole_words": false
                        }),
                    );
                }
            }
            Mode::Normal => {
                // In normal mode enter insert and paste the text, like pressing `i` then typing.
                self.mode = Mode::Insert;
                self.insert_buffer.push_str(&text);
                if self.try_vlf_insert_text(&text) {
                    return;
                }
                let _ = self.backend.send_edit("paste", json!({ "chars": text }));
            }
            #[cfg(feature = "agents")]
            Mode::Agent => {
                // Paste into the agents composer draft.
                self.agents_append_draft(&text);
            }
            _ => {}
        }
    }
    pub(super) fn enter_normal_mode(&mut self) {
        if self.mode == Mode::Insert {
            // Flush accumulated block insert if pending.
            if self.block_insert.is_some() {
                self.apply_block_insert();
            }
            // Record insert mode text for `.` repeat.
            if !self.insert_buffer.is_empty() {
                self.last_change = Some(LastChange::Insert(self.insert_buffer.clone()));
                self.push_change();
            }
            self.insert_buffer.clear();
        }
        // Save visual selection for `gv`.
        if self.mode.is_visual() {
            if let Some((al, ac)) = self.visual_anchor {
                self.last_visual = Some((self.mode, al, ac));
            }
            self.visual_anchor = None;
            if self.backend.is_vlf {
                if let Some((line, col)) = self.visual_restore_cursor.take() {
                    self.backend.cursor_line =
                        line.min(self.backend.line_count().saturating_sub(1));
                    let max_col = self
                        .backend
                        .get_line(self.backend.cursor_line)
                        .map(str::len)
                        .unwrap_or(col);
                    self.backend.cursor_col = col.min(max_col);
                }
            } else {
                self.visual_restore_cursor = None;
            }
            if !self.backend.is_vlf {
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
        }
        self.mode = Mode::Normal;
        self.command_buffer.clear();
        self.swift_motion = None;
    }
    /// Apply operator to the current xi selection, then return to Normal (or
    /// Insert for Change).  Resets input state before returning.
    pub(super) fn apply_operator(&mut self, op: Operator) {
        match op {
            Operator::Delete => {
                let reg = self.take_register();
                let text = self.selected_text_preview(false);
                self.registers.delete(&reg, text, false);
                self.record_edit("delete_forward", json!([]));
                self.push_change();
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Change => {
                let reg = self.take_register();
                let text = self.selected_text_preview(false);
                self.registers.delete(&reg, text, false);
                self.record_edit("delete_forward", json!([]));
                self.push_change();
                self.input_state.reset();
                self.enter_normal_mode();
                self.mode = Mode::Insert;
            }
            Operator::Yank => {
                let reg = self.take_register();
                let text = self.selected_text_preview(false);
                self.registers.yank(&reg, text, false);
                // Collapse selection without modifying buffer.
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Indent => {
                self.record_edit("indent", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Outdent => {
                self.record_edit("outdent", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Uppercase => {
                self.record_edit("uppercase", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::Lowercase => {
                self.record_edit("lowercase", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
            Operator::CaseToggle => {
                // xi has no char-level toggle; capitalize (first letter of each word)
                // is the closest available primitive.
                self.record_edit("capitalize", json!([]));
                self.push_change();
                let _ = self.backend.send_edit("collapse_selections", json!([]));
                self.input_state.reset();
                self.enter_normal_mode();
            }
        }
    }
    /// Apply operator to the current line (double-operator: dd, cc, yy, >>, <<, …).
    pub(super) fn apply_operator_to_line(&mut self, op: Operator) {
        match op {
            Operator::Delete => {
                // Move to start, select to end, delete line content, then delete newline.
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
            }
            Operator::Change => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
            }
            Operator::Yank => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("delete_forward", json!([]));
                let _ = self.backend.send_edit("yank", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            Operator::Indent => {
                let _ = self.backend.send_edit("indent", json!([]));
            }
            Operator::Outdent => {
                let _ = self.backend.send_edit("outdent", json!([]));
            }
            Operator::Uppercase => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("uppercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            Operator::Lowercase => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("lowercase", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            Operator::CaseToggle => {
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                let _ = self.backend.send_edit("capitalize", json!([]));
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
        }
        if op == Operator::Change {
            self.input_state.reset();
            self.enter_normal_mode();
            self.mode = Mode::Insert;
        } else {
            self.input_state.reset();
            self.enter_normal_mode();
        }
    }
    /// Handle a char in operator-pending mode.
    pub(super) fn handle_operator_pending(&mut self, ch: char) {
        let op = match self.input_state.pending_operator {
            Some(op) => op,
            None => {
                self.enter_normal_mode();
                self.input_state.reset();
                return;
            }
        };

        // Priority 1: consume a pending char-find target.
        if let Some(find) = self.input_state.pending_find.take() {
            let count = self.input_state.count();
            self.input_state.reset();
            for _ in 0..count {
                self.jump_to_char_selecting(ch, find.forward, find.inclusive);
            }
            self.apply_operator(op);
            return;
        }

        // Priority 2: consume a text-object specifier (iw, aw, i", …).
        if let Some(inclusive) = self.input_state.text_obj_inclusive.take() {
            self.apply_text_object_operator(op, inclusive, ch);
            return;
        }

        let count = self.input_state.count();

        // Priority 3: double operator means "act on whole line".
        let is_double = matches!(
            (op, ch),
            (Operator::Delete, 'd')
                | (Operator::Change, 'c')
                | (Operator::Yank, 'y')
                | (Operator::Indent, '>')
                | (Operator::Outdent, '<')
                | (Operator::Uppercase, 'U')
                | (Operator::Lowercase, 'u')
                | (Operator::CaseToggle, '~')
        );
        if is_double {
            for _ in 0..count {
                self.apply_operator_to_line(op);
            }
            return;
        }

        // Priority 4: text-object prefix.
        if ch == 'i' && self.input_state.prefix.is_none() {
            self.input_state.text_obj_inclusive = Some(false);
            return;
        }
        if ch == 'a' && self.input_state.prefix.is_none() {
            self.input_state.text_obj_inclusive = Some(true);
            return;
        }

        // Priority 5: 'g' prefix for gg motion.
        if ch == 'g' && self.input_state.prefix.is_none() {
            self.input_state.prefix = Some('g');
            return;
        }

        // Priority 6: count digits.
        if ch.is_ascii_digit() {
            let d = ch as u8 - b'0';
            if d > 0 || !self.input_state.count_digits.is_empty() {
                self.input_state.count_digits.push(d);
                return;
            }
        }

        // Priority 7: '0' as line-start motion when no count is active.
        if ch == '0' && self.input_state.count_digits.is_empty() {
            let _ =
                self.backend.send_edit("move_to_left_end_of_line_and_modify_selection", json!([]));
            self.apply_operator(op);
            return;
        }

        // Priority 8: char-find operators (f/F/t/T).
        match ch {
            'f' | 'F' | 't' | 'T' => {
                let forward = matches!(ch, 'f' | 't');
                let inclusive = matches!(ch, 'f' | 'F');
                self.input_state.prefix = None;
                self.input_state.pending_find = Some(PendingCharFind { forward, inclusive });
                return;
            }
            _ => {}
        }

        // Priority 9: motions that extend selection.
        let motion_cmd = match (ch, self.input_state.prefix) {
            ('h', None) => Some("move_left_and_modify_selection"),
            ('l', None) => Some("move_right_and_modify_selection"),
            ('j', None) => Some("move_down_and_modify_selection"),
            ('k', None) => Some("move_up_and_modify_selection"),
            ('w', None) | ('e', None) => Some("move_word_right_and_modify_selection"),
            ('b', None) => Some("move_word_left_and_modify_selection"),
            ('$', None) => Some("move_to_right_end_of_line_and_modify_selection"),
            ('^', None) => Some("move_to_beginning_of_paragraph_and_modify_selection"),
            ('G', None) => Some("move_to_end_of_document_and_modify_selection"),
            ('g', Some('g')) => Some("move_to_beginning_of_document_and_modify_selection"),
            _ => None,
        };
        if let Some(cmd) = motion_cmd {
            for _ in 0..count {
                let _ = self.backend.send_edit(cmd, json!([]));
            }
            self.apply_operator(op);
            return;
        }

        // Unknown key – cancel.
        self.enter_normal_mode();
        self.input_state.reset();
    }
    /// Like [`Self::jump_to_char`] but uses `select_extend` so the region from the
    /// current cursor position to the found char is selected (for operators).
    pub(super) fn jump_to_char_selecting(&mut self, target: char, forward: bool, inclusive: bool) {
        let _ = self.backend.find_char(target, forward, inclusive, true);
    }
    pub(super) fn select_range(
        &mut self,
        start_line: usize,
        start_col: usize,
        end_line: usize,
        end_col: usize,
    ) {
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": start_line as u64,
                "col": start_col as u64,
                "ty": { "select": { "granularity": "point", "multi": false } }
            }),
        );
        let _ = self.backend.send_edit(
            "gesture",
            json!({
                "line": end_line as u64,
                "col": end_col as u64,
                "ty": { "select_extend": { "granularity": "point" } }
            }),
        );
    }
    pub(super) fn select_range_on_line(&mut self, line_idx: usize, start: usize, end: usize) {
        self.select_range(line_idx, start, line_idx, end);
    }
    /// Select `(start, end)` byte range on `line_idx` and apply the operator.
    pub(super) fn select_range_and_apply(
        &mut self,
        line_idx: usize,
        start: usize,
        end: usize,
        op: Operator,
    ) {
        self.select_range_on_line(line_idx, start, end);
        self.apply_operator(op);
    }
    pub(super) fn line_text_object_range(
        &self,
        inclusive: bool,
        spec: char,
    ) -> Option<(usize, usize, usize, String)> {
        let line_idx = self.backend.cursor_line;
        let line = self.backend.get_line(line_idx)?.to_owned();
        let cursor_byte = self.backend.cursor_col.min(line.len());
        let range = match spec {
            'w' | 'W' => {
                let big_word = spec == 'W';
                text_obj_word(&line, cursor_byte, inclusive, big_word)
            }
            '"' | '\'' | '`' => text_obj_quote(&line, cursor_byte, spec, inclusive),
            '(' | ')' | 'b' => text_obj_bracket(&line, cursor_byte, '(', ')', inclusive),
            '[' | ']' => text_obj_bracket(&line, cursor_byte, '[', ']', inclusive),
            '{' | '}' | 'B' => text_obj_bracket(&line, cursor_byte, '{', '}', inclusive),
            '<' | '>' => text_obj_bracket(&line, cursor_byte, '<', '>', inclusive),
            't' => text_obj_tag(&line, cursor_byte, inclusive),
            _ => None,
        }?;
        Some((line_idx, range.0, range.1, line))
    }
    pub(super) fn select_text_object(&mut self, inclusive: bool, spec: char) -> Result<(), String> {
        match spec {
            'p' => {
                let _ = self.backend.send_edit("move_to_beginning_of_paragraph", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_end_of_paragraph_and_modify_selection", json!([]));
                Ok(())
            }
            's' => {
                let line_idx = self.backend.cursor_line;
                let line_len = self.backend.line_len(line_idx).unwrap_or(0);
                self.select_range_on_line(line_idx, 0, line_len);
                Ok(())
            }
            _ => {
                let Some((line_idx, start, end, _)) = self.line_text_object_range(inclusive, spec)
                else {
                    return Err(format!("textobject: unsupported specifier `{spec}`"));
                };
                self.select_range_on_line(line_idx, start, end);
                Ok(())
            }
        }
    }
    pub(super) fn line_ending_str(&self) -> &'static str {
        match self.config.end_of_line {
            crate::config::EndOfLine::Lf => "\n",
            crate::config::EndOfLine::CrLf => "\r\n",
            crate::config::EndOfLine::Cr => "\r",
        }
    }
    pub(super) fn move_current_line_adjacent(&mut self, down: bool) -> Result<(), String> {
        let current_line = self.backend.cursor_line;
        let Some(current_text) = self.backend.get_line(current_line).map(str::to_owned) else {
            return Err("move_line: cursor is outside buffer".to_owned());
        };

        let Some(target_line) =
            (if down { current_line.checked_add(1) } else { current_line.checked_sub(1) })
        else {
            return Err(if down {
                "move_line_down: already at last line".to_owned()
            } else {
                "move_line_up: already at first line".to_owned()
            });
        };

        let Some(target_text) = self.backend.get_line(target_line).map(str::to_owned) else {
            return Err(if down {
                "move_line_down: already at last line".to_owned()
            } else {
                "move_line_up: already at first line".to_owned()
            });
        };

        let start_line = current_line.min(target_line);
        let end_line = current_line.max(target_line);
        let end_col = self.backend.line_len(end_line).unwrap_or(0);
        let replacement = if down {
            format!("{target_text}{}{current_text}", self.line_ending_str())
        } else {
            format!("{current_text}{}{target_text}", self.line_ending_str())
        };

        self.begin_record();
        self.select_range(start_line, 0, end_line, end_col);
        self.record_edit("insert", json!({ "chars": replacement }));
        self.end_record();

        let cursor_col = self.backend.cursor_col.min(current_text.len());
        self.move_cursor_to(target_line, cursor_col);
        self.push_change();
        Ok(())
    }
    pub(super) fn current_surrounding_range(&self) -> Option<(usize, usize, usize, String)> {
        fn take_best(
            best: &mut Option<(usize, usize, String)>,
            line: &str,
            start: usize,
            end: usize,
        ) {
            let Some(inner) = line.get(start + 1..end.saturating_sub(1)) else { return };
            let replace = match best.as_ref() {
                Some((best_start, best_end, _)) => end - start < best_end - best_start,
                None => true,
            };
            if replace {
                *best = Some((start, end, inner.to_owned()));
            }
        }

        let line_idx = self.backend.cursor_line;
        let line = self.backend.get_line(line_idx)?.to_owned();
        let cursor_byte = self.backend.cursor_col.min(line.len());
        let mut best = None;

        if let Some((start, end)) = text_obj_quote(&line, cursor_byte, '"', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_quote(&line, cursor_byte, '\'', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_quote(&line, cursor_byte, '`', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_bracket(&line, cursor_byte, '(', ')', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_bracket(&line, cursor_byte, '[', ']', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_bracket(&line, cursor_byte, '{', '}', true) {
            take_best(&mut best, &line, start, end);
        }
        if let Some((start, end)) = text_obj_bracket(&line, cursor_byte, '<', '>', true) {
            take_best(&mut best, &line, start, end);
        }

        let (start, end, inner) = best?;
        Some((line_idx, start, end, inner))
    }
    pub(super) fn surround_add(
        &mut self,
        pair_spec: &str,
        textobject: Option<char>,
    ) -> Result<(), String> {
        let Some((open, close)) = parse_surround_pair(pair_spec) else {
            return Err(format!("surround_add: unsupported pair `{pair_spec}`"));
        };

        if let Some(spec) = textobject {
            let Some((line_idx, start, end, line)) = self.line_text_object_range(false, spec)
            else {
                return Err(format!("surround_add: unsupported textobject `{spec}`"));
            };
            let Some(selected) = line.get(start..end) else {
                return Err("surround_add: invalid textobject range".to_owned());
            };
            self.begin_record();
            self.select_range_on_line(line_idx, start, end);
            self.record_edit("insert", json!({ "chars": format!("{open}{selected}{close}") }));
            self.end_record();
            self.push_change();
            return Ok(());
        }

        let selected = self
            .backend
            .selected_text_preview(false)
            .map_err(|err| format!("surround_add failed: {err}"))?;
        if selected.is_empty() {
            return Err("surround_add: usage: :surround_add <pair> [textobject]".to_owned());
        }

        self.begin_record();
        self.record_edit("insert", json!({ "chars": format!("{open}{selected}{close}") }));
        self.end_record();
        self.push_change();
        Ok(())
    }
    pub(super) fn surround_replace(&mut self, pair_spec: &str) -> Result<(), String> {
        let Some((open, close)) = parse_surround_pair(pair_spec) else {
            return Err(format!("surround_replace: unsupported pair `{pair_spec}`"));
        };
        let Some((line_idx, start, end, inner)) = self.current_surrounding_range() else {
            return Err("surround_replace: no surrounding pair at cursor".to_owned());
        };

        self.begin_record();
        self.select_range_on_line(line_idx, start, end);
        self.record_edit("insert", json!({ "chars": format!("{open}{inner}{close}") }));
        self.end_record();
        self.push_change();
        Ok(())
    }
    pub(super) fn surround_delete(&mut self) -> Result<(), String> {
        let Some((line_idx, start, end, inner)) = self.current_surrounding_range() else {
            return Err("surround_delete: no surrounding pair at cursor".to_owned());
        };

        self.begin_record();
        self.select_range_on_line(line_idx, start, end);
        self.record_edit("insert", json!({ "chars": inner }));
        self.end_record();
        self.push_change();
        Ok(())
    }
    /// Apply operator to the text object specified by `inclusive`+`spec`.
    pub(super) fn apply_text_object_operator(&mut self, op: Operator, inclusive: bool, spec: char) {
        let range = match spec {
            'p' => {
                // Paragraph: use xi's paragraph motions instead of byte range.
                let _ = self.backend.send_edit("move_to_beginning_of_paragraph", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_end_of_paragraph_and_modify_selection", json!([]));
                self.apply_operator(op);
                return;
            }
            's' => {
                // Sentence: treat as current line for simplicity.
                let _ = self.backend.send_edit("move_to_left_end_of_line", json!([]));
                let _ = self
                    .backend
                    .send_edit("move_to_right_end_of_line_and_modify_selection", json!([]));
                self.apply_operator(op);
                return;
            }
            _ => {
                self.line_text_object_range(inclusive, spec).map(|(_, start, end, _)| (start, end))
            }
        };

        match range {
            Some((start, end)) => {
                let line_idx = self.backend.cursor_line;
                self.select_range_and_apply(line_idx, start, end, op)
            }
            None => {
                self.enter_normal_mode();
                self.input_state.reset();
            }
        }
    }
}
