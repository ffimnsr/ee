//! Insert/delete region iterators.
use super::*;

pub struct InsertsIter<'a, N: NodeInfo + 'a> {
    pub(crate) pos: usize,
    pub(crate) last_end: usize,
    pub(crate) els_iter: slice::Iter<'a, DeltaElement<N>>,
}

#[derive(Debug, PartialEq)]
pub struct DeltaRegion {
    pub old_offset: usize,
    pub new_offset: usize,
    pub len: usize,
}

impl DeltaRegion {
    pub(crate) fn new(old_offset: usize, new_offset: usize, len: usize) -> Self {
        DeltaRegion { old_offset, new_offset, len }
    }
}

impl<'a, N: NodeInfo> Iterator for InsertsIter<'a, N> {
    type Item = DeltaRegion;

    fn next(&mut self) -> Option<Self::Item> {
        let mut result = None;
        for elem in &mut self.els_iter {
            match *elem {
                DeltaElement::Copy(b, e) => {
                    self.pos += e - b;
                    self.last_end = e;
                }
                DeltaElement::Insert(ref n) => {
                    result = Some(DeltaRegion::new(self.last_end, self.pos, n.len()));
                    self.pos += n.len();
                    self.last_end += n.len();
                    break;
                }
            }
        }
        result
    }
}

pub struct DeletionsIter<'a, N: NodeInfo + 'a> {
    pub(crate) pos: usize,
    pub(crate) last_end: usize,
    pub(crate) base_len: usize,
    pub(crate) els_iter: slice::Iter<'a, DeltaElement<N>>,
}

impl<'a, N: NodeInfo> Iterator for DeletionsIter<'a, N> {
    type Item = DeltaRegion;

    fn next(&mut self) -> Option<Self::Item> {
        let mut result = None;
        for elem in &mut self.els_iter {
            match *elem {
                DeltaElement::Copy(b, e) => {
                    if b > self.last_end {
                        result = Some(DeltaRegion::new(self.last_end, self.pos, b - self.last_end));
                    }
                    self.pos += e - b;
                    self.last_end = e;
                    if result.is_some() {
                        break;
                    }
                }
                DeltaElement::Insert(ref n) => {
                    self.pos += n.len();
                    self.last_end += n.len();
                }
            }
        }
        if result.is_none() && self.last_end < self.base_len {
            result = Some(DeltaRegion::new(self.last_end, self.pos, self.base_len - self.last_end));
            self.last_end = self.base_len;
        }
        result
    }
}
