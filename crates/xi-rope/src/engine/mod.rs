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

//! An engine for handling edits (possibly from async sources) and undo. It
//! conceptually represents the current text and all edit history for that
//! text.
//!
//! This module actually implements a mini Conflict-free Replicated Data Type
//! under `Engine::edit_rev`, which is considerably simpler than the usual
//! CRDT implementation techniques, because all operations are serialized in
//! this central engine. It provides the ability to apply edits that depend on
//! a previously committed version of the text rather than the current text,
//! which is sufficient for asynchronous plugins that can only have one
//! pending edit in flight each.
//!
//! There is also a full CRDT merge operation implemented under
//! `Engine::merge`, which is more powerful but considerably more complex.
//! It enables support for full asynchronous and even peer-to-peer editing.

pub(crate) use im::OrdSet;
#[cfg(feature = "serde")]
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use std::borrow::Borrow;
pub(crate) use std::borrow::Cow;
pub(crate) use std::collections::BTreeSet;
pub(crate) use std::collections::HashSet;
pub(crate) use std::collections::hash_map::DefaultHasher;

pub(crate) use crate::delta::{Delta, InsertDelta};
pub(crate) use crate::interval::Interval;
pub(crate) use crate::multiset::{CountMatcher, Subset};
pub(crate) use crate::rope::{Rope, RopeInfo};

/// Represents the current state of a document and all of its history
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Engine {
    /// The session ID used to create new `RevId`s for edits made on this device
    #[cfg_attr(feature = "serde", serde(default = "default_session", skip_serializing))]
    session: SessionId,
    /// The incrementing revision number counter for this session used for `RevId`s
    #[cfg_attr(feature = "serde", serde(default = "initial_revision_counter", skip_serializing))]
    rev_id_counter: u32,
    /// The current contents of the document as would be displayed on screen
    text: Rope,
    /// Storage for all the characters that have been deleted  but could
    /// return if a delete is un-done or an insert is re- done.
    tombstones: Rope,
    /// Imagine a "union string" that contained all the characters ever
    /// inserted, including the ones that were later deleted, in the locations
    /// they would be if they hadn't been deleted.
    ///
    /// This is a `Subset` of the "union string" representing the characters
    /// that are currently deleted, and thus in `tombstones` rather than
    /// `text`. The count of a character in `deletes_from_union` represents
    /// how many times it has been deleted, so if a character is deleted twice
    /// concurrently it will have count `2` so that undoing one delete but not
    /// the other doesn't make it re-appear.
    ///
    /// You could construct the "union string" from `text`, `tombstones` and
    /// `deletes_from_union` by splicing a segment of `tombstones` into `text`
    /// wherever there's a non-zero-count segment in `deletes_from_union`.
    deletes_from_union: Subset,
    /// Persistent set of undo-group ids currently toggled off.
    undone_groups: OrdSet<usize>,
    /// The revision history of the document
    revs: Vec<Revision>,
}

