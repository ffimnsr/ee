// Copyright 2018 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A container for the state relevant to a single event.

use std::cell::RefCell;
use std::iter;
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant};

use log::{debug, error, warn};
use serde_json::{self, Value, json};

use xi_rope::{Cursor, Interval, LinesMetric, Rope, RopeDelta};
use xi_rpc::{Error as RpcError, RemoteError};

use crate::plugins::rpc::{
    ClientPluginInfo, GetDiagnosticsResponse, GetSelectionsResponse, Hover, PluginBufferInfo,
    PluginNotification, PluginRequest, PluginUpdate, PluginUpdateAck, SelectionRange,
};
use crate::rpc::{EditNotification, EditRequest, LineRange, Position as ClientPosition};

use crate::WeakXiCore;
use crate::client::Client;
use crate::config::{BufferItems, Table};
use crate::edit_types::{EventDomain, SpecialEvent};
use crate::editor::{EditType, Editor};
use crate::file::FileInfo;
use crate::line_offset::LineOffset;
use crate::plugins::{Plugin, PluginCapability};
use crate::recorder::Recorder;
use crate::selection::InsertDrift;
use crate::styles::ThemeStyleMap;
use crate::syntax::LanguageId;
use crate::tabs::{
    BufferId, FIND_VIEW_IDLE_MASK, PluginId, RENDER_VIEW_IDLE_MASK, REWRAP_VIEW_IDLE_MASK, ViewId,
};
use crate::view::View;
use crate::width_cache::WidthCache;

// Maximum returned result from plugin get_data RPC.
pub const MAX_SIZE_LIMIT: usize = 1024 * 1024;

//TODO: tune this. a few ms can make a big difference. We may in the future
//want to make this tuneable at runtime, or to be configured by the client.
/// The render delay after an edit occurs; plugin updates received in this
/// window will be sent to the view along with the edit.
const RENDER_DELAY: Duration = Duration::from_millis(2);

/// A collection of all the state relevant for handling a particular event.
///
/// This is created dynamically for each event that arrives to the core,
/// such as a user-initiated edit or style updates from a plugin.
pub struct EventContext<'a> {
    pub(crate) view_id: ViewId,
    pub(crate) buffer_id: BufferId,
    pub(crate) editor: &'a RefCell<Editor>,
    pub(crate) info: Option<&'a FileInfo>,
    pub(crate) config: &'a BufferItems,
    pub(crate) recorder: &'a RefCell<Recorder>,
    pub(crate) language: LanguageId,
    pub(crate) view: &'a RefCell<View>,
    pub(crate) siblings: Vec<&'a RefCell<View>>,
    pub(crate) plugins: Vec<&'a Plugin>,
    pub(crate) client: &'a Client,
    pub(crate) style_map: &'a RefCell<ThemeStyleMap>,
    pub(crate) width_cache: &'a RefCell<WidthCache>,
    pub(crate) kill_ring: &'a RefCell<Rope>,
    pub(crate) weak_core: &'a WeakXiCore,
}

fn edit_type_to_string(edit_type: EditType) -> String {
    match edit_type {
        EditType::Other => "other",
        EditType::InsertChars => "insert",
        EditType::InsertNewline => "newline",
        EditType::Indent => "indent",
        EditType::Delete => "delete",
        EditType::Undo => "undo",
        EditType::Redo => "redo",
        EditType::Transpose => "transpose",
        EditType::Surround => "surround",
    }
    .to_string()
}

