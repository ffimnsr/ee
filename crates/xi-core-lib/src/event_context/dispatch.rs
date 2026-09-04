use log::warn;
use serde_json::Value;
use xi_rope::{LinesMetric, Rope, RopeDelta};

use crate::edit_types::{BufferEvent, EventDomain, SpecialEvent};
use crate::editor::Editor;
use crate::fold_support::{fold_parse_timeout, fold_ranges_for_text};
use crate::indent::SyntaxIndentContext;
use crate::line_offset::LineOffset;
use crate::rpc::{EditNotification, FoldRangePreview, LineRange};
use crate::selection::{InsertDrift, Selection};
use crate::tabs::RENDER_VIEW_IDLE_MASK;
use crate::tree_sitter_support::syntax_feature_availability;
use crate::view::View;

use super::{BufferItems, EventContext, RENDER_DELAY};

impl<'a> EventContext<'a> {
    /// Executes a closure with mutable references to the editor and the view,
    /// common in edit actions that modify the text.
    pub(crate) fn with_editor<R, F>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Editor, &mut View, &mut Rope, &BufferItems) -> R,
    {
        let mut editor = self.editor.borrow_mut();
        let mut view = self.view.borrow_mut();
        let mut kill_ring = self.kill_ring.borrow_mut();
        f(&mut editor, &mut view, &mut kill_ring, self.config)
    }

    /// Executes a closure with a mutable reference to the view and a reference
    /// to the current text. This is common to most edits that just modify
    /// selection or viewport state.
    pub(crate) fn with_view<R, F>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut View, &Rope) -> R,
    {
        let editor = self.editor.borrow();
        let mut view = self.view.borrow_mut();
        f(&mut view, editor.get_buffer())
    }

    pub(super) fn dispatch_command_to_plugins(&self, method: &str, params: &Value) {
        let mut dispatched = false;
        self.plugins.iter().filter(|plugin| plugin.manifest.supports_command(method)).for_each(
            |plugin| {
                dispatched = true;
                plugin.dispatch_command(self.view_id, method, params);
            },
        );

        if !dispatched {
            warn!("no running plugin registered command {:?}", method);
        }
    }

    /// Dispatches an incoming edit notification from the client, records the
    /// event if recording is active, and triggers a redraw if needed.
    ///
    /// # Preconditions
    ///
    /// The `editor` and `view` `RefCell`s must not be borrowed when this is called.
    pub(crate) fn do_edit(&mut self, cmd: EditNotification) {
        let event: EventDomain = cmd.into();

        let pending_selection = self.dispatch_event(event);
        self.after_edit("core");
        if let Some(selection) = pending_selection {
            self.with_view(|view, text| view.set_selection(text, selection));
        }
        self.render_if_needed();
    }

    pub(crate) fn preview_fold_ranges(
        &self,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Vec<FoldRangePreview> {
        let editor = self.editor.borrow();
        if editor.is_vlf() {
            return Vec::new();
        }

        let text = editor.get_buffer().slice_to_cow(0..editor.get_buffer().len()).into_owned();
        let file_path = self.info.map(|info| info.path.as_path());
        let folds = fold_ranges_for_text(
            Some(self.language.as_ref()),
            file_path,
            &text,
            fold_parse_timeout(text.len()),
        );
        let start_line = start_line.unwrap_or(0);
        let end_line = end_line.unwrap_or(usize::MAX);
        folds
            .into_iter()
            .filter(|fold| fold.header_line >= start_line && fold.header_line <= end_line)
            .map(|fold| FoldRangePreview {
                header_line: fold.header_line,
                body_start: fold.body_start,
                body_end: fold.body_end,
            })
            .collect()
    }

    fn dispatch_event(&mut self, event: EventDomain) -> Option<Selection> {
        use self::EventDomain as E;
        match event {
            E::View(cmd) => {
                if !self.validate_view_event_bounds(&cmd) {
                    return None;
                }

                if self.editor.borrow().is_vlf() {
                    match cmd {
                        crate::edit_types::ViewEvent::Find {
                            chars,
                            case_sensitive,
                            regex,
                            whole_words,
                        } => {
                            self.do_vlf_find(chars, case_sensitive, regex, whole_words);
                            return None;
                        }
                        crate::edit_types::ViewEvent::FindNext { wrap_around, .. } => {
                            self.do_vlf_find_next(false, wrap_around);
                            return None;
                        }
                        crate::edit_types::ViewEvent::FindPrevious { wrap_around, .. } => {
                            self.do_vlf_find_next(true, wrap_around);
                            return None;
                        }
                        crate::edit_types::ViewEvent::MultiFind { queries } => {
                            if let Some(query) = queries.into_iter().next() {
                                self.do_vlf_find(
                                    query.chars,
                                    query.case_sensitive,
                                    query.regex,
                                    query.whole_words,
                                );
                            }
                            return None;
                        }
                        crate::edit_types::ViewEvent::FindAll => {
                            self.client.alert("find_all: unsupported in VLF");
                            return None;
                        }
                        crate::edit_types::ViewEvent::SelectionForFind { .. } => {
                            self.client.alert("selection_for_find: unsupported in VLF");
                            return None;
                        }
                        _ => {}
                    }
                }

                self.with_view(|view, text| view.do_edit(text, cmd));
                self.editor.borrow_mut().update_edit_type();
                if self.with_view(|v, t| v.needs_wrap_in_visible_region(t)) {
                    self.rewrap();
                }
                if self.with_view(|v, _| v.find_in_progress())
                    || self.view.borrow().vlf_find_in_progress()
                {
                    self.do_incremental_find();
                }
                None
            }
            E::Buffer(cmd) => {
                if self.editor.borrow().is_vlf() {
                    let feature = super::vlf_buffer_feature_name(&cmd);
                    let reason = self.vlf_edit_dispatch_reason(feature, false);
                    self.client.alert(reason);
                    return None;
                }
                match cmd {
                    BufferEvent::InsertNewline => {
                        let mode = self.editor.borrow().document_mode();
                        let language = self.language.clone();
                        let file_path = self.info.map(|info| info.path.clone());
                        self.with_editor(|ed, view, _, conf| {
                            let syntax_context = SyntaxIndentContext::new(
                                language.as_ref(),
                                file_path.as_deref(),
                                mode,
                            );
                            ed.do_insert_newline_with_context(view, conf, Some(&syntax_context));
                        });
                    }
                    other => {
                        self.with_editor(|ed, view, k_ring, conf| {
                            ed.do_edit(view, k_ring, conf, other)
                        });
                    }
                }
                None
            }
            E::Special(cmd) => self.do_special(cmd),
        }
    }

    fn do_special(&mut self, cmd: SpecialEvent) -> Option<Selection> {
        if self.editor.borrow().is_vlf()
            && let Some(feature) = super::vlf_special_feature_name(&cmd)
            && !matches!(
                cmd,
                SpecialEvent::VlfViewport { .. } | SpecialEvent::VlfReplaceRange { .. }
            )
        {
            self.client.alert(self.vlf_edit_dispatch_reason(feature, true));
            return None;
        }

        match cmd {
            SpecialEvent::Resize(size) => {
                self.with_view(|view, _| view.set_size(size));
                if self.config.word_wrap {
                    self.update_wrap_settings(false);
                }
                None
            }
            SpecialEvent::RequestLines(LineRange { first, last }) => {
                self.do_request_lines(first as usize, last as usize);
                None
            }
            SpecialEvent::RequestHover { request_id, position } => {
                self.do_request_hover(request_id, position);
                None
            }
            SpecialEvent::DispatchPluginCommand { capability, method, params } => {
                self.dispatch_capability_command(capability, method, &params);
                None
            }
            SpecialEvent::DeleteLineRange { start_line, end_line } => {
                self.do_delete_line_range(start_line, end_line);
                None
            }
            SpecialEvent::DeleteBlock { start_line, end_line, left_col, right_col } => {
                self.do_delete_block(start_line, end_line, left_col, right_col);
                None
            }
            SpecialEvent::ReplayBlockInsert { start_line, end_line, column, text, append } => {
                self.do_replay_block_insert(start_line, end_line, column, &text, append);
                None
            }
            SpecialEvent::ApplyLineReplacements { replacements } => {
                self.do_apply_line_replacements(&replacements);
                None
            }
            SpecialEvent::ReplaceLineRange { start_line, end_line, lines } => {
                self.do_replace_line_range(start_line, end_line, &lines);
                None
            }
            SpecialEvent::SetSelections { selections } => self.do_set_selections(&selections),
            SpecialEvent::GotoColumn { display_col, modify_selection } => {
                self.do_goto_column(display_col, modify_selection)
            }
            SpecialEvent::AddNewlineAbove => self.do_add_newline_above(),
            SpecialEvent::AddNewlineBelow => self.do_add_newline_below(),
            SpecialEvent::JoinSelections { select_space } => self.do_join_selections(select_space),
            SpecialEvent::ExtendLineBelow { count } => self.do_extend_line_below(count),
            SpecialEvent::ExtendLineAbove => self.do_extend_line_above(),
            SpecialEvent::SelectLineAbove => self.do_select_line_above(),
            SpecialEvent::SelectLineBelow => self.do_select_line_below(),
            SpecialEvent::ExtendToLineBounds => self.do_extend_to_line_bounds(),
            SpecialEvent::ShrinkToLineBounds => self.do_shrink_to_line_bounds(),
            SpecialEvent::MoveWordStart { forward, long_word, modify_selection } => {
                self.do_move_word_start(forward, long_word, modify_selection)
            }
            SpecialEvent::MoveWordEnd { long_word, modify_selection } => {
                self.do_move_word_end(long_word, modify_selection)
            }
            SpecialEvent::FindChar { target, forward, inclusive, modify_selection } => {
                self.do_find_char(target, forward, inclusive, modify_selection)
            }
            SpecialEvent::CommitUndoCheckpoint => {
                self.editor.borrow_mut().commit_undo_checkpoint();
                None
            }
            SpecialEvent::MoveToMatchingBracket { modify_selection } => {
                self.do_move_to_matching_bracket(modify_selection)
            }
            SpecialEvent::ToggleComment => {
                self.do_toggle_comment();
                None
            }
            SpecialEvent::ToggleLineComment => {
                self.do_toggle_line_comment();
                None
            }
            SpecialEvent::ToggleBlockComment => {
                self.do_toggle_block_comment();
                None
            }
            SpecialEvent::Reindent => {
                let mode = self.editor.borrow().document_mode();
                let file_path = self.info.map(|info| info.path.as_path());
                let capabilities =
                    syntax_feature_availability(Some(self.language.as_ref()), file_path, mode);
                if !capabilities.reindent && !mode.feature_gates().whole_doc_ops {
                    self.client.alert(format!(
                        "reindent: disabled in {mode:?} mode \
                         (whole-document operations require Normal mode)"
                    ));
                    return None;
                }
                self.begin_async_reindent();
                None
            }
            SpecialEvent::NormalizeLineEndings { .. } => None,
            SpecialEvent::SyntaxSelection(action) => {
                self.do_syntax_selection(action);
                None
            }
            SpecialEvent::SyntaxNavigation(action) => {
                self.do_syntax_navigation(action);
                None
            }
            SpecialEvent::GotoParagraph { forward } => {
                self.do_goto_paragraph(forward);
                None
            }
            SpecialEvent::VlfViewport { line_start, line_end, generation } => {
                self.do_vlf_viewport(line_start, line_end, generation);
                None
            }
            SpecialEvent::VlfReplaceRange { start_line, start_col, end_line, end_col, text } => {
                if let Err(err) =
                    self.do_vlf_replace_range(start_line, start_col, end_line, end_col, &text)
                {
                    self.client.alert(err);
                }
                None
            }
        }
    }

    fn validate_view_event_bounds(&mut self, cmd: &crate::edit_types::ViewEvent) -> bool {
        use crate::edit_types::ViewEvent;

        let validation = match cmd {
            ViewEvent::GotoLine { line } => self.validate_view_line_col("goto_line", *line, 0),
            ViewEvent::Gesture { line, col, .. } => {
                self.validate_view_line_col("gesture", *line, *col)
            }
            ViewEvent::Click(mouse) => {
                self.validate_view_line_col("click", mouse.line, mouse.column)
            }
            ViewEvent::Drag(mouse) => self.validate_view_line_col("drag", mouse.line, mouse.column),
            _ => return true,
        };

        if let Err(err) = validation {
            self.client.alert(err);
            return false;
        }
        true
    }

    fn validate_view_line_col(&mut self, method: &str, line: u64, col: u64) -> Result<(), String> {
        let line = usize::try_from(line).map_err(|_| format!("{method}: line index overflow"))?;
        let col = usize::try_from(col).map_err(|_| format!("{method}: column index overflow"))?;
        self.with_view(|view, text| view.try_line_col_to_offset(text, line, col))
            .map(|_| ())
            .map_err(|err| format!("{method}: {err}"))
    }
}

