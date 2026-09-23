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
    ///
    /// Debug builds panic if either boundary is not on a char boundary; use
    /// [`Rope::byte_slice`] for a validated, non-panicking entry point.
    pub fn new<T: IntervalBounds>(rope: &'a Rope, interval: T) -> Self {
        let interval = interval.into_interval(rope.len());
        debug_assert!(
            rope.is_codepoint_boundary(interval.start()),
            "RopeSlice start {} is not a char boundary",
            interval.start()
        );
        debug_assert!(
            rope.is_codepoint_boundary(interval.end()),
            "RopeSlice end {} is not a char boundary",
            interval.end()
        );
        RopeSlice { rope, interval }
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

    /// Number of chars (Unicode scalar values) in this view.
    ///
    /// Time complexity: O(log n).
    pub fn len_chars(&self) -> usize {
        self.rope.count::<CharsMetric>(self.interval.end())
            - self.rope.count::<CharsMetric>(self.interval.start())
    }

    /// Byte position (relative to this view) of the char at relative char
    /// index `char_pos`. One-past-the-end returns the view length in bytes.
    ///
    /// Time complexity: O(log n).
    pub fn char_to_byte(&self, char_pos: usize) -> usize {
        let start_char = self.rope.byte_to_char(self.interval.start());
        self.rope.char_to_byte(start_char + char_pos) - self.interval.start()
    }

    /// Char index (relative to this view) of the given byte position
    /// (relative to this view). A mid-char byte position rounds down to the
    /// start of the char containing it.
    ///
    /// Time complexity: O(log n).
    pub fn byte_to_char(&self, byte_pos: usize) -> usize {
        self.rope.byte_to_char(self.interval.start() + byte_pos)
            - self.rope.byte_to_char(self.interval.start())
    }

    /// Returns the char at relative char index `char_pos`, or `None` if it
    /// is out of bounds of the view.
    ///
    /// Time complexity: O(log n).
    pub fn get_char(&self, char_pos: usize) -> Option<char> {
        if char_pos >= self.len_chars() {
            return None;
        }
        let start_char = self.rope.byte_to_char(self.interval.start());
        self.rope.get_char(start_char + char_pos)
    }

    /// Returns the char at relative char index `char_pos`.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `char_pos >= len_chars()`.
    pub fn char_at(&self, char_pos: usize) -> char {
        self.get_char(char_pos)
            .unwrap_or_else(|| panic!("RopeSlice::char_at callers must validate bounds"))
    }

    /// An iterator over the chars of this view.
    pub fn chars(&self) -> Chars<'a> {
        Chars::new(self.rope, self.interval.start(), self.interval.end())
    }

    /// An iterator over the bytes of this view.
    pub fn bytes(&self) -> Bytes<'a> {
        Bytes::new(self.rope, self.interval.start(), self.interval.end())
    }

    /// An iterator over the chars of this view, yielding `(char_idx, char)`
    /// with indices relative to the view (starting at 0).
    pub fn char_indices(&self) -> CharIndices<'a> {
        CharIndices::new_at(self.rope, self.interval.start(), self.interval.end())
    }

    /// An iterator over the bytes of this view, yielding `(byte_idx, u8)`
    /// with indices relative to the view (starting at 0).
    pub fn byte_indices(&self) -> ByteIndices<'a> {
        ByteIndices::new_at(self.rope, self.interval.start(), self.interval.end())
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