fn buffer_items_to_table(config: &BufferItems) -> Table {
    match serde_json::to_value(config) {
        Ok(Value::Object(table)) => table,
        Ok(other) => {
            error!("buffer config serialized to non-object value: {:?}", other);
            Table::new()
        }
        Err(err) => {
            error!("failed to serialize buffer config: {:?}", err);
            Table::new()
        }
    }
}

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
    fn with_view<R, F>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut View, &Rope) -> R,
    {
        let editor = self.editor.borrow();
        let mut view = self.view.borrow_mut();
        f(&mut view, editor.get_buffer())
    }

    fn dispatch_command_to_plugins(&self, method: &str, params: &Value) {
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

        {
            // Handle recording-- clone every non-toggle and play event into the recording buffer
            let mut recorder = self.recorder.borrow_mut();
            match (recorder.is_recording(), &event) {
                (_, EventDomain::Special(SpecialEvent::ToggleRecording(recording_name))) => {
                    recorder.toggle_recording(recording_name.clone());
                }
                // Don't save special events
                (true, EventDomain::Special(_)) => {
                    warn!("Special events cannot be recorded-- ignoring event {:?}", event)
                }
                (true, event) => recorder.record(event.clone()),
                _ => {}
            }
        }

        self.dispatch_event(event);
        self.after_edit("core");
        self.render_if_needed();
    }

    fn dispatch_event(&mut self, event: EventDomain) {
        use self::EventDomain as E;
        match event {
            E::View(cmd) => {
                self.with_view(|view, text| view.do_edit(text, cmd));
                self.editor.borrow_mut().update_edit_type();
                if self.with_view(|v, t| v.needs_wrap_in_visible_region(t)) {
                    self.rewrap();
                }
                if self.with_view(|v, _| v.find_in_progress()) {
                    self.do_incremental_find();
                }
            }
            E::Buffer(cmd) => {
                self.with_editor(|ed, view, k_ring, conf| ed.do_edit(view, k_ring, conf, cmd))
            }
            E::Special(cmd) => self.do_special(cmd),
        }
    }

    fn do_special(&mut self, cmd: SpecialEvent) {
        match cmd {
            SpecialEvent::Resize(size) => {
                self.with_view(|view, _| view.set_size(size));
                if self.config.word_wrap {
                    self.update_wrap_settings(false);
                }
            }
            SpecialEvent::DebugRewrap | SpecialEvent::DebugWrapWidth => {
                warn!("debug wrapping methods are removed, use the config system")
            }
            SpecialEvent::DebugPrintSpans => self.with_editor(|ed, view, _, _| {
                if let Some(sel) = view.sel_regions().last() {
                    let iv = Interval::new(sel.min(), sel.max());
                    ed.get_layers().debug_print_spans(iv);
                } else {
                    warn!("debug_print_spans requested without an active selection");
                }
            }),
            SpecialEvent::RequestLines(LineRange { first, last }) => {
                self.do_request_lines(first as usize, last as usize)
            }
            SpecialEvent::RequestHover { request_id, position } => {
                self.do_request_hover(request_id, position)
            }
            SpecialEvent::DebugToggleComment => self.do_debug_toggle_comment(),
            SpecialEvent::Reindent => self.do_reindent(),
            SpecialEvent::ToggleRecording(_) => {}
            SpecialEvent::PlayRecording(recording_name) => {
                let recorder = self.recorder.borrow();

                let starting_revision = self.editor.borrow_mut().get_head_rev_token();

                // Don't group with the previous action
                self.editor.borrow_mut().update_edit_type();
                self.editor.borrow_mut().calculate_undo_group();

                // No matter what, our entire block must belong to the same undo group
                self.editor.borrow_mut().set_force_undo_group(true);
                recorder.play(&recording_name, |event| {
                    self.dispatch_event(event.clone());

                    let mut editor = self.editor.borrow_mut();
                    let (delta, last_text, drift) = match editor.commit_delta() {
                        Some(edit_info) => edit_info,
                        None => return,
                    };
                    self.update_views(&editor, &delta, &last_text, drift);
                });
                self.editor.borrow_mut().set_force_undo_group(false);

                // The action that follows the block must belong to a separate undo group
                self.editor.borrow_mut().update_edit_type();

                let delta = self.editor.borrow_mut().delta_rev_head(starting_revision);
                if let Some(delta) = delta {
                    self.update_plugins(&mut self.editor.borrow_mut(), delta, "core");
                } else {
                    warn!(
                        "recording playback could not compute delta from revision {:?}",
                        starting_revision
                    );
                }
            }
            SpecialEvent::ClearRecording(recording_name) => {
                let mut recorder = self.recorder.borrow_mut();
                recorder.clear(&recording_name);
            }
        }
    }

    /// Handles a synchronous edit request from the client and returns a result
    /// value (e.g. for cut/copy operations).
    ///
    /// # Preconditions
    ///
    /// The `editor` and `view` `RefCell`s must not be borrowed when this is called.
    pub(crate) fn do_edit_sync(&mut self, cmd: EditRequest) -> Result<Value, RemoteError> {
        use self::EditRequest::*;
        let result = match cmd {
            Cut => Ok(self.with_editor(|ed, view, _, _| ed.do_cut(view))),
            Copy => Ok(self.with_editor(|ed, view, _, _| ed.do_copy(view))),
        };
        self.after_edit("core");
        self.render_if_needed();
        result
    }

    /// Dispatches an incoming notification from a plugin (fire-and-forget).
    ///
    /// # Preconditions
    ///
    /// `plugin` must refer to a plugin that is currently running for this buffer.
    pub(crate) fn do_plugin_cmd(&mut self, plugin: PluginId, cmd: PluginNotification) {
        use self::PluginNotification::*;
        match cmd {
            AddScopes { scopes } => {
                let mut ed = self.editor.borrow_mut();
                let style_map = self.style_map.borrow();
                ed.get_layers_mut().add_scopes(plugin, scopes, &style_map);
            }
            UpdateSpans { start, len, spans, rev } => self.with_editor(|ed, view, _, _| {
                ed.update_spans(view, plugin, start, len, spans, rev)
            }),
            Edit { edit } => {
                let ack = self.with_editor(|ed, _, _, _| ed.apply_plugin_edit(edit));
                if !ack.applied {
                    warn!("plugin edit rejected at revision {}: {:?}", ack.rev, ack.reason);
                }
            }
            Alert { msg } => self.client.alert(&msg),
            AddStatusItem { key, value, alignment } => {
                let plugin_name = self
                    .plugins
                    .iter()
                    .find(|p| p.id == plugin)
                    .map(|plugin| plugin.name.as_str())
                    .unwrap_or_else(|| {
                        warn!("status item update from unknown plugin {:?}", plugin);
                        "unknown-plugin"
                    });
                self.client.add_status_item(self.view_id, plugin_name, &key, &value, &alignment);
            }
            UpdateStatusItem { key, value } => {
                self.client.update_status_item(self.view_id, &key, &value)
            }
            UpdateAnnotations { start, len, spans, annotation_type, rev } => {
                self.with_editor(|ed, view, _, _| {
                    ed.update_annotations(view, plugin, start, len, spans, annotation_type, rev)
                })
            }
            UpdateDiagnostics { diagnostics } => {
                self.with_view(|view, _| view.update_diagnostics(plugin, diagnostics))
            }
            RemoveStatusItem { key } => self.client.remove_status_item(self.view_id, &key),
            ShowHover { request_id, result } => self.do_show_hover(request_id, result),
            ShowCompletions { items } => self.client.show_completions(self.view_id, &items),
            ShowLocations { title, locations } => {
                self.client.show_locations(self.view_id, &title, &locations)
            }
        };
        self.after_edit(&plugin.to_string());
        self.render_if_needed();
    }

    pub(crate) fn do_plugin_cmd_sync(
        &mut self,
        _plugin: PluginId,
        cmd: PluginRequest,
    ) -> Result<Value, RemoteError> {
        use self::PluginRequest::*;
        match cmd {
            ApplyEdit { edit } => {
                Ok(json!(self.with_editor(|ed, _, _, _| ed.apply_plugin_edit(edit))))
            }
            LineCount => Ok(json!(self.editor.borrow().plugin_n_lines())),
            GetData { start, unit, max_size, rev } => {
                Ok(json!(self.editor.borrow().plugin_get_data(start, unit, max_size, rev)))
            }
            GetSelections => {
                let selections = self
                    .view
                    .borrow()
                    .sel_regions()
                    .iter()
                    .map(|region| SelectionRange { start: region.start, end: region.end })
                    .collect();
                Ok(json!(GetSelectionsResponse { selections }))
            }
            GetDiagnostics => Ok(json!(GetDiagnosticsResponse {
                diagnostics: self.view.borrow().get_diagnostics(),
            })),
            FormatDocument(..) => Err(RemoteError::custom(
                501,
                "document formatting is not implemented for plugins",
                None,
            )),
            GetCodeActions(..) => {
                Err(RemoteError::custom(501, "code actions are not implemented for plugins", None))
            }
        }
    }

    /// Commits any changes to the buffer, updating views and plugins as needed.
    /// This only updates internal state; it does not update the client.
    fn after_edit(&mut self, author: &str) {
        let _t = tracing::trace_span!("EventContext::after_edit", categories = "core").entered();

        let edit_info = self.editor.borrow_mut().commit_delta();
        let (delta, last_text, drift) = match edit_info {
            Some(edit_info) => edit_info,
            None => return,
        };

        self.update_views(&self.editor.borrow(), &delta, &last_text, drift);
        self.update_plugins(&mut self.editor.borrow_mut(), delta, author);

        //if we have no plugins we always render immediately.
        if !self.plugins.is_empty() {
            let mut view = self.view.borrow_mut();
            if !view.has_pending_render() {
                let timeout = Instant::now() + RENDER_DELAY;
                let view_id: usize = self.view_id.into();
                let token = RENDER_VIEW_IDLE_MASK | view_id;
                self.client.schedule_timer(timeout, token);
                view.set_has_pending_render(true);
            }
        }
    }

    fn update_views(&self, ed: &Editor, delta: &RopeDelta, last_text: &Rope, drift: InsertDrift) {
        let mut width_cache = self.width_cache.borrow_mut();
        let iter_views = iter::once(&self.view).chain(self.siblings.iter());
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
        let new_len = delta.new_document_len();
        let nb_lines = ed.get_buffer().measure::<LinesMetric>() + 1;
        // don't send the actual delta if it is too large, by some heuristic
        let approx_size = delta.inserts_len() + (delta.els.len() * 10);
        let delta = if approx_size > MAX_SIZE_LIMIT { None } else { Some(delta) };

        let undo_group = ed.get_active_undo_group();
        //TODO: we want to just put EditType on the wire, but don't want
        //to update the plugin lib quite yet.
        let edit_type_str = edit_type_to_string(ed.get_edit_type());

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

        // we always increment and decrement regardless of whether we're
        // sending plugins, to ensure that GC runs.
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
    fn render(&mut self) {
        let _t = tracing::trace_span!("EventContext::render", categories = "core").entered();
        let ed = self.editor.borrow();
        //TODO: render other views
        self.view.borrow_mut().render_if_dirty(
            ed.get_buffer(),
            self.client,
            self.style_map,
            ed.get_layers().get_merged(),
            ed.is_pristine(),
        )
    }
}

/// Helpers related to specific commands.
///
/// Certain events and actions don't generalize well; handling these
/// requires access to particular combinations of state. We isolate such
/// special cases here.
impl<'a> EventContext<'a> {
    /// Initialises view-level wrapping settings.
    ///
    /// Must be called once before [`finish_init`] so that wrap state is correct
    /// before the first render.
    ///
    /// [`finish_init`]: Self::finish_init
    pub(crate) fn view_init(&mut self) {
        let wrap_width = self.config.wrap_width;
        let word_wrap = self.config.word_wrap;

        self.with_view(|view, text| view.update_wrap_settings(text, wrap_width, word_wrap));
    }

    /// Completes buffer initialisation: notifies plugins, sends initial
    /// config and language to the client, performs the first rewrap pass,
    /// and schedules an initial render.
    ///
    /// # Preconditions
    ///
    /// [`view_init`] must have been called before this method.
    ///
    /// [`view_init`]: Self::view_init
    pub(crate) fn finish_init(&mut self, config: &Table) {
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

        self.client.config_changed(self.view_id, config);
        self.client.language_changed(self.view_id, &self.language);

        // Rewrap and request a render.
        // This is largely similar to update_wrap_settings(), the only difference
        // being that the view is expected to be already initialized.
        self.rewrap();

        if self.view.borrow().needs_more_wrap() {
            self.schedule_rewrap();
        }

        self.with_view(|view, text| view.set_dirty(text));
        self.render()
    }

