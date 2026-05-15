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

//! A rope data structure with a line count metric and (soon) other useful
//! info.

#![allow(clippy::needless_return)]

use std::borrow::Cow;
use std::cmp::{Ordering, max, min};
use std::fmt;
use std::io;
use std::ops::Add;
use std::str::{self, FromStr};
use std::string::ParseError;

use crate::delta::{Delta, DeltaElement};
use crate::interval::{Interval, IntervalBounds};
use crate::tree::{Cursor, DefaultMetric, Leaf, Metric, Node, NodeInfo, TreeBuilder};

use memchr::{memchr, memrchr};
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

const MIN_LEAF: usize = 511;
const MAX_LEAF: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RopeError {
    OffsetOutOfBounds { offset: usize, len: usize },
    LineOutOfBounds { line: usize, max_line: usize },
    ReversedInterval { start: usize, end: usize },
    IntervalOutOfBounds { start: usize, end: usize, len: usize },
}

impl RopeError {
    fn offset_out_of_bounds(offset: usize, len: usize) -> Self {
        Self::OffsetOutOfBounds { offset, len }
    }

    fn line_out_of_bounds(line: usize, max_line: usize) -> Self {
        Self::LineOutOfBounds { line, max_line }
    }

    fn reversed_interval(iv: Interval) -> Self {
        Self::ReversedInterval { start: iv.start(), end: iv.end() }
    }

    fn interval_out_of_bounds(iv: Interval, len: usize) -> Self {
        Self::IntervalOutOfBounds { start: iv.start(), end: iv.end(), len }
    }
}

impl fmt::Display for RopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OffsetOutOfBounds { offset, len } => {
                write!(f, "offset {offset} beyond rope length {len}")
            }
            Self::LineOutOfBounds { line, max_line } => {
                write!(f, "line {line} beyond last line {max_line}")
            }
            Self::ReversedInterval { start, end } => {
                write!(f, "invalid interval [{start}, {end}): start exceeds end")
            }
            Self::IntervalOutOfBounds { start, end, len } => {
                write!(f, "interval [{start}, {end}) beyond rope length {len}")
            }
        }
    }
}

impl std::error::Error for RopeError {}

/// A rope data structure.
///
/// A [rope](https://en.wikipedia.org/wiki/Rope_(data_structure)) is a data structure
/// for strings, specialized for incremental editing operations. Most operations
/// (such as insert, delete, substring) are O(log n). This module provides an immutable
/// (also known as [persistent](https://en.wikipedia.org/wiki/Persistent_data_structure))
/// version of Ropes, and if there are many copies of similar strings, the common parts
/// are shared.
///
/// Internally, the implementation uses thread safe reference counting.
/// Mutations are generally copy-on-write, though in-place edits are
/// supported as an optimization when only one reference exists, making the
/// implementation as efficient as a mutable version.
///
/// Also note: in addition to the `From` traits described below, this module
/// implements `From<Rope> for String` and `From<&Rope> for String`, for easy
/// conversions in both directions.
///
/// # Examples
///
/// Create a `Rope` from a `String`:
///
/// ```rust
/// # use xi_rope::Rope;
/// let a = Rope::from("hello ");
/// let b = Rope::from("world");
/// assert_eq!("hello world", String::from(a.clone() + b.clone()));
/// assert!("hello world" == String::from(a + b));
/// ```
///
/// Get a slice of a `Rope`:
///
/// ```rust
/// # use xi_rope::Rope;
/// let a = Rope::from("hello world");
/// let b = a.slice(1..9);
/// assert_eq!("ello wor", String::from(&b));
/// let c = b.slice(1..7);
/// assert_eq!("llo wo", String::from(c));
/// ```
///
/// Replace part of a `Rope`:
///
/// ```rust
/// # use xi_rope::Rope;
/// let mut a = Rope::from("hello world");
/// a.edit(1..9, "era");
/// assert_eq!("herald", String::from(a));
/// ```
pub type Rope = Node<RopeInfo>;

/// Represents a transform from one rope to another.
pub type RopeDelta = Delta<RopeInfo>;

/// An element in a `RopeDelta`.
pub type RopeDeltaElement = DeltaElement<RopeInfo>;

/// Streaming builder for linear rope construction.
pub struct RopeBuilder {
    tree: TreeBuilder<RopeInfo>,
    pending: String,
}

impl Default for RopeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RopeBuilder {
    pub fn new() -> Self {
        Self { tree: TreeBuilder::new(), pending: String::new() }
    }

    pub fn push_str(&mut self, mut text: &str) {
        while !text.is_empty() {
            let take = clamp_to_char_boundary(
                text,
                text.len().min(MAX_LEAF + MIN_LEAF - self.pending.len()),
            );
            debug_assert!(take > 0);
            self.pending.push_str(&text[..take]);
            text = &text[take..];

            if self.pending.len() > MAX_LEAF {
                self.flush_full_leaf();
            }
        }
    }

    pub fn append(&mut self, rope: &Rope) {
        for chunk in rope.iter_chunks(..) {
            self.push_str(chunk);
        }
    }

    pub fn finish(mut self) -> Rope {
        if !self.pending.is_empty() {
            self.tree.push_leaf(std::mem::take(&mut self.pending));
        }
        self.tree.build()
    }

    fn flush_full_leaf(&mut self) {
        let splitpoint = find_leaf_split_for_merge(&self.pending);
        let remainder = self.pending.split_off(splitpoint);
        let leaf = std::mem::replace(&mut self.pending, remainder);
        self.tree.push_leaf(leaf);
    }
}

/// Borrowed read-only view into a subrange of a [`Rope`].
#[derive(Clone, Copy, Debug)]
pub struct RopeSlice<'a> {
    rope: &'a Rope,
    interval: Interval,
}

impl<'a> RopeSlice<'a> {
    /// Create a borrowed view for `interval` within `rope`.
    pub fn new<T: IntervalBounds>(rope: &'a Rope, interval: T) -> Self {
        RopeSlice { rope, interval: interval.into_interval(rope.len()) }
    }

    /// Borrow underlying rope.
    pub fn rope(&self) -> &'a Rope {
        self.rope
    }

    /// Absolute interval covered by this view.
    pub fn interval(&self) -> Interval {
        self.interval
    }

    /// Start offset of this view in underlying rope.
    pub fn start(&self) -> usize {
        self.interval.start()
    }

    /// End offset of this view in underlying rope.
    pub fn end(&self) -> usize {
        self.interval.end()
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.interval.size()
    }

    pub fn is_empty(&self) -> bool {
        self.interval.is_empty()
    }

    /// Returns nested borrowed view relative to this view.
    pub fn slice<T: IntervalBounds>(&self, interval: T) -> RopeSlice<'a> {
        let rel = interval.into_interval(self.len());
        RopeSlice { rope: self.rope, interval: rel.translate(self.interval.start()) }
    }

    /// Materialize this borrowed view as owned rope.
    pub fn to_rope(&self) -> Rope {
        self.rope.subseq(self.interval)
    }

    /// Iterate borrowed chunks in this view.
    pub fn iter_chunks(&self) -> ChunkIter<'a> {
        self.rope.iter_chunks(self.interval)
    }

    /// Iterate raw lines in this view.
    pub fn lines_raw(&self) -> LinesRaw<'a> {
        self.rope.lines_raw(self.interval)
    }

    /// Iterate logical lines in this view.
    pub fn lines(&self) -> Lines<'a> {
        self.rope.lines(self.interval)
    }

    /// Return borrowed-or-owned text for this view.
    pub fn slice_to_cow(&self) -> Cow<'a, str> {
        self.rope.slice_to_cow(self.interval)
    }

    /// Create cursor bounded to this view.
    pub fn cursor(&self, position: usize) -> RopeSliceCursor<'a> {
        RopeSliceCursor::new(*self, position)
    }
}

/// Cursor bounded to a [`RopeSlice`].
pub struct RopeSliceCursor<'a> {
    cursor: Cursor<'a, RopeInfo>,
    interval: Interval,
}

impl<'a> RopeSliceCursor<'a> {
    pub fn new(slice: RopeSlice<'a>, position: usize) -> Self {
        RopeSliceCursor {
            cursor: Cursor::new(slice.rope, slice.interval.start() + position),
            interval: slice.interval,
        }
    }

