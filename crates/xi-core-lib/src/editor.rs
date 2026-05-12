// Copyright 2016 The xi-editor Authors.
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

use std::borrow::{Borrow, Cow};
use std::cmp::{max, min};
use std::collections::BTreeSet;
use std::path::Path;

use log::error;
use serde::{Deserialize, Serialize};

use xi_rope::diff::{Diff, LineHashDiff};
use xi_rope::engine::{Engine, RevId, RevToken};
use xi_rope::rope::count_newlines;
use xi_rope::spans::SpansBuilder;
use xi_rope::{DeltaBuilder, Interval, LinesMetric, Rope, RopeDelta, Transformer};

use crate::annotations::{AnnotationType, Annotations};
use crate::config::BufferItems;
use crate::edit_ops::{self, IndentDirection};
use crate::edit_types::BufferEvent;
use crate::event_context::MAX_SIZE_LIMIT;
use crate::line_offset::{LineOffset, LogicalLines};
use crate::movement::Movement;
use crate::plugins::PluginId;
use crate::plugins::rpc::{DataSpan, GetDataResponse, PluginEdit, PluginEditAck, TextUnit};
use crate::rpc::{LineRange, SelectionModifier};
use crate::selection::{InsertDrift, SelRegion, Selection};
use crate::text_store::DocumentMode;
use crate::view::{Replace, View};

// TODO This could go much higher without issue but while developing it is
// better to keep it low to expose bugs in the GC during casual testing.
const MAX_UNDOS: usize = 20;

pub struct Editor {
    /// The contents of the buffer.
    text: Rope,
    /// Normal or constrained-normal mode for rope-backed buffers.
    rope_mode: DocumentMode,
    /// The CRDT engine, which tracks edit history and manages concurrent edits.
    engine: Engine,

    /// VLF store for large files opened in VLF mode.
    ///
    /// When `Some`, this editor was opened in VLF mode. `text` is an empty
    /// placeholder; all content reads go through `vlf_store`.
    /// `None` for Normal / ConstrainedNormal editors.
    /// Editor owns dirty/pristine revision state for this VLF backing.
    pub(crate) vlf_store: Option<Box<crate::vlf::store::VlfStore>>,

    /// The most recent revision.
    last_rev_id: RevId,
    /// The revision of the last save.
    pristine_rev_id: RevId,
    /// Monotonic VLF overlay revision counter.
    vlf_head_revision: u64,
    /// VLF overlay revision that matches on-disk state.
    vlf_pristine_revision: u64,
    undo_group_id: usize,
    /// Undo groups that may still be toggled
    live_undos: Vec<usize>,
    /// The index of the current undo; subsequent undos are currently 'undone'
    /// (but may be redone)
    cur_undo: usize,
    /// undo groups that are undone
    undos: BTreeSet<usize>,
    /// undo groups that are no longer live and should be gc'ed
    gc_undos: BTreeSet<usize>,
    force_undo_group: bool,

    this_edit_type: EditType,
    last_edit_type: EditType,

    revs_in_flight: usize,

    /// Tracks an in-flight async whole-document scan operation (e.g. reindent).
    pub(crate) whole_scan_task: crate::whole_scan::WholeScanTask,
    /// Tracks an in-flight async save operation for rope-backed buffers.
    pub(crate) save_task: crate::whole_scan::SaveTask,
}

impl Editor {
    /// Creates a new `Editor` with a new empty buffer.
    pub fn new() -> Editor {
        Self::with_text("")
    }

    /// Creates a new `Editor`, loading text into a new buffer.
    pub fn with_text<T: Into<Rope>>(text: T) -> Editor {
        Self::with_text_mode(text, DocumentMode::Normal)
    }

    /// Creates a new rope-backed `Editor` with explicit document mode.
    pub fn with_text_mode<T: Into<Rope>>(text: T, mode: DocumentMode) -> Editor {
        let engine = Engine::new(text.into());
        let buffer = engine.get_head().clone();
        let last_rev_id = engine.get_head_rev_id();

        Editor {
            text: buffer,
            rope_mode: mode,
            engine,
            last_rev_id,
            pristine_rev_id: last_rev_id,
            vlf_head_revision: 0,
            vlf_pristine_revision: 0,
            undo_group_id: 1,
            // GC only works on undone edits or prefixes of the visible edits,
            // but initial file loading can create an edit with undo group 0,
            // so we want to collect that as part of the prefix.
            live_undos: vec![0],
            cur_undo: 1,
            undos: BTreeSet::new(),
            gc_undos: BTreeSet::new(),
            force_undo_group: false,
            last_edit_type: EditType::Other,
            this_edit_type: EditType::Other,
            revs_in_flight: 0,
            vlf_store: None,
            whole_scan_task: crate::whole_scan::WholeScanTask::new(),
            save_task: crate::whole_scan::SaveTask::new(),
        }
    }

