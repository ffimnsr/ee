//! Sequential delta builder.
use super::*;

///
/// Note that all edit operations must be sorted; the start point of each
/// interval must be no less than the end point of the previous one.
pub struct Builder<N: NodeInfo> {
    pub(crate) delta: Delta<N>,
    pub(crate) last_offset: usize,
}

impl<N: NodeInfo> Builder<N> {
    /// Creates a new builder, applicable to a base rope of length `base_len`.
    pub fn new(base_len: usize) -> Builder<N> {
        Builder { delta: Delta { els: Vec::new(), base_len }, last_offset: 0 }
    }

    /// Deletes the given interval. Panics if interval is not properly sorted.
    pub fn delete<T: IntervalBounds>(&mut self, interval: T) {
        let interval = interval.into_interval(self.delta.base_len);
        let (start, end) = interval.start_end();
        assert!(start >= self.last_offset, "Delta builder: intervals not properly sorted");
        if start > self.last_offset {
            self.delta.els.push(DeltaElement::Copy(self.last_offset, start));
        }
        self.last_offset = end;
    }

    /// Replaces the given interval with the new rope. Panics if interval
    /// is not properly sorted.
    pub fn replace<T: IntervalBounds>(&mut self, interval: T, rope: Node<N>) {
        self.delete(interval);
        if !rope.is_empty() {
            self.delta.els.push(DeltaElement::Insert(rope));
        }
    }

    /// Determines if delta would be a no-op transformation if built.
    pub fn is_empty(&self) -> bool {
        self.last_offset == 0 && self.delta.els.is_empty()
    }

    /// Builds the `Delta`.
    pub fn build(mut self) -> Delta<N> {
        if self.last_offset < self.delta.base_len {
            self.delta.els.push(DeltaElement::Copy(self.last_offset, self.delta.base_len));
        }
        self.delta
    }
}