    /// Called after the buffer has been saved to `path`. Notifies plugins,
    /// marks the buffer as pristine, and schedules a render.
    pub(crate) fn after_save(&mut self, path: &Path) {
        // notify plugins
        self.plugins.iter().for_each(|plugin| plugin.did_save(self.view_id, path));

        self.editor.borrow_mut().set_pristine();
        self.with_view(|view, text| view.set_dirty(text));
        self.render()
    }

    /// Returns `true` if this was the last view
    pub(crate) fn close_view(&self) -> bool {
        // we probably want to notify plugins _before_ we close the view
        // TODO: determine what plugins we're stopping
        self.plugins.iter().for_each(|plug| plug.close_view(self.view_id));
        self.siblings.is_empty()
    }

    /// Notifies all plugins about a configuration change, updates the client,
    /// and schedules a render when wrap-related settings change.
    pub(crate) fn config_changed(&mut self, changes: &Table) {
        if changes.contains_key("wrap_width") || changes.contains_key("word_wrap") {
            // FIXME: if switching from measurement-based widths to columnar widths,
            // we need to reset the cache, since we're using different coordinate spaces
            // for the same IDs. The long-term solution would be to include font
            // information in the width cache, and then use real width even in the column
            // case, getting the unit width for a typeface and multiplying that by
            // a string's unicode width.
            if changes.contains_key("word_wrap") {
                debug!("clearing {} items from width cache", self.width_cache.borrow().len());
                self.width_cache.replace(WidthCache::new());
            }
            self.update_wrap_settings(true);
        }

        self.client.config_changed(self.view_id, changes);
        self.plugins.iter().for_each(|plug| plug.config_changed(self.view_id, changes));
        self.render()
    }

    /// Notifies all plugins and the client that the active language has changed.
    pub(crate) fn language_changed(&mut self, new_language_id: &LanguageId) {
        self.language = new_language_id.clone();
        self.client.language_changed(self.view_id, new_language_id);
        self.plugins.iter().for_each(|plug| plug.language_changed(self.view_id, new_language_id));
    }

    /// Replaces buffer contents with `text`, preserving undo history, and
    /// triggers plugin updates and a render.
    pub(crate) fn reload(&mut self, text: Rope) {
        self.with_editor(|ed, _, _, _| ed.reload(text));
        self.after_edit("core");
        self.render();
    }

    /// Builds a [`PluginBufferInfo`] snapshot describing the current buffer
    /// state for delivery to plugins during initialisation or restart.
    pub(crate) fn plugin_info(&mut self) -> PluginBufferInfo {
        let ed = self.editor.borrow();
        let nb_lines = ed.get_buffer().measure::<LinesMetric>() + 1;
        let views: Vec<ViewId> = iter::once(&self.view)
            .chain(self.siblings.iter())
            .map(|v| v.borrow().get_view_id())
            .collect();

        let changes = buffer_items_to_table(self.config);
        let path = self.info.map(|info| info.path.to_owned());
        PluginBufferInfo::new(
            self.buffer_id,
            &views,
            ed.get_head_rev_token(),
            ed.get_buffer().len(),
            nb_lines,
            path,
            self.language.clone(),
            changes,
        )
    }

    /// Notifies the client that `plugin` has started for this view.
    pub(crate) fn plugin_started(&self, plugin: &Plugin) {
        self.client.plugin_started(self.view_id, &plugin.name)
    }

    /// Notifies the client that `plugin` has stopped and removes its style
    /// layer, scheduling a render if the display changed.
    pub(crate) fn plugin_stopped(&mut self, plugin: &Plugin) {
        self.client.plugin_stopped(self.view_id, &plugin.name, 0);
        let needs_render = self.with_editor(|ed, view, _, _| {
            if ed.get_layers_mut().remove_layer(plugin.id).is_some() {
                view.set_dirty(ed.get_buffer());
                true
            } else {
                false
            }
        });
        if needs_render {
            self.render();
        }
    }

    /// Handles the acknowledgement from a plugin after an update was delivered.
    /// Decrements the in-flight revision counter, enabling CRDT garbage collection.
    pub(crate) fn do_plugin_update(&mut self, update: Result<Value, RpcError>) {
        match update.map(serde_json::from_value::<PluginUpdateAck>) {
            Ok(Ok(_)) => (),
            Ok(Err(err)) => error!("plugin response json err: {:?}", err),
            Err(err) => error!("plugin shutdown, do something {:?}", err),
        }
        self.editor.borrow_mut().dec_revs_in_flight();
    }

    /// Handles the response to a hover request from a plugin, forwarding the
    /// result to the client or logging an error on failure.
    pub(crate) fn do_plugin_hover(&mut self, request_id: usize, hover: Result<Value, RpcError>) {
        match hover.map(serde_json::from_value::<Hover>) {
            Ok(Ok(hover)) => self.do_show_hover(request_id, Ok(hover)),
            Ok(Err(err)) => error!("hover response json err: {:?}", err),
            Err(RpcError::RequestCancelled) => debug!("hover request {} cancelled", request_id),
            Err(RpcError::RemoteError(err)) => self.do_show_hover(request_id, Err(err)),
            Err(err) => warn!("hover request {} failed: {:?}", request_id, err),
        }
    }

    /// Returns the text to be saved, appending a newline if necessary.
    pub(crate) fn text_for_save(&mut self) -> Rope {
        let editor = self.editor.borrow();
        let mut rope = editor.get_buffer().clone();
        let rope_len = rope.len();

        if rope_len < 1 || !self.config.save_with_newline {
            return rope;
        }

        let cursor = Cursor::new(&rope, rope.len());
        let has_newline_at_eof = match cursor.get_leaf() {
            Some((last_chunk, _)) => last_chunk.ends_with(&self.config.line_ending),
            None => {
                warn!("text_for_save could not inspect final rope chunk at EOF");
                return rope;
            }
        };

        if !has_newline_at_eof {
            let line_ending = &self.config.line_ending;
            rope.edit(rope_len.., line_ending);
        }
        rope
    }