    pub fn total_len(&self) -> usize {
        self.interval.size()
    }

    pub fn pos(&self) -> usize {
        self.cursor.pos().saturating_sub(self.interval.start())
    }

    pub fn set(&mut self, position: usize) {
        self.cursor.set(self.interval.start() + position);
    }

    pub fn get_leaf(&self) -> Option<(&'a str, usize)> {
        let (leaf, pos_in_leaf, _, _) = self.current_leaf_window()?;
        Some((leaf, pos_in_leaf))
    }

    pub fn next_leaf(&mut self) -> Option<(&'a str, usize)> {
        let (_, _, _, visible_end) = self.current_leaf_window()?;
        if visible_end >= self.interval.end() {
            self.cursor.set(self.interval.end());
            return None;
        }
        self.cursor.set(visible_end);
        self.get_leaf()
    }

    pub fn next_base(&mut self) -> Option<usize> {
        let next = self.cursor.next::<BaseMetric>()?;
        if next > self.interval.end() {
            self.cursor.set(self.interval.end());
            return None;
        }
        Some(self.pos())
    }

    pub fn next_codepoint(&mut self) -> Option<char> {
        let ch = self.cursor.next_codepoint()?;
        if self.cursor.pos() > self.interval.end() {
            self.cursor.set(self.interval.end());
            return None;
        }
        Some(ch)
    }

    fn current_leaf_window(&self) -> Option<(&'a str, usize, usize, usize)> {
        if self.cursor.pos() >= self.interval.end() {
            return None;
        }
        let (leaf, _) = self.cursor.get_leaf()?;
        let leaf_start = self.cursor.leaf_start_offset()?;
        let visible_start = leaf_start.max(self.interval.start());
        let visible_end = (leaf_start + leaf.len()).min(self.interval.end());
        if visible_start >= visible_end {
            return None;
        }
        let start_in_leaf = visible_start - leaf_start;
        let end_in_leaf = visible_end - leaf_start;
        let pos_in_leaf = self.cursor.pos() - visible_start;
        Some((&leaf[start_in_leaf..end_in_leaf], pos_in_leaf, visible_start, visible_end))
    }
}

impl Leaf for String {
    fn len(&self) -> usize {
        self.len()
    }

    fn is_ok_child(&self) -> bool {
        self.len() >= MIN_LEAF
    }

    fn push_maybe_split(&mut self, other: &String, iv: Interval) -> Option<String> {
        //println!("push_maybe_split [{}] [{}] {:?}", self, other, iv);
        let (start, end) = iv.start_end();
        self.push_str(&other[start..end]);
        if self.len() <= MAX_LEAF {
            None
        } else {
            let splitpoint = find_leaf_split_for_merge(self);
            let right_str = self[splitpoint..].to_owned();
            self.truncate(splitpoint);
            self.shrink_to_fit();
            Some(right_str)
        }
    }
}

#[derive(Clone, Copy)]
pub struct RopeInfo {
    lines: usize,
    utf16_size: usize,
}

impl NodeInfo for RopeInfo {
    type L = String;

    fn accumulate(&mut self, other: &Self) {
        self.lines += other.lines;
        self.utf16_size += other.utf16_size;
    }

    fn compute_info(s: &String) -> Self {
        RopeInfo { lines: count_newlines(s), utf16_size: count_utf16_code_units(s) }
    }

    fn identity() -> Self {
        RopeInfo { lines: 0, utf16_size: 0 }
    }
}

impl DefaultMetric for RopeInfo {
    type DefaultMetric = BaseMetric;
}

//TODO: document metrics, based on https://github.com/google/xi-editor/issues/456
//See ../docs/MetricsAndBoundaries.md for more information.
/// This metric let us walk utf8 text by code point.
///
/// `BaseMetric` implements the trait [Metric].  Both its _measured unit_ and
/// its _base unit_ are utf8 code unit.
///
/// Offsets that do not correspond to codepoint boundaries are _invalid_, and
/// calling functions that assume valid offsets with invalid offets will panic
/// in debug mode.
///
/// Boundary is atomic and determined by codepoint boundary.  Atomicity is
/// implicit, because offsets between two utf8 code units that form a code
/// point is considered invalid. For example, if a string starts with a
/// 0xC2 byte, then `offset=1` is invalid.
#[derive(Clone, Copy)]
pub struct BaseMetric(());

impl Metric<RopeInfo> for BaseMetric {
    fn measure(_: &RopeInfo, len: usize) -> usize {
        len
    }

    fn to_base_units(s: &String, in_measured_units: usize) -> usize {
        debug_assert!(s.is_char_boundary(in_measured_units));
        in_measured_units
    }

    fn from_base_units(s: &String, in_base_units: usize) -> usize {
        debug_assert!(s.is_char_boundary(in_base_units));
        in_base_units
    }

    fn is_boundary(s: &String, offset: usize) -> bool {
        s.is_char_boundary(offset)
    }

    fn prev(s: &String, offset: usize) -> Option<usize> {
        if offset == 0 {
            // I think it's a precondition that this will never be called
            // with offset == 0, but be defensive.
            None
        } else {
            let mut len = 1;
            while !s.is_char_boundary(offset - len) {
                len += 1;
            }
            Some(offset - len)
        }
    }

    fn next(s: &String, offset: usize) -> Option<usize> {
        if offset == s.len() {
            // I think it's a precondition that this will never be called
            // with offset == s.len(), but be defensive.
            None
        } else {
            let b = s.as_bytes()[offset];
            Some(offset + len_utf8_from_first_byte(b))
        }
    }

    fn can_fragment() -> bool {
        false
    }
}

/// Given the inital byte of a UTF-8 codepoint, returns the number of
/// bytes required to represent the codepoint.
/// RFC reference: <https://tools.ietf.org/html/rfc3629#section-4>
pub fn len_utf8_from_first_byte(b: u8) -> usize {
    match b {
        b if b < 0x80 => 1,
        b if b < 0xe0 => 2,
        b if b < 0xf0 => 3,
        _ => 4,
    }
}

#[derive(Clone, Copy)]
pub struct LinesMetric; // number of lines

/// Measured unit is newline amount.
/// Base unit is utf8 code unit.
/// Boundary is trailing and determined by a newline char.
impl Metric<RopeInfo> for LinesMetric {
    fn measure(info: &RopeInfo, _: usize) -> usize {
        info.lines
    }

    fn is_boundary(s: &String, offset: usize) -> bool {
        if offset == 0 {
            // shouldn't be called with this, but be defensive
            false
        } else {
            s.as_bytes()[offset - 1] == b'\n'
        }
    }

    fn to_base_units(s: &String, in_measured_units: usize) -> usize {
        let mut offset = 0;
        for _ in 0..in_measured_units {
            match memchr(b'\n', &s.as_bytes()[offset..]) {
                Some(pos) => offset += pos + 1,
                _ => panic!("to_base_units called with arg too large"),
            }
        }
        offset
    }

    fn from_base_units(s: &String, in_base_units: usize) -> usize {
        count_newlines(&s[..in_base_units])
    }

    fn prev(s: &String, offset: usize) -> Option<usize> {
        debug_assert!(offset > 0, "caller is responsible for validating input");
        memrchr(b'\n', &s.as_bytes()[..offset - 1]).map(|pos| pos + 1)
    }

    fn next(s: &String, offset: usize) -> Option<usize> {
        memchr(b'\n', &s.as_bytes()[offset..]).map(|pos| offset + pos + 1)
    }

    fn can_fragment() -> bool {
        true
    }
}

#[derive(Clone, Copy)]
pub struct Utf16CodeUnitsMetric;

impl Metric<RopeInfo> for Utf16CodeUnitsMetric {
    fn measure(info: &RopeInfo, _: usize) -> usize {
        info.utf16_size
    }

    fn is_boundary(s: &String, offset: usize) -> bool {
        s.is_char_boundary(offset)
    }

    fn to_base_units(s: &String, in_measured_units: usize) -> usize {
        let mut cur_len_utf16 = 0;
        let mut cur_len_utf8 = 0;
        for u in s.chars() {
            if cur_len_utf16 >= in_measured_units {
                break;
            }
            cur_len_utf16 += u.len_utf16();
            cur_len_utf8 += u.len_utf8();
        }
        cur_len_utf8
    }

