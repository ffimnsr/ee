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

#![allow(clippy::range_plus_one)]

use std::ops::Range;

use xi_rope::{Rope, RopeError};

use crate::linewrap::Lines;
use crate::selection::SelRegion;

/// A trait from which lines and columns in a document can be calculated
/// into offsets inside a rope an vice versa.
pub trait LineOffset {
    // use own breaks if present, or text if not (no line wrapping)

    fn try_offset_of_line(&self, text: &Rope, line: usize) -> Result<usize, RopeError> {
        text.try_offset_of_line(line)
    }

    /// Returns the byte offset corresponding to the given line.
    fn offset_of_line(&self, text: &Rope, line: usize) -> usize {
        self.try_offset_of_line(text, line)
            .expect("LineOffset::offset_of_line callers must validate bounds")
    }

    fn try_line_of_offset(&self, text: &Rope, offset: usize) -> Result<usize, RopeError> {
        text.try_line_of_offset(offset)
    }

    /// Returns the visible line number containing the given offset.
    fn line_of_offset(&self, text: &Rope, offset: usize) -> usize {
        self.try_line_of_offset(text, offset)
            .expect("LineOffset::line_of_offset callers must validate bounds")
    }

    // How should we count "column"? Valid choices include:
    // * Unicode codepoints
    // * grapheme clusters
    // * Unicode width (so CJK counts as 2)
    // * Actual measurement in text layout
    // * Code units in some encoding
    //
    // Of course, all these are identical for ASCII. For now we use UTF-8 code units
    // for simplicity.

    fn try_offset_to_line_col(
        &self,
        text: &Rope,
        offset: usize,
    ) -> Result<(usize, usize), RopeError> {
        let line = self.try_line_of_offset(text, offset)?;
        Ok((line, offset - self.try_offset_of_line(text, line)?))
    }

    fn offset_to_line_col(&self, text: &Rope, offset: usize) -> (usize, usize) {
        self.try_offset_to_line_col(text, offset)
            .expect("LineOffset::offset_to_line_col callers must validate bounds")
    }

    fn try_line_col_to_offset(
        &self,
        text: &Rope,
        line: usize,
        col: usize,
    ) -> Result<usize, RopeError> {
        let mut offset = self.try_offset_of_line(text, line)?.saturating_add(col);
        if offset >= text.len() {
            offset = text.len();
            if self.try_line_of_offset(text, offset)? <= line {
                return Ok(offset);
            }
        } else {
            // Snap to grapheme cluster boundary
            offset = text.prev_grapheme_offset(offset + 1).unwrap();
        }

        // clamp to end of line
        let next_line_offset = self.try_offset_of_line(text, line + 1)?;
        if offset >= next_line_offset {
            if let Some(prev) = text.prev_grapheme_offset(next_line_offset) {
                offset = prev;
            }
        }
        Ok(offset)
    }

    fn line_col_to_offset(&self, text: &Rope, line: usize, col: usize) -> usize {
        self.try_line_col_to_offset(text, line, col)
            .expect("LineOffset::line_col_to_offset callers must validate bounds")
    }

    /// Get the line range of a selected region.
    fn get_line_range(&self, text: &Rope, region: &SelRegion) -> Range<usize> {
        let (first_line, _) = self.offset_to_line_col(text, region.min());
        let (mut last_line, last_col) = self.offset_to_line_col(text, region.max());
        if last_col == 0 && last_line > first_line {
            last_line -= 1;
        }

        first_line..(last_line + 1)
    }
}

/// A struct from which the default definitions for `offset_of_line`
/// and `line_of_offset` can be accessed, and think in logical lines.
pub struct LogicalLines;

impl LineOffset for LogicalLines {}

impl LineOffset for xi_rope::breaks::Breaks {
    fn try_offset_of_line(&self, _text: &Rope, line: usize) -> Result<usize, RopeError> {
        let max_line = self.measure::<xi_rope::breaks::BreaksMetric>() + 1;
        if line > max_line {
            return Err(RopeError::LineOutOfBounds { line, max_line });
        }
        Ok(self.count_base_units::<xi_rope::breaks::BreaksMetric>(line))
    }

    fn offset_of_line(&self, _text: &Rope, line: usize) -> usize {
        self.count_base_units::<xi_rope::breaks::BreaksMetric>(line)
    }

    fn try_line_of_offset(&self, text: &Rope, offset: usize) -> Result<usize, RopeError> {
        text.try_line_of_offset(offset)?;
        Ok(self.count::<xi_rope::breaks::BreaksMetric>(offset))
    }

    fn line_of_offset(&self, text: &Rope, offset: usize) -> usize {
        let offset = offset.min(text.len());
        self.count::<xi_rope::breaks::BreaksMetric>(offset)
    }
}

impl LineOffset for Lines {
    fn try_offset_of_line(&self, text: &Rope, line: usize) -> Result<usize, RopeError> {
        self.try_offset_of_visual_line(text, line)
    }

    fn offset_of_line(&self, text: &Rope, line: usize) -> usize {
        self.offset_of_visual_line(text, line)
    }

    fn try_line_of_offset(&self, text: &Rope, offset: usize) -> Result<usize, RopeError> {
        self.try_visual_line_of_offset(text, offset)
    }

    fn line_of_offset(&self, text: &Rope, offset: usize) -> usize {
        self.visual_line_of_offset(text, offset)
    }
}