    /// Creates a new `Editor` for a VLF file.
    ///
    /// The `text` buffer starts empty; all content reads must go through the
    /// `VlfStore`.  VLF editors start read-only; callers must explicitly opt
    /// into overlay editing via [`Self::enable_vlf_editing`].
    pub fn with_vlf_store(store: crate::vlf::store::VlfStore) -> Editor {
        let engine = Engine::new(Rope::from(""));
        let buffer = engine.get_head().clone();
        let last_rev_id = engine.get_head_rev_id();
        Editor {
            text: buffer,
            rope_mode: DocumentMode::Vlf,
            engine,
            last_rev_id,
            pristine_rev_id: last_rev_id,
            vlf_head_revision: 0,
            vlf_pristine_revision: 0,
            undo_group_id: 1,
            live_undos: vec![0],
            cur_undo: 1,
            undos: BTreeSet::new(),
            gc_undos: BTreeSet::new(),
            force_undo_group: false,
            last_edit_type: EditType::Other,
            this_edit_type: EditType::Other,
            revs_in_flight: 0,
            vlf_store: Some(Box::new(store)),
            whole_scan_task: crate::whole_scan::WholeScanTask::new(),
            save_task: crate::whole_scan::SaveTask::new(),
        }
    }

    /// Returns `true` when this editor was opened in VLF mode.
    pub(crate) fn is_vlf(&self) -> bool {
        self.vlf_store.is_some()
    }

    pub(crate) fn set_document_mode(&mut self, mode: DocumentMode) {
        if !self.is_vlf() {
            self.rope_mode = mode;
        }
    }

    pub(crate) fn document_mode(&self) -> DocumentMode {
        if self.is_vlf() { DocumentMode::Vlf } else { self.rope_mode }
    }

    pub(crate) fn get_buffer(&self) -> &Rope {
        // Mutation and rope-only algorithms may still use the backing rope.
        // Read-only hot paths should prefer `text_store_snapshot()`.
        &self.text
    }

    /// Return a `RopeTextStore` snapshot of the current buffer.
    ///
    /// Use this for read-only and query operations (viewport line reads, search
    /// chunk iteration, status reporting) instead of reaching into `self.text`
    /// directly. Edit mutations continue to use `self.text` via the existing
    /// `Editor`/`Rope` path.
    pub(crate) fn text_store_snapshot(&self) -> crate::text_store::rope_store::RopeTextStore {
        crate::text_store::rope_store::RopeTextStore::new_with_mode_and_snapshot(
            self.text.clone(),
            self.rope_mode,
            self.engine.get_head_rev_id().token(),
        )
    }

    pub(crate) fn get_head_rev_token(&self) -> u64 {
        self.engine.get_head_rev_id().token()
    }

    pub(crate) fn get_head_rev_id(&self) -> RevId {
        self.engine.get_head_rev_id()
    }

    pub(crate) fn get_edit_type(&self) -> EditType {
        self.this_edit_type
    }

    pub(crate) fn get_active_undo_group(&self) -> usize {
        *self.live_undos.last().unwrap_or(&0)
    }

    #[allow(dead_code)]
    pub(crate) fn enable_vlf_editing(&mut self) -> bool {
        let Some(store) = self.vlf_store.as_ref() else {
            return false;
        };
        store.enable_editing();
        true
    }

    pub(crate) fn vlf_save_enabled(&self) -> bool {
        self.vlf_store.as_ref().is_some_and(|store| store.is_save_enabled())
    }

    pub(crate) fn refresh_after_vlf_save(&mut self, path: &Path) -> std::io::Result<()> {
        let Some(store) = self.vlf_store.as_mut() else {
            return Ok(());
        };
        store.refresh_after_save(path)
    }

    #[allow(dead_code)]
    pub(crate) fn next_vlf_overlay_edit_context(
        &mut self,
        edit_type: EditType,
    ) -> Option<crate::vlf::overlay::OverlayEditContext> {
        self.vlf_store.as_ref()?;
        self.this_edit_type = edit_type;
        let revision_id = self.vlf_head_revision.saturating_add(1);
        let undo_group = self.calculate_undo_group();
        self.last_edit_type = self.this_edit_type;
        Some(crate::vlf::overlay::OverlayEditContext { revision_id, undo_group })
    }

