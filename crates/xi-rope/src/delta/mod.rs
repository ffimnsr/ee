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

//! A data structure for representing editing operations on ropes.
//! It's useful to explicitly represent these operations so they can be
//! shared across multiple subsystems.
pub(crate) use crate::interval::{Interval, IntervalBounds};
pub(crate) use crate::multiset::{CountMatcher, Subset, SubsetBuilder};
pub(crate) use crate::tree::{Node, NodeInfo, TreeBuilder};
pub(crate) use std::cmp::min;
pub(crate) use std::fmt;
pub(crate) use std::ops::Deref;
pub(crate) use std::slice;

#[derive(Clone)]
pub enum DeltaElement<N: NodeInfo> {
    /// Represents a range of text in the base document. Includes beginning, excludes end.
    Copy(usize, usize), // note: for now, we lose open/closed info at interval endpoints
    Insert(Node<N>),
}

/// Represents changes to a document by describing the new document as a
/// sequence of sections copied from the old document and of new inserted
/// text. Deletions are represented by gaps in the ranges copied from the old
/// document.
///
/// For example, Editing "abcd" into "acde" could be represented as:
/// `[Copy(0,1),Copy(2,4),Insert("e")]`
#[derive(Clone)]
pub struct Delta<N: NodeInfo> {
    pub els: Vec<DeltaElement<N>>,
    pub base_len: usize,
}

/// A struct marking that a Delta contains only insertions. That is, it copies
/// all of the old document in the same order. It has a `Deref` impl so all
/// normal `Delta` methods can also be used on it.
#[derive(Clone)]
pub struct InsertDelta<N: NodeInfo>(Delta<N>);

pub use builder::Builder;
pub use iter::DeltaRegion;
pub use transform::Transformer;

mod apply;
mod builder;
mod iter;
mod transform;

#[cfg(test)]
mod tests;
