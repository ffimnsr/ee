use std::path::Path;

use log::{debug, warn};
use xi_rope::{Cursor, Rope};

use crate::plugins::rpc::ClientPluginInfo;
use crate::tabs::{FIND_VIEW_IDLE_MASK, REWRAP_VIEW_IDLE_MASK};
use crate::width_cache::WidthCache;

use super::EventContext;

impl<'a> EventContext<'a> {
    /// Initialises view-level wrapping settings.
    ///
    /// Must be called once before [`finish_init`] so that wrap state is correct
    /// before the first render.
    pub(crate) fn view_init(&mut self) {
        let wrap_width = self.config.wrap_width;
        let word_wrap = self.config.word_wrap;

        self.with_view(|view, text| view.update_wrap_settings(text, wrap_width, word_wrap));
    }

    /// Completes buffer initialisation: notifies plugins, sends initial
    /// config and language to the client, performs the first rewrap pass,
    /// and schedules an initial render.
    pub(crate) fn finish_init(&mut self, _config: &super::Table) {
        if !self.plugins.is_empty() {
            let info = self.plugin_info();

            self.plugins.iter().for_each(|plugin| {
                plugin.new_buffer(&info);
                self.plugin_started(plugin);
            });
        }

        let available_plugins = self
            .plugins
            .iter()
            .map(|plugin| ClientPluginInfo { name: plugin.name.clone(), running: true })
            .collect::<Vec<_>>();
        self.client.available_plugins(self.view_id, &available_plugins);

        self.client.language_changed(self.view_id, &self.language);

        let is_vlf = self.editor.borrow().is_vlf();
        self.client.document_mode_changed(self.view_id, is_vlf);
        if is_vlf {
            self.do_vlf_viewport(0, 199, 0);
        }

        self.rewrap();

        if self.view.borrow().needs_more_wrap() {
            self.schedule_rewrap();
        }

        self.with_view(|view, text| view.set_dirty(text));
        self.render()
    }

    /// Called after a rope-backed buffer snapshot has been saved to `path`.
    pub(crate) fn after_save_with_rev(
        &mut self,
        path: &Path,
        saved_rev_id: xi_rope::engine::RevId,
    ) {
        self.plugins.iter().for_each(|plugin| plugin.did_save(self.view_id, path));

        self.editor.borrow_mut().set_pristine_if_equivalent_revision(saved_rev_id);
        self.with_view(|view, text| view.set_dirty(text));
        self.render()
    }

    /// Returns `true` if this was the last view
    pub(crate) fn close_view(&self) -> bool {
        self.plugins.iter().for_each(|plug| plug.close_view(self.view_id));
        self.siblings.is_empty()
    }

    /// Notifies all plugins about a configuration change, updates the client,
    /// and schedules a render when wrap-related settings change.
    pub(crate) fn config_changed(&mut self, changes: &super::Table) {
        if changes.contains_key("wrap_width") || changes.contains_key("word_wrap") {
            if changes.contains_key("word_wrap") {
                debug!("clearing {} items from width cache", self.width_cache.borrow().len());
                self.width_cache.replace(WidthCache::new());
            }
            self.update_wrap_settings(true);
        }

        self.plugins.iter().for_each(|plug| plug.config_changed(self.view_id, changes));
        self.render()
    }

    /// Notifies all plugins and the client that the active language has changed.
    pub(crate) fn language_changed(&mut self, new_language_id: &super::LanguageId) {
        self.language = new_language_id.clone();
        self.client.language_changed(self.view_id, new_language_id);
        self.plugins.iter().for_each(|plug| plug.language_changed(self.view_id, new_language_id));
        self.with_view(|view, text| view.set_dirty(text));
        self.render();
    }

    /// Replaces buffer contents with `text`, preserving undo history, and
    /// triggers plugin updates and a render.
    pub(crate) fn reload(&mut self, text: Rope) {
        self.with_editor(|ed, _, _, _| ed.reload(text));
        self.after_edit("core");
        self.render();
    }

