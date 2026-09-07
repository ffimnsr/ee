// Copyright 2020 The xi-editor Authors.
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

//! Functions for editing ropes.
pub(crate) use std::borrow::Cow;
pub(crate) use std::collections::BTreeSet;

pub(crate) use regex::Regex;
pub(crate) use unicode_width::UnicodeWidthChar;
pub(crate) use xi_rope::{Cursor, DeltaBuilder, Interval, LinesMetric, Rope, RopeDelta};

pub(crate) use crate::backspace::offset_for_delete_backwards;
pub(crate) use crate::config::BufferItems;
pub(crate) use crate::indent::{IndentOutcome, SyntaxIndentContext, syntax_indent_outcome};
pub(crate) use crate::line_offset::{LineOffset, LogicalLines};
pub(crate) use crate::linewrap::Lines;
pub(crate) use crate::movement::{Movement, region_movement};
pub(crate) use crate::selection::{SelRegion, Selection};
pub(crate) use crate::word_boundaries::WordCursor;

#[derive(Debug, Copy, Clone)]
pub enum IndentDirection {
    In,
    Out,
}

pub use align::{
    align_it, align_selections, expand_tabs_in_lines, reflow_lines, reverse_selection_contents,
    rotate_selection_contents, sort_lines, transform_text, transpose,
};
pub use change::{capitalize_text, change_number};
pub(crate) use delete::{
    delete_block, delete_by_movement, delete_line_range, paste_register, replay_block_insert,
};
pub use insert::{delete_backward, duplicate_line, insert, insert_preserving_case, surround};
pub(crate) use newline::insert_newline_with_context;
pub use newline::{insert_newline, insert_tab, modify_indent};

mod align;
mod change;
mod delete;
mod insert;
mod newline;

#[cfg(test)]
mod tests;
