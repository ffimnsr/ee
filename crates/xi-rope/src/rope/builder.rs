//! Rope and tree builders.
use super::metrics::{clamp_to_char_boundary, find_leaf_split_for_bulk, find_leaf_split_for_merge};
use super::*;

/// Streaming builder for linear rope construction.
pub struct RopeBuilder {
    pub(crate) tree: TreeBuilder<RopeInfo>,
    pub(crate) pending: String,
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

    pub(crate) fn flush_full_leaf(&mut self) {
        let splitpoint = find_leaf_split_for_merge(&self.pending);
        let remainder = self.pending.split_off(splitpoint);
        let leaf = std::mem::replace(&mut self.pending, remainder);
        self.tree.push_leaf(leaf);
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