    /// Returns a cheap rope snapshot for saving, appending a newline if needed.
    pub(crate) fn rope_snapshot_for_save(&mut self) -> (Rope, xi_rope::engine::RevId) {
        let editor = self.editor.borrow();
        let saved_rev_id = editor.get_head_rev_id();
        let mut rope = editor.get_buffer().clone();
        let rope_len = rope.len();

        if rope_len < 1 || !self.config.save_with_newline {
            return (rope, saved_rev_id);
        }

        let cursor = Cursor::new(&rope, rope.len());
        let has_newline_at_eof = match cursor.get_leaf() {
            Some((last_chunk, _)) => last_chunk.ends_with(&self.config.line_ending),
            None => {
                warn!("rope_snapshot_for_save could not inspect final rope chunk at EOF");
                return (rope, saved_rev_id);
            }
        };

        if !has_newline_at_eof {
            let line_ending = &self.config.line_ending;
            rope.edit(rope_len.., line_ending);
        }
        (rope, saved_rev_id)
    }
}

// ── Wrap / rewrap helpers ──

impl<'a> EventContext<'a> {
    /// Called after anything changes that effects word wrap, such as the size of
    /// the window or the user's wrap settings.
    pub(super) fn update_wrap_settings(&mut self, rewrap_immediately: bool) {
        let wrap_width = self.config.wrap_width;
        let word_wrap = self.config.word_wrap;
        self.with_view(|view, text| view.update_wrap_settings(text, wrap_width, word_wrap));
        if rewrap_immediately {
            self.rewrap();
            self.with_view(|view, text| view.set_dirty(text));
        }
        if self.view.borrow().needs_more_wrap() {
            self.schedule_rewrap();
        }
    }

    /// Tells the view to rewrap a batch of lines, if needed.
    pub(super) fn rewrap(&mut self) {
        let mut view = self.view.borrow_mut();
        let ed = self.editor.borrow();
        let mut width_cache = self.width_cache.borrow_mut();
        view.rewrap(ed.get_buffer(), &mut width_cache, self.client);
    }

    /// Does a rewrap batch, and schedules follow-up work if needed.
    pub(crate) fn do_rewrap_batch(&mut self) {
        self.rewrap();
        if self.view.borrow().needs_more_wrap() {
            self.schedule_rewrap();
        }
        self.render_if_needed();
    }

    pub(super) fn schedule_rewrap(&self) {
        let view_id: usize = self.view_id.into();
        let token = REWRAP_VIEW_IDLE_MASK | view_id;
        self.client.schedule_idle(token);
    }
}

// ── Find-related methods ──

impl<'a> EventContext<'a> {
    /// Does incremental find.
    pub(crate) fn do_incremental_find(&mut self) {
        let _t = tracing::trace_span!("EventContext::do_incremental_find", categories = "find")
            .entered();

        if self.editor.borrow().is_vlf() {
            self.do_incremental_vlf_find();
            self.render_if_needed();
            return;
        }

        self.find();
        if self.view.borrow().find_in_progress() {
            let ed = self.editor.borrow();
            self.client.find_status(
                self.view_id,
                &serde_json::json!(self.view.borrow().find_status(ed.get_buffer(), true)),
            );
            self.schedule_find();
        }
        self.render_if_needed();
    }

    pub(super) fn schedule_find(&self) {
        let view_id: usize = self.view_id.into();
        let token = FIND_VIEW_IDLE_MASK | view_id;
        self.client.schedule_idle(token);
    }

    /// Tells the view to execute find on a batch of lines, if needed.
    fn find(&mut self) {
        let mut view = self.view.borrow_mut();
        let ed = self.editor.borrow();
        view.do_find(ed.get_buffer());
    }

    fn do_incremental_vlf_find(&mut self) {
        let status = {
            let editor = self.editor.borrow();
            let Some(store) = editor.vlf_store.as_ref() else {
                return;
            };
            let mut view = self.view.borrow_mut();
            match view.scan_vlf_find(store) {
                Ok(status) => status,
                Err(err) => {
                    self.client.alert(format!("vlf search failed: {err}"));
                    None
                }
            }
        };

        if let Some(status) = status {
            self.client.vlf_search_status(
                self.view_id,
                &status.query,
                status.scanned_bytes,
                status.total_bytes,
                status.complete,
                status.stored_match_count,
                &status.ranges,
            );
        }

        if self.view.borrow().vlf_find_in_progress() {
            self.schedule_find();
        }
    }
}
