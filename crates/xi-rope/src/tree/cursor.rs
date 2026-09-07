//! Cursor navigation, leaf descent, and cursor iterators.
use super::*;

pub(crate) const CURSOR_CACHE_SIZE: usize = 4;

/// A data structure for traversing boundaries in a tree.
///
/// It is designed to be efficient both for random access and for iteration. The
/// cursor itself is agnostic to which [`Metric`] is used to determine boundaries, but
/// the methods to find boundaries are parametrized on the [`Metric`].
///
/// A cursor can be valid or invalid. It is always valid when created or after
/// [`set`](#method.set) is called, and becomes invalid after [`prev`](#method.prev)
/// or [`next`](#method.next) fails to find a boundary.
///
/// [`Metric`]: struct.Metric.html
pub struct Cursor<'a, N: 'a + NodeInfo> {
    /// The tree being traversed by this cursor.
    pub(crate) root: &'a Node<N>,
    /// The current position of the cursor.
    ///
    /// It is always less than or equal to the tree length.
    pub(crate) position: usize,
    /// The cache holds the tail of the path from the root to the current leaf.
    ///
    /// Each entry is a reference to the parent node and the index of the child. It
    /// is stored bottom-up; `cache[0]` is the parent of the leaf and the index of
    /// the leaf within that parent.
    ///
    /// The main motivation for this being a fixed-size array is to keep the cursor
    /// an allocation-free data structure.
    pub(crate) cache: [Option<(&'a Node<N>, usize)>; CURSOR_CACHE_SIZE],
    /// The leaf containing the current position, when the cursor is valid.
    ///
    /// The position is only at the end of the leaf when it is at the end of the tree.
    pub(crate) leaf: Option<&'a N::L>,
    /// The offset of `leaf` within the tree.
    pub(crate) offset_of_leaf: usize,
}

impl<'a, N: NodeInfo> Cursor<'a, N> {
    /// Create a new cursor at the given position.
    pub fn new(n: &'a Node<N>, position: usize) -> Cursor<'a, N> {
        let mut result = Cursor {
            root: n,
            position,
            cache: [None; CURSOR_CACHE_SIZE],
            leaf: None,
            offset_of_leaf: 0,
        };
        result.descend();
        result
    }

    /// The length of the tree.
    pub fn total_len(&self) -> usize {
        self.root.len()
    }

    /// Return a reference to the root node of the tree.
    pub fn root(&self) -> &'a Node<N> {
        self.root
    }

    /// Get the current leaf of the cursor.
    ///
    /// If the cursor is valid, returns the leaf containing the current position,
    /// and the offset of the current position within the leaf. That offset is equal
    /// to the leaf length only at the end, otherwise it is less than the leaf length.
    pub fn get_leaf(&self) -> Option<(&'a N::L, usize)> {
        self.leaf.map(|l| (l, self.position - self.offset_of_leaf))
    }

    /// Returns the base-unit offset at the start of the current leaf.
    pub fn leaf_start_offset(&self) -> Option<usize> {
        self.leaf.map(|_| self.offset_of_leaf)
    }

    /// Returns the accumulated measure at the start of the current leaf.
    pub fn leaf_start_measure<M: Metric<N>>(&self) -> Option<usize> {
        self.leaf?;
        let mut measure = 0;
        for (node, child_idx) in self.cache.iter().flatten() {
            let children = node.get_children();
            if *child_idx > children.len() {
                debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                return None;
            }
            for child in &children[..*child_idx] {
                measure += child.measure::<M>();
            }
        }
        Some(measure)
    }

    /// Set the position of the cursor.
    ///
    /// The cursor is valid after this call.
    ///
    /// Precondition: `position` is less than or equal to the length of the tree.
    pub fn set(&mut self, position: usize) {
        self.position = position;
        if let Some(l) = self.leaf {
            if self.position >= self.offset_of_leaf && self.position < self.offset_of_leaf + l.len()
            {
                return;
            }
            if self.position == self.offset_of_leaf + l.len()
                && self.position < self.root.len()
                && self.next_leaf().is_some()
            {
                return;
            }
            if self.position + 1 == self.offset_of_leaf && self.prev_leaf().is_some() {
                return;
            }
        }
        self.descend();
    }

    /// Set the cursor to the leaf containing the given metric position.
    pub fn set_measure<M: Metric<N>>(&mut self, measure: usize) {
        self.descend_metric::<M>(measure);
    }

    /// Get the position of the cursor.
    pub fn pos(&self) -> usize {
        self.position
    }

    /// Determine whether the current position is a boundary.
    ///
    /// Note: the beginning and end of the tree may or may not be boundaries, depending on the
    /// metric. If the metric is not `can_fragment`, then they always are.
    pub fn is_boundary<M: Metric<N>>(&mut self) -> bool {
        let Some(leaf) = self.leaf else {
            return false;
        };
        if self.position == self.offset_of_leaf && !M::can_fragment() {
            return true;
        }
        if self.position == 0 || self.position > self.offset_of_leaf {
            return M::is_boundary(leaf, self.position - self.offset_of_leaf);
        }
        self.peek_prev_leaf()
            .is_some_and(|(prev_leaf, _)| M::is_boundary(prev_leaf, prev_leaf.len()))
    }

    /// Moves the cursor to the previous boundary.
    ///
    /// When there is no previous boundary, returns `None` and the cursor becomes invalid.
    ///
    /// Return value: the position of the boundary, if it exists.
    pub fn prev<M: Metric<N>>(&mut self) -> Option<usize> {
        if self.position == 0 || self.leaf.is_none() {
            self.leaf = None;
            return None;
        }
        let orig_pos = self.position;
        let offset_in_leaf = orig_pos - self.offset_of_leaf;
        if offset_in_leaf > 0 {
            let l = self.leaf.unwrap();
            if let Some(offset_in_leaf) = M::prev(l, offset_in_leaf) {
                self.position = self.offset_of_leaf + offset_in_leaf;
                return Some(self.position);
            }
        }

        // not in same leaf, need to scan backwards
        self.prev_leaf()?;
        if let Some(offset) = self.last_inside_leaf::<M>(orig_pos) {
            return Some(offset);
        }

        // Not found in previous leaf, find using measurement.
        let measure = self.measure_leaf::<M>(self.position);
        if measure == 0 {
            self.leaf = None;
            self.position = 0;
            return None;
        }
        self.descend_metric::<M>(measure);
        self.last_inside_leaf::<M>(orig_pos)
    }

    /// Moves the cursor to the next boundary.
    ///
    /// When there is no next boundary, returns `None` and the cursor becomes invalid.
    ///
    /// Return value: the position of the boundary, if it exists.
    pub fn next<M: Metric<N>>(&mut self) -> Option<usize> {
        if self.position >= self.root.len() || self.leaf.is_none() {
            self.leaf = None;
            return None;
        }

        if let Some(offset) = self.next_inside_leaf::<M>() {
            return Some(offset);
        }

        self.next_leaf()?;
        if let Some(offset) = self.next_inside_leaf::<M>() {
            return Some(offset);
        }

        // Leaf is 0-measure (otherwise would have already succeeded).
        let measure = self.measure_leaf::<M>(self.position);
        self.descend_metric::<M>(measure + 1);
        if let Some(offset) = self.next_inside_leaf::<M>() {
            return Some(offset);
        }

        // Not found, properly invalidate cursor.
        self.position = self.root.len();
        self.leaf = None;
        None
    }

    /// Returns the current position if it is a boundary in this [`Metric`],
    /// else behaves like [`next`](#method.next).
    ///
    /// [`Metric`]: struct.Metric.html
    pub fn at_or_next<M: Metric<N>>(&mut self) -> Option<usize> {
        if self.is_boundary::<M>() { Some(self.pos()) } else { self.next::<M>() }
    }

    /// Returns the current position if it is a boundary in this [`Metric`],
    /// else behaves like [`prev`](#method.prev).
    ///
    /// [`Metric`]: struct.Metric.html
    pub fn at_or_prev<M: Metric<N>>(&mut self) -> Option<usize> {
        if self.is_boundary::<M>() { Some(self.pos()) } else { self.prev::<M>() }
    }

    /// Returns an iterator with this cursor over the given [`Metric`].
    ///
    /// # Examples:
    ///
    /// ```
    /// # use xi_rope::{Cursor, LinesMetric, Rope};
    /// #
    /// let text: Rope = "one line\ntwo line\nred line\nblue".into();
    /// let mut cursor = Cursor::new(&text, 0);
    /// let line_offsets = cursor.iter::<LinesMetric>().collect::<Vec<_>>();
    /// assert_eq!(line_offsets, vec![9, 18, 27]);
    ///
    /// ```
    /// [`Metric`]: struct.Metric.html
    pub fn iter<'c, M: Metric<N>>(&'c mut self) -> CursorIter<'c, 'a, N, M> {
        CursorIter { cursor: self, _metric: PhantomData }
    }

    /// Tries to find the last boundary in the leaf the cursor is currently in.
    ///
    /// If the last boundary is at the end of the leaf, it is only counted if
    /// it is less than `orig_pos`.
    #[inline]
    pub(crate) fn last_inside_leaf<M: Metric<N>>(&mut self, orig_pos: usize) -> Option<usize> {
        let l = self.leaf.expect("inconsistent, shouldn't get here");
        let len = l.len();
        if self.offset_of_leaf + len < orig_pos && M::is_boundary(l, len) {
            let _ = self.next_leaf();
            return Some(self.position);
        }
        let offset_in_leaf = M::prev(l, len)?;
        self.position = self.offset_of_leaf + offset_in_leaf;
        Some(self.position)
    }

    /// Tries to find the next boundary in the leaf the cursor is currently in.
    #[inline]
    pub(crate) fn next_inside_leaf<M: Metric<N>>(&mut self) -> Option<usize> {
        let l = self.leaf.expect("inconsistent, shouldn't get here");
        let offset_in_leaf = self.position - self.offset_of_leaf;
        let offset_in_leaf = M::next(l, offset_in_leaf)?;
        if offset_in_leaf == l.len() && self.offset_of_leaf + offset_in_leaf != self.root.len() {
            let _ = self.next_leaf();
        } else {
            self.position = self.offset_of_leaf + offset_in_leaf;
        }
        Some(self.position)
    }

    /// Move to beginning of next leaf.
    ///
    /// Return value: same as [`get_leaf`](#method.get_leaf).
    pub fn next_leaf(&mut self) -> Option<(&'a N::L, usize)> {
        let leaf = self.leaf?;
        self.position = self.offset_of_leaf + leaf.len();
        for i in 0..CURSOR_CACHE_SIZE {
            let Some((node, j)) = self.cache[i] else {
                self.leaf = None;
                return None;
            };
            let children = node.get_children();
            if j + 1 < children.len() {
                self.cache[i] = Some((node, j + 1));
                let mut node_down = &children[j + 1];
                for k in (0..i).rev() {
                    let node_children = node_down.get_children();
                    if node_children.is_empty() {
                        debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                        self.leaf = None;
                        return None;
                    }
                    self.cache[k] = Some((node_down, 0));
                    node_down = &node_children[0];
                }
                self.leaf = Some(node_down.get_leaf());
                self.offset_of_leaf = self.position;
                return self.get_leaf();
            }
        }
        if self.offset_of_leaf + leaf.len() == self.root.len() {
            self.leaf = None;
            return None;
        }
        self.descend();
        self.get_leaf()
    }

    /// Move to beginning of previous leaf.
    ///
    /// Return value: same as [`get_leaf`](#method.get_leaf).
    pub fn prev_leaf(&mut self) -> Option<(&'a N::L, usize)> {
        if self.offset_of_leaf == 0 {
            self.leaf = None;
            self.position = 0;
            return None;
        }
        for i in 0..CURSOR_CACHE_SIZE {
            if self.cache[i].is_none() {
                // this probably can't happen
                self.leaf = None;
                return None;
            }
            let (node, j) = self.cache[i].unwrap();
            if j > 0 {
                self.cache[i] = Some((node, j - 1));
                let children = node.get_children();
                if children.is_empty() {
                    debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                    self.leaf = None;
                    return None;
                }
                let mut node_down = &children[j - 1];
                for k in (0..i).rev() {
                    let node_children = node_down.get_children();
                    if node_children.is_empty() {
                        debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                        self.leaf = None;
                        return None;
                    }
                    let last_ix = node_children.len() - 1;
                    self.cache[k] = Some((node_down, last_ix));
                    node_down = &node_children[last_ix];
                }
                let leaf = node_down.get_leaf();
                self.leaf = Some(leaf);
                self.offset_of_leaf -= leaf.len();
                self.position = self.offset_of_leaf;
                return self.get_leaf();
            }
        }
        self.position = self.offset_of_leaf - 1;
        self.descend();
        self.position = self.offset_of_leaf;
        self.get_leaf()
    }

    pub(crate) fn peek_prev_leaf(&self) -> Option<(&'a N::L, usize)> {
        if self.offset_of_leaf == 0 {
            return None;
        }
        for i in 0..CURSOR_CACHE_SIZE {
            let Some((node, j)) = self.cache[i] else {
                break;
            };
            if j > 0 {
                let children = node.get_children();
                if children.is_empty() {
                    debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                    return None;
                }
                let mut node_down = &children[j - 1];
                for _ in (0..i).rev() {
                    let node_children = node_down.get_children();
                    if node_children.is_empty() {
                        debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                        return None;
                    }
                    node_down = &node_children[node_children.len() - 1];
                }
                let leaf = node_down.get_leaf();
                return Some((leaf, self.offset_of_leaf - leaf.len()));
            }
        }
        self.leaf_at(self.offset_of_leaf - 1)
    }

    pub(crate) fn leaf_at(&self, position: usize) -> Option<(&'a N::L, usize)> {
        let mut node = self.root;
        let mut offset = 0;
        while node.height() > 0 {
            let children = node.get_children();
            if children.is_empty() {
                debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                return None;
            }
            let mut i = 0;
            loop {
                if i + 1 == children.len() {
                    break;
                }
                let nextoff = offset + children[i].len();
                if nextoff > position {
                    break;
                }
                offset = nextoff;
                i += 1;
            }
            node = &children[i];
        }
        Some((node.get_leaf(), offset))
    }

    /// Go to the leaf containing the current position.
    ///
    /// Sets `leaf` to the leaf containing `position`, and updates `cache` and
    /// `offset_of_leaf` to be consistent.
    pub(crate) fn descend(&mut self) {
        let mut node = self.root;
        let mut offset = 0;
        while node.height() > 0 {
            let children = node.get_children();
            if children.is_empty() {
                debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                self.leaf = None;
                return;
            }
            let mut i = 0;
            loop {
                if i + 1 == children.len() {
                    break;
                }
                let nextoff = offset + children[i].len();
                if nextoff > self.position {
                    break;
                }
                offset = nextoff;
                i += 1;
            }
            let cache_ix = node.height() - 1;
            if cache_ix < CURSOR_CACHE_SIZE {
                self.cache[cache_ix] = Some((node, i));
            }
            node = &children[i];
        }
        self.leaf = Some(node.get_leaf());
        self.offset_of_leaf = offset;
    }

    /// Returns the measure at the beginning of the leaf containing `pos`.
    ///
    /// This method is O(log n) no matter the current cursor state.
    pub(crate) fn measure_leaf<M: Metric<N>>(&self, mut pos: usize) -> usize {
        let mut node = self.root;
        let mut metric = 0;
        while node.height() > 0 {
            let children = node.get_children();
            if children.is_empty() {
                debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                return metric;
            }
            for child in children {
                let len = child.len();
                if pos < len {
                    node = child;
                    break;
                }
                pos -= len;
                metric += child.measure::<M>();
            }
        }
        metric
    }

    /// Find the leaf having the given measure.
    ///
    /// This function sets `self.position` to the beginning of the leaf
    /// containing the smallest offset with the given metric, and also updates
    /// state as if [`descend`](#method.descend) was called.
    ///
    /// If `measure` is greater than the measure of the whole tree, then moves
    /// to the last node.
    pub(crate) fn descend_metric<M: Metric<N>>(&mut self, mut measure: usize) {
        let mut node = self.root;
        let mut offset = 0;
        while node.height() > 0 {
            let children = node.get_children();
            if children.is_empty() {
                debug_assert!(false, "{}", TreeInvariantError::InternalNodeWithoutChildren);
                self.leaf = None;
                return;
            }
            let mut i = 0;
            loop {
                if i + 1 == children.len() {
                    break;
                }
                let child = &children[i];
                let child_m = child.measure::<M>();
                if child_m >= measure {
                    break;
                }
                offset += child.len();
                measure -= child_m;
                i += 1;
            }
            let cache_ix = node.height() - 1;
            if cache_ix < CURSOR_CACHE_SIZE {
                self.cache[cache_ix] = Some((node, i));
            }
            node = &children[i];
        }
        self.leaf = Some(node.get_leaf());
        self.position = offset;
        self.offset_of_leaf = offset;
    }
}

/// An iterator generated by a [`Cursor`], for some [`Metric`].
///
/// [`Cursor`]: struct.Cursor.html
/// [`Metric`]: struct.Metric.html
pub struct CursorIter<'c, 'a: 'c, N: 'a + NodeInfo, M: 'a + Metric<N>> {
    pub(crate) cursor: &'c mut Cursor<'a, N>,
    pub(crate) _metric: PhantomData<&'a M>,
}

impl<'c, 'a, N: NodeInfo, M: Metric<N>> Iterator for CursorIter<'c, 'a, N, M> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        self.cursor.next::<M>()
    }
}

impl<'c, 'a, N: NodeInfo, M: Metric<N>> CursorIter<'c, 'a, N, M> {
    /// Returns the current position of the underlying [`Cursor`].
    ///
    /// [`Cursor`]: struct.Cursor.html
    pub fn pos(&self) -> usize {
        self.cursor.pos()
    }
}
