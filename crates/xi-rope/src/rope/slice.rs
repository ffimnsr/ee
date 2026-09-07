//! Rope slices, cursors, and line iterators.
use super::*;

/// Borrowed read-only view into a subrange of a [`Rope`].
#[derive(Clone, Copy, Debug)]
pub struct RopeSlice<'a> {
    pub(crate) rope: &'a Rope,
    pub(crate) interval: Interval,
}

impl<'a> RopeSlice<'a> {
    /// Create a borrowed view for `interval` within `rope`.
    pub fn new<T: IntervalBounds>(rope: &'a Rope, interval: T) -> Self {
        RopeSlice { rope, interval: interval.into_interval(rope.len()) }
    }

    /// Borrow underlying rope.
    pub fn rope(&self) -> &'a Rope {
        self.rope
    }

    /// Absolute interval covered by this view.
    pub fn interval(&self) -> Interval {
        self.interval
    }

    /// Start offset of this view in underlying rope.
    pub fn start(&self) -> usize {
        self.interval.start()
    }

    /// End offset of this view in underlying rope.
    pub fn end(&self) -> usize {
        self.interval.end()
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.interval.size()
    }

    pub fn is_empty(&self) -> bool {
        self.interval.is_empty()
    }

    /// Returns nested borrowed view relative to this view.
    pub fn slice<T: IntervalBounds>(&self, interval: T) -> RopeSlice<'a> {
        let rel = interval.into_interval(self.len());
        RopeSlice { rope: self.rope, interval: rel.translate(self.interval.start()) }
    }

    /// Materialize this borrowed view as owned rope.
    pub fn to_rope(&self) -> Rope {
        self.rope.subseq(self.interval)
    }

    /// Iterate borrowed chunks in this view.
    pub fn iter_chunks(&self) -> ChunkIter<'a> {
        self.rope.iter_chunks(self.interval)
    }

    /// Iterate raw lines in this view.
    pub fn lines_raw(&self) -> LinesRaw<'a> {
        self.rope.lines_raw(self.interval)
    }

    /// Iterate logical lines in this view.
    pub fn lines(&self) -> Lines<'a> {
        self.rope.lines(self.interval)
    }

    /// Return borrowed-or-owned text for this view.
    pub fn slice_to_cow(&self) -> Cow<'a, str> {
        self.rope.slice_to_cow(self.interval)
    }

    /// Create cursor bounded to this view.
    pub fn cursor(&self, position: usize) -> RopeSliceCursor<'a> {
        RopeSliceCursor::new(*self, position)
    }
}

/// Cursor bounded to a [`RopeSlice`].
pub struct RopeSliceCursor<'a> {
    pub(crate) cursor: Cursor<'a, RopeInfo>,
    pub(crate) interval: Interval,
}

impl<'a> RopeSliceCursor<'a> {
    pub fn new(slice: RopeSlice<'a>, position: usize) -> Self {
        RopeSliceCursor {
            cursor: Cursor::new(slice.rope, slice.interval.start() + position),
            interval: slice.interval,
        }
    }

    pub fn total_len(&self) -> usize {
        self.interval.size()
    }

    pub fn pos(&self) -> usize {
        self.cursor.pos().saturating_sub(self.interval.start())
    }

    pub fn set(&mut self, position: usize) {
        self.cursor.set(self.interval.start() + position);
    }

    pub fn get_leaf(&self) -> Option<(&'a str, usize)> {
        let (leaf, pos_in_leaf, _, _) = self.current_leaf_window()?;
        Some((leaf, pos_in_leaf))
    }

    pub fn next_leaf(&mut self) -> Option<(&'a str, usize)> {
        let (_, _, _, visible_end) = self.current_leaf_window()?;
        if visible_end >= self.interval.end() {
            self.cursor.set(self.interval.end());
            return None;
        }
        self.cursor.set(visible_end);
        self.get_leaf()
    }

    pub fn next_base(&mut self) -> Option<usize> {
        let next = self.cursor.next::<BaseMetric>()?;
        if next > self.interval.end() {
            self.cursor.set(self.interval.end());
            return None;
        }
        Some(self.pos())
    }

    pub fn next_codepoint(&mut self) -> Option<char> {
        let ch = self.cursor.next_codepoint()?;
        if self.cursor.pos() > self.interval.end() {
            self.cursor.set(self.interval.end());
            return None;
        }
        Some(ch)
    }

    pub(crate) fn current_leaf_window(&self) -> Option<(&'a str, usize, usize, usize)> {
        if self.cursor.pos() >= self.interval.end() {
            return None;
        }
        let (leaf, _) = self.cursor.get_leaf()?;
        let leaf_start = self.cursor.leaf_start_offset()?;
        let visible_start = leaf_start.max(self.interval.start());
        let visible_end = (leaf_start + leaf.len()).min(self.interval.end());
        if visible_start >= visible_end {
            return None;
        }
        let start_in_leaf = visible_start - leaf_start;
        let end_in_leaf = visible_end - leaf_start;
        let pos_in_leaf = self.cursor.pos() - visible_start;
        Some((&leaf[start_in_leaf..end_in_leaf], pos_in_leaf, visible_start, visible_end))
    }
}

pub struct LinesRaw<'a> {
    pub(crate) inner: ChunkIter<'a>,
    pub(crate) fragment: &'a str,
}

pub(crate) fn cow_append<'a>(a: Cow<'a, str>, b: &'a str) -> Cow<'a, str> {
    if a.is_empty() { Cow::from(b) } else { Cow::from(a.into_owned() + b) }
}

impl<'a> Iterator for LinesRaw<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Cow<'a, str>> {
        let mut result = Cow::from("");
        loop {
            if self.fragment.is_empty() {
                match self.inner.next() {
                    Some(chunk) => self.fragment = chunk,
                    None => return if result.is_empty() { None } else { Some(result) },
                }
                if self.fragment.is_empty() {
                    // can only happen on empty input
                    return None;
                }
            }
            match memchr(b'\n', self.fragment.as_bytes()) {
                Some(i) => {
                    result = cow_append(result, &self.fragment[..=i]);
                    self.fragment = &self.fragment[i + 1..];
                    return Some(result);
                }
                None => {
                    result = cow_append(result, self.fragment);
                    self.fragment = "";
                }
            }
        }
    }
}

pub struct Lines<'a> {
    pub(crate) inner: LinesRaw<'a>,
}

impl<'a> Iterator for Lines<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Cow<'a, str>> {
        match self.inner.next() {
            Some(Cow::Borrowed(mut s)) => {
                if s.ends_with('\n') {
                    s = &s[..s.len() - 1];
                    if s.ends_with('\r') {
                        s = &s[..s.len() - 1];
                    }
                }
                Some(Cow::from(s))
            }
            Some(Cow::Owned(mut s)) => {
                if s.ends_with('\n') {
                    let _ = s.pop();
                    if s.ends_with('\r') {
                        let _ = s.pop();
                    }
                }
                Some(Cow::from(s))
            }
            None => None,
        }
    }
}