    fn from_base_units(s: &String, in_base_units: usize) -> usize {
        count_utf16_code_units(&s[..in_base_units])
    }

    fn prev(s: &String, offset: usize) -> Option<usize> {
        if offset == 0 {
            // I think it's a precondition that this will never be called
            // with offset == 0, but be defensive.
            None
        } else {
            let mut len = 1;
            while !s.is_char_boundary(offset - len) {
                len += 1;
            }
            Some(offset - len)
        }
    }

    fn next(s: &String, offset: usize) -> Option<usize> {
        if offset == s.len() {
            // I think it's a precondition that this will never be called
            // with offset == s.len(), but be defensive.
            None
        } else {
            let b = s.as_bytes()[offset];
            Some(offset + len_utf8_from_first_byte(b))
        }
    }

    fn can_fragment() -> bool {
        false
    }
}

// Low level functions

pub fn count_newlines(s: &str) -> usize {
    bytecount::count(s.as_bytes(), b'\n')
}

fn count_utf16_code_units(s: &str) -> usize {
    let mut utf16_count = 0;
    for &b in s.as_bytes() {
        if (b as i8) >= -0x40 {
            utf16_count += 1;
        }
        if b >= 0xf0 {
            utf16_count += 1;
        }
    }
    utf16_count
}

fn clamp_to_char_boundary(s: &str, splitpoint: usize) -> usize {
    let mut splitpoint = splitpoint.min(s.len());
    while splitpoint > 0 && !s.is_char_boundary(splitpoint) {
        splitpoint -= 1;
    }
    splitpoint
}

fn is_crlf_split_point(s: &str, splitpoint: usize) -> bool {
    splitpoint > 0
        && splitpoint < s.len()
        && s.as_bytes()[splitpoint - 1] == b'\r'
        && s.as_bytes()[splitpoint] == b'\n'
}

fn adjust_splitpoint_for_crlf(
    s: &str,
    minsplit: usize,
    maxsplit: usize,
    splitpoint: usize,
) -> usize {
    if !is_crlf_split_point(s, splitpoint) {
        return splitpoint;
    }
    if splitpoint < maxsplit {
        splitpoint + 1
    } else if splitpoint > minsplit {
        splitpoint - 1
    } else {
        splitpoint
    }
}

fn find_leaf_split_for_bulk(s: &str) -> usize {
    find_leaf_split(s, MIN_LEAF)
}

fn find_leaf_split_for_merge(s: &str) -> usize {
    find_leaf_split(s, max(MIN_LEAF, s.len() - MAX_LEAF))
}

// Try to split at newline boundary (leaning left), if not, then split at codepoint
fn find_leaf_split(s: &str, minsplit: usize) -> usize {
    let maxsplit = min(MAX_LEAF, s.len() - MIN_LEAF);
    let splitpoint = match memrchr(b'\n', &s.as_bytes()[minsplit - 1..maxsplit]) {
        Some(pos) => minsplit + pos,
        None => clamp_to_char_boundary(s, maxsplit),
    };
    adjust_splitpoint_for_crlf(s, minsplit, maxsplit, splitpoint)
}

// Additional APIs custom to strings

impl FromStr for Rope {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Rope, Self::Err> {
        let mut b = RopeBuilder::new();
        b.push_str(s);
        Ok(b.finish())
    }
}

impl Rope {
    fn validate_offset(&self, offset: usize) -> Result<(), RopeError> {
        if offset > self.len() {
            Err(RopeError::offset_out_of_bounds(offset, self.len()))
        } else {
            Ok(())
        }
    }

    fn max_line_index(&self) -> usize {
        self.measure::<LinesMetric>() + 1
    }

    fn validate_line(&self, line: usize) -> Result<(), RopeError> {
        let max_line = self.max_line_index();
        if line > max_line { Err(RopeError::line_out_of_bounds(line, max_line)) } else { Ok(()) }
    }

    fn validate_interval<T: IntervalBounds>(&self, iv: T) -> Result<Interval, RopeError> {
        let iv = iv.into_interval(self.len());
        if iv.start() > iv.end() {
            return Err(RopeError::reversed_interval(iv));
        }
        if iv.end() > self.len() {
            return Err(RopeError::interval_out_of_bounds(iv, self.len()));
        }
        Ok(iv)
    }

    /// Edit the string, replacing the byte range `start..end` with `new`.
    ///
    /// Time complexity: O(log n)
    #[deprecated(since = "0.3.0", note = "Use Rope::edit instead")]
    pub fn edit_str<T: IntervalBounds>(&mut self, iv: T, new: &str) {
        self.edit(iv, new)
    }

    pub fn try_edit<T, IV>(&mut self, iv: IV, new: T) -> Result<(), RopeError>
    where
        T: Into<Rope>,
        IV: IntervalBounds,
    {
        let iv = self.validate_interval(iv)?;
        self.edit(iv, new);
        Ok(())
    }

    /// Returns a new Rope with the contents of the provided range.
    pub fn slice<T: IntervalBounds>(&self, iv: T) -> Rope {
        self.try_slice(iv).expect("Rope::slice callers must validate bounds")
    }

    pub fn try_slice<T: IntervalBounds>(&self, iv: T) -> Result<Rope, RopeError> {
        Ok(self.subseq(self.validate_interval(iv)?))
    }