    /// Called after anything changes that effects word wrap, such as the size of
    /// the window or the user's wrap settings. `rewrap_immediately` should be `true`
    /// except in the resize case; during live resize we want to delay recalculation
    /// to avoid unnecessary work.
    fn update_wrap_settings(&mut self, rewrap_immediately: bool) {
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

    /// Tells the view to rewrap a batch of lines, if needed. This guarantees that
    /// the currently visible region will be correctly wrapped; the caller should
    /// check if additional wrapping is necessary and schedule that if so.
    fn rewrap(&mut self) {
        let mut view = self.view.borrow_mut();
        let ed = self.editor.borrow();
        let mut width_cache = self.width_cache.borrow_mut();
        view.rewrap(ed.get_buffer(), &mut width_cache, self.client, ed.get_layers().get_merged());
    }

    /// Does incremental find.
    pub(crate) fn do_incremental_find(&mut self) {
        let _t = tracing::trace_span!("EventContext::do_incremental_find", categories = "find")
            .entered();

        self.find();
        if self.view.borrow().find_in_progress() {
            let ed = self.editor.borrow();
            self.client.find_status(
                self.view_id,
                &json!(self.view.borrow().find_status(ed.get_buffer(), true)),
            );
            self.schedule_find();
        }
        self.render_if_needed();
    }

    fn schedule_find(&self) {
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

    /// Does a rewrap batch, and schedules follow-up work if needed.
    pub(crate) fn do_rewrap_batch(&mut self) {
        self.rewrap();
        if self.view.borrow().needs_more_wrap() {
            self.schedule_rewrap();
        }
        self.render_if_needed();
    }

    fn schedule_rewrap(&self) {
        let view_id: usize = self.view_id.into();
        let token = REWRAP_VIEW_IDLE_MASK | view_id;
        self.client.schedule_idle(token);
    }

    fn do_request_lines(&mut self, first: usize, last: usize) {
        let mut view = self.view.borrow_mut();
        let ed = self.editor.borrow();
        view.request_lines(
            ed.get_buffer(),
            self.client,
            self.style_map,
            ed.get_layers().get_merged(),
            first,
            last,
            ed.is_pristine(),
        )
    }

    fn selected_line_ranges(&mut self) -> Vec<(usize, usize)> {
        let ed = self.editor.borrow();
        let mut prev_range: Option<Range<usize>> = None;
        let mut line_ranges = Vec::new();
        // we send selection state to syntect in the form of a vec of line ranges,
        // so we combine overlapping selections to get the minimum set of ranges.
        for region in self.view.borrow().sel_regions().iter() {
            let start = ed.get_buffer().line_of_offset(region.min());
            let end = ed.get_buffer().line_of_offset(region.max()) + 1;
            let line_range = start..end;
            let prev = prev_range.take();
            match (prev, line_range) {
                (None, range) => prev_range = Some(range),
                (Some(ref prev), ref range) if range.start <= prev.end => {
                    let combined =
                        Range { start: prev.start.min(range.start), end: prev.end.max(range.end) };
                    prev_range = Some(combined);
                }
                (Some(prev), range) => {
                    line_ranges.push((prev.start, prev.end));
                    prev_range = Some(range);
                }
            }
        }

        if let Some(prev) = prev_range {
            line_ranges.push((prev.start, prev.end));
        }

        line_ranges
    }

    fn do_reindent(&mut self) {
        let line_ranges = self.selected_line_ranges();
        self.dispatch_command_to_plugins("reindent", &json!(line_ranges));
    }

    fn do_debug_toggle_comment(&mut self) {
        let line_ranges = self.selected_line_ranges();
        self.dispatch_command_to_plugins("toggle_comment", &json!(line_ranges));
    }

    fn do_request_hover(&mut self, request_id: usize, position: Option<ClientPosition>) {
        if let Some(position) = self.get_resolved_position(position) {
            let hover_plugins = self
                .plugins
                .iter()
                .filter(|plugin| plugin.manifest.has_capability(PluginCapability::Hover))
                .copied()
                .collect::<Vec<_>>();

            hover_plugins.into_iter().for_each(|plugin| {
                if let Some(previous_request) =
                    self.with_view(|view, _| view.take_pending_hover_request(plugin.id))
                {
                    plugin.cancel_request(previous_request);
                }

                let weak_core = self.weak_core.clone();
                let plugin_id = plugin.id;
                let view_id = self.view_id;
                let hover_request = plugin.request_hover(view_id, position, move |resp| {
                    weak_core.handle_plugin_hover(plugin_id, view_id, request_id, resp);
                });
                self.with_view(|view, _| {
                    view.replace_pending_hover_request(plugin_id, hover_request)
                });
            })
        }
    }

    fn do_show_hover(&mut self, request_id: usize, hover: Result<Hover, RemoteError>) {
        match hover {
            Ok(hover) => {
                // TODO: Get Range from hover here and use it to highlight text
                self.client.show_hover(self.view_id, request_id, hover.content)
            }
            Err(err) => warn!("Hover Response from Client Error {:?}", err),
        }
    }

    /// Gives the requested position in UTF-8 offset format to be sent to plugin
    /// If position is `None`, it tries to get the current Caret Position and use
    /// that instead
    fn get_resolved_position(&mut self, position: Option<ClientPosition>) -> Option<usize> {
        position
            .map(|p| self.with_view(|view, text| view.line_col_to_offset(text, p.line, p.column)))
            .or_else(|| self.view.borrow().get_caret_offset())
    }
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;
    use crate::config::ConfigManager;
    use crate::core::dummy_weak_core;
    use crate::plugins::rpc::{
        CodeActionRequest, Diagnostic, DiagnosticSeverity, FormatDocumentRequest,
        GetDiagnosticsResponse, GetSelectionsResponse, SelectionRange,
    };
    use crate::tabs::BufferId;
    use xi_rpc::test_utils::DummyPeer;

    struct ContextHarness {
        view: RefCell<View>,
        editor: RefCell<Editor>,
        client: Client,
        core_ref: WeakXiCore,
        kill_ring: RefCell<Rope>,
        style_map: RefCell<ThemeStyleMap>,
        width_cache: RefCell<WidthCache>,
        config_manager: ConfigManager,
        recorder: RefCell<Recorder>,
    }

    impl ContextHarness {
        fn new<S: AsRef<str>>(s: S) -> Self {
            // we could make this take a config, which would let us test
            // behaviour with different config settings?
            let view_id = ViewId(1);
            let buffer_id = BufferId(2);
            let mut config_manager = ConfigManager::new(None, None);
            let config = config_manager.add_buffer(buffer_id, None);
            let view = RefCell::new(View::new(view_id, buffer_id));
            let editor = RefCell::new(Editor::with_text(s));
            let client = Client::new(Box::new(DummyPeer));
            let core_ref = dummy_weak_core();
            let kill_ring = RefCell::new(Rope::from(""));
            let style_map = RefCell::new(ThemeStyleMap::new(None));
            let width_cache = RefCell::new(WidthCache::new());
            let recorder = RefCell::new(Recorder::new());
            let harness = ContextHarness { view, editor, client, core_ref, kill_ring,
                             style_map, width_cache, config_manager, recorder };
            harness.make_context().view_init();
            harness.make_context().finish_init(&config);
            harness

        }

        /// Renders the text and selections. cursors are represented with
        /// the pipe '|', and non-caret regions are represented by \[braces\].
        fn debug_render(&self) -> String {
            let b = self.editor.borrow();
            let mut text: String = b.get_buffer().into();
            let v = self.view.borrow();
            for sel in v.sel_regions().iter().rev() {
                if sel.end == sel.start {
                    text.insert(sel.end, '|');
                } else if sel.end > sel.start {
                    text.insert_str(sel.end, "|]");
                    text.insert(sel.start, '[');
                } else {
                    text.insert(sel.start, ']');
                    text.insert_str(sel.end, "[|");
                }
            }
            text
        }

        fn make_context(&self) -> EventContext<'_> {
            let view_id = ViewId(1);
            let buffer_id = self.view.borrow().get_buffer_id();
            let config = self.config_manager.get_buffer_config(buffer_id);
            let language = self.config_manager.get_buffer_language(buffer_id);
            EventContext {
                view_id,
                buffer_id,
                view: &self.view,
                editor: &self.editor,
                config: &config.items,
                language,
                info: None,
                siblings: Vec::new(),
                plugins: Vec::new(),
                recorder: &self.recorder,
                client: &self.client,
                kill_ring: &self.kill_ring,
                style_map: &self.style_map,
                width_cache: &self.width_cache,
                weak_core: &self.core_ref,
            }
        }
    }

    #[test]
    fn smoke_test() {
        let harness = ContextHarness::new("");
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Insert { chars: "hello".into() });
        ctx.do_edit(EditNotification::Insert { chars: " ".into() });
        ctx.do_edit(EditNotification::Insert { chars: "world".into() });
        ctx.do_edit(EditNotification::Insert { chars: "!".into() });
        assert_eq!(harness.debug_render(),"hello world!|");
        ctx.do_edit(EditNotification::MoveWordLeft);
        ctx.do_edit(EditNotification::InsertNewline);
        assert_eq!(harness.debug_render(),"hello \n|world!");
        ctx.do_edit(EditNotification::MoveWordRightAndModifySelection);
        assert_eq!(harness.debug_render(), "hello \n[world|]!");
        ctx.do_edit(EditNotification::Insert { chars: "friends".into() });
        assert_eq!(harness.debug_render(), "hello \nfriends|!");
    }

    #[test]
    fn get_selections_returns_current_selection_ranges() {
        let harness = ContextHarness::new("hello world");
        let mut ctx = harness.make_context();

        ctx.do_edit(EditNotification::MoveToRightEndOfLineAndModifySelection);

        let response: GetSelectionsResponse = serde_json::from_value(
            ctx.do_plugin_cmd_sync(crate::plugins::PluginPid(9), PluginRequest::GetSelections)
                .expect("selection request should succeed"),
        )
        .expect("selection response should deserialize");

        assert_eq!(response.selections, vec![SelectionRange { start: 0, end: 11 }]);
    }

