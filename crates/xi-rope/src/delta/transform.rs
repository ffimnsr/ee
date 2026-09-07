//! Insert-delta transforms and the query transformer.
use super::*;

impl<N: NodeInfo> InsertDelta<N> {
    /// Expands this insert-only delta from the document selected by `xform`
    /// into `xform`'s full coordinate space.
    ///
    /// `xform.len_after_delete()` must equal this delta's base length. Applying
    /// the returned delta to the full document produces the same visible edit
    /// as deleting `xform` first and applying `self`. When both transformations
    /// insert at one coordinate, `after` selects whether `self`'s insertion is
    /// ordered after or before `xform`'s insertion.
    pub fn transform_expand(&self, xform: &Subset, after: bool) -> InsertDelta<N> {
        let current_elements = &self.0.els;
        let mut elements = Vec::new();
        let mut source_coordinate = 0;
        let mut expanded_coordinate = 0;
        let mut element_index = 0;
        let mut copy_start = 0;
        let mut xform_ranges = xform.complement_iter();
        let mut current_xform_range = xform_ranges.next();
        let expanded_len = xform.count(CountMatcher::All);
        while expanded_coordinate < expanded_len || element_index < current_elements.len() {
            let next_range_start =
                current_xform_range.map_or(expanded_len, |(range_start, _)| range_start);
            if after && expanded_coordinate < next_range_start {
                expanded_coordinate = next_range_start;
            }
            while element_index < current_elements.len() {
                match current_elements[element_index] {
                    DeltaElement::Insert(ref node) => {
                        if expanded_coordinate > copy_start {
                            elements.push(DeltaElement::Copy(copy_start, expanded_coordinate));
                        }
                        copy_start = expanded_coordinate;
                        elements.push(DeltaElement::Insert(node.clone()));
                        element_index += 1;
                    }
                    DeltaElement::Copy(_, copy_end) => {
                        if expanded_coordinate >= next_range_start {
                            let mut next_expanded =
                                copy_end + expanded_coordinate - source_coordinate;
                            if let Some((_, range_end)) = current_xform_range {
                                next_expanded = min(next_expanded, range_end);
                            }
                            source_coordinate += next_expanded - expanded_coordinate;
                            expanded_coordinate = next_expanded;
                            if source_coordinate == copy_end {
                                element_index += 1;
                            }
                            if current_xform_range
                                .is_some_and(|(_, range_end)| expanded_coordinate == range_end)
                            {
                                current_xform_range = xform_ranges.next();
                            }
                        }
                        break;
                    }
                }
            }
            if !after && expanded_coordinate < next_range_start {
                expanded_coordinate = next_range_start;
            }
        }
        if expanded_coordinate > copy_start {
            elements.push(DeltaElement::Copy(copy_start, expanded_coordinate));
        }
        InsertDelta(Delta { els: elements, base_len: expanded_len })
    }

    /// Shrink this insert-only delta through deletion of copied regions with
    /// the same base. For example, if `self` applies to a union string and
    /// `xform` contains deletions from that union, resulting delta applies to
    /// visible text. Restricting this operation to [`InsertDelta`] guarantees
    /// inserted nodes remain valid while copied coordinates are remapped.
    pub fn transform_shrink(&self, xform: &Subset) -> InsertDelta<N> {
        let mut m = xform.mapper(CountMatcher::Zero);
        let els = self
            .0
            .els
            .iter()
            .map(|elem| match *elem {
                DeltaElement::Copy(b, e) => {
                    DeltaElement::Copy(m.doc_index_to_subset(b), m.doc_index_to_subset(e))
                }
                DeltaElement::Insert(ref n) => DeltaElement::Insert(n.clone()),
            })
            .collect();
        InsertDelta(Delta { els, base_len: xform.len_after_delete() })
    }

    /// Return a Subset containing the inserted ranges.
    ///
    /// `d.inserted_subset().delete_from_string(d.apply_to_string(s)) == s`
    pub fn inserted_subset(&self) -> Subset {
        let mut sb = SubsetBuilder::new();
        for elem in &self.0.els {
            match *elem {
                DeltaElement::Copy(b, e) => {
                    sb.push_segment(e - b, 0);
                }
                DeltaElement::Insert(ref n) => {
                    sb.push_segment(n.len(), 1);
                }
            }
        }
        sb.build()
    }
}

/// An InsertDelta is a certain kind of Delta, and anything that applies to a
/// Delta that may include deletes also applies to one that definitely
/// doesn't. This impl allows implicit use of those methods.
impl<N: NodeInfo> Deref for InsertDelta<N> {
    type Target = Delta<N>;

    fn deref(&self) -> &Delta<N> {
        &self.0
    }
}

pub struct Transformer<'a, N: NodeInfo + 'a> {
    pub(crate) delta: &'a Delta<N>,
    pub(crate) element_index: usize,
    pub(crate) transformed_offset: usize,
    pub(crate) last_query: Option<(usize, bool)>,
}

impl<'a, N: NodeInfo + 'a> Transformer<'a, N> {
    /// Create a new transformer from a delta.
    pub fn new(delta: &'a Delta<N>) -> Self {
        Transformer { delta, element_index: 0, transformed_offset: 0, last_query: None }
    }

    /// Transform a single coordinate. The `after` parameter indicates whether it
    /// should land before or after an inserted region.
    pub fn transform(&mut self, ix: usize, after: bool) -> usize {
        let must_reset = self.last_query.is_some_and(|(last_ix, last_after)| {
            ix < last_ix || (ix == last_ix && last_after && !after)
        });
        if must_reset {
            self.element_index = 0;
            self.transformed_offset = 0;
        }
        self.last_query = Some((ix, after));

        if ix == 0 && !after {
            return 0;
        }

        while let Some(element) = self.delta.els.get(self.element_index) {
            match *element {
                DeltaElement::Copy(beg, end) => {
                    if ix <= beg {
                        return self.transformed_offset;
                    }
                    if ix < end || (ix == end && !after) {
                        return self.transformed_offset + ix - beg;
                    }
                    self.transformed_offset += end - beg;
                }
                DeltaElement::Insert(ref node) => {
                    self.transformed_offset += node.len();
                }
            }
            self.element_index += 1;
        }
        self.transformed_offset
    }

    /// Determine whether a given interval is untouched by the transformation.
    pub fn interval_untouched<T: IntervalBounds>(&mut self, iv: T) -> bool {
        let iv = iv.into_interval(self.delta.base_len);
        let mut last_was_ins = true;
        for el in &self.delta.els {
            match *el {
                DeltaElement::Copy(beg, end) => {
                    if iv.is_before(end) {
                        if last_was_ins {
                            if iv.is_after(beg) {
                                return true;
                            }
                        } else if !iv.is_before(beg) {
                            return true;
                        }
                    } else {
                        return false;
                    }
                    last_was_ins = false;
                }
                _ => {
                    last_was_ins = true;
                }
            }
        }
        false
    }
}
