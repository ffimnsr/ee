//! Char-indexed rope operations: counts, byte<->char conversions, and
//! char-based editing.
//!
//! These mirror ropey's char-first API on top of the fork's byte-based delta
//! machinery. All indices and ranges here are in Unicode scalar values
//! (chars), so callers cannot accidentally create invalid utf8 the way
//! raw byte-interval edits can.

use std::ops::{Bound, RangeBounds};

use super::metrics::{CharsMetric, Utf16CodeUnitsMetric};
use super::*;

impl Rope {
    /// Total number of bytes in the rope.
    ///
    /// Time complexity: O(1).
    pub fn len_bytes(&self) -> usize {
        self.len()
    }

    /// Total number of chars (Unicode scalar values) in the rope.
    ///
    /// Time complexity: O(1).
    pub fn len_chars(&self) -> usize {
        self.measure::<CharsMetric>()
    }

    /// Total number of UTF-16 code units that would be in the rope if it were
    /// encoded as UTF-16.
    ///
    /// Primarily useful for interoperability with protocols that use UTF-16
    /// offsets (e.g. LSP).
    ///
    /// Time complexity: O(1).
    pub fn len_utf16_cu(&self) -> usize {
        self.measure::<Utf16CodeUnitsMetric>()
    }

    /// Returns the char at `char_idx`.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `char_idx >= len_chars()`.
    pub fn char_at(&self, char_idx: usize) -> char {
        self.get_char(char_idx)
            .unwrap_or_else(|| panic!("Rope::char_at callers must validate bounds"))
    }

    /// Non-panicking version of [`Rope::char_at`].
    pub fn get_char(&self, char_idx: usize) -> Option<char> {
        if char_idx >= self.len_chars() {
            return None;
        }
        let byte_idx = self.count_base_units::<CharsMetric>(char_idx);
        let cursor = Cursor::new(self, byte_idx);
        let (leaf, pos) = cursor.get_leaf()?;
        leaf[pos..].chars().next()
    }