    #[test]
    fn typed_plugin_requests_return_structured_results_or_errors() {
        let harness = ContextHarness::new("hello world");
        let mut ctx = harness.make_context();

        let diagnostics = ctx
            .do_plugin_cmd_sync(crate::plugins::PluginPid(9), PluginRequest::GetDiagnostics)
            .expect("diagnostics request should succeed");
        let format_err = ctx
            .do_plugin_cmd_sync(
                crate::plugins::PluginPid(9),
                PluginRequest::FormatDocument(FormatDocumentRequest { options: None }),
            )
            .expect_err("formatting should be unsupported");
        let code_actions_err = ctx
            .do_plugin_cmd_sync(
                crate::plugins::PluginPid(9),
                PluginRequest::GetCodeActions(CodeActionRequest {
                    range: crate::plugins::rpc::Range { start: 0, end: 5 },
                    diagnostics: Vec::new(),
                }),
            )
            .expect_err("code actions should be unsupported");

        let diagnostics: GetDiagnosticsResponse =
            serde_json::from_value(diagnostics).expect("diagnostics response should deserialize");

        assert!(diagnostics.diagnostics.is_empty());
        assert!(matches!(format_err, RemoteError::Custom { code: 501, .. }));
        assert!(matches!(code_actions_err, RemoteError::Custom { code: 501, .. }));
    }

    #[test]
    fn plugin_diagnostics_round_trip_through_view_state() {
        let harness = ContextHarness::new("hello world");
        let mut ctx = harness.make_context();

        ctx.do_plugin_cmd(
            crate::plugins::PluginPid(9),
            PluginNotification::UpdateDiagnostics {
                diagnostics: vec![Diagnostic {
                    range: crate::plugins::rpc::Range { start: 1, end: 4 },
                    severity: DiagnosticSeverity::Warning,
                    message: String::from("warn"),
                    source: Some(String::from("lsp")),
                    code: Some(String::from("W1")),
                }],
            },
        );

        let diagnostics: GetDiagnosticsResponse = serde_json::from_value(
            ctx.do_plugin_cmd_sync(crate::plugins::PluginPid(9), PluginRequest::GetDiagnostics)
                .expect("diagnostics request should succeed"),
        )
        .expect("diagnostics response should deserialize");

        assert_eq!(diagnostics.diagnostics.len(), 1);
        assert_eq!(diagnostics.diagnostics[0].message, "warn");
        assert_eq!(diagnostics.diagnostics[0].severity, DiagnosticSeverity::Warning);
    }

