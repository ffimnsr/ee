use crate::line_offset::{LineOffset, LogicalLines};
use crate::object::{self, SyntaxNavigationAction, SyntaxSelectionAction};
use crate::selection::{SelRegion, Selection};
use crate::tree_sitter_support::syntax_feature_availability;
use xi_rope::Rope;

use super::EventContext;

impl<'a> EventContext<'a> {
    // ── Syntax navigation / selection ──

    pub(super) fn do_syntax_selection(&mut self, action: SyntaxSelectionAction) {
        let language = self.language.clone();
        let file_path = self.info.map(|info| info.path.clone());
        let mode = self.editor.borrow().document_mode();
        let capabilities =
            syntax_feature_availability(Some(language.as_ref()), file_path.as_deref(), mode);
        if !capabilities.semantic_motions {
            self.client.alert(format!(
                "{}: {}",
                action.method_name(),
                object::SyntaxSelectionError::SyntaxTreeUnavailable.message()
            ));
            return;
        }
        let result = if self.editor.borrow().is_vlf() {
            self.do_vlf_syntax_selection(language.as_ref(), file_path.as_deref(), action)
        } else {
            self.with_view(|view, text| {
                let current = view.selection().clone();
                object::apply_syntax_selection(
                    text,
                    &current,
                    view.syntax_selection_history_mut(),
                    language.as_ref(),
                    file_path.as_deref(),
                    action,
                )
                .map(|selection| view.set_selection(text, selection))
            })
        };

        if let Err(err) = result {
            self.client.alert(format!("{}: {}", action.method_name(), err.message()));
        }
    }

    pub(super) fn do_syntax_navigation(&mut self, action: SyntaxNavigationAction) {
        let language = self.language.clone();
        let file_path = self.info.map(|info| info.path.clone());
        let mode = self.editor.borrow().document_mode();
        let capabilities =
            syntax_feature_availability(Some(language.as_ref()), file_path.as_deref(), mode);
        if !capabilities.semantic_motions {
            self.client.alert(format!(
                "{}: {}",
                action.method_name(),
                object::SyntaxSelectionError::SyntaxTreeUnavailable.message()
            ));
            return;
        }
        let result = if self.editor.borrow().is_vlf() {
            self.do_vlf_syntax_navigation(language.as_ref(), file_path.as_deref(), action)
        } else {
            self.with_view(|view, text| {
                let current = view.selection().clone();
                object::apply_syntax_navigation(
                    text,
                    &current,
                    language.as_ref(),
                    file_path.as_deref(),
                    action,
                )
                .map(|selection| view.set_selection(text, selection))
            })
        };

        if let Err(err) = result {
            self.client.alert(format!("{}: {}", action.method_name(), err.message()));
        }
    }

    // ── Paragraph movement ──

    pub(super) fn do_goto_paragraph(&mut self, forward: bool) {
        self.with_view(|view, text| {
            let current = view.selection().clone();
            let next = Self::paragraph_selection(text, &current, forward);
            view.set_selection(text, next);
        });
    }

    fn paragraph_selection(text: &Rope, current: &Selection, forward: bool) -> Selection {
        let mut selection = Selection::new();
        let last_line = LogicalLines.line_of_offset(text, text.len());

        for &region in current.iter() {
            let line = LogicalLines.line_of_offset(text, region.end.min(text.len()));
            let target_line = if forward {
                Self::next_paragraph_line(text, line, last_line)
            } else {
                Self::prev_paragraph_line(text, line)
            };
            selection.add_region(SelRegion::caret(LogicalLines.offset_of_line(text, target_line)));
        }

        selection
    }

    fn next_paragraph_line(text: &Rope, current_line: usize, last_line: usize) -> usize {
        let mut line = current_line;
        while line <= last_line && !Self::is_blank_line(text, line) {
            line += 1;
        }
        while line <= last_line && Self::is_blank_line(text, line) {
            line += 1;
        }
        line.min(last_line)
    }

    fn prev_paragraph_line(text: &Rope, current_line: usize) -> usize {
        if current_line == 0 {
            return 0;
        }

        let mut line = current_line;
        while line > 0 && Self::is_blank_line(text, line) {
            line -= 1;
        }
        while line > 0 && !Self::is_blank_line(text, line - 1) {
            line -= 1;
        }
        if line == 0 {
            return 0;
        }

        line -= 1;
        while line > 0 && Self::is_blank_line(text, line) {
            line -= 1;
        }
        while line > 0 && !Self::is_blank_line(text, line - 1) {
            line -= 1;
        }
        line
    }

    fn is_blank_line(text: &Rope, line: usize) -> bool {
        let start = LogicalLines.offset_of_line(text, line).min(text.len());
        let end = LogicalLines.offset_of_line(text, line + 1).min(text.len());
        text.slice_to_cow(start..end).trim().is_empty()
    }

    // ── Word / Char / Bracket movement ──

    pub(super) fn do_move_word_start(
        &mut self,
        forward: bool,
        long_word: bool,
        modify_selection: bool,
    ) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = super::move_word_start_selection(
                text,
                view.sel_regions(),
                forward,
                long_word,
                modify_selection,
            );
            (!selection.is_empty()).then_some(selection)
        })
    }

    pub(super) fn do_move_word_end(
        &mut self,
        long_word: bool,
        modify_selection: bool,
    ) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = super::move_word_end_selection(
                text,
                view.sel_regions(),
                long_word,
                modify_selection,
            );
            (!selection.is_empty()).then_some(selection)
        })
    }

    pub(super) fn do_find_char(
        &mut self,
        target: char,
        forward: bool,
        inclusive: bool,
        modify_selection: bool,
    ) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = super::find_char_selection(
                text,
                view.sel_regions(),
                target,
                forward,
                inclusive,
                modify_selection,
            );
            (!selection.is_empty()).then_some(selection)
        })
    }

    pub(super) fn do_move_to_matching_bracket(
        &mut self,
        modify_selection: bool,
    ) -> Option<Selection> {
        self.with_view(|view, text| {
            let selection = super::move_to_matching_bracket_selection(
                text,
                view.sel_regions(),
                modify_selection,
            );
            (!selection.is_empty()).then_some(selection)
        })
    }
}
