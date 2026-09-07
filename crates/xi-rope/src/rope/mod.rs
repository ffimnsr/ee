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

pub(crate) use std::borrow::Cow;
pub(crate) use std::cmp::{Ordering, max, min};
pub(crate) use std::fmt;
pub(crate) use std::io;
pub(crate) use std::ops::Add;
pub(crate) use std::str::{self, FromStr};
pub(crate) use std::string::ParseError;

pub(crate) use crate::delta::{Delta, DeltaElement};
pub(crate) use crate::interval::{Interval, IntervalBounds};
pub(crate) use crate::tree::{Cursor, DefaultMetric, Leaf, Metric, Node, NodeInfo, TreeBuilder};

pub(crate) use memchr::{memchr, memrchr};
pub(crate) use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

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

pub use builder::RopeBuilder;
pub use metrics::{LinesMetric, RopeInfo, count_newlines};
pub use slice::{Lines, LinesRaw, RopeSlice, RopeSliceCursor};

pub(crate) use impls::ChunkIter;
pub use metrics::{BaseMetric, Utf16CodeUnitsMetric};
#[cfg(test)]
pub(crate) use metrics::{clamp_to_char_boundary, find_leaf_split_for_merge, is_crlf_split_point};

mod builder;
mod core;
mod impls;
mod metrics;
mod slice;

#[cfg(test)]
mod tests;
