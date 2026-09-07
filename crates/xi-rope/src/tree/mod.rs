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

//! A general b-tree structure suitable for ropes and the like.

pub(crate) use std::cmp::{Ordering, min};
pub(crate) use std::fmt;
pub(crate) use std::marker::PhantomData;
pub(crate) use std::sync::Arc;

pub(crate) use crate::interval::{Interval, IntervalBounds};

const MIN_CHILDREN: usize = 4;
const MAX_CHILDREN: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TreeInvariantError {
    UnexpectedLeafSplit,
    GetChildrenOnLeaf,
    GetLeafOnInternal,
    WithLeafMutOnInternal,
    MergeLeavesOnNonLeaf,
    EmptyBuilderLevel,
    EmptyBuilderStack,
    InternalNodeWithoutChildren,
}

impl fmt::Display for TreeInvariantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            TreeInvariantError::UnexpectedLeafSplit => {
                "unexpected split while building leaf subsequence"
            }
            TreeInvariantError::GetChildrenOnLeaf => "get_children called on leaf node",
            TreeInvariantError::GetLeafOnInternal => "get_leaf called on internal node",
            TreeInvariantError::WithLeafMutOnInternal => "with_leaf_mut called on internal node",
            TreeInvariantError::MergeLeavesOnNonLeaf => "merge_leaves called on non-leaf",
            TreeInvariantError::EmptyBuilderLevel => "tree builder stack level unexpectedly empty",
            TreeInvariantError::EmptyBuilderStack => "tree builder stack unexpectedly empty",
            TreeInvariantError::InternalNodeWithoutChildren => {
                "internal node unexpectedly missing children"
            }
        };
        f.write_str(message)
    }
}

pub trait NodeInfo: Clone {
    /// The type of the leaf.
    ///
    /// A given `NodeInfo` is for exactly one type of leaf. That is why
    /// the leaf type is an associated type rather than a type parameter.
    type L: Leaf;

    /// An operator that combines info from two subtrees. It is intended
    /// (but not strictly enforced) that this operator be associative and
    /// obey an identity property. In mathematical terms, the accumulate
    /// method is the operation of a monoid.
    fn accumulate(&mut self, other: &Self);

    /// A mapping from a leaf into the info type. It is intended (but
    /// not strictly enforced) that applying the accumulate method to
    /// the info derived from two leaves gives the same result as
    /// deriving the info from the concatenation of the two leaves. In
    /// mathematical terms, the compute_info method is a monoid
    /// homomorphism.
    fn compute_info(_: &Self::L) -> Self;

    /// The identity of the monoid. Need not be implemented because it
    /// can be computed from the leaf default.
    ///
    /// This is here to demonstrate that this is a monoid.
    fn identity() -> Self {
        Self::compute_info(&Self::L::default())
    }

    /// The interval covered by the first `len` base units of this node. The
    /// default impl is sufficient for most types, but interval trees may need
    /// to override it.
    fn interval(&self, len: usize) -> Interval {
        Interval::new(0, len)
    }
}

/// A trait indicating the default metric of a NodeInfo.
///
/// Adds quality of life functions to
/// Node\<N\>, where N is a DefaultMetric.
/// For example, [Node\<DefaultMetric\>.count](struct.Node.html#method.count).
pub trait DefaultMetric: NodeInfo {
    type DefaultMetric: Metric<Self>;
}

/// A trait for the leaves of trees of type [Node](struct.Node.html).
///
/// Two leafs can be concatenated using `push_maybe_split`.
pub trait Leaf: Sized + Clone + Default {
    /// Measurement of leaf in base units.
    /// A 'base unit' refers to the smallest discrete unit
    /// by which a given concrete type can be indexed.
    /// Concretely, for Rust's String type the base unit is the byte.
    fn len(&self) -> usize;

    /// Generally a minimum size requirement for leaves.
    fn is_ok_child(&self) -> bool;

    /// Combine the part `other` denoted by the `Interval` `iv` into `self`,
    /// optionly splitting off a new `Leaf` if `self` would have become too big.
    /// Returns either `None` if no splitting was needed, or `Some(rest)` if
    /// `rest` was split off.
    ///
    /// Interval is in "base units".  Generally implements a maximum size.
    ///
    /// # Invariants:
    /// - If one or the other input is empty, then no split.
    /// - If either input satisfies `is_ok_child`, then, on return, `self`
    ///   satisfies this, as does the optional split.
    fn push_maybe_split(&mut self, other: &Self, iv: Interval) -> Option<Self>;