    /// Returns borrowed read-only view over provided range.
    pub fn slice_view<T: IntervalBounds>(&self, iv: T) -> RopeSlice<'_> {
        RopeSlice::new(self, iv)
    }

    // encourage callers to use Cursor instead?

    /// Determine whether `offset` lies on a codepoint boundary.
    pub fn is_codepoint_boundary(&self, offset: usize) -> bool {
        let mut cursor = Cursor::new(self, offset);
        cursor.is_boundary::<BaseMetric>()
    }

    /// Return the offset of the codepoint before `offset`.
    pub fn prev_codepoint_offset(&self, offset: usize) -> Option<usize> {
        let mut cursor = Cursor::new(self, offset);
        cursor.prev::<BaseMetric>()
    }

    /// Return the offset of the codepoint after `offset`.
    pub fn next_codepoint_offset(&self, offset: usize) -> Option<usize> {
        let mut cursor = Cursor::new(self, offset);
        cursor.next::<BaseMetric>()
    }

    /// Returns `offset` if it lies on a codepoint boundary. Otherwise returns
    /// the codepoint after `offset`.
    pub fn at_or_next_codepoint_boundary(&self, offset: usize) -> Option<usize> {
        if self.is_codepoint_boundary(offset) {
            Some(offset)
        } else {
            self.next_codepoint_offset(offset)
        }
    }

    /// Returns `offset` if it lies on a codepoint boundary. Otherwise returns
    /// the codepoint before `offset`.
    pub fn at_or_prev_codepoint_boundary(&self, offset: usize) -> Option<usize> {
        if self.is_codepoint_boundary(offset) {
            Some(offset)
        } else {
            self.prev_codepoint_offset(offset)
        }
    }

    pub fn prev_grapheme_offset(&self, offset: usize) -> Option<usize> {
        let mut cursor = Cursor::new(self, offset);
        cursor.prev_grapheme()
    }

    pub fn next_grapheme_offset(&self, offset: usize) -> Option<usize> {
        let mut cursor = Cursor::new(self, offset);
        cursor.next_grapheme()
    }

    /// Return the line number corresponding to the byte index `offset`.
    ///
    /// The line number is 0-based, thus this is equivalent to the count of newlines
    /// in the slice up to `offset`.
    ///
    /// Time complexity: O(log n)
    ///
    /// # Panics
    ///
    /// This function will panic if `offset > self.len()`. Callers are expected to
    /// validate their input.
    pub fn line_of_offset(&self, offset: usize) -> usize {
        self.try_line_of_offset(offset).expect("Rope::line_of_offset callers must validate bounds")
    }

    pub fn try_line_of_offset(&self, offset: usize) -> Result<usize, RopeError> {
        self.validate_offset(offset)?;
        Ok(self.count::<LinesMetric>(offset))
    }

    /// Return the byte offset corresponding to the line number `line`.
    /// If `line` is equal to one plus the current number of lines,
    /// this returns the offset of the end of the rope. Arguments higher
    /// than this will panic.
    ///
    /// The line number is 0-based.
    ///
    /// Time complexity: O(log n)
    ///
    /// # Panics
    ///
    /// This function will panic if `line > self.measure::<LinesMetric>() + 1`.
    /// Callers are expected to validate their input.
    pub fn offset_of_line(&self, line: usize) -> usize {
        self.try_offset_of_line(line).expect("Rope::offset_of_line callers must validate bounds")
    }

    pub fn try_offset_of_line(&self, line: usize) -> Result<usize, RopeError> {
        self.validate_line(line)?;
        Ok(match line.cmp(&self.max_line_index()) {
            Ordering::Equal => self.len(),
            Ordering::Less => self.count_base_units::<LinesMetric>(line),
            Ordering::Greater => unreachable!("validate_line rejects oversized indices"),
        })
    }

    /// Returns chunk containing `byte_offset` and chunk start metrics.
    pub fn chunk_at_offset(&self, byte_offset: usize) -> Option<(&str, usize, usize, usize)> {
        if byte_offset > self.len() {
            return None;
        }
        let cursor = Cursor::new(self, byte_offset);
        let (leaf, _) = cursor.get_leaf()?;
        Some((
            leaf.as_str(),
            cursor.leaf_start_offset()?,
            cursor.leaf_start_measure::<LinesMetric>()?,
            cursor.leaf_start_measure::<Utf16CodeUnitsMetric>()?,
        ))
    }

    /// Returns chunk containing line boundary `line` and chunk start metrics.
    pub fn chunk_at_line(&self, line: usize) -> Option<(&str, usize, usize, usize)> {
        let max_line = self.measure::<LinesMetric>() + 1;
        if line > max_line {
            return None;
        }
        self.chunk_at_offset(self.count_base_units::<LinesMetric>(line))
    }

    /// Returns chunk containing UTF-16 boundary `utf16_offset` and chunk start metrics.
    pub fn chunk_at_utf16(&self, utf16_offset: usize) -> Option<(&str, usize, usize, usize)> {
        let max_utf16 = self.measure::<Utf16CodeUnitsMetric>();
        if utf16_offset > max_utf16 {
            return None;
        }
        self.chunk_at_offset(self.count_base_units::<Utf16CodeUnitsMetric>(utf16_offset))
    }

    /// Returns an iterator over chunks of the rope.
    ///
    /// Each chunk is a `&str` slice borrowed from the rope's storage. The size
    /// of the chunks is indeterminate but for large strings will generally be
    /// in the range of 511-1024 bytes.
    ///
    /// The empty string will yield a single empty slice. In all other cases, the
    /// slices will be nonempty.
    ///
    /// Time complexity: technically O(n log n), but the constant factor is so
    /// tiny it is effectively O(n). This iterator does not allocate.
    pub fn iter_chunks<T: IntervalBounds>(&self, range: T) -> ChunkIter<'_> {
        let Interval { start, end } = range.into_interval(self.len());

        ChunkIter { cursor: Cursor::new(self, start), end }
    }

    /// An iterator over the raw lines. The lines, except the last, include the
    /// terminating newline.
    ///
    /// The return type is a `Cow<str>`, and in most cases the lines are slices
    /// borrowed from the rope.
    pub fn lines_raw<T: IntervalBounds>(&self, range: T) -> LinesRaw<'_> {
        LinesRaw { inner: self.iter_chunks(range), fragment: "" }
    }

    /// An iterator over the lines of a rope.
    ///
    /// Lines are ended with either Unix (`\n`) or MS-DOS (`\r\n`) style line endings.
    /// The line ending is stripped from the resulting string. The final line ending
    /// is optional.
    ///
    /// The return type is a `Cow<str>`, and in most cases the lines are slices borrowed
    /// from the rope.
    ///
    /// The semantics are intended to match `str::lines()`.
    pub fn lines<T: IntervalBounds>(&self, range: T) -> Lines<'_> {
        Lines { inner: self.lines_raw(range) }
    }

    // callers should be encouraged to use cursor instead
    pub fn byte_at(&self, offset: usize) -> u8 {
        let cursor = Cursor::new(self, offset);
        let (leaf, pos) = cursor.get_leaf().unwrap();
        leaf.as_bytes()[pos]
    }

    pub fn slice_to_cow<T: IntervalBounds>(&self, range: T) -> Cow<'_, str> {
        let mut iter = self.iter_chunks(range);
        let first = iter.next();
        let second = iter.next();

        match (first, second) {
            (None, None) => Cow::from(""),
            (Some(s), None) => Cow::from(s),
            (Some(one), Some(two)) => {
                let mut result = [one, two].concat();
                for chunk in iter {
                    result.push_str(chunk);
                }
                Cow::from(result)
            }
            (None, Some(_)) => unreachable!(),
        }
    }

    /// Stream rope contents into a byte writer without flattening first.
    pub fn write_to<W: io::Write>(&self, mut writer: W) -> io::Result<()> {
        for chunk in self.iter_chunks(..) {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }
}

// should make this generic, but most leaf types aren't going to be sliceable
pub struct ChunkIter<'a> {
    cursor: Cursor<'a, RopeInfo>,
    end: usize,
}

impl<'a> Iterator for ChunkIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.cursor.pos() >= self.end {
            return None;
        }
        let (leaf, start_pos) = self.cursor.get_leaf().unwrap();
        let len = min(self.end - self.cursor.pos(), leaf.len() - start_pos);
        self.cursor.next_leaf();
        Some(&leaf[start_pos..start_pos + len])
    }
}

impl TreeBuilder<RopeInfo> {
    /// Push a string on the accumulating tree in the naive way.
    ///
    /// Splits the provided string in chunks that fit in a leaf
    /// and pushes the leaves one by one onto the tree by calling
    /// `push_leaf` on the builder.
    pub fn push_str(&mut self, mut s: &str) {
        if s.len() <= MAX_LEAF {
            if !s.is_empty() {
                self.push_leaf(s.to_owned());
            }
            return;
        }
        while !s.is_empty() {
            let splitpoint = if s.len() > MAX_LEAF { find_leaf_split_for_bulk(s) } else { s.len() };
            self.push_leaf(s[..splitpoint].to_owned());
            s = &s[splitpoint..];
        }
    }
}

impl<T: AsRef<str>> From<T> for Rope {
    fn from(s: T) -> Rope {
        Rope::from_str(s.as_ref()).unwrap()
    }
}

impl From<Rope> for String {
    // maybe explore grabbing leaf? would require api in tree
    fn from(r: Rope) -> String {
        String::from(&r)
    }
}

impl From<&Rope> for String {
    fn from(r: &Rope) -> String {
        r.slice_to_cow(..).into_owned()
    }
}

impl fmt::Display for Rope {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for s in self.iter_chunks(..) {
            write!(f, "{}", s)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Rope {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if f.alternate() {
            write!(f, "{}", String::from(self))
        } else {
            write!(f, "Rope({:?})", String::from(self))
        }
    }
}

impl Add for Rope {
    type Output = Rope;
    fn add(self, rhs: Rope) -> Rope {
        let mut b = TreeBuilder::new();
        b.push(self);
        b.push(rhs);
        b.build()
    }
}

//additional cursor features

impl<'a> Cursor<'a, RopeInfo> {
    /// Get previous codepoint before cursor position, and advance cursor backwards.
    pub fn prev_codepoint(&mut self) -> Option<char> {
        self.prev::<BaseMetric>();
        if let Some((l, offset)) = self.get_leaf() { l[offset..].chars().next() } else { None }
    }

    /// Get next codepoint after cursor position, and advance cursor.
    pub fn next_codepoint(&mut self) -> Option<char> {
        if let Some((l, offset)) = self.get_leaf() {
            self.next::<BaseMetric>();
            l[offset..].chars().next()
        } else {
            None
        }
    }

    /// Get the next codepoint after the cursor position, without advancing
    /// the cursor.
    pub fn peek_next_codepoint(&self) -> Option<char> {
        self.get_leaf().and_then(|(l, off)| l[off..].chars().next())
    }

