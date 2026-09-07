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

pub(crate) use std::cmp::{max, min};
pub(crate) use std::collections::HashMap;
pub(crate) use std::iter;
pub(crate) use std::ops::Range;
pub(crate) use std::path::Path;
pub(crate) use std::time::Duration;

pub(crate) use log::warn;
pub(crate) use regex::RegexBuilder;
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use serde_json::{Value, json};

pub(crate) use crate::annotations::{AnnotationStore, Annotations, ToAnnotation};
pub(crate) use crate::client::{Client, Update, UpdateOp};
pub(crate) use crate::edit_types::ViewEvent;
pub(crate) use crate::find::{Find, FindStatus};
pub(crate) use crate::line_cache_shadow::{self, LineCacheShadow, RenderPlan, RenderTactic};
pub(crate) use crate::line_offset::LineOffset;
pub(crate) use crate::linewrap::{InvalLines, Lines, VisualLine, WrapWidth};
pub(crate) use crate::movement::{Movement, region_movement, selection_movement};
pub(crate) use crate::object::{self, SyntaxNavigationAction, SyntaxSelectionAction};
pub(crate) use crate::plugins::PluginId;
pub(crate) use crate::rpc::{
    FindQuery, GestureType, MouseAction, SelectionGranularity, SelectionModifier,
};
pub(crate) use crate::selection::{Affinity, InsertDrift, SelRegion, Selection};
pub(crate) use crate::tabs::{BufferId, Counter, ViewId};
pub(crate) use crate::tree_sitter_support::{
    VisibleSyntaxLimits, VisibleSyntaxSpan, chunk_syntax_spans,
};
pub(crate) use crate::vlf::search::{VlfMatchRange, VlfSearchState, VlfSearchStatus};
pub(crate) use crate::width_cache::WidthCache;
pub(crate) use crate::word_boundaries::WordCursor;
pub(crate) use xi_rope::{Cursor, Interval, LinesMetric, Rope, RopeDelta, RopeError};
pub(crate) use xi_rpc::RequestId;

/// A flag used to indicate when legacy actions should modify selections
const FLAG_SELECT: u64 = 2;

/// Size of batches as number of bytes used during incremental find.
const FIND_BATCH_SIZE: usize = 500000;

/// Bounded context for normal/constrained backend syntax rendering.
const BACKEND_SYNTAX_CONTEXT_LINES: usize = 32;
/// Normal/constrained buffers can spend more than VLF's tight visible-range budget.
const BACKEND_SYNTAX_TIMEOUT: Duration = Duration::from_millis(100);

/// A view to a buffer. It is the buffer plus additional information
/// like line breaks and selection state.
pub struct View {
    view_id: ViewId,
    buffer_id: BufferId,

    /// Tracks whether this view has been scheduled to render.
    /// We attempt to reduce duplicate renders by setting a small timeout
    /// after an edit is applied, to allow batching with any plugin updates.
    pending_render: bool,
    size: Size,
    /// The selection state for this view. Invariant: non-empty.
    selection: Selection,
    /// Index of the primary region within the sorted selection set.
    primary_selection_idx: usize,

    /// Previous syntax-object selections used by `shrink_selection`.
    object_selection_history: Vec<Selection>,

    /// Cached visible VLF semantic parse for repeated syntax-object commands.
    semantic_parse_cache: object::SyntaxParseCache,

    drag_state: Option<DragState>,

    /// vertical scroll position
    first_line: usize,
    /// height of visible portion
    height: usize,
    lines: Lines,

    /// Front end's line cache state for this view. See the `LineCacheShadow`
    /// description for the invariant.
    lc_shadow: LineCacheShadow,

    /// New offset to be scrolled into position after an edit.
    scroll_to: Option<usize>,

    /// The state for finding text for this view.
    /// Each instance represents a separate search query.
    find: Vec<Find>,

    /// Tracks the IDs for additional search queries in find.
    find_id_counter: Counter,

    /// Tracks whether there has been changes in find results or find parameters.
    /// This is used to determined whether FindStatus should be sent to the frontend.
    find_changed: FindStatusChange,

    /// Tracks the progress of incremental find.
    find_progress: FindProgress,

    /// Tracks whether find highlights should be rendered.
    /// Highlights are only rendered when search dialog is open.
    highlight_find: bool,

    /// Streaming search state for Very Large File mode.
    vlf_find: Option<VlfSearchState>,

    /// The state for replacing matches for this view.
    replace: Option<Replace>,

    /// Tracks whether the replacement string or replace parameters changed.
    replace_changed: bool,

    /// Annotations provided by plugins.
    annotations: AnnotationStore,

    diagnostics: HashMap<PluginId, Vec<crate::plugins::rpc::Diagnostic>>,

    pending_hover_requests: HashMap<PluginId, RequestId>,
}

/// Indicates what changed in the find state.
#[derive(PartialEq, Debug)]
enum FindStatusChange {
    /// None of the find parameters or number of matches changed.
    None,

    /// Find parameters and number of matches changed.
    All,

    /// Only number of matches changed
    Matches,
}

/// Indicates what changed in the find state.
#[derive(PartialEq, Debug, Clone)]
enum FindProgress {
    /// Incremental find is done/not running.
    Ready,

    /// The find process just started.
    Started,

    /// Incremental find is in progress. Keeps tracked of already searched range.
    InProgress(Range<usize>),
}

/// Contains replacement string and replace options.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize, Clone)]
pub struct Replace {
    /// Replacement string.
    pub chars: String,
    pub preserve_case: bool,
}

/// A size, in pixel units (not display pixels).
#[derive(Debug, Default, PartialEq, Serialize, Deserialize, Clone)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

/// State required to resolve a drag gesture into a selection.
struct DragState {
    /// All the selection regions other than the one being dragged.
    base_sel: Selection,

    /// Start of the region selected when drag was started (region is
    /// assumed to be forward).
    min: usize,

    /// End of the region selected when drag was started.
    max: usize,

    granularity: SelectionGranularity,
}

impl View {
    /// Exposed for benchmarking
    #[doc(hidden)]
    pub fn debug_force_rewrap_cols(&mut self, text: &Rope, cols: usize) {
        use xi_rpc::test_utils::DummyPeer;

        let mut width_cache = WidthCache::new();
        let client = Client::new(Box::new(DummyPeer));
        self.update_wrap_settings(text, cols, false);
        self.rewrap(text, &mut width_cache, &client);
    }
}

impl LineOffset for View {
    fn try_offset_of_line(&self, text: &Rope, line: usize) -> Result<usize, RopeError> {
        self.lines.try_offset_of_visual_line(text, line)
    }

    fn offset_of_line(&self, text: &Rope, line: usize) -> usize {
        self.lines.offset_of_visual_line(text, line)
    }

    fn try_line_of_offset(&self, text: &Rope, offset: usize) -> Result<usize, RopeError> {
        self.lines.try_visual_line_of_offset(text, offset)
    }

    fn line_of_offset(&self, text: &Rope, offset: usize) -> usize {
        self.lines.visual_line_of_offset(text, offset)
    }
}

// utility function to clamp a value within the given range
fn clamp(x: usize, min: usize, max: usize) -> usize {
    if x < min {
        min
    } else if x < max {
        x
    } else {
        max
    }
}

mod core;
mod find;
mod render;
mod select;
mod selection;

#[cfg(test)]
mod tests;