    /// Same meaning as push_maybe_split starting from an empty
    /// leaf, but maybe can be implemented more efficiently?
    fn subseq(&self, iv: Interval) -> Self {
        let mut result = Self::default();
        let split = result.push_maybe_split(self, iv);
        debug_assert!(split.is_none(), "{}", TreeInvariantError::UnexpectedLeafSplit);
        result
    }
}

/// A b-tree node storing leaves at the bottom, and with info
/// retained at each node. It is implemented with atomic reference counting
/// and copy-on-write semantics, so an immutable clone is a very cheap
/// operation, and nodes can be shared across threads. Even so, it is
/// designed to be updated in place, with efficiency similar to a mutable
/// data structure, using uniqueness of reference count to detect when
/// this operation is safe.
///
/// When the leaf is a string, this is a rope data structure (a persistent
/// rope in functional programming jargon). However, it is not restricted
/// to strings, and it is expected to be the basis for a number of data
/// structures useful for text processing.
#[derive(Clone)]
pub struct Node<N: NodeInfo>(Arc<NodeBody<N>>);

#[derive(Clone)]
struct NodeBody<N: NodeInfo> {
    height: usize,
    len: usize,
    info: N,
    val: NodeVal<N>,
}

#[derive(Clone)]
enum NodeVal<N: NodeInfo> {
    Leaf(N::L),
    Internal(Vec<Node<N>>),
}

// also consider making Metric a newtype for usize, so type system can
// help separate metrics

/// A trait for quickly processing attributes of a
/// [NodeInfo](struct.NodeInfo.html).
///
/// For the conceptual background see the
/// [blog post, Rope science, part 2: metrics](https://github.com/google/xi-editor/blob/master/docs/docs/rope_science_02.md).
pub trait Metric<N: NodeInfo> {
    /// Return the size of the
    /// [NodeInfo::L](trait.NodeInfo.html#associatedtype.L), as measured by this
    /// metric.
    ///
    /// The usize argument is the total size/length of the node, in base units.
    ///
    /// # Examples
    /// For the [LinesMetric](../rope/struct.LinesMetric.html), this gives the number of
    /// lines in string contained in the leaf. For the
    /// [BaseMetric](../rope/struct.BaseMetric.html), this gives the size of the string
    /// in uft8 code units, that is, bytes.
    ///
    fn measure(info: &N, len: usize) -> usize;

    /// Returns the smallest offset, in base units, for an offset in measured units.
    ///
    /// # Invariants:
    ///
    /// - `from_base_units(to_base_units(x)) == x` is True for valid `x`
    fn to_base_units(l: &N::L, in_measured_units: usize) -> usize;

    /// Returns the smallest offset in measured units corresponding to an offset in base units.
    ///
    /// # Invariants:
    ///
    /// - `from_base_units(to_base_units(x)) == x` is True for valid `x`
    fn from_base_units(l: &N::L, in_base_units: usize) -> usize;

    /// Return whether the offset in base units is a boundary of this metric.
    /// If a boundary is at end of a leaf then this method must return true.
    /// However, a boundary at the beginning of a leaf is optional
    /// (the previous leaf will be queried).
    fn is_boundary(l: &N::L, offset: usize) -> bool;

    /// Returns the index of the boundary directly preceding offset,
    /// or None if no such boundary exists. Input and result are in base units.
    fn prev(l: &N::L, offset: usize) -> Option<usize>;

    /// Returns the index of the first boundary for which index > offset,
    /// or None if no such boundary exists. Input and result are in base units.
    fn next(l: &N::L, offset: usize) -> Option<usize>;

    /// Returns true if the measured units in this metric can span multiple
    /// leaves.  As an example, in a metric that measures lines in a rope, a
    /// line may start in one leaf and end in another; however in a metric
    /// measuring bytes, storage of a single byte cannot extend across leaves.
    fn can_fragment() -> bool;
}

pub use builder::TreeBuilder;
pub use cursor::{Cursor, CursorIter};

mod builder;
mod cursor;
mod node;

#[cfg(test)]
mod test;
