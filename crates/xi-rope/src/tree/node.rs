//! Node construction, merge, edit, and metrics.
use super::*;

impl<N: NodeInfo> Node<N> {
    pub fn from_leaf(l: N::L) -> Node<N> {
        let len = l.len();
        let info = N::compute_info(&l);
        Node(Arc::new(NodeBody { height: 0, len, info, val: NodeVal::Leaf(l) }))
    }

    /// Create a node from a vec of nodes.
    ///
    /// The input must satisfy the following balancing requirements:
    /// * The length of `nodes` must be <= MAX_CHILDREN and > 1.
    /// * All the nodes are the same height.
    /// * All the nodes must satisfy is_ok_child.
    pub(crate) fn from_nodes(nodes: Vec<Node<N>>) -> Node<N> {
        debug_assert!(nodes.len() > 1);
        debug_assert!(nodes.len() <= MAX_CHILDREN);
        let height = nodes[0].0.height + 1;
        let mut len = nodes[0].0.len;
        let mut info = nodes[0].0.info.clone();
        debug_assert!(nodes[0].is_ok_child());
        for child in &nodes[1..] {
            debug_assert_eq!(child.height() + 1, height);
            debug_assert!(child.is_ok_child());
            len += child.0.len;
            info.accumulate(&child.0.info);
        }
        Node(Arc::new(NodeBody { height, len, info, val: NodeVal::Internal(nodes) }))
    }