    pub fn next_grapheme(&mut self) -> Option<usize> {
        let (mut l, mut offset) = self.get_leaf()?;
        let mut pos = self.pos();
        while offset < l.len() && !l.is_char_boundary(offset) {
            pos -= 1;
            offset -= 1;
        }
        let mut leaf_offset = pos - offset;
        let mut c = GraphemeCursor::new(pos, self.total_len(), true);
        let mut next_boundary = c.next_boundary(l, leaf_offset);
        while let Err(incomp) = next_boundary {
            if let GraphemeIncomplete::PreContext(_) = incomp {
                let (pl, poffset) = self.prev_leaf()?;
                c.provide_context(pl, self.pos() - poffset);
            } else if incomp == GraphemeIncomplete::NextChunk {
                self.set(pos);
                let (nl, noffset) = self.next_leaf()?;
                l = nl;
                leaf_offset = self.pos() - noffset;
                pos = leaf_offset + nl.len();
            } else {
                return None;
            }
            next_boundary = c.next_boundary(l, leaf_offset);
        }
        next_boundary.unwrap_or(None)
    }

    pub fn prev_grapheme(&mut self) -> Option<usize> {
        let (mut l, mut offset) = self.get_leaf()?;
        let mut pos = self.pos();
        while offset < l.len() && !l.is_char_boundary(offset) {
            pos += 1;
            offset += 1;
        }
        let mut leaf_offset = pos - offset;
        let mut c = GraphemeCursor::new(pos, l.len() + leaf_offset, true);
        let mut prev_boundary = c.prev_boundary(l, leaf_offset);
        while let Err(incomp) = prev_boundary {
            if let GraphemeIncomplete::PreContext(_) = incomp {
                let (pl, poffset) = self.prev_leaf()?;
                c.provide_context(pl, self.pos() - poffset);
            } else if incomp == GraphemeIncomplete::PrevChunk {
                self.set(pos);
                let (pl, poffset) = self.prev_leaf()?;
                l = pl;
                leaf_offset = self.pos() - poffset;
                pos = leaf_offset + pl.len();
            } else {
                return None;
            }
            prev_boundary = c.prev_boundary(l, leaf_offset);
        }
        prev_boundary.unwrap_or(None)
    }
}

// line iterators

pub struct LinesRaw<'a> {
    inner: ChunkIter<'a>,
    fragment: &'a str,
}

fn cow_append<'a>(a: Cow<'a, str>, b: &'a str) -> Cow<'a, str> {
    if a.is_empty() { Cow::from(b) } else { Cow::from(a.into_owned() + b) }
}

impl<'a> Iterator for LinesRaw<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Cow<'a, str>> {
        let mut result = Cow::from("");
        loop {
            if self.fragment.is_empty() {
                match self.inner.next() {
                    Some(chunk) => self.fragment = chunk,
                    None => return if result.is_empty() { None } else { Some(result) },
                }
                if self.fragment.is_empty() {
                    // can only happen on empty input
                    return None;
                }
            }
            match memchr(b'\n', self.fragment.as_bytes()) {
                Some(i) => {
                    result = cow_append(result, &self.fragment[..=i]);
                    self.fragment = &self.fragment[i + 1..];
                    return Some(result);
                }
                None => {
                    result = cow_append(result, self.fragment);
                    self.fragment = "";
                }
            }
        }
    }
}

pub struct Lines<'a> {
    inner: LinesRaw<'a>,
}

impl<'a> Iterator for Lines<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Cow<'a, str>> {
        match self.inner.next() {
            Some(Cow::Borrowed(mut s)) => {
                if s.ends_with('\n') {
                    s = &s[..s.len() - 1];
                    if s.ends_with('\r') {
                        s = &s[..s.len() - 1];
                    }
                }
                Some(Cow::from(s))
            }
            Some(Cow::Owned(mut s)) => {
                if s.ends_with('\n') {
                    let _ = s.pop();
                    if s.ends_with('\r') {
                        let _ = s.pop();
                    }
                }
                Some(Cow::from(s))
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter {
        written: Vec<u8>,
        remaining: usize,
        fail_kind: io::ErrorKind,
    }

    impl io::Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(self.fail_kind, "writer interrupted"));
            }

            let written = self.remaining.min(buf.len());
            self.written.extend_from_slice(&buf[..written]);
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn replace_small() {
        let mut a = Rope::from("hello world");
        a.edit(1..9, "era");
        assert_eq!("herald", String::from(a));
    }

    #[test]
    fn lines_raw_small() {
        let a = Rope::from("a\nb\nc");
        assert_eq!(vec!["a\n", "b\n", "c"], a.lines_raw(..).collect::<Vec<_>>());
        assert_eq!(vec!["a\n", "b\n", "c"], a.lines_raw(..).collect::<Vec<_>>());

        let a = Rope::from("a\nb\n");
        assert_eq!(vec!["a\n", "b\n"], a.lines_raw(..).collect::<Vec<_>>());

        let a = Rope::from("\n");
        assert_eq!(vec!["\n"], a.lines_raw(..).collect::<Vec<_>>());

        let a = Rope::from("");
        assert_eq!(0, a.lines_raw(..).count());
    }