// ── Edit lifecycle (kept as methods so `self` plumbing stays natural) ──

impl<'a> EventContext<'a> {
    /// Commits any changes to the buffer, updating views and plugins as needed.
    /// This only updates internal state; does not update the client.
    pub(crate) fn after_edit(&mut self, author: &str) {
        let _t = tracing::trace_span!("EventContext::after_edit", categories = "core").entered();

        let edit_info = self.editor.borrow_mut().commit_delta();
        let (delta, last_text, drift) = match edit_info {
            Some(edit_info) => edit_info,
            None => return,
        };

        self.update_views(&self.editor.borrow(), &delta, &last_text, drift);
        self.update_plugins(&mut self.editor.borrow_mut(), delta, author);

        // if we have no plugins we always render immediately.
        if !self.plugins.is_empty() {
            let mut view = self.view.borrow_mut();
            if !view.has_pending_render() {
                let timeout = std::time::Instant::now() + RENDER_DELAY;
                let view_id: usize = self.view_id.into();
                let token = RENDER_VIEW_IDLE_MASK | view_id;
                self.client.schedule_timer(timeout, token);
                view.set_has_pending_render(true);
            }
        }
    }

    fn update_views(&self, ed: &Editor, delta: &RopeDelta, last_text: &Rope, drift: InsertDrift) {
        let mut width_cache = self.width_cache.borrow_mut();
        let iter_views = std::iter::once(&self.view).chain(self.siblings.iter());
        iter_views.for_each(|view| {
            view.borrow_mut().after_edit(
                ed.get_buffer(),
                last_text,
                delta,
                self.client,
                &mut width_cache,
                drift,
            )
        });
    }