    #[allow(dead_code)]
    pub(crate) fn commit_vlf_overlay_revision(&mut self, revision_id: u64) {
        if self.is_vlf() {
            self.vlf_head_revision = self.vlf_head_revision.max(revision_id);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn vlf_overlay_delta_for_undo_group(
        &self,
        undo_group: usize,
    ) -> Option<crate::vlf::overlay::OverlayDelta> {
        self.vlf_store.as_ref()?.overlay_delta_for_undo_group(undo_group)
    }

    pub(crate) fn update_edit_type(&mut self) {
        self.last_edit_type = self.this_edit_type;
        self.this_edit_type = EditType::Other
    }

    pub(crate) fn commit_undo_checkpoint(&mut self) {
        self.last_edit_type = EditType::Other;
        self.this_edit_type = EditType::Other;
    }

    pub(crate) fn set_pristine(&mut self) {
        if self.is_vlf() {
            self.vlf_pristine_revision = self.vlf_head_revision;
        } else {
            self.pristine_rev_id = self.engine.get_head_rev_id();
        }
    }

    pub(crate) fn set_pristine_if_equivalent_revision(&mut self, saved_rev_id: RevId) -> bool {
        if self.is_vlf() {
            self.set_pristine();
            return true;
        }
        let head_rev_id = self.engine.get_head_rev_id();
        if self.engine.is_equivalent_revision(saved_rev_id, head_rev_id) {
            self.pristine_rev_id = head_rev_id;
            true
        } else {
            false
        }
    }

    pub(crate) fn is_pristine(&self) -> bool {
        if self.is_vlf() {
            self.vlf_pristine_revision == self.vlf_head_revision
        } else {
            self.engine.is_equivalent_revision(self.pristine_rev_id, self.engine.get_head_rev_id())
        }
    }

    /// Applies `delta` directly with the given `edit_type`.
    ///
    /// Use this when core (not a plugin) computes a delta for language-aware
    /// features such as toggle-comment or reindent.
    pub(crate) fn apply_direct_delta(&mut self, edit_type: EditType, delta: RopeDelta) {
        self.this_edit_type = edit_type;
        self.add_delta(delta);
    }

    /// Sets this Editor's contents to `text`, preserving undo state and cursor
    /// position when possible.
    pub fn reload(&mut self, text: Rope) {
        let delta = LineHashDiff::compute_delta(self.get_buffer(), &text);
        self.add_delta(delta);
        self.set_pristine();
    }

    /// Increments the count of plugin revisions in flight.
    ///
    /// Each outstanding plugin edit holds a reference to the current revision;
    /// CRDT garbage collection is deferred until all in-flight revisions are
    /// acknowledged via [`dec_revs_in_flight`].
    ///
    /// [`dec_revs_in_flight`]: Self::dec_revs_in_flight
    pub fn increment_revs_in_flight(&mut self) {
        self.revs_in_flight += 1;
    }

    /// Decrements the count of plugin revisions in flight and triggers CRDT
    /// garbage collection when the count reaches zero.
    ///
    /// Must only be called after a corresponding [`increment_revs_in_flight`].
    ///
    /// [`increment_revs_in_flight`]: Self::increment_revs_in_flight
    pub fn dec_revs_in_flight(&mut self) {
        self.revs_in_flight -= 1;
        self.gc_undos();
    }

    /// Applies a delta to the text, and updates undo state.
    ///
    /// Records the delta into the CRDT engine so that it can be undone. Also
    /// contains the logic for merging edits into the same undo group. At call
    /// time, self.this_edit_type should be set appropriately.
    ///
    /// This method can be called multiple times, accumulating deltas that will
    /// be committed at once with `commit_delta`. Note that it does not update
    /// the views. Thus, view-associated state such as the selection and line
    /// breaks are to be considered invalid after this method, until the
    /// `commit_delta` call.
    fn add_delta(&mut self, delta: RopeDelta) {
        let head_rev_id = self.engine.get_head_rev_id();
        let undo_group = self.calculate_undo_group();
        self.last_edit_type = self.this_edit_type;
        let priority = 0x10000;
        self.engine.edit_rev(priority, undo_group, head_rev_id.token(), delta);
        self.text = self.engine.get_head().clone();
    }

    pub(crate) fn calculate_undo_group(&mut self) -> usize {
        let has_undos = !self.live_undos.is_empty();
        let force_undo_group = self.force_undo_group;
        let is_unbroken_group = !self.this_edit_type.breaks_undo_group(self.last_edit_type);

        if has_undos && (force_undo_group || is_unbroken_group) {
            *self.live_undos.last().unwrap()
        } else {
            let undo_group = self.undo_group_id;
            self.gc_undos.extend(self.live_undos[self.cur_undo..].iter().copied());
            self.live_undos.truncate(self.cur_undo);
            self.live_undos.push(undo_group);
            if self.live_undos.len() <= MAX_UNDOS {
                self.cur_undo += 1;
            } else {
                self.gc_undos.insert(self.live_undos.remove(0));
            }
            self.undo_group_id += 1;
            undo_group
        }
    }

    /// generates a delta from a plugin's response and applies it to the buffer.
    pub fn apply_plugin_edit(&mut self, edit: PluginEdit) -> PluginEditAck {
        let _t = tracing::trace_span!("Editor::apply_plugin_edit", categories = "core").entered();
        //TODO: get priority working, so that plugin edits don't necessarily move cursor
        let PluginEdit { rev, delta, priority, undo_group, .. } = edit;
        let priority = priority as usize;
        let undo_group = undo_group.unwrap_or_else(|| self.calculate_undo_group());
        match self.engine.try_edit_rev(priority, undo_group, rev, delta) {
            Err(e) => {
                let reason = e.to_string();
                error!("Error applying plugin edit: {}", reason);
                PluginEditAck { applied: false, rev, reason: Some(reason) }
            }
            Ok(_) => {
                self.text = self.engine.get_head().clone();
                PluginEditAck { applied: true, rev, reason: None }
            }
        }
    }

    /// Commits the current delta. If the buffer has changed, returns
    /// buffer, and an `InsertDrift` enum describing the correct selection update
    /// behaviour.
    pub(crate) fn commit_delta(&mut self) -> Option<(RopeDelta, Rope, InsertDrift)> {
        let _t = tracing::trace_span!("Editor::commit_delta", categories = "core").entered();

        if self.engine.get_head_rev_id() == self.last_rev_id {
            return None;
        }

        let last_token = self.last_rev_id.token();
        let delta = self.engine.try_delta_rev_head(last_token).expect("last_rev not found");
        // TODO (performance): it's probably quicker to stash last_text
        // rather than resynthesize it.
        let last_text = self.engine.get_rev(last_token).expect("last_rev not found");

        // Transpose can rotate characters inside of a selection; this is why it's an Inside edit.
        // Surround adds characters on either side of a selection, that's why it's an Outside edit.
        let drift = match self.this_edit_type {
            EditType::Transpose => InsertDrift::Inside,
            EditType::Surround => InsertDrift::Outside,
            _ => InsertDrift::Default,
        };
        self.last_rev_id = self.engine.get_head_rev_id();
        Some((delta, last_text, drift))
    }

    fn gc_undos(&mut self) {
        if self.revs_in_flight == 0 && !self.gc_undos.is_empty() {
            let gc_groups: Vec<usize> = self.gc_undos.iter().copied().collect();
            self.engine.gc(self.gc_undos.iter());
            if let Some(store) = self.vlf_store.as_ref() {
                for undo_group in &gc_groups {
                    store.gc_undo_group(*undo_group);
                }
            }
            self.undos = &self.undos - &self.gc_undos;
            self.gc_undos.clear();
        }
    }

    fn do_insert(&mut self, view: &View, config: &BufferItems, chars: &str) {
        let pair_search = config.surrounding_pairs.iter().find(|pair| pair.0 == chars);
        let caret_exists = view.sel_regions().iter().any(|region| region.is_caret());
        if let (Some(pair), false) = (pair_search, caret_exists) {
            self.this_edit_type = EditType::Surround;
            self.add_delta(edit_ops::surround(
                &self.text,
                view.sel_regions(),
                pair.0.to_string(),
                pair.1.to_string(),
            ));
        } else {
            self.this_edit_type = EditType::InsertChars;
            self.add_delta(edit_ops::insert(&self.text, view.sel_regions(), chars));
        }
    }

    fn do_paste(&mut self, view: &View, chars: &str) {
        if view.sel_regions().len() == 1 || view.sel_regions().len() != count_lines(chars) {
            self.add_delta(edit_ops::insert(&self.text, view.sel_regions(), chars));
        } else {
            let mut builder = DeltaBuilder::new(self.text.len());
            for (sel, line) in view.sel_regions().iter().zip(chars.lines()) {
                let iv = Interval::new(sel.min(), sel.max());
                builder.replace(iv, line.into());
            }
            self.add_delta(builder.build());
        }
    }

    fn do_undo(&mut self) {
        if self.cur_undo > 1 {
            self.cur_undo -= 1;
            assert!(self.undos.insert(self.live_undos[self.cur_undo]));
            self.this_edit_type = EditType::Undo;
            self.update_undos();
        }
    }

    fn do_redo(&mut self) {
        if self.cur_undo < self.live_undos.len() {
            assert!(self.undos.remove(&self.live_undos[self.cur_undo]));
            self.cur_undo += 1;
            self.this_edit_type = EditType::Redo;
            self.update_undos();
        }
    }

    fn update_undos(&mut self) {
        self.engine.undo(self.undos.iter());
        self.text = self.engine.get_head().clone();
    }

    fn do_replace(&mut self, view: &mut View, replace_all: bool) {
        if let Some(Replace { chars, .. }) = view.get_replace() {
            // todo: implement preserve case
            // store old selection because in case nothing is found the selection will be preserved
            let mut old_selection = Selection::new();
            for &region in view.sel_regions() {
                old_selection.add_region(region);
            }
            view.collapse_selections(&self.text);

            if replace_all {
                view.do_find_all(&self.text);
            } else {
                view.do_find_next(&self.text, false, true, true, &SelectionModifier::Set);
            }

            if last_selection_region(view.sel_regions()).is_some() {
                self.add_delta(edit_ops::insert(&self.text, view.sel_regions(), chars));
            } else {
                view.set_selection(&self.text, old_selection);
            }
        }
    }

    fn do_delete_by_movement(
        &mut self,
        view: &View,
        movement: Movement,
        save: bool,
        kill_ring: &mut Rope,
    ) {
        let (delta, rope) = edit_ops::delete_by_movement(
            &self.text,
            view.sel_regions(),
            view.get_lines(),
            movement,
            view.scroll_height(),
            save,
        );
        if let Some(rope) = rope {
            *kill_ring = rope;
        }
        if !delta.is_identity() {
            self.this_edit_type = EditType::Delete;
            self.add_delta(delta);
        }
    }

    fn do_delete_backward(&mut self, view: &View, config: &BufferItems) {
        let delta = edit_ops::delete_backward(&self.text, view.sel_regions(), config);
        if !delta.is_identity() {
            self.this_edit_type = EditType::Delete;
            self.add_delta(delta);
        }
    }

    fn do_transpose(&mut self, view: &View) {
        let delta = edit_ops::transpose(&self.text, view.sel_regions());
        if !delta.is_identity() {
            self.this_edit_type = EditType::Transpose;
            self.add_delta(delta);
        }
    }

    fn do_rotate_selection_contents(&mut self, view: &View, forward: bool) {
        let delta = edit_ops::rotate_selection_contents(&self.text, view.sel_regions(), forward);
        if !delta.is_identity() {
            self.this_edit_type = EditType::Transpose;
            self.add_delta(delta);
        }
    }

    fn do_align_selections(&mut self, view: &View, config: &BufferItems) {
        let delta = edit_ops::align_selections(&self.text, view.sel_regions(), config.tab_size);
        if !delta.is_identity() {
            self.this_edit_type = EditType::Other;
            self.add_delta(delta);
        }
    }

    fn do_align_it(
        &mut self,
        view: &View,
        config: &BufferItems,
        pattern: &str,
        regex: bool,
        occurrence: i64,
        all: bool,
        format: &str,
        range: Option<LineRange>,
    ) {
        let range = range.and_then(|range| {
            if range.first < 0 || range.last < 0 {
                return None;
            }
            Some((min(range.first, range.last) as usize, max(range.first, range.last) as usize))
        });
        let delta = edit_ops::align_it(
            &self.text,
            view.sel_regions(),
            config.tab_size,
            pattern,
            regex,
            occurrence,
            all,
            format,
            range,
        );
        if !delta.is_identity() {
            self.this_edit_type = EditType::Other;
            self.add_delta(delta);
        }
    }

    fn do_transform_text<F: Fn(&str) -> String>(&mut self, view: &View, transform_function: F) {
        let delta = edit_ops::transform_text(&self.text, view.sel_regions(), transform_function);
        if !delta.is_identity() {
            self.this_edit_type = EditType::Other;
            self.add_delta(delta);
        }
    }

    fn do_capitalize_text(&mut self, view: &mut View) {
        let (delta, final_selection) = edit_ops::capitalize_text(&self.text, view.sel_regions());
        if !delta.is_identity() {
            self.this_edit_type = EditType::Other;
            self.add_delta(delta);
        }

        // at the end of the transformation carets are located at the end of the words that were
        // transformed last in the selections
        view.collapse_selections(&self.text);
        view.set_selection(&self.text, final_selection);
    }

    fn do_modify_indent(&mut self, view: &View, config: &BufferItems, direction: IndentDirection) {
        let delta = edit_ops::modify_indent(&self.text, view.sel_regions(), config, direction);
        self.add_delta(delta);
        self.this_edit_type = match direction {
            IndentDirection::In => EditType::InsertChars,
            IndentDirection::Out => EditType::Delete,
        }
    }

    fn do_insert_newline(&mut self, view: &View, config: &BufferItems) {
        let delta = edit_ops::insert_newline(&self.text, view.sel_regions(), config);
        self.add_delta(delta);
        self.this_edit_type = EditType::InsertNewline;
    }

    fn do_insert_tab(&mut self, view: &View, config: &BufferItems) {
        let regions = view.sel_regions();
        let delta = edit_ops::insert_tab(&self.text, regions, config);

        // if we indent multiple regions or multiple lines,
        // we treat this as an indentation adjustment; otherwise it is
        // just inserting text.
        let condition = regions
            .first()
            .map(|x| LogicalLines.get_line_range(&self.text, x).len() > 1)
            .unwrap_or(false);

        self.add_delta(delta);
        self.this_edit_type =
            if regions.len() > 1 || condition { EditType::Indent } else { EditType::InsertChars };
    }

    fn do_yank(&mut self, view: &View, kill_ring: &Rope) {
        // TODO: if there are multiple cursors and the number of newlines
        // is one less than the number of cursors, split and distribute one
        // line per cursor.
        let delta = edit_ops::insert(&self.text, view.sel_regions(), kill_ring.clone());
        self.add_delta(delta);
    }

    fn do_duplicate_line(&mut self, view: &View, config: &BufferItems) {
        let delta = edit_ops::duplicate_line(&self.text, view.sel_regions(), config);
        self.add_delta(delta);
        self.this_edit_type = EditType::Other;
    }

    fn do_change_number<F: Fn(i128) -> Option<i128>>(
        &mut self,
        view: &View,
        transform_function: F,
    ) {
        let delta = edit_ops::change_number(&self.text, view.sel_regions(), transform_function);
        if !delta.is_identity() {
            self.this_edit_type = EditType::Other;
            self.add_delta(delta);
        }
    }

    pub(crate) fn do_edit(
        &mut self,
        view: &mut View,
        kill_ring: &mut Rope,
        config: &BufferItems,
        cmd: BufferEvent,
    ) {
        use self::BufferEvent::*;
        match cmd {
            Delete { movement, kill } => {
                self.do_delete_by_movement(view, movement, kill, kill_ring)
            }
            Backspace => self.do_delete_backward(view, config),
            Transpose => self.do_transpose(view),
            Undo => self.do_undo(),
            Redo => self.do_redo(),
            Uppercase => self.do_transform_text(view, |s| s.to_uppercase()),
            Lowercase => self.do_transform_text(view, |s| s.to_lowercase()),
            Capitalize => self.do_capitalize_text(view),
            Indent => self.do_modify_indent(view, config, IndentDirection::In),
            Outdent => self.do_modify_indent(view, config, IndentDirection::Out),
            InsertNewline => self.do_insert_newline(view, config),
            InsertTab => self.do_insert_tab(view, config),
            Insert(chars) => self.do_insert(view, config, &chars),
            Paste(chars) => self.do_paste(view, &chars),
            PasteRegister { chars, before } => {
                self.this_edit_type = EditType::Other;
                self.add_delta(edit_ops::paste_register(
                    &self.text,
                    view.sel_regions(),
                    &chars,
                    before,
                    &config.line_ending,
                ));
            }
            Yank => self.do_yank(view, kill_ring),
            ReplaceNext => self.do_replace(view, false),
            ReplaceAll => self.do_replace(view, true),
            DuplicateLine => self.do_duplicate_line(view, config),
            IncreaseNumber => self.do_change_number(view, |s| s.checked_add(1)),
            DecreaseNumber => self.do_change_number(view, |s| s.checked_sub(1)),
            AlignSelections => self.do_align_selections(view, config),
            AlignIt { pattern, regex, occurrence, all, format, range } => {
                self.do_align_it(view, config, &pattern, regex, occurrence, all, &format, range)
            }
            RotateSelectionContentsBackward => self.do_rotate_selection_contents(view, false),
            RotateSelectionContentsForward => self.do_rotate_selection_contents(view, true),
            ReverseSelectionContents => {
                let delta = edit_ops::reverse_selection_contents(&self.text, view.sel_regions());
                if !delta.is_identity() {
                    self.this_edit_type = EditType::Transpose;
                    self.add_delta(delta);
                }
            }
        }
    }

    /// Returns the number of lines in the buffer as seen by plugins
    /// (always at least 1, even for an empty buffer).
    pub fn plugin_n_lines(&self) -> usize {
        self.text.measure::<LinesMetric>() + 1
    }

    /// Applies annotation span updates from a plugin, transforming spans if
    /// `rev` is stale.
    ///
    /// # Preconditions
    ///
    /// `start` and `len` must describe a valid byte range within the buffer at
    /// the revision identified by `rev`.
    pub fn update_annotations(
        &mut self,
        view: &mut View,
        plugin: PluginId,
        start: usize,
        len: usize,
        annotation_spans: Vec<DataSpan>,
        annotation_type: AnnotationType,
        rev: RevToken,
    ) {
        let _t = tracing::trace_span!("Editor::update_annotations", categories = "core").entered();

        let mut start = start;
        let mut end_offset = start + len;
        let mut sb = SpansBuilder::new(len);
        for span in annotation_spans {
            sb.add_span(Interval::new(span.start, span.end), span.data);
        }
        let mut spans = sb.build();
        if rev != self.engine.get_head_rev_id().token() {
            if let Ok(delta) = self.engine.try_delta_rev_head(rev) {
                let mut transformer = Transformer::new(&delta);
                let new_start = transformer.transform(start, false);
                if !transformer.interval_untouched(Interval::new(start, end_offset)) {
                    spans = spans.transform(start, end_offset, &mut transformer);
                }
                start = new_start;
                end_offset = transformer.transform(end_offset, true);
            } else {
                error!("Revision {} not found", rev);
            }
        }
        let iv = Interval::new(start, end_offset);
        view.update_annotations(plugin, iv, Annotations { items: spans, annotation_type });
    }

    pub(crate) fn get_rev(&self, rev: RevToken) -> Option<Cow<'_, Rope>> {
        let text_cow = if rev == self.engine.get_head_rev_id().token() {
            Cow::Borrowed(&self.text)
        } else {
            match self.engine.get_rev(rev) {
                None => return None,
                Some(text) => Cow::Owned(text),
            }
        };

        Some(text_cow)
    }

    /// Returns a contiguous chunk of the buffer at the given `start` position
    /// for plugin consumption. Returns `None` if the revision or offset is
    /// invalid.
    ///
    /// # Preconditions
    ///
    /// `rev` must be a valid revision token issued by this editor's CRDT engine.
    pub fn plugin_get_data(
        &self,
        start: usize,
        unit: TextUnit,
        max_size: usize,
        rev: RevToken,
    ) -> Option<GetDataResponse> {
        let _t = tracing::trace_span!("Editor::plugin_get_data", categories = "core").entered();
        let text_cow = self.get_rev(rev)?;
        let text = &text_cow;
        // convert our offset into a valid byte offset
        let offset = unit.resolve_offset(text.borrow(), start)?;

        let max_size = min(max_size, MAX_SIZE_LIMIT);
        let mut end_off = offset.saturating_add(max_size);
        if end_off >= text.len() {
            end_off = text.len();
        } else {
            // Snap end to codepoint boundary.
            end_off = text.prev_codepoint_offset(end_off + 1).unwrap();
        }

        let chunk = text.slice_to_cow(offset..end_off).into_owned();
        let first_line = text.line_of_offset(offset);
        let first_line_offset = offset - text.offset_of_line(first_line);

        Some(GetDataResponse { chunk, offset, first_line, first_line_offset })
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditType {
    /// A catchall for edits that don't fit elsewhere, and which should
    /// always have their own undo groups; used for things like cut/copy/paste.
    Other,
    /// An insert from the keyboard/IME (not a paste or a yank).
    #[serde(rename = "insert")]
    InsertChars,
    #[serde(rename = "newline")]
    InsertNewline,
    /// An indentation adjustment.
    Indent,
    Delete,
    Undo,
    Redo,
    Transpose,
    Surround,
}

impl EditType {
    /// Checks whether a new undo group should be created between two edits.
    fn breaks_undo_group(self, previous: EditType) -> bool {
        self == EditType::Other || self == EditType::Transpose || self != previous
    }
}

fn last_selection_region(regions: &[SelRegion]) -> Option<&SelRegion> {
    regions.iter().rev().find(|&region| !region.is_caret()).map(|v| v as _)
}

/// Counts the number of lines in the string, not including any trailing newline.
fn count_lines(s: &str) -> usize {
    let mut newlines = count_newlines(s);
    if s.as_bytes().last() == Some(&0xa) {
        newlines -= 1;
    }
    1 + newlines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_store::TextStore;
    use crate::vlf::store::VlfStore;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn insert_text(editor: &mut Editor, text: &str) {
        let mut builder = DeltaBuilder::new(editor.get_buffer().len());
        builder.replace(editor.get_buffer().len()..editor.get_buffer().len(), text.into());
        editor.this_edit_type = EditType::InsertChars;
        editor.add_delta(builder.build());
        let _ = editor.commit_delta();
        editor.update_edit_type();
    }

    fn vlf_editor(content: &str) -> (Editor, NamedTempFile) {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();

        let store = VlfStore::open(file.path()).unwrap();
        store.enable_editing();
        (Editor::with_vlf_store(store), file)
    }

    #[test]
    fn commit_undo_checkpoint_starts_new_undo_group() {
        let mut editor = Editor::new();

        insert_text(&mut editor, "a");
        editor.commit_undo_checkpoint();
        insert_text(&mut editor, "b");

        editor.do_undo();
        assert_eq!(editor.get_buffer().to_string(), "a");

        editor.do_undo();
        assert_eq!(editor.get_buffer().to_string(), "");
    }

    #[test]
    fn text_store_snapshot_preserves_constrained_mode() {
        let editor = Editor::with_text_mode("alpha\nbeta", DocumentMode::ConstrainedNormal);
        let store = editor.text_store_snapshot();

        assert_eq!(store.mode(), DocumentMode::ConstrainedNormal);
    }

    #[test]
    fn set_pristine_if_equivalent_revision_preserves_dirty_newer_head() {
        let mut editor = Editor::with_text("alpha");
        let saved_rev_id = editor.get_head_rev_id();

        insert_text(&mut editor, "!");

        assert!(!editor.set_pristine_if_equivalent_revision(saved_rev_id));
        assert!(!editor.is_pristine());
    }

    #[test]
    fn vlf_overlay_context_reuses_editor_undo_grouping() {
        let (mut editor, _file) = vlf_editor("hello");

        let first = editor.next_vlf_overlay_edit_context(EditType::InsertChars).unwrap();
        editor.vlf_store.as_ref().unwrap().apply_insert(5, " world", first).unwrap();
        editor.commit_vlf_overlay_revision(first.revision_id);
        editor.update_edit_type();

        let second = editor.next_vlf_overlay_edit_context(EditType::InsertChars).unwrap();
        editor.vlf_store.as_ref().unwrap().apply_insert(11, "!", second).unwrap();
        editor.commit_vlf_overlay_revision(second.revision_id);
        editor.update_edit_type();

        assert_eq!(first.undo_group, second.undo_group);
        let delta = editor.vlf_overlay_delta_for_undo_group(first.undo_group).unwrap();
        assert_eq!(delta.undo_group, first.undo_group);
        assert_eq!(delta.revision_id, first.revision_id);
        assert_eq!(delta.ops.len(), 2);

        editor.commit_undo_checkpoint();
        let third = editor.next_vlf_overlay_edit_context(EditType::InsertChars).unwrap();
        assert_ne!(third.undo_group, first.undo_group);
    }

    #[test]
    fn vlf_overlay_history_gcs_with_editor_undo_gc() {
        let (mut editor, _file) = vlf_editor("hello");
        let mut first_group = None;

        for index in 0..=MAX_UNDOS {
            editor.commit_undo_checkpoint();
            let ctx = editor.next_vlf_overlay_edit_context(EditType::InsertChars).unwrap();
            if first_group.is_none() {
                first_group = Some(ctx.undo_group);
            }
            editor.vlf_store.as_ref().unwrap().apply_insert(5 + index as u64, "x", ctx).unwrap();
            editor.commit_vlf_overlay_revision(ctx.revision_id);
            editor.update_edit_type();
        }

        let first_group = first_group.unwrap();
        assert!(editor.vlf_overlay_delta_for_undo_group(first_group).is_some());

        editor.gc_undos();

        assert!(editor.vlf_overlay_delta_for_undo_group(first_group).is_none());
    }

    #[test]
    fn editable_vlf_pristine_tracks_overlay_revisions() {
        let (mut editor, _file) = vlf_editor("hello");
        assert!(editor.is_pristine(), "fresh VLF editor should start pristine");

        let ctx = editor.next_vlf_overlay_edit_context(EditType::InsertChars).unwrap();
        editor.vlf_store.as_ref().unwrap().apply_insert(5, " world", ctx).unwrap();
        editor.commit_vlf_overlay_revision(ctx.revision_id);
        assert!(!editor.is_pristine(), "overlay edit should mark editor dirty");

        editor.set_pristine();
        assert!(editor.is_pristine(), "successful VLF save should reset pristine state");
    }

    #[test]
    fn plugin_edit() {
        let base_text = "hello";
        let mut editor = Editor::with_text(base_text);
        let mut builder = DeltaBuilder::new(base_text.len());
        builder.replace(0..0, "s".into());
        let delta = builder.build();
        let rev = editor.get_head_rev_token();

        let edit_one = PluginEdit {
            rev,
            delta,
            priority: 55,
            after_cursor: false,
            undo_group: None,
            author: "plugin_one".into(),
        };

        editor.apply_plugin_edit(edit_one.clone());
        editor.apply_plugin_edit(edit_one);

        assert_eq!(editor.get_buffer().to_string(), "sshello");
    }
}
