//! `impl App` methods: swift_motion domain.
use super::*;

impl App {
    pub(super) fn start_swift_motion(&mut self) {
        if self.mode != Mode::Normal {
            self.enter_normal_mode();
        }
        self.input_state.reset();
        self.command_buffer.clear();
        self.swift_motion = Some(SwiftMotionState::default());
    }
    pub(super) fn handle_swift_motion_key(&mut self, key: KeyEvent) -> bool {
        if self.swift_motion.is_none() {
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                self.swift_motion = None;
                self.backend.status_message = Some("swift_motion: cancelled".to_owned());
                true
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.advance_swift_motion(ch);
                true
            }
            _ => true,
        }
    }
    pub(super) fn advance_swift_motion(&mut self, typed: char) {
        let typed = typed.to_ascii_lowercase();

        if self.swift_motion.as_ref().is_some_and(|state| !state.targets.is_empty()) {
            self.advance_swift_motion_label(typed);
            return;
        }

        if !typed.is_ascii_graphic() {
            self.swift_motion = None;
            self.backend.status_message =
                Some("swift_motion: requires printable ASCII query".to_owned());
            return;
        }

        let mut query = match self.swift_motion.as_ref() {
            Some(state) => state.query.clone(),
            None => return,
        };
        query.push(typed);

        if query.chars().count() < 2 {
            if let Some(state) = self.swift_motion.as_mut() {
                state.query = query;
            }
            return;
        }

        let mut targets = self.collect_swift_motion_targets(&query);
        if targets.is_empty() {
            self.swift_motion = None;
            self.backend.status_message =
                Some(format!("swift_motion: no visible matches for `{query}`"));
            return;
        }
        if targets.len() == 1 {
            let target = targets.remove(0);
            self.swift_motion = None;
            self.jump_to_display_position(target.line, target.display_col);
            return;
        }
        if targets.len() > SWIFT_MOTION_MAX_TARGETS {
            let count = targets.len();
            self.swift_motion = None;
            self.backend.status_message =
                Some(format!("swift_motion: {count} visible matches for `{query}`; narrow query"));
            return;
        }

        self.assign_swift_motion_labels(&mut targets);

        self.swift_motion = Some(SwiftMotionState { query, label_prefix: None, targets });
    }
    pub(super) fn advance_swift_motion_label(&mut self, typed: char) {
        let Some(state) = self.swift_motion.as_ref() else { return };

        if state.label_prefix.is_some() {
            let target = state.targets.iter().find(|target| target.label == typed).cloned();
            if let Some(target) = target {
                self.swift_motion = None;
                self.jump_to_display_position(target.line, target.display_col);
            } else {
                self.backend.status_message =
                    Some(format!("swift_motion: unknown label `{typed}`"));
            }
            return;
        }

        let mut narrowed = state
            .targets
            .iter()
            .filter(|target| target.label == typed)
            .cloned()
            .collect::<Vec<_>>();
        if narrowed.is_empty() {
            self.backend.status_message = Some(format!("swift_motion: unknown label `{typed}`"));
            return;
        }
        if narrowed.len() == 1 {
            let target = narrowed.remove(0);
            self.swift_motion = None;
            self.jump_to_display_position(target.line, target.display_col);
            return;
        }

        for target in &mut narrowed {
            target.label = target.next_label.unwrap_or(target.label);
            target.next_label = None;
        }

        self.swift_motion = Some(SwiftMotionState {
            query: state.query.clone(),
            label_prefix: Some(typed),
            targets: narrowed,
        });
    }
    pub(super) fn assign_swift_motion_labels(&self, targets: &mut [SwiftMotionTarget]) {
        if targets.len() <= SWIFT_MOTION_LABELS.len() {
            for (target, label) in targets.iter_mut().zip(SWIFT_MOTION_LABELS.iter().copied()) {
                target.label = label as char;
                target.next_label = None;
            }
            return;
        }

        for (index, target) in targets.iter_mut().enumerate() {
            let primary = SWIFT_MOTION_LABELS[index % SWIFT_MOTION_LABELS.len()] as char;
            let secondary = SWIFT_MOTION_LABELS[index / SWIFT_MOTION_LABELS.len()] as char;
            target.label = primary;
            target.next_label = Some(secondary);
        }
    }
    pub(super) fn collect_swift_motion_targets(&self, query: &str) -> Vec<SwiftMotionTarget> {
        let buf = self.backend.active();
        let visible_rows = self.last_editor_height.max(1);
        let left = self.viewport.left_col;
        let width = if self.config.wrap_lines { usize::MAX } else { self.last_editor_width.max(1) };
        let right = left.saturating_add(width);
        let mut line_index = self.viewport.top_line;
        let mut rendered = 0usize;
        let mut targets = Vec::new();

        while rendered < visible_rows && line_index < buf.line_count() {
            if let Some(line) = buf.get_line(line_index) {
                let mut search_from = 0usize;
                while search_from < line.len() {
                    let Some(found) = line[search_from..].find(query) else { break };
                    let byte_idx = search_from + found;
                    let Some(first_char) = line[byte_idx..].chars().next() else { break };
                    let advance = first_char.len_utf8().max(1);
                    if !first_char.is_ascii() {
                        search_from = byte_idx.saturating_add(advance);
                        continue;
                    }

                    let display_col = byte_col_to_display_col(line, byte_idx);
                    let end_display_col = byte_col_to_display_col(line, byte_idx + query.len());
                    let visible = self.config.wrap_lines
                        || (display_col >= left && display_col < right && end_display_col > left);
                    if visible {
                        targets.push(SwiftMotionTarget {
                            line: line_index,
                            display_col,
                            end_display_col,
                            label: '\0',
                            next_label: None,
                        });
                        if targets.len() > SWIFT_MOTION_MAX_TARGETS {
                            return targets;
                        }
                    }

                    search_from = byte_idx.saturating_add(advance);
                }
            }

            rendered += 1;
            if let Some((_, end)) = self.folds.fold_at(buf.id, line_index) {
                line_index = end + 1;
            } else {
                line_index += 1;
            }
        }

        targets
    }
    pub(super) fn jump_to_display_position(&mut self, line: usize, display_col: usize) {
        self.jump_to_line(line);
        if display_col > 0 {
            self.goto_column(display_col);
        }
    }
}