    fn update_plugins(&self, ed: &mut Editor, delta: RopeDelta, author: &str) {
        use crate::plugins::rpc::PluginUpdate;

        let new_len = delta.new_document_len();
        let nb_lines = ed.get_buffer().measure::<LinesMetric>() + 1;
        let approx_size = delta.inserts_len() + (delta.els.len() * 10);
        let delta = if approx_size > super::MAX_SIZE_LIMIT { None } else { Some(delta) };

        let undo_group = ed.get_active_undo_group();
        let edit_type_str = super::edit_type_to_string(ed.get_edit_type());

        let update = PluginUpdate::new(
            self.view_id,
            ed.get_head_rev_token(),
            delta,
            new_len,
            nb_lines,
            Some(undo_group),
            edit_type_str,
            author.into(),
        );

        ed.increment_revs_in_flight();

        self.plugins.iter().for_each(|plugin| {
            ed.increment_revs_in_flight();
            plugin.update(&update, self.weak_core.clone(), self.view_id);
        });
        ed.dec_revs_in_flight();
        ed.update_edit_type();
    }

    /// Renders the view, if a render has not already been scheduled.
    pub(crate) fn render_if_needed(&mut self) {
        let needed = !self.view.borrow().has_pending_render();
        if needed {
            self.render()
        }
    }

    pub(crate) fn _finish_delayed_render(&mut self) {
        self.render();
        self.view.borrow_mut().set_has_pending_render(false);
    }

    /// Flushes any changes in the views out to the frontend.
    pub(crate) fn render(&mut self) {
        let _t = tracing::trace_span!("EventContext::render", categories = "core").entered();
        let ed = self.editor.borrow();
        if ed.is_vlf() {
            return;
        }
        let file_path = self.info.map(|info| info.path.as_path());
        let capabilities = syntax_feature_availability(
            Some(self.language.as_ref()),
            file_path,
            ed.document_mode(),
        );
        let syntax_enabled = capabilities.syntax_spans && !ed.is_vlf();
        self.view.borrow_mut().render_if_dirty(
            ed.get_buffer(),
            self.client,
            ed.is_pristine(),
            self.language.as_ref(),
            syntax_enabled,
        )
    }
}