// The advantage of using a session ID over random numbers is that it can be
// easily delta-compressed later.
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RevId {
    // 96 bits has a 10^(-12) chance of collision with 400 million sessions and 10^(-6) with 100 billion.
    // `session1==session2==0` is reserved for initialization which is the same on all sessions.
    // A colliding session will break merge invariants and the document will start crashing Xi.
    session1: u64,
    // if this was a tuple field instead of two fields, alignment padding would add 8 more bytes.
    session2: u32,
    // There will probably never be a document with more than 4 billion edits
    // in a single session.
    num: u32,
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
struct Revision {
    /// This uniquely represents the identity of this revision and it stays
    /// the same even if it is rebased or merged between devices.
    rev_id: RevId,
    /// The largest undo group number of any edit in the history up to this
    /// point. Used to optimize undo to not look further back.
    max_undo_so_far: usize,
    edit: Contents,
}

/// Valid within a session. If there's a collision the most recent matching
/// Revision will be used, which means only the (small) set of concurrent edits
/// could trigger incorrect behavior if they collide, so u64 is safe.
pub type RevToken = u64;

/// the session ID component of a `RevId`
pub type SessionId = (u64, u32);

/// Type for errors that occur during CRDT operations.
#[derive(Clone)]
pub enum Error {
    /// An edit specified a revision that did not exist. The revision may
    /// have been GC'd, or it may have specified incorrectly.
    MissingRevision(RevToken),
    /// A delta was applied which had a `base_len` that did not match the length
    /// of the revision it was applied to.
    MalformedDelta { rev_len: usize, delta_len: usize },
}

#[derive(Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
struct FullPriority {
    priority: usize,
    session_id: SessionId,
}

use self::Contents::*;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
enum Contents {
    Edit {
        /// Used to order concurrent inserts, for example auto-indentation
        /// should go before typed text.
        priority: usize,
        /// Groups related edits together so that they are undone and re-done
        /// together. For example, an auto-indent insertion would be un-done
        /// along with the newline that triggered it.
        undo_group: usize,
        /// The subset of the characters of the union string from after this
        /// revision that were added by this revision.
        inserts: Subset,
        /// The subset of the characters of the union string from after this
        /// revision that were deleted by this revision.
        deletes: Subset,
    },
    Undo {
        /// The set of groups toggled between undone and done.
        /// Just the `symmetric_difference` (XOR) of the two sets.
        toggled_groups: OrdSet<usize>, // set of undo_group id's
        /// Used to store a reversible difference between the old
        /// and new deletes_from_union
        deletes_bitxor: Subset,
    },
}

/// for single user cases, used by serde and ::empty
fn default_session() -> (u64, u32) {
    (1, 0)
}

/// Revision 0 is always an Undo of the empty set of groups
#[cfg(feature = "serde")]
fn initial_revision_counter() -> u32 {
    1
}

impl RevId {
    /// Returns a u64 that will be equal for equivalent revision IDs and
    /// should be as unlikely to collide as two random u64s.
    pub fn token(&self) -> RevToken {
        use std::hash::{Hash, Hasher};
        // Rust is unlikely to break the property that this hash is strongly collision-resistant
        // and it only needs to be consistent over one execution.
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }

    pub fn session_id(&self) -> SessionId {
        (self.session1, self.session2)
    }
}
fn shuffle_tombstones(
    text: &Rope,
    tombstones: &Rope,
    old_deletes_from_union: &Subset,
    new_deletes_from_union: &Subset,
) -> Rope {
    // Taking the complement of deletes_from_union leads to an interleaving valid for swapped text and tombstones,
    // allowing us to use the same method to insert the text into the tombstones.
    let inverse_tombstones_map = old_deletes_from_union.complement();
    let move_delta =
        Delta::synthesize(text, &inverse_tombstones_map, &new_deletes_from_union.complement());
    move_delta.apply(tombstones)
}

/// Move sections from text to tombstones and vice versa based on a new and old set of deletions.
/// Returns a tuple of a new text `Rope` and a new `Tombstones` rope described by `new_deletes_from_union`.
fn shuffle(
    text: &Rope,
    tombstones: &Rope,
    old_deletes_from_union: &Subset,
    new_deletes_from_union: &Subset,
) -> (Rope, Rope) {
    // Delta that deletes the right bits from the text
    let del_delta = Delta::synthesize(tombstones, old_deletes_from_union, new_deletes_from_union);
    let new_text = del_delta.apply(text);
    // println!("shuffle: old={:?} new={:?} old_text={:?} new_text={:?} old_tombstones={:?}",
    //     old_deletes_from_union, new_deletes_from_union, text, new_text, tombstones);
    (new_text, shuffle_tombstones(text, tombstones, old_deletes_from_union, new_deletes_from_union))
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::MissingRevision(_) => write!(f, "Revision not found"),
            Error::MalformedDelta { delta_len, rev_len } => {
                write!(f, "Delta base_len {} does not match revision length {}", delta_len, rev_len)
            }
        }
    }
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for Error {}

mod core;
mod edit;
mod gc;
mod merge;
mod undo;

#[cfg(test)]
#[rustfmt::skip]
mod tests;