    #[test]
    fn lines_small() {
        let a = Rope::from("a\nb\nc");
        assert_eq!(vec!["a", "b", "c"], a.lines(..).collect::<Vec<_>>());
        assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

        let a = Rope::from("a\nb\n");
        assert_eq!(vec!["a", "b"], a.lines(..).collect::<Vec<_>>());
        assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

        let a = Rope::from("\n");
        assert_eq!(vec![""], a.lines(..).collect::<Vec<_>>());
        assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

        let a = Rope::from("");
        assert_eq!(0, a.lines(..).count());
        assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

        let a = Rope::from("a\r\nb\r\nc");
        assert_eq!(vec!["a", "b", "c"], a.lines(..).collect::<Vec<_>>());
        assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

        let a = Rope::from("a\rb\rc");
        assert_eq!(vec!["a\rb\rc"], a.lines(..).collect::<Vec<_>>());
        assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());
    }

    #[test]
    fn lines_med() {
        let mut a = String::new();
        let mut b = String::new();
        let line_len = MAX_LEAF + MIN_LEAF - 1;
        for _ in 0..line_len {
            a.push('a');
            b.push('b');
        }
        a.push('\n');
        b.push('\n');
        let r = Rope::from(&a[..MAX_LEAF]);
        let r = r + Rope::from(String::from(&a[MAX_LEAF..]) + &b[..MIN_LEAF]);
        let r = r + Rope::from(&b[MIN_LEAF..]);
        //println!("{:?}", r.iter_chunks().collect::<Vec<_>>());

        assert_eq!(vec![a.as_str(), b.as_str()], r.lines_raw(..).collect::<Vec<_>>());
        assert_eq!(vec![&a[..line_len], &b[..line_len]], r.lines(..).collect::<Vec<_>>());
        assert_eq!(String::from(&r).lines().collect::<Vec<_>>(), r.lines(..).collect::<Vec<_>>());

        // additional tests for line indexing
        assert_eq!(a.len(), r.offset_of_line(1));
        assert_eq!(r.len(), r.offset_of_line(2));
        assert_eq!(0, r.line_of_offset(a.len() - 1));
        assert_eq!(1, r.line_of_offset(a.len()));
        assert_eq!(1, r.line_of_offset(r.len() - 1));
        assert_eq!(2, r.line_of_offset(r.len()));
    }

    #[test]
    fn append_large() {
        let mut a = Rope::from("");
        let mut b = String::new();
        for i in 0..5_000 {
            let c = i.to_string() + "\n";
            b.push_str(&c);
            a = a + Rope::from(&c);
        }
        assert_eq!(b, String::from(a));
    }

    #[test]
    fn prev_codepoint_offset_small() {
        let a = Rope::from("a\u{00A1}\u{4E00}\u{1F4A9}");
        assert_eq!(Some(6), a.prev_codepoint_offset(10));
        assert_eq!(Some(3), a.prev_codepoint_offset(6));
        assert_eq!(Some(1), a.prev_codepoint_offset(3));
        assert_eq!(Some(0), a.prev_codepoint_offset(1));
        assert_eq!(None, a.prev_codepoint_offset(0));
        let b = a.slice(1..10);
        assert_eq!(Some(5), b.prev_codepoint_offset(9));
        assert_eq!(Some(2), b.prev_codepoint_offset(5));
        assert_eq!(Some(0), b.prev_codepoint_offset(2));
        assert_eq!(None, b.prev_codepoint_offset(0));
    }

    #[test]
    fn next_codepoint_offset_small() {
        let a = Rope::from("a\u{00A1}\u{4E00}\u{1F4A9}");
        assert_eq!(Some(10), a.next_codepoint_offset(6));
        assert_eq!(Some(6), a.next_codepoint_offset(3));
        assert_eq!(Some(3), a.next_codepoint_offset(1));
        assert_eq!(Some(1), a.next_codepoint_offset(0));
        assert_eq!(None, a.next_codepoint_offset(10));
        let b = a.slice(1..10);
        assert_eq!(Some(9), b.next_codepoint_offset(5));
        assert_eq!(Some(5), b.next_codepoint_offset(2));
        assert_eq!(Some(2), b.next_codepoint_offset(0));
        assert_eq!(None, b.next_codepoint_offset(9));
    }

    #[test]
    fn peek_next_codepoint() {
        let inp = Rope::from("$¢€£💶");
        let mut cursor = Cursor::new(&inp, 0);
        assert_eq!(cursor.peek_next_codepoint(), Some('$'));
        assert_eq!(cursor.peek_next_codepoint(), Some('$'));
        assert_eq!(cursor.next_codepoint(), Some('$'));
        assert_eq!(cursor.peek_next_codepoint(), Some('¢'));
        assert_eq!(cursor.prev_codepoint(), Some('$'));
        assert_eq!(cursor.peek_next_codepoint(), Some('$'));
        assert_eq!(cursor.next_codepoint(), Some('$'));
        assert_eq!(cursor.next_codepoint(), Some('¢'));
        assert_eq!(cursor.peek_next_codepoint(), Some('€'));
        assert_eq!(cursor.next_codepoint(), Some('€'));
        assert_eq!(cursor.peek_next_codepoint(), Some('£'));
        assert_eq!(cursor.next_codepoint(), Some('£'));
        assert_eq!(cursor.peek_next_codepoint(), Some('💶'));
        assert_eq!(cursor.next_codepoint(), Some('💶'));
        assert_eq!(cursor.peek_next_codepoint(), None);
        assert_eq!(cursor.next_codepoint(), None);
        assert_eq!(cursor.peek_next_codepoint(), None);
    }

    #[test]
    fn prev_grapheme_offset() {
        // A with ring, hangul, regional indicator "US"
        let a = Rope::from("A\u{030a}\u{110b}\u{1161}\u{1f1fa}\u{1f1f8}");
        assert_eq!(Some(9), a.prev_grapheme_offset(17));
        assert_eq!(Some(3), a.prev_grapheme_offset(9));
        assert_eq!(Some(0), a.prev_grapheme_offset(3));
        assert_eq!(None, a.prev_grapheme_offset(0));
    }

    #[test]
    fn next_grapheme_offset() {
        // A with ring, hangul, regional indicator "US"
        let a = Rope::from("A\u{030a}\u{110b}\u{1161}\u{1f1fa}\u{1f1f8}");
        assert_eq!(Some(3), a.next_grapheme_offset(0));
        assert_eq!(Some(9), a.next_grapheme_offset(3));
        assert_eq!(Some(17), a.next_grapheme_offset(9));
        assert_eq!(None, a.next_grapheme_offset(17));
    }

    #[test]
    fn next_grapheme_offset_with_ris_of_leaf_boundaries() {
        let s1 = "\u{1f1fa}\u{1f1f8}".repeat(100);
        let a = Rope::concat(
            Rope::from(s1.clone()),
            Rope::concat(Rope::from(s1.clone() + "\u{1f1fa}"), Rope::from(s1.clone())),
        );
        for i in 1..(s1.len() * 3) {
            assert_eq!(Some((i - 1) / 8 * 8), a.prev_grapheme_offset(i));
            assert_eq!(Some(i / 8 * 8 + 8), a.next_grapheme_offset(i));
        }
        for i in (s1.len() * 3 + 1)..(s1.len() * 3 + 4) {
            assert_eq!(Some(s1.len() * 3), a.prev_grapheme_offset(i));
            assert_eq!(Some(s1.len() * 3 + 4), a.next_grapheme_offset(i));
        }
        assert_eq!(None, a.prev_grapheme_offset(0));
        assert_eq!(Some(8), a.next_grapheme_offset(0));
        assert_eq!(Some(s1.len() * 3), a.prev_grapheme_offset(s1.len() * 3 + 4));
        assert_eq!(None, a.next_grapheme_offset(s1.len() * 3 + 4));
    }

    #[test]
    fn line_of_offset_small() {
        let a = Rope::from("a\nb\nc");
        assert_eq!(0, a.line_of_offset(0));
        assert_eq!(0, a.line_of_offset(1));
        assert_eq!(1, a.line_of_offset(2));
        assert_eq!(1, a.line_of_offset(3));
        assert_eq!(2, a.line_of_offset(4));
        assert_eq!(2, a.line_of_offset(5));
        let b = a.slice(2..4);
        assert_eq!(0, b.line_of_offset(0));
        assert_eq!(0, b.line_of_offset(1));
        assert_eq!(1, b.line_of_offset(2));
    }

    #[test]
    fn offset_of_line_small() {
        let a = Rope::from("a\nb\nc");
        assert_eq!(0, a.offset_of_line(0));
        assert_eq!(2, a.offset_of_line(1));
        assert_eq!(4, a.offset_of_line(2));
        assert_eq!(5, a.offset_of_line(3));
        let b = a.slice(2..4);
        assert_eq!(0, b.offset_of_line(0));
        assert_eq!(2, b.offset_of_line(1));
    }

    #[test]
    #[allow(clippy::eq_op)]
    fn eq_small() {
        let a = Rope::from("a");
        let a2 = Rope::from("a");
        let b = Rope::from("b");
        let empty = Rope::from("");
        assert!(a == a2);
        assert!(a != b);
        assert!(a != empty);
        assert!(empty == empty);
        assert!(a.slice(0..0) == empty);
    }

    #[test]
    fn eq_med() {
        let mut a = String::new();
        let mut b = String::new();
        let line_len = MAX_LEAF + MIN_LEAF - 1;
        for _ in 0..line_len {
            a.push('a');
            b.push('b');
        }
        a.push('\n');
        b.push('\n');
        let r = Rope::from(&a[..MAX_LEAF]);
        let r = r + Rope::from(String::from(&a[MAX_LEAF..]) + &b[..MIN_LEAF]);
        let r = r + Rope::from(&b[MIN_LEAF..]);

        let a_rope = Rope::from(&a);
        let b_rope = Rope::from(&b);
        assert!(r != a_rope);
        assert!(r.slice(..a.len()) == a_rope);
        assert!(r.slice(a.len()..) == b_rope);
        assert!(r == a_rope.clone() + b_rope.clone());
        assert!(r != b_rope + a_rope);
    }

    #[test]
    fn line_offsets() {
        let rope = Rope::from("hi\ni'm\nfour\nlines");
        assert_eq!(rope.offset_of_line(0), 0);
        assert_eq!(rope.offset_of_line(1), 3);
        assert_eq!(rope.line_of_offset(0), 0);
        assert_eq!(rope.line_of_offset(3), 1);
        // interior of first line should be first line
        assert_eq!(rope.line_of_offset(1), 0);
        // interior of last line should be last line
        assert_eq!(rope.line_of_offset(15), 3);
        assert_eq!(rope.offset_of_line(4), rope.len());
    }

    #[test]
    fn default_metric_test() {
        let rope = Rope::from("hi\ni'm\nfour\nlines\n");
        assert_eq!(
            rope.convert_metrics::<BaseMetric, LinesMetric>(rope.len()),
            rope.count::<LinesMetric>(rope.len())
        );
        assert_eq!(
            rope.convert_metrics::<LinesMetric, BaseMetric>(2),
            rope.count_base_units::<LinesMetric>(2)
        );
    }

    #[test]
    #[should_panic]
    fn line_of_offset_panic() {
        let rope = Rope::from("hi\ni'm\nfour\nlines");
        rope.line_of_offset(20);
    }

    #[test]
    #[should_panic]
    fn offset_of_line_panic() {
        let rope = Rope::from("hi\ni'm\nfour\nlines");
        rope.offset_of_line(5);
    }

    #[test]
    fn try_line_of_offset_reports_bounds_error() {
        let rope = Rope::from("hi\ni'm\nfour\nlines");
        assert_eq!(
            rope.try_line_of_offset(20),
            Err(RopeError::OffsetOutOfBounds { offset: 20, len: rope.len() })
        );
    }

    #[test]
    fn try_offset_of_line_reports_bounds_error() {
        let rope = Rope::from("hi\ni'm\nfour\nlines");
        assert_eq!(
            rope.try_offset_of_line(5),
            Err(RopeError::LineOutOfBounds { line: 5, max_line: 4 })
        );
    }

    #[test]
    fn try_slice_reports_bounds_error() {
        let rope = Rope::from("hello");
        assert_eq!(
            rope.try_slice(0..10),
            Err(RopeError::IntervalOutOfBounds { start: 0, end: 10, len: 5 })
        );
    }

    #[test]
    fn try_edit_reports_bounds_error() {
        let mut rope = Rope::from("hello");
        assert_eq!(
            rope.try_edit(4..8, "!"),
            Err(RopeError::IntervalOutOfBounds { start: 4, end: 8, len: 5 })
        );
        assert_eq!(String::from(&rope), "hello");
    }

    #[test]
    fn write_to_streams_full_rope() {
        let text = format!("{}{}", "a".repeat(1200), "b".repeat(1200));
        let rope = Rope::from(text.as_str());
        let mut bytes = Vec::new();

        rope.write_to(&mut bytes).unwrap();

        assert_eq!(String::from_utf8(bytes).unwrap(), text);
    }

    #[test]
    fn write_to_propagates_partial_write_errors() {
        let text = format!("{}{}", "a".repeat(1200), "b".repeat(1200));
        let rope = Rope::from(text.as_str());
        let mut writer = FailingWriter {
            written: Vec::new(),
            remaining: 1300,
            fail_kind: io::ErrorKind::BrokenPipe,
        };

        let err = rope.write_to(&mut writer).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(writer.written, text.as_bytes()[..1300]);
    }

    #[test]
    fn clone_edit_preserves_snapshot_and_shares_unchanged_chunks() {
        let text = format!("{}{}", "left-side-line\n".repeat(200), "right-side-line\n".repeat(200));
        let mut rope = Rope::from(text.as_str());
        let snapshot = rope.clone();
        let snapshot_chunk_ptrs = snapshot.iter_chunks(..).map(str::as_ptr).collect::<Vec<_>>();

        assert!(rope.ptr_eq(&snapshot));

        rope.edit(0..4, "LEFT");

        assert_eq!(String::from(&snapshot), text);
        assert!(!rope.ptr_eq(&snapshot));
        assert!(
            rope.iter_chunks(..).skip(1).any(|chunk| snapshot_chunk_ptrs.contains(&chunk.as_ptr())),
            "unchanged suffix chunks should remain shared after copy-on-write edit"
        );
    }

    #[test]
    fn rope_builder_preserves_content_metrics_and_leaf_boundaries() {
        let text = format!(
            "{}\n{}🙂{}",
            "a".repeat(MAX_LEAF - 24),
            "b".repeat(MAX_LEAF),
            "é".repeat(MIN_LEAF + 17)
        );
        let expected = Rope::from(text.as_str());
        let mut builder = RopeBuilder::new();
        let mut start = 0;

        while start < text.len() {
            let mut end = (start + 137).min(text.len());
            end = clamp_to_char_boundary(&text, end);
            builder.push_str(&text[start..end]);
            start = end;
        }

        let built = builder.finish();
        let boundaries = chunk_boundaries(&built);

        assert_eq!(String::from(&built), text);
        assert_eq!(built.measure::<LinesMetric>(), expected.measure::<LinesMetric>());
        assert_eq!(
            built.measure::<Utf16CodeUnitsMetric>(),
            expected.measure::<Utf16CodeUnitsMetric>()
        );
        assert!(boundaries.len() >= 2, "expected multi-leaf rope");
    }

    #[test]
    fn rope_builder_append_streams_existing_rope() {
        let suffix_text = format!("{}{}", "suffix-".repeat(120), "🙂tail");
        let suffix = Rope::from(suffix_text.as_str());
        let mut builder = RopeBuilder::new();

        builder.push_str("prefix-");
        builder.append(&suffix);

        let built = builder.finish();

        assert_eq!(String::from(&built), format!("prefix-{suffix_text}"));
        assert_eq!(built.measure::<LinesMetric>(), suffix.measure::<LinesMetric>());
    }

    #[test]
    fn find_leaf_split_for_merge_prefers_newline_boundary() {
        let text = format!("{}\n{}", "a".repeat(MAX_LEAF - 8), "b".repeat(MIN_LEAF + 32));

        let splitpoint = find_leaf_split_for_merge(&text);

        assert!(text[..splitpoint].ends_with('\n'));
    }

    #[test]
    fn find_leaf_split_for_merge_avoids_crlf_boundary() {
        let text = format!("{}\r\n{}", "a".repeat(MAX_LEAF - 1), "b".repeat(MIN_LEAF + 32));

        let splitpoint = find_leaf_split_for_merge(&text);

        assert!(!is_crlf_split_point(&text, splitpoint));
        assert!(splitpoint >= max(MIN_LEAF, text.len() - MAX_LEAF));
        assert!(splitpoint <= min(MAX_LEAF, text.len() - MIN_LEAF));
    }

    #[test]
    fn utf16_code_units_metric() {
        let rope = Rope::from("hi\ni'm\nfour\nlines");
        let utf16_units = rope.measure::<Utf16CodeUnitsMetric>();
        assert_eq!(utf16_units, 17);

        // position after 'f' in four
        let utf8_offset = 9;
        let utf16_units = rope.count::<Utf16CodeUnitsMetric>(utf8_offset);
        assert_eq!(utf16_units, 9);

        let utf8_offset = rope.count_base_units::<Utf16CodeUnitsMetric>(utf16_units);
        assert_eq!(utf8_offset, 9);

        let rope_with_emoji = Rope::from("hi\ni'm\n😀 four\nlines");
        let utf16_units = rope_with_emoji.measure::<Utf16CodeUnitsMetric>();

        assert_eq!(utf16_units, 20);

        // position after 'f' in four
        let utf8_offset = 13;
        let utf16_units = rope_with_emoji.count::<Utf16CodeUnitsMetric>(utf8_offset);
        assert_eq!(utf16_units, 11);

        let utf8_offset = rope_with_emoji.count_base_units::<Utf16CodeUnitsMetric>(utf16_units);
        assert_eq!(utf8_offset, 13);

        //for next line
        let utf8_offset = 19;
        let utf16_units = rope_with_emoji.count::<Utf16CodeUnitsMetric>(utf8_offset);
        assert_eq!(utf16_units, 17);

        let utf8_offset = rope_with_emoji.count_base_units::<Utf16CodeUnitsMetric>(utf16_units);
        assert_eq!(utf8_offset, 19);
    }

    fn chunk_boundaries(rope: &Rope) -> Vec<(usize, String)> {
        let mut offset = 0;
        let mut result = Vec::new();
        for chunk in rope.iter_chunks(..) {
            result.push((offset, chunk.to_owned()));
            offset += chunk.len();
        }
        result
    }

    fn two_leaf_rope(left: &str, right: &str) -> Rope {
        Rope::from(left) + Rope::from(right)
    }

    fn assert_no_crlf_chunk_split(rope: &Rope) {
        let boundaries = chunk_boundaries(rope);
        for window in boundaries.windows(2) {
            assert!(
                !(window[0].1.ends_with('\r') && window[1].1.starts_with('\n')),
                "chunk boundary split CRLF at byte {}",
                window[1].0
            );
        }
    }

    fn assert_crlf_line_metrics(rope: &Rope, expected: &str, line_break: usize) {
        assert_eq!(String::from(rope), expected);
        assert_eq!(rope.lines(..).collect::<Vec<_>>(), expected.lines().collect::<Vec<_>>());
        assert_eq!(rope.measure::<LinesMetric>(), count_newlines(expected));
        assert_eq!(rope.offset_of_line(1), line_break + 2);
        assert_eq!(rope.line_of_offset(line_break), 0);
        assert_eq!(rope.line_of_offset(line_break + 1), 0);
        assert_eq!(rope.line_of_offset(line_break + 2), 1);
    }

    #[test]
    fn chunk_at_offset_empty_rope() {
        let rope = Rope::from("");
        assert_eq!(rope.chunk_at_offset(0), Some(("", 0, 0, 0)));
        assert_eq!(rope.chunk_at_line(0), Some(("", 0, 0, 0)));
        assert_eq!(rope.chunk_at_utf16(0), Some(("", 0, 0, 0)));
        assert_eq!(rope.chunk_at_offset(1), None);
    }

    #[test]
    fn chunk_at_offset_reports_leaf_boundary_metrics() {
        let left = "a".repeat(MAX_LEAF);
        let right = format!("{}{}", "b".repeat(MAX_LEAF), "\nccc");
        let rope = two_leaf_rope(&left, &right);
        let boundaries = chunk_boundaries(&rope);
        assert!(boundaries.len() >= 2, "expected multi-leaf rope");

        let (boundary, expected_chunk) = &boundaries[1];
        let located = rope.chunk_at_offset(*boundary).expect("chunk at boundary");
        assert_eq!(located.0, expected_chunk);
        assert_eq!(located.1, *boundary);
        assert_eq!(located.2, rope.line_of_offset(*boundary));
        assert_eq!(located.3, rope.count::<Utf16CodeUnitsMetric>(*boundary));
    }

    #[test]
    fn chunk_at_line_handles_crlf_seam_across_leaves() {
        let left = format!("{}\r", "a".repeat(MIN_LEAF + 32));
        let right = format!("\n{}", "b".repeat(MIN_LEAF + 32));
        let rope = two_leaf_rope(&left, &right);
        let located = rope.chunk_at_offset(left.len()).expect("chunk at CRLF seam");
        assert_eq!(located.0, right);
        assert_eq!(located.1, left.len());
        assert_eq!(located.2, 0);
        assert_eq!(rope.chunk_at_line(1).expect("line 1 chunk").1, left.len());
        assert_eq!(rope.chunk_at_line(1).expect("line 1 chunk").2, 0);
    }

    #[test]
    fn rope_builder_preserves_crlf_line_metrics_near_leaf_boundary() {
        let line_break = MAX_LEAF - 1;
        let text = format!("{}\r\n{}", "a".repeat(line_break), "b".repeat(MIN_LEAF + 32));
        let rope = Rope::from(text.as_str());

        assert_no_crlf_chunk_split(&rope);
        assert_crlf_line_metrics(&rope, &text, line_break);
    }

    #[test]
    fn edit_preserves_crlf_line_metrics_at_edit_boundary() {
        let line_break = MAX_LEAF - 1;
        let suffix = "b".repeat(MIN_LEAF + 32);
        let mut rope = Rope::from(format!("{}\n{}", "a".repeat(line_break), suffix));
        rope.edit(line_break..line_break, "\r");
        let expected = format!("{}\r\n{}", "a".repeat(line_break), suffix);

        assert_no_crlf_chunk_split(&rope);
        assert_crlf_line_metrics(&rope, &expected, line_break);
    }

    #[test]
    fn chunk_at_utf16_tracks_multibyte_leaf_boundary() {
        let left = "a".repeat(MIN_LEAF + 32);
        let right = format!("{}{}", "é".repeat(MIN_LEAF + 16), "🙂");
        let rope = two_leaf_rope(&left, &right);
        let byte_start = left.len();
        let utf16_start = rope.count::<Utf16CodeUnitsMetric>(byte_start);
        let expected = rope.chunk_at_offset(byte_start).expect("chunk at byte boundary");

        let located = rope.chunk_at_utf16(utf16_start).expect("chunk at utf16 boundary");
        assert_eq!(located.0, expected.0);
        assert_eq!(located.1, expected.1);
        assert_eq!(located.2, rope.line_of_offset(byte_start));
        assert_eq!(located.3, utf16_start);
    }

    #[test]
    fn slice_to_cow_small_string() {
        let short_text = "hi, i'm a small piece of text.";

        let rope = Rope::from(short_text);

        let cow = rope.slice_to_cow(..);

        assert!(short_text.len() <= 1024);
        assert_eq!(cow, Cow::Borrowed(short_text) as Cow<str>);
    }

    #[test]
    fn slice_to_cow_long_string_long_slice() {
        // 32 char long string, repeat it 33 times so it is longer than 1024 bytes
        let long_text =
            "1234567812345678123456781234567812345678123456781234567812345678".repeat(33);

        let rope = Rope::from(&long_text);

        let cow = rope.slice_to_cow(..);

        assert!(long_text.len() > 1024);
        assert_eq!(cow, Cow::Owned(long_text) as Cow<str>);
    }

    #[test]
    fn slice_to_cow_long_string_short_slice() {
        // 32 char long string, repeat it 33 times so it is longer than 1024 bytes
        let long_text =
            "1234567812345678123456781234567812345678123456781234567812345678".repeat(33);

        let rope = Rope::from(&long_text);

        let cow = rope.slice_to_cow(..500);

        assert!(long_text.len() > 1024);
        assert_eq!(cow, Cow::Borrowed(&long_text[..500]));
    }

    #[test]
    fn slice_view_iterates_chunks_without_materializing() {
        let text = format!("{}{}{}", "a".repeat(MAX_LEAF), "\n", "b".repeat(MAX_LEAF));
        let rope = Rope::from(&text);
        let view = rope.slice_view(MAX_LEAF - 8..MAX_LEAF + 9);

        let collected: String = view.iter_chunks().collect();
        assert_eq!(collected, text[MAX_LEAF - 8..MAX_LEAF + 9]);
        assert_eq!(
            view.lines_raw().collect::<Vec<_>>(),
            vec![
                Cow::from(&text[MAX_LEAF - 8..MAX_LEAF + 1]),
                Cow::from(&text[MAX_LEAF + 1..MAX_LEAF + 9])
            ]
        );
    }

    #[test]
    fn nested_slice_view_reuses_original_chunk_storage() {
        let text = format!("{}{}{}", "a".repeat(MAX_LEAF), "0123456789", "b".repeat(MAX_LEAF));
        let rope = Rope::from(&text);
        let nested = rope.slice_view(MAX_LEAF - 4..MAX_LEAF + 14).slice(3..15);

        let direct_chunks: Vec<_> = rope.iter_chunks(MAX_LEAF - 1..MAX_LEAF + 11).collect();
        let nested_chunks: Vec<_> = nested.iter_chunks().collect();

        assert_eq!(nested_chunks, direct_chunks);
        assert!(
            nested_chunks
                .iter()
                .zip(direct_chunks.iter())
                .all(|(nested, direct)| nested.as_ptr() == direct.as_ptr())
        );
    }

    #[test]
    fn slice_view_cursor_stops_at_view_end() {
        let rope = Rope::from(format!("{}needle-after", "x".repeat(MAX_LEAF + 32)));
        let view = rope.slice_view(MAX_LEAF + 20..MAX_LEAF + 26);
        let mut cursor = view.cursor(0);

        assert_eq!(cursor.get_leaf().map(|(leaf, _)| leaf), Some("xxxxxx"));
        assert_eq!(cursor.next_leaf(), None);
        assert_eq!(cursor.pos(), view.len());
    }
}

#[cfg(all(test, feature = "serde"))]
mod serde_tests {
    use super::*;
    use crate::Rope;
    use serde_test::{Token, assert_tokens};

    #[test]
    fn serialize_and_deserialize() {
        const TEST_LINE: &str = "test line\n";

        // repeat test line enough times to exceed maximum leaf size
        let n_seg = MAX_LEAF / TEST_LINE.len() + 1;
        let test_str = TEST_LINE.repeat(n_seg);

        let rope = Rope::from(test_str.as_str());
        let json = serde_json::to_string(&rope).expect("error serializing");
        let deserialized_rope =
            serde_json::from_str::<Rope>(json.as_str()).expect("error deserializing");
        assert_eq!(rope, deserialized_rope);
    }

    #[test]
    fn test_ser_de() {
        let rope = Rope::from("a\u{00A1}\u{4E00}\u{1F4A9}");
        assert_tokens(&rope, &[Token::Str("a\u{00A1}\u{4E00}\u{1F4A9}")]);
        assert_tokens(&rope, &[Token::String("a\u{00A1}\u{4E00}\u{1F4A9}")]);
        assert_tokens(&rope, &[Token::BorrowedStr("a\u{00A1}\u{4E00}\u{1F4A9}")]);
    }
}