    #[test]
    fn test_gestures() {
        use crate::rpc::GestureType::*;
        let initial_text = "\
        this is a string\n\
        that has three\n\
        lines.";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();

        ctx.do_edit(EditNotification::MoveDown);
        ctx.do_edit(EditNotification::MoveDown);
        ctx.do_edit(EditNotification::MoveToEndOfParagraph);
        assert_eq!(harness.debug_render(),"\
        this is a string\n\
        that has three\n\
        lines.|" );

        ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
        ctx.do_edit(EditNotification::MoveToEndOfParagraphAndModifySelection);
        assert_eq!(harness.debug_render(),"\
        [this is a string|]\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::MoveToEndOfParagraph);
        ctx.do_edit(EditNotification::MoveToBeginningOfParagraphAndModifySelection);
        assert_eq!(harness.debug_render(),"\
        [|this is a string]\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
        assert_eq!(harness.debug_render(),"\
        |this is a string\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::Gesture { line: 0, col: 5, ty: PointSelect });
        assert_eq!(harness.debug_render(),"\
        this |is a string\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: ToggleSel });
        assert_eq!(harness.debug_render(),"\
        this |is a string\n\
        that |has three\n\
        lines." );

        ctx.do_edit(EditNotification::MoveToRightEndOfLineAndModifySelection);
        assert_eq!(harness.debug_render(),"\
        this [is a string|]\n\
        that [has three|]\n\
        lines." );

        ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: MultiWordSelect });
        assert_eq!(harness.debug_render(),"\
        this [is a string|]\n\
        that [has three|]\n\
        [lines|]." );

        ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: ToggleSel });
        assert_eq!(harness.debug_render(),"\
        this [is a string|]\n\
        that [has three|]\n\
        lines." );

        ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: ToggleSel });
        assert_eq!(harness.debug_render(),"\
        this [is a string|]\n\
        that [has three|]\n\
        li|nes." );

        ctx.do_edit(EditNotification::MoveToLeftEndOfLine);
        assert_eq!(harness.debug_render(),"\
        |this is a string\n\
        |that has three\n\
        |lines." );

        ctx.do_edit(EditNotification::MoveWordRight);
        assert_eq!(harness.debug_render(),"\
        this| is a string\n\
        that| has three\n\
        lines|." );

        ctx.do_edit(EditNotification::MoveToLeftEndOfLineAndModifySelection);
        assert_eq!(harness.debug_render(),"\
        [|this] is a string\n\
        [|that] has three\n\
        [|lines]." );

        ctx.do_edit(EditNotification::CollapseSelections);
        ctx.do_edit(EditNotification::MoveToRightEndOfLine);
        assert_eq!(harness.debug_render(),"\
        this is a string|\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: MultiLineSelect });
        assert_eq!(harness.debug_render(),"\
        this is a string|\n\
        that has three\n\
        [lines.|]" );

        ctx.do_edit(EditNotification::SelectAll);
        assert_eq!(harness.debug_render(),"\
        [this is a string\n\
        that has three\n\
        lines.|]" );

        ctx.do_edit(EditNotification::CollapseSelections);
        ctx.do_edit(EditNotification::AddSelectionAbove);
        assert_eq!(harness.debug_render(),"\
        this is a string\n\
        that h|as three\n\
        lines.|" );

        ctx.do_edit(EditNotification::MoveRight);
        assert_eq!(harness.debug_render(),"\
        this is a string\n\
        that ha|s three\n\
        lines.|" );

        ctx.do_edit(EditNotification::MoveLeft);
        assert_eq!(harness.debug_render(),"\
        this is a string\n\
        that h|as three\n\
        lines|." );
    }

    #[test]
    fn delete_combining_enclosing_keycaps_tests() {
        use crate::rpc::GestureType::*;

        let initial_text = "1\u{E0101}\u{20E3}";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 8, ty: PointSelect });

        assert_eq!(harness.debug_render(), "1\u{E0101}\u{20E3}|");

        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // multiple COMBINING ENCLOSING KEYCAP
        ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{20E3}".into() });
        assert_eq!(harness.debug_render(), "1\u{20E3}\u{20E3}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "1\u{20E3}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Isolated COMBINING ENCLOSING KEYCAP
        ctx.do_edit(EditNotification::Insert { chars: "\u{20E3}".into() });
        assert_eq!(harness.debug_render(), "\u{20E3}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Isolated multiple COMBINING ENCLOSING KEYCAP
        ctx.do_edit(EditNotification::Insert { chars: "\u{20E3}\u{20E3}".into() });
        assert_eq!(harness.debug_render(), "\u{20E3}\u{20E3}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{20E3}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");
    }

    #[test]
    fn delete_variation_selector_tests() {
        use crate::rpc::GestureType::*;

        let initial_text = "\u{FE0F}";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 3, ty: PointSelect });

        assert_eq!(harness.debug_render(), "\u{FE0F}|");

        // Isolated variation selector
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{E0100}".into() });
        assert_eq!(harness.debug_render(), "\u{E0100}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Isolated multiple variation selectors
        ctx.do_edit(EditNotification::Insert { chars: "\u{FE0F}\u{FE0F}".into() });
        assert_eq!(harness.debug_render(), "\u{FE0F}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{FE0F}\u{E0100}".into() });
        assert_eq!(harness.debug_render(), "\u{FE0F}\u{E0100}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{E0100}\u{FE0F}".into() });
        assert_eq!(harness.debug_render(), "\u{E0100}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{E0100}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{E0100}\u{E0100}".into() });
        assert_eq!(harness.debug_render(), "\u{E0100}\u{E0100}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{E0100}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Multiple variation selectors
        ctx.do_edit(EditNotification::Insert { chars: "#\u{FE0F}\u{FE0F}".into() });
        assert_eq!(harness.debug_render(), "#\u{FE0F}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "#\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "#\u{FE0F}\u{E0100}".into() });
        assert_eq!(harness.debug_render(), "#\u{FE0F}\u{E0100}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "#\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "#\u{E0100}\u{FE0F}".into() });
        assert_eq!(harness.debug_render(), "#\u{E0100}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "#\u{E0100}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "#\u{E0100}\u{E0100}".into() });
        assert_eq!(harness.debug_render(), "#\u{E0100}\u{E0100}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "#\u{E0100}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");
    }

    #[test]
    fn delete_emoji_zwj_sequence_tests() {
        use crate::rpc::GestureType::*;
        let initial_text = "\u{1F441}\u{200D}\u{1F5E8}";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
        assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{1F5E8}|");

        // U+200D is ZERO WIDTH JOINER.
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}\u{1F5E8}\u{FE0E}".into() });
        assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{1F5E8}\u{FE0E}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{1F373}".into() });
        assert_eq!(harness.debug_render(), "\u{1F469}\u{200D}\u{1F373}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{1F487}\u{200D}\u{2640}".into() });
        assert_eq!(harness.debug_render(), "\u{1F487}\u{200D}\u{2640}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{1F487}\u{200D}\u{2640}\u{FE0F}".into() });
        assert_eq!(harness.debug_render(), "\u{1F487}\u{200D}\u{2640}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{1F468}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F48B}\u{200D}\u{1F468}".into() });
        assert_eq!(harness.debug_render(), "\u{1F468}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F48B}\u{200D}\u{1F468}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Emoji modifier can be appended to the first emoji.
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{1F3FB}\u{200D}\u{1F4BC}".into() });
        assert_eq!(harness.debug_render(), "\u{1F469}\u{1F3FB}\u{200D}\u{1F4BC}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // End with ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}".into() });
        assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F441}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Start with ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{1F5E8}".into() });
        assert_eq!(harness.debug_render(), "\u{200D}\u{1F5E8}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::Insert { chars: "\u{FE0E}\u{200D}\u{1F5E8}".into() });
        assert_eq!(harness.debug_render(), "\u{FE0E}\u{200D}\u{1F5E8}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{FE0E}\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{FE0E}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Multiple ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}\u{200D}\u{1F5E8}".into() });
        assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{200D}\u{1F5E8}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F441}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Isolated ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{200D}".into() });
        assert_eq!(harness.debug_render(), "\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Isolated multiple ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{200D}".into() });
        assert_eq!(harness.debug_render(), "\u{200D}\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");
    }

    #[test]
    fn delete_flags_tests() {
        use crate::rpc::GestureType::*;
        let initial_text = "\u{1F1FA}";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 4, ty: PointSelect });

        // Isolated regional indicator symbol
        assert_eq!(harness.debug_render(), "\u{1F1FA}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Odd numbered regional indicator symbols
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{1F1F8}\u{1F1FA}".into() });
        assert_eq!(harness.debug_render(), "\u{1F1FA}\u{1F1F8}\u{1F1FA}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F1FA}\u{1F1F8}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Incomplete sequence. (no tag_term: U+E007E)
        ctx.do_edit(EditNotification::Insert { chars: "a\u{1F3F4}\u{E0067}b".into() });
        assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E0067}b|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E0067}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{1F3F4}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a|");

        // No tag_base
        ctx.do_edit(EditNotification::Insert { chars: "\u{E0067}\u{E007F}b".into() });
        assert_eq!(harness.debug_render(), "a\u{E0067}\u{E007F}b|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{E0067}\u{E007F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{E0067}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a|");

        // Isolated tag chars
        ctx.do_edit(EditNotification::Insert { chars: "\u{E0067}\u{E0067}b".into() });
        assert_eq!(harness.debug_render(), "a\u{E0067}\u{E0067}b|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{E0067}\u{E0067}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{E0067}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a|");

        // Isolated tab term.
        ctx.do_edit(EditNotification::Insert { chars: "\u{E007F}\u{E007F}b".into() });
        assert_eq!(harness.debug_render(), "a\u{E007F}\u{E007F}b|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{E007F}\u{E007F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{E007F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a|");

        // Immediate tag_term after tag_base
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F3F4}\u{E007F}\u{1F3F4}\u{E007F}b".into() });
        assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E007F}\u{1F3F4}\u{E007F}b|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E007F}\u{1F3F4}\u{E007F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a\u{1F3F4}\u{E007F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "a|");
    }

    #[test]
    fn delete_emoji_modifier_tests() {
        use crate::rpc::GestureType::*;
        let initial_text = "\u{1F466}\u{1F3FB}";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 8, ty: PointSelect });

        // U+1F3FB is EMOJI MODIFIER FITZPATRICK TYPE-1-2.
        assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Isolated emoji modifier
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F3FB}".into() });
        assert_eq!(harness.debug_render(), "\u{1F3FB}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Isolated multiple emoji modifier
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F3FB}\u{1F3FB}".into() });
        assert_eq!(harness.debug_render(), "\u{1F3FB}\u{1F3FB}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F3FB}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Multiple emoji modifiers
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{1F3FB}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");
    }

    #[test]
    fn delete_mixed_edge_cases_tests() {
        use crate::rpc::GestureType::*;
        let initial_text = "";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 7, ty: PointSelect });

        // COMBINING ENCLOSING KEYCAP + variation selector
        ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{FE0F}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "1|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Variation selector + COMBINING ENCLOSING KEYCAP
        ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{20E3}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");
        // COMBINING ENCLOSING KEYCAP + ending with ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{200D}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "1\u{20E3}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // COMBINING ENCLOSING KEYCAP + ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{200D}\u{1F5E8}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "1\u{20E3}\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "1\u{20E3}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Start with ZERO WIDTH JOINER + COMBINING ENCLOSING KEYCAP
        ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{20E3}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // ZERO WIDTH JOINER + COMBINING ENCLOSING KEYCAP
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F441}\u{200D}\u{20E3}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F441}\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F441}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // COMBINING ENCLOSING KEYCAP + regional indicator symbol
        ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{1F1FA}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "1\u{20E3}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Regional indicator symbol + COMBINING ENCLOSING KEYCAP
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{20E3}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F1FA}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // COMBINING ENCLOSING KEYCAP + emoji modifier
        ctx.do_edit(EditNotification::Insert { chars: "1\u{20E3}\u{1F3FB}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "1\u{20E3}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Emoji modifier + COMBINING ENCLOSING KEYCAP
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{20E3}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1f466}\u{1F3FB}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Variation selector + end with ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{200D}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Variation selector + ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F469}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Start with ZERO WIDTH JOINER + variation selector
        ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{FE0F}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // ZERO WIDTH JOINER + variation selector
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{FE0F}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F469}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Variation selector + regional indicator symbol
        ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{1F1FA}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Regional indicator symbol + variation selector
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{FE0F}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Variation selector + emoji modifier
        ctx.do_edit(EditNotification::Insert { chars: "\u{2665}\u{FE0F}\u{1F3FB}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{2665}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Emoji modifier + variation selector
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{FE0F}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F466}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Start withj ZERO WIDTH JOINER + regional indicator symbol
        ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{1F1FA}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // ZERO WIDTH JOINER + Regional indicator symbol
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{1F1FA}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F469}\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F469}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Regional indicator symbol + end with ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{200D}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F1FA}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Regional indicator symbol + ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{200D}\u{1F469}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Start with ZERO WIDTH JOINER + emoji modifier
        ctx.do_edit(EditNotification::Insert { chars: "\u{200D}\u{1F3FB}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // ZERO WIDTH JOINER + emoji modifier
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F469}\u{200D}\u{1F3FB}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F469}\u{200D}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F469}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Emoji modifier + end with ZERO WIDTH JOINER
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{200D}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Regional indicator symbol + Emoji modifier
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F1FA}\u{1F3FB}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F1FA}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // Emoji modifier + regional indicator symbol
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F466}\u{1F3FB}\u{1F1FA}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F466}\u{1F3FB}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");

        // RIS + LF
        ctx.do_edit(EditNotification::Insert { chars: "\u{1F1E6}\u{000A}".into() });
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "\u{1F1E6}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");
    }

    #[test]
    fn delete_variation_selector_with_combining_mark_uses_grapheme_boundary() {
        use crate::rpc::GestureType::*;

        let harness = ContextHarness::new("e\u{0301}\u{FE0F}");
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 6, ty: PointSelect });

        assert_eq!(harness.debug_render(), "e\u{0301}\u{FE0F}|");
        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(), "|");
    }

    #[test]
    fn edit_type_to_string_matches_wire_names() {
        assert_eq!(edit_type_to_string(EditType::InsertChars), "insert");
        assert_eq!(edit_type_to_string(EditType::InsertNewline), "newline");
        assert_eq!(edit_type_to_string(EditType::Other), "other");
    }

    #[test]
    fn delete_tests() {
        use crate::rpc::GestureType::*;
        let initial_text = "\
        this is a string\n\
        that has three\n\
        lines.";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });

        ctx.do_edit(EditNotification::MoveRight);
        assert_eq!(harness.debug_render(),"\
        t|his is a string\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::DeleteBackward);
        assert_eq!(harness.debug_render(),"\
        |his is a string\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::DeleteForward);
        assert_eq!(harness.debug_render(),"\
        |is is a string\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::MoveWordRight);
        ctx.do_edit(EditNotification::DeleteWordForward);
        assert_eq!(harness.debug_render(),"\
        is| a string\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::DeleteWordBackward);
        assert_eq!(harness.debug_render(),"| \
        a string\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::MoveToRightEndOfLine);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        assert_eq!(harness.debug_render(),"\
        |\nthat has three\n\
        lines." );

        ctx.do_edit(EditNotification::DeleteToEndOfParagraph);
        ctx.do_edit(EditNotification::DeleteToEndOfParagraph);
        assert_eq!(harness.debug_render(),"\
        |\nlines." );
    }

    #[test]
    fn simple_indentation_test() {
        use crate::rpc::GestureType::*;
        let harness = ContextHarness::new("");
        let mut ctx = harness.make_context();
        // Single indent and outdent test
        ctx.do_edit(EditNotification::Insert { chars: "hello".into() });
        ctx.do_edit(EditNotification::Indent);
        assert_eq!(harness.debug_render(),"    hello|");
        ctx.do_edit(EditNotification::Outdent);
        assert_eq!(harness.debug_render(),"hello|");

        // Test when outdenting with less than 4 spaces
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
        ctx.do_edit(EditNotification::Insert { chars: "  ".into() });
        assert_eq!(harness.debug_render(),"  |hello");
        ctx.do_edit(EditNotification::Outdent);
        assert_eq!(harness.debug_render(),"|hello");

        // Non-selection one line indent and outdent test
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::Indent);
        ctx.do_edit(EditNotification::InsertNewline);
        ctx.do_edit(EditNotification::Insert { chars: "world".into() });
        assert_eq!(harness.debug_render(),"    hello\nworld|");

        ctx.do_edit(EditNotification::MoveWordLeft);
        ctx.do_edit(EditNotification::MoveToBeginningOfDocumentAndModifySelection);
        ctx.do_edit(EditNotification::Indent);
        assert_eq!(harness.debug_render(),"    [|    hello\n]world");

        ctx.do_edit(EditNotification::Outdent);
        assert_eq!(harness.debug_render(),"[|    hello\n]world");

        ctx.do_edit(EditNotification::SelectAll);
        ctx.do_edit(EditNotification::DeleteBackward);
        ctx.do_edit(EditNotification::Insert { chars: "hello".into() });
        ctx.do_edit(EditNotification::SelectAll);
        ctx.do_edit(EditNotification::InsertTab);
        assert_eq!(harness.debug_render(),"    |");
    }

    #[test]
    fn multiline_indentation_test() {
        use crate::rpc::GestureType::*;
        let initial_text = "\
        this is a string\n\
        that has three\n\
        lines.";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();

        ctx.do_edit(EditNotification::Gesture { line: 0, col: 5, ty: PointSelect });
        assert_eq!(harness.debug_render(),"\
        this |is a string\n\
        that has three\n\
        lines." );

        ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: ToggleSel });
        assert_eq!(harness.debug_render(),"\
        this |is a string\n\
        that |has three\n\
        lines." );

        // Simple multi line indent/outdent test
        ctx.do_edit(EditNotification::Indent);
        assert_eq!(harness.debug_render(),"    \
        this |is a string\n    \
        that |has three\n\
        lines." );

        ctx.do_edit(EditNotification::Outdent);
        ctx.do_edit(EditNotification::Outdent);
        assert_eq!(harness.debug_render(),"\
        this |is a string\n\
        that |has three\n\
        lines." );

        // Different position indent/outdent test
        // Shouldn't change cursor position
        ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: ToggleSel });
        ctx.do_edit(EditNotification::Gesture { line: 1, col: 10, ty: ToggleSel });
        assert_eq!(harness.debug_render(),"\
        this |is a string\n\
        that has t|hree\n\
        lines." );

        ctx.do_edit(EditNotification::Indent);
        assert_eq!(harness.debug_render(),"    \
        this |is a string\n    \
        that has t|hree\n\
        lines." );

        ctx.do_edit(EditNotification::Outdent);
        assert_eq!(harness.debug_render(),"\
        this |is a string\n\
        that has t|hree\n\
        lines." );

        // Multi line selection test
        ctx.do_edit(EditNotification::Gesture { line: 1, col: 10, ty: ToggleSel });
        ctx.do_edit(EditNotification::MoveToEndOfDocumentAndModifySelection);
        ctx.do_edit(EditNotification::Indent);
        assert_eq!(harness.debug_render(),"    \
        this [is a string\n    \
        that has three\n    \
        lines.|]" );

        ctx.do_edit(EditNotification::Outdent);
        assert_eq!(harness.debug_render(),"\
        this [is a string\n\
        that has three\n\
        lines.|]" );

        // Multi cursor different line indent test
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
        ctx.do_edit(EditNotification::Gesture { line: 2, col: 0, ty: ToggleSel });
        assert_eq!(harness.debug_render(),"\
        |this is a string\n\
        that has three\n\
        |lines." );

        ctx.do_edit(EditNotification::Indent);
        assert_eq!(harness.debug_render(),"    \
        |this is a string\n\
        that has three\n    \
        |lines." );

        ctx.do_edit(EditNotification::Outdent);
        assert_eq!(harness.debug_render(),"\
        |this is a string\n\
        that has three\n\
        |lines." );
    }

    #[test]
    fn number_change_tests() {
        use crate::rpc::GestureType::*;
        let harness = ContextHarness::new("");
        let mut ctx = harness.make_context();
        // Single indent and outdent test
        ctx.do_edit(EditNotification::Insert { chars: "1234".into() });
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "1235|");

        ctx.do_edit(EditNotification::Gesture { line: 0, col: 2, ty: PointSelect });
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "1236|");

        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "-42".into() });
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "-41|");

        // Cursor is on the 3
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a 336 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
        ctx.do_edit(EditNotification::DecreaseNumber);
        assert_eq!(harness.debug_render(), "this is a 335| text example");

        // Cursor is on of the 3
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a -336 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
        ctx.do_edit(EditNotification::DecreaseNumber);
        assert_eq!(harness.debug_render(), "this is a -337| text example");

        // Cursor is on the 't' of text
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a -336 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 15, ty: PointSelect });
        ctx.do_edit(EditNotification::DecreaseNumber);
        assert_eq!(harness.debug_render(), "this is a -336 |text example");

        // test multiple iterations
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a 336 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
        ctx.do_edit(EditNotification::IncreaseNumber);
        ctx.do_edit(EditNotification::IncreaseNumber);
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "this is a 339| text example");

        // test changing number of chars
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a 10 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
        ctx.do_edit(EditNotification::DecreaseNumber);
        assert_eq!(harness.debug_render(), "this is a 9| text example");

        // test going negative
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a 0 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
        ctx.do_edit(EditNotification::DecreaseNumber);
        assert_eq!(harness.debug_render(), "this is a -1| text example");

        // test going positive
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a -1 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 12, ty: PointSelect });
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "this is a 0| text example");

        // if it begins in a region, nothing will happen
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a 10 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 10, ty: PointSelect });
        ctx.do_edit(EditNotification::MoveToEndOfDocumentAndModifySelection);
        ctx.do_edit(EditNotification::DecreaseNumber);
        assert_eq!(harness.debug_render(), "this is a [10 text example|]");

        // If a number just happens to be in a region, nothing will happen
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a 10 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 5, ty: PointSelect });
        ctx.do_edit(EditNotification::MoveToEndOfDocumentAndModifySelection);
        ctx.do_edit(EditNotification::DecreaseNumber);
        assert_eq!(harness.debug_render(), "this [is a 10 text example|]");

        // if it ends on a region, the number will be changed
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a 10".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
        ctx.do_edit(EditNotification::MoveToEndOfDocumentAndModifySelection);
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "[this is a 11|]");

        // if only a part of a number is in a region, the whole number will be changed
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "this is a 1000 text example".into() });
        ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
        ctx.do_edit(EditNotification::MoveRightAndModifySelection);
        ctx.do_edit(EditNotification::DecreaseNumber);
        assert_eq!(harness.debug_render(), "this is a 999| text example");

        // invalid numbers
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "10_000".into() });
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "10_000|");

        // decimals are kinda accounted for (i.e. 4.55 becomes 4.56 (good), but 4.99 becomes 4.100 (bad)
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "4.55".into() });
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "4.56|");

        // invalid numbers
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        ctx.do_edit(EditNotification::Insert { chars: "0xFF03".into() });
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "0xFF03|");

        // Test multiple selections
        ctx.do_edit(EditNotification::MoveToEndOfDocument);
        ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
        let multi_text = "\
        example 42 number\n\
        example 90 number\n\
        Done.";
        ctx.do_edit(EditNotification::Insert { chars: multi_text.into() });
        ctx.do_edit(EditNotification::Gesture { line: 1, col: 9, ty: PointSelect });
        ctx.do_edit(EditNotification::AddSelectionAbove);
        ctx.do_edit(EditNotification::IncreaseNumber);
        assert_eq!(harness.debug_render(), "\
        example 43| number\n\
        example 91| number\n\
        Done.");
    }

    #[test]
    fn text_recording() {
        use crate::rpc::GestureType::*;
        let initial_text = "";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();

        let recording_name = String::new();

        ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
        assert_eq!(harness.debug_render(), "|");

        ctx.do_edit(EditNotification::ToggleRecording { recording_name: Some(recording_name.clone()) });

        ctx.do_edit(EditNotification::Insert { chars: "Foo ".to_owned() });
        ctx.do_edit(EditNotification::Insert { chars: "B".to_owned() });
        ctx.do_edit(EditNotification::Insert { chars: "A".to_owned() });
        ctx.do_edit(EditNotification::Insert { chars: "R".to_owned() });
        assert_eq!(harness.debug_render(), "Foo BAR|");

        ctx.do_edit(EditNotification::ToggleRecording { recording_name: Some(recording_name.clone())});
        ctx.do_edit(EditNotification::Insert { chars: " ".to_owned() });

        ctx.do_edit(EditNotification::PlayRecording { recording_name });
        assert_eq!(harness.debug_render(), "Foo BAR Foo BAR|");
    }

    #[test]
    fn movement_recording() {
        use crate::rpc::GestureType::*;
        let initial_text = "\
        this is a string\n\
        that has about\n\
        four really nice\n\
        lines to see.";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();

        let recording_name = String::new();

        ctx.do_edit(EditNotification::Gesture { line: 0, col: 5, ty: PointSelect });
        assert_eq!(harness.debug_render(),"\
        this |is a string\n\
        that has about\n\
        four really nice\n\
        lines to see." );

        ctx.do_edit(EditNotification::ToggleRecording { recording_name: Some(recording_name.clone()) });

        // Swap last word of the current line and the line below
        ctx.do_edit(EditNotification::AddSelectionBelow);
        ctx.do_edit(EditNotification::MoveToRightEndOfLine);
        ctx.do_edit(EditNotification::MoveWordLeftAndModifySelection);
        ctx.do_edit(EditNotification::Transpose);
        ctx.do_edit(EditNotification::CollapseSelections);
        ctx.do_edit(EditNotification::MoveToRightEndOfLine);
        assert_eq!(harness.debug_render(),"\
        this is a about|\n\
        that has string\n\
        four really nice\n\
        lines to see." );

        ctx.do_edit(EditNotification::ToggleRecording { recording_name: Some(recording_name.clone())});

        ctx.do_edit(EditNotification::Gesture { line: 2, col: 5, ty: PointSelect });
        ctx.do_edit(EditNotification::PlayRecording { recording_name: recording_name.clone() });
        assert_eq!(harness.debug_render(),"\
        this is a about\n\
        that has string\n\
        four really see.|\n\
        lines to nice" );

        // Undo entire playback in a single command
        ctx.do_edit(EditNotification::Undo);
        assert_eq!(harness.debug_render(),"\
        this is a about\n\
        that has string\n\
        four really nice|\n\
        lines to see." );

        // Make sure we can redo in a single command as well
        ctx.do_edit(EditNotification::Redo);
        assert_eq!(harness.debug_render(),"\
        this is a about\n\
        that has string\n\
        four really see.|\n\
        lines to nice" );

        // We shouldn't be able to use cleared recordings
        ctx.do_edit(EditNotification::Undo);
        ctx.do_edit(EditNotification::Undo);
        ctx.do_edit(EditNotification::ClearRecording { recording_name: recording_name.clone() });
        ctx.do_edit(EditNotification::PlayRecording { recording_name });
        assert_eq!(harness.debug_render(),"\
        this is a string\n\
        that has about\n\
        four really nice|\n\
        lines to see." );
    }

    #[test]
    fn test_exact_position() {
        use crate::rpc::GestureType::*;
        let initial_text = "\
        this is a string\n\
        that has three\n\
        \n\
        lines.\n\
        And lines with very different length.";
        let harness = ContextHarness::new(initial_text);
        let mut ctx = harness.make_context();
        ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: PointSelect });
        ctx.do_edit(EditNotification::AddSelectionAbove);
        assert_eq!(harness.debug_render(),"\
        this |is a string\n\
        that |has three\n\
        \n\
        lines.\n\
        And lines with very different length.");

        ctx.do_edit(EditNotification::CollapseSelections);
        ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: PointSelect });
        ctx.do_edit(EditNotification::AddSelectionBelow);
        assert_eq!(harness.debug_render(),"\
        this is a string\n\
        that |has three\n\
        \n\
        lines|.\n\
        And lines with very different length.");

        ctx.do_edit(EditNotification::CollapseSelections);
        ctx.do_edit(EditNotification::Gesture { line: 4, col: 10, ty: PointSelect });
        ctx.do_edit(EditNotification::AddSelectionAbove);
        assert_eq!(harness.debug_render(),"\
        this is a string\n\
        that has t|hree\n\
        \n\
        lines.\n\
        And lines |with very different length.");
    }

    #[test]
    fn test_illegal_plugin_edit() {
        use xi_rope::DeltaBuilder;
        use crate::plugins::rpc::{PluginNotification, PluginEdit};
        use crate::plugins::PluginPid;

        let text = "text";
        let harness = ContextHarness::new(text);
        let mut ctx = harness.make_context();
        let rev_token = ctx.editor.borrow().get_head_rev_token();

        let iv = Interval::new(1, 1);
        let mut builder = DeltaBuilder::new(0); // wrong length
        builder.replace(iv, "1".into());

        let edit_one = PluginEdit {
            rev: rev_token,
            delta: builder.build(),
            priority: 55,
            after_cursor: false,
            undo_group: None,
            author: "plugin_one".into(),
        };

        ctx.do_plugin_cmd(PluginPid(1), PluginNotification::Edit { edit: edit_one });
        let new_rev_token = ctx.editor.borrow().get_head_rev_token();
        // no change should be made
        assert_eq!(rev_token, new_rev_token);
    }


    #[test]
    fn empty_transpose() {
        let harness = ContextHarness::new("");
        let mut ctx = harness.make_context();

        ctx.do_edit(EditNotification::Transpose);

        assert_eq!(harness.debug_render(), "|"); // should be noop
    }

    // This is the issue reported by #962
    #[test]
    fn eol_multicursor_transpose() {
        use crate::rpc::GestureType::*;

        let harness = ContextHarness::new("word\n");
        let mut ctx = harness.make_context();

        ctx.do_edit(EditNotification::Gesture{line: 0, col: 4, ty: PointSelect}); // end of first line
        ctx.do_edit(EditNotification::AddSelectionBelow); // add cursor below that, at eof
        ctx.do_edit(EditNotification::Transpose);

        assert_eq!(harness.debug_render(), "wor\nd|");
    }
}