    pub fn len(&self) -> usize {
        self.0.len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` if these two `Node`s share the same underlying data.
    ///
    /// This is principally intended to be used by the druid crate, without needing
    /// to actually add a feature and implement druid's `Data` trait.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn height(&self) -> usize {
        self.0.height
    }

    pub(crate) fn is_leaf(&self) -> bool {
        self.0.height == 0
    }

    pub(crate) fn interval(&self) -> Interval {
        self.0.info.interval(self.0.len)
    }

    pub(crate) fn get_children(&self) -> &[Node<N>] {
        if let NodeVal::Internal(ref v) = self.0.val {
            v
        } else {
            debug_assert!(false, "{}", TreeInvariantError::GetChildrenOnLeaf);
            // Keep release builds on a checked path after an invariant violation.
            // Callers that index children still guard against this empty fallback.
            &[]
        }
    }

    pub(crate) fn get_leaf(&self) -> &N::L {
        if let NodeVal::Leaf(ref l) = self.0.val {
            l
        } else {
            debug_assert!(false, "{}", TreeInvariantError::GetLeafOnInternal);
            // Fall back to left-most descendant leaf so callers can keep returning
            // a valid reference without panicking in release builds.
            let children = self.get_children();
            children[0].get_leaf()
        }
    }

    /// Call a callback with a mutable reference to a leaf.
    ///
    /// This clones the leaf if the reference is shared. It also recomputes
    /// length and info after the leaf is mutated.
    pub(crate) fn try_with_leaf_mut<T>(&mut self, f: impl FnOnce(&mut N::L) -> T) -> Option<T> {
        let inner = Arc::make_mut(&mut self.0);
        if let NodeVal::Leaf(ref mut l) = inner.val {
            let result = f(l);
            inner.len = l.len();
            inner.info = N::compute_info(l);
            Some(result)
        } else {
            debug_assert!(false, "{}", TreeInvariantError::WithLeafMutOnInternal);
            None
        }
    }

    pub(crate) fn is_ok_child(&self) -> bool {
        match self.0.val {
            NodeVal::Leaf(ref l) => l.is_ok_child(),
            NodeVal::Internal(ref nodes) => nodes.len() >= MIN_CHILDREN,
        }
    }

    pub(crate) fn merge_nodes(children1: &[Node<N>], children2: &[Node<N>]) -> Node<N> {
        let n_children = children1.len() + children2.len();
        if n_children <= MAX_CHILDREN {
            Node::from_nodes([children1, children2].concat())
        } else {
            // Note: this leans left. Splitting at midpoint is also an option
            let splitpoint = min(MAX_CHILDREN, n_children - MIN_CHILDREN);
            let mut iter = children1.iter().chain(children2.iter()).cloned();
            let left = iter.by_ref().take(splitpoint).collect();
            let right = iter.collect();
            let parent_nodes = vec![Node::from_nodes(left), Node::from_nodes(right)];
            Node::from_nodes(parent_nodes)
        }
    }

    pub(crate) fn merge_leaves(mut rope1: Node<N>, rope2: Node<N>) -> Node<N> {
        debug_assert!(rope1.is_leaf() && rope2.is_leaf());

        let both_ok = rope1.get_leaf().is_ok_child() && rope2.get_leaf().is_ok_child();
        if both_ok {
            return Node::from_nodes(vec![rope1, rope2]);
        }
        let res = {
            let node1 = Arc::make_mut(&mut rope1.0);
            let leaf2 = rope2.get_leaf();
            if let NodeVal::Leaf(ref mut leaf1) = node1.val {
                let leaf2_iv = Interval::new(0, leaf2.len());
                let new = leaf1.push_maybe_split(leaf2, leaf2_iv);
                node1.len = leaf1.len();
                node1.info = N::compute_info(leaf1);
                new
            } else {
                debug_assert!(false, "{}", TreeInvariantError::MergeLeavesOnNonLeaf);
                None
            }
        };
        match res {
            Some(new) => Node::from_nodes(vec![rope1, Node::from_leaf(new)]),
            None => rope1,
        }
    }

    pub fn concat(rope1: Node<N>, rope2: Node<N>) -> Node<N> {
        let h1 = rope1.height();
        let h2 = rope2.height();

        match h1.cmp(&h2) {
            Ordering::Less => {
                let children2 = rope2.get_children();
                if h1 == h2 - 1 && rope1.is_ok_child() {
                    return Node::merge_nodes(&[rope1], children2);
                }
                let newrope = Node::concat(rope1, children2[0].clone());
                if newrope.height() == h2 - 1 {
                    Node::merge_nodes(&[newrope], &children2[1..])
                } else {
                    Node::merge_nodes(newrope.get_children(), &children2[1..])
                }
            }
            Ordering::Equal => {
                if rope1.is_ok_child() && rope2.is_ok_child() {
                    return Node::from_nodes(vec![rope1, rope2]);
                }
                if h1 == 0 {
                    return Node::merge_leaves(rope1, rope2);
                }
                Node::merge_nodes(rope1.get_children(), rope2.get_children())
            }
            Ordering::Greater => {
                let children1 = rope1.get_children();
                if h2 == h1 - 1 && rope2.is_ok_child() {
                    return Node::merge_nodes(children1, &[rope2]);
                }
                let lastix = children1.len() - 1;
                let newrope = Node::concat(children1[lastix].clone(), rope2);
                if newrope.height() == h1 - 1 {
                    Node::merge_nodes(&children1[..lastix], &[newrope])
                } else {
                    Node::merge_nodes(&children1[..lastix], newrope.get_children())
                }
            }
        }
    }

    pub fn measure<M: Metric<N>>(&self) -> usize {
        M::measure(&self.0.info, self.0.len)
    }

    pub fn subseq<T: IntervalBounds>(&self, iv: T) -> Node<N> {
        let iv = iv.into_interval(self.len());
        let mut b = TreeBuilder::new();
        b.push_slice(self, iv);
        b.build()
    }

    pub fn edit<T, IV>(&mut self, iv: IV, new: T)
    where
        T: Into<Node<N>>,
        IV: IntervalBounds,
    {
        let mut b = TreeBuilder::new();
        let iv = iv.into_interval(self.len());
        let self_iv = self.interval();
        b.push_slice(self, self_iv.prefix(iv));
        b.push(new.into());
        b.push_slice(self, self_iv.suffix(iv));
        *self = b.build();
    }

    // doesn't deal with endpoint, handle that specially if you need it
    pub fn convert_metrics<M1: Metric<N>, M2: Metric<N>>(&self, mut m1: usize) -> usize {
        if m1 == 0 {
            return 0;
        }
        // If M1 can fragment, then we must land on the leaf containing
        // the m1 boundary. Otherwise, we can land on the beginning of
        // the leaf immediately following the M1 boundary, which may be
        // more efficient.
        let m1_fudge = if M1::can_fragment() { 1 } else { 0 };
        let mut m2 = 0;
        let mut node = self;
        while node.height() > 0 {
            for child in node.get_children() {
                let child_m1 = child.measure::<M1>();
                if m1 < child_m1 + m1_fudge {
                    node = child;
                    break;
                }
                m2 += child.measure::<M2>();
                m1 -= child_m1;
            }
        }
        let l = node.get_leaf();
        let base = M1::to_base_units(l, m1);
        m2 + M2::from_base_units(l, base)
    }
}

impl<N: DefaultMetric> Node<N> {
    /// Measures the length of the text bounded by ``DefaultMetric::measure(offset)`` with another metric.
    ///
    /// # Examples
    /// ```
    /// use crate::xi_rope::{Rope, LinesMetric};
    ///
    /// // the default metric of Rope is BaseMetric (aka number of bytes)
    /// let my_rope = Rope::from("first line \n second line \n");
    ///
    /// // count the number of lines in my_rope
    /// let num_lines = my_rope.count::<LinesMetric>(my_rope.len());
    /// assert_eq!(2, num_lines);
    /// ```
    pub fn count<M: Metric<N>>(&self, offset: usize) -> usize {
        self.convert_metrics::<N::DefaultMetric, M>(offset)
    }

    /// Measures the length of the text bounded by ``M::measure(offset)`` with the default metric.
    ///
    /// # Examples
    /// ```
    /// use crate::xi_rope::{Rope, LinesMetric};
    ///
    /// // the default metric of Rope is BaseMetric (aka number of bytes)
    /// let my_rope = Rope::from("first line \n second line \n");
    ///
    /// // get the byte offset of the line at index 1
    /// let byte_offset = my_rope.count_base_units::<LinesMetric>(1);
    /// assert_eq!(12, byte_offset);
    /// ```
    pub fn count_base_units<M: Metric<N>>(&self, offset: usize) -> usize {
        self.convert_metrics::<M, N::DefaultMetric>(offset)
    }
}

impl<N: NodeInfo> Default for Node<N> {
    fn default() -> Node<N> {
        Node::from_leaf(N::L::default())
    }
}