    /// Returns the byte index of the given char index.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `char_idx > len_chars()`.
    pub fn char_to_byte(&self, char_idx: usize) -> usize {
        self.try_char_to_byte(char_idx).expect("Rope::char_to_byte callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::char_to_byte`].
    pub fn try_char_to_byte(&self, char_idx: usize) -> Result<usize, RopeError> {
        if char_idx > self.len_chars() {
            return Err(RopeError::CharOffsetOutOfBounds {
                offset: char_idx,
                len: self.len_chars(),
            });
        }
        Ok(self.count_base_units::<CharsMetric>(char_idx))
    }

    /// Returns the char index of the given byte index.
    ///
    /// If the byte index falls in the middle of a multi-byte char, returns
    /// the index of the char that contains it. A one-past-the-end byte index
    /// returns `len_chars()`.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `byte_idx > len()`.
    pub fn byte_to_char(&self, byte_idx: usize) -> usize {
        self.try_byte_to_char(byte_idx).expect("Rope::byte_to_char callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::byte_to_char`].
    pub fn try_byte_to_char(&self, byte_idx: usize) -> Result<usize, RopeError> {
        self.validate_offset(byte_idx)?;
        // Clamp mid-char offsets down to the start of the containing char so
        // the metric traversal only ever operates on valid boundaries.
        let boundary = self.at_or_prev_codepoint_boundary(byte_idx).unwrap_or(0);
        Ok(self.count::<CharsMetric>(boundary))
    }

    /// Inserts `text` at char index `char_idx`.
    ///
    /// Time complexity: O(log n), plus the size of `text`.
    ///
    /// # Panics
    ///
    /// Panics if `char_idx > len_chars()`.
    pub fn insert(&mut self, char_idx: usize, text: &str) {
        self.try_insert(char_idx, text).expect("Rope::insert callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::insert`].
    pub fn try_insert(&mut self, char_idx: usize, text: &str) -> Result<(), RopeError> {
        if char_idx > self.len_chars() {
            return Err(RopeError::CharOffsetOutOfBounds {
                offset: char_idx,
                len: self.len_chars(),
            });
        }
        let byte_idx = self.count_base_units::<CharsMetric>(char_idx);
        self.edit(byte_idx..byte_idx, text);
        Ok(())
    }

    /// Inserts a single char `ch` at char index `char_idx`.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `char_idx > len_chars()`.
    pub fn insert_char(&mut self, char_idx: usize, ch: char) {
        self.try_insert_char(char_idx, ch).expect("Rope::insert_char callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::insert_char`].
    pub fn try_insert_char(&mut self, char_idx: usize, ch: char) -> Result<(), RopeError> {
        let mut buf = [0u8; 4];
        self.try_insert(char_idx, ch.encode_utf8(&mut buf))
    }

    /// Removes the text in the given char-index range.
    ///
    /// Uses range syntax, e.g. `2..7`, `2..`, `1..=3`. The range is in char
    /// indices.
    ///
    /// Time complexity: O(log n), plus the size of the removed range.
    ///
    /// # Panics
    ///
    /// Panics if the start of the range is greater than the end, or if the
    /// end is out of bounds (i.e. `end > len_chars()`).
    pub fn remove<R: RangeBounds<usize>>(&mut self, char_range: R) {
        self.try_remove(char_range).expect("Rope::remove callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::remove`].
    pub fn try_remove<R: RangeBounds<usize>>(&mut self, char_range: R) -> Result<(), RopeError> {
        let (start, end) = char_range_to_indices(char_range, self.len_chars())?;
        let start_byte = self.count_base_units::<CharsMetric>(start);
        let end_byte = self.count_base_units::<CharsMetric>(end);
        self.edit(start_byte..end_byte, "");
        Ok(())
    }

    /// Replaces the text in the given char-index range with `text`.
    ///
    /// Uses range syntax, e.g. `2..7`, `2..`, `1..=3`. The range is in char
    /// indices.
    ///
    /// Time complexity: O(log n), plus the size of `text`.
    ///
    /// # Panics
    ///
    /// Panics if the start of the range is greater than the end, or if the
    /// end is out of bounds (i.e. `end > len_chars()`).
    pub fn replace<R: RangeBounds<usize>>(&mut self, char_range: R, text: &str) {
        self.try_replace(char_range, text).expect("Rope::replace callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::replace`].
    pub fn try_replace<R: RangeBounds<usize>>(
        &mut self,
        char_range: R,
        text: &str,
    ) -> Result<(), RopeError> {
        let (start, end) = char_range_to_indices(char_range, self.len_chars())?;
        let start_byte = self.count_base_units::<CharsMetric>(start);
        let end_byte = self.count_base_units::<CharsMetric>(end);
        self.edit(start_byte..end_byte, text);
        Ok(())
    }
    /// Splits the rope at `char_idx`, returning the right part.
    ///
    /// `self` becomes the left part `[0, char_idx)`; the returned rope is
    /// `[char_idx, len_chars())`. Subtrees not on the split path are shared
    /// between the two ropes (copy-on-write), so splitting is O(log n) with
    /// no bulk copying.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `char_idx > len_chars()`.
    pub fn split_off(&mut self, char_idx: usize) -> Rope {
        self.try_split_off(char_idx).expect("Rope::split_off callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::split_off`].
    pub fn try_split_off(&mut self, char_idx: usize) -> Result<Rope, RopeError> {
        let byte_idx = self.try_char_to_byte(char_idx)?;
        let right = self.subseq(byte_idx..);
        self.edit(byte_idx.., "");
        Ok(right)
    }

    /// Appends `other` to the end of this rope, consuming `other`.
    ///
    /// Structural concatenation: subtrees are shared where possible and the
    /// spine is rebalanced by tree height, so this is O(log n) regardless of
    /// the size of either rope.
    ///
    /// Time complexity: O(log n).
    pub fn append(&mut self, other: Rope) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            *self = other;
            return;
        }
        *self = Node::concat(std::mem::take(self), other);
    }
}

/// Converts a char-index range to `(start, end)` char indices, validating
/// against `len`.
fn char_range_to_indices<R: RangeBounds<usize>>(
    range: R,
    len: usize,
) -> Result<(usize, usize), RopeError> {
    let start = match range.start_bound() {
        Bound::Included(&n) => n,
        Bound::Excluded(&n) => n + 1,
        Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        Bound::Included(&n) => n + 1,
        Bound::Excluded(&n) => n,
        Bound::Unbounded => len,
    };
    if start > end {
        return Err(RopeError::CharReversedInterval { start, end });
    }
    if end > len {
        return Err(RopeError::CharIntervalOutOfBounds { start, end, len });
    }
    Ok((start, end))
}
