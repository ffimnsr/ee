//! Tree builder with leaf-merge stickiness.
use super::*;

/// A builder for creating new trees.
pub struct TreeBuilder<N: NodeInfo> {
    // A stack of partially built trees. These are kept in order of
    // strictly descending height, and all vectors have a length less
    // than MAX_CHILDREN and greater than zero.
    //
    // In addition, there is a balancing invariant: for each vector
    // of length greater than one, all elements satisfy `is_ok_child`.
    pub(crate) stack: Vec<Vec<Node<N>>>,
}

impl<N: NodeInfo> TreeBuilder<N> {
    /// A new, empty builder.
    pub fn new() -> TreeBuilder<N> {
        TreeBuilder { stack: Vec::new() }
    }

    /// Append a node to the tree being built.
    pub fn push(&mut self, mut n: Node<N>) {
        loop {
            let ord = if let Some(last) = self.stack.last() {
                last[0].height().cmp(&n.height())
            } else {
                Ordering::Greater
            };
            match ord {
                Ordering::Less => {
                    n = Node::concat(self.pop(), n);
                }
                Ordering::Equal => {
                    let Some(tos) = self.stack.last_mut() else {
                        debug_assert!(false, "{}", TreeInvariantError::EmptyBuilderStack);
                        self.stack.push(vec![n]);
                        break;
                    };
                    if tos.last().is_some_and(|node| node.is_ok_child()) && n.is_ok_child() {
                        tos.push(n);
                    } else if n.height() == 0 {
                        let iv = Interval::new(0, n.len());
                        let Some(last) = tos.last_mut() else {
                            debug_assert!(false, "{}", TreeInvariantError::EmptyBuilderLevel);
                            tos.push(n);
                            break;
                        };
                        let Some(new_leaf) =
                            last.try_with_leaf_mut(|l| l.push_maybe_split(n.get_leaf(), iv))
                        else {
                            tos.push(n);
                            break;
                        };
                        if let Some(new_leaf) = new_leaf {
                            tos.push(Node::from_leaf(new_leaf));
                        }
                    } else {
                        let Some(last) = tos.pop() else {
                            debug_assert!(false, "{}", TreeInvariantError::EmptyBuilderLevel);
                            tos.push(n);
                            break;
                        };
                        let children1 = last.get_children();
                        let children2 = n.get_children();
                        if children1.is_empty() || children2.is_empty() {
                            debug_assert!(
                                false,
                                "{}",
                                TreeInvariantError::InternalNodeWithoutChildren
                            );
                            tos.push(last);
                            tos.push(n);
                            break;
                        }
                        let n_children = children1.len() + children2.len();
                        if n_children <= MAX_CHILDREN {
                            tos.push(Node::from_nodes([children1, children2].concat()));
                        } else {
                            // Note: this leans left. Splitting at midpoint is also an option
                            let splitpoint = min(MAX_CHILDREN, n_children - MIN_CHILDREN);
                            let mut iter = children1.iter().chain(children2.iter()).cloned();
                            let left = iter.by_ref().take(splitpoint).collect();
                            let right = iter.collect();
                            tos.push(Node::from_nodes(left));
                            tos.push(Node::from_nodes(right));
                        }
                    }
                    if tos.len() < MAX_CHILDREN {
                        break;
                    }
                    n = self.pop()
                }
                Ordering::Greater => {
                    self.stack.push(vec![n]);
                    break;
                }
            }
        }
    }

    /// Push a subsequence of a rope.
    ///
    /// Pushes the subsequence of another tree `n` defined by the interval `iv`
    /// onto the builder.
    ///
    /// This is intended as an efficient operation. It is equivalent to taking
    /// the subsequence of `n` and pushing that, but attempts to minimize the
    /// allocation of intermediate results.
    pub fn push_slice(&mut self, n: &Node<N>, iv: Interval) {
        if iv.is_empty() {
            return;
        }
        if iv == n.interval() {
            self.push(n.clone());
            return;
        }
        match n.0.val {
            NodeVal::Leaf(ref l) => {
                self.push_leaf_slice(l, iv);
            }
            NodeVal::Internal(ref v) => {
                let mut offset = 0;
                for child in v {
                    if iv.is_before(offset) {
                        break;
                    }
                    let child_iv = child.interval();
                    // easier just to use signed ints?
                    let rec_iv = iv.intersect(child_iv.translate(offset)).translate_neg(offset);
                    self.push_slice(child, rec_iv);
                    offset += child.len();
                }
            }
        }
    }

    /// Append a sequence of leaves.
    pub fn push_leaves(&mut self, leaves: impl IntoIterator<Item = N::L>) {
        for leaf in leaves.into_iter() {
            self.push(Node::from_leaf(leaf));
        }
    }

    /// Append a single leaf.
    pub fn push_leaf(&mut self, l: N::L) {
        self.push(Node::from_leaf(l))
    }

    /// Append a slice of a single leaf.
    pub fn push_leaf_slice(&mut self, l: &N::L, iv: Interval) {
        self.push(Node::from_leaf(l.subseq(iv)))
    }

    /// Build the final tree.
    ///
    /// The tree is the concatenation of all the nodes and leaves that have been pushed
    /// on the builder, in order.
    pub fn build(mut self) -> Node<N> {
        if self.stack.is_empty() {
            Node::from_leaf(N::L::default())
        } else {
            let mut n = self.pop();
            while !self.stack.is_empty() {
                n = Node::concat(self.pop(), n);
            }
            n
        }
    }

    /// Pop the last vec-of-nodes off the stack, resulting in a node.
    pub(crate) fn pop(&mut self) -> Node<N> {
        let Some(nodes) = self.stack.pop() else {
            debug_assert!(false, "{}", TreeInvariantError::EmptyBuilderStack);
            return Node::from_leaf(N::L::default());
        };
        match nodes.len() {
            0 => {
                debug_assert!(false, "{}", TreeInvariantError::EmptyBuilderLevel);
                Node::from_leaf(N::L::default())
            }
            1 => {
                let mut iter = nodes.into_iter();
                if let Some(node) = iter.next() {
                    node
                } else {
                    debug_assert!(false, "{}", TreeInvariantError::EmptyBuilderLevel);
                    Node::default()
                }
            }
            _ => Node::from_nodes(nodes),
        }
    }
}
