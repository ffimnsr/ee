//! Positional byte/char iterators over ropes.
//!
//! Ropey-style `*_at` iteration: start mid-rope without re-scanning the
//! prefix. All iterators are allocation-free and borrow from the rope.

use super::*;

/// An iterator over the bytes of a byte interval of a rope, yielding `u8`.
///
/// Unlike the chunk iterator, byte iteration is safe on mid-char start
/// positions (no utf8 slicing is performed).
pub struct Bytes<'a> {
    cursor: Cursor<'a, RopeInfo>,
    end: usize,
}

impl<'a> Bytes<'a> {
    pub(crate) fn new(rope: &'a Rope, start: usize, end: usize) -> Self {
        Bytes { cursor: Cursor::new(rope, start), end }
    }
}

impl<'a> Iterator for Bytes<'a> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        loop {
            if self.cursor.pos() >= self.end {
                return None;
            }
            let (leaf, start_pos) = self.cursor.get_leaf()?;
            let leaf_end = self.cursor.pos() + (leaf.len() - start_pos);
            let visible_end = leaf_end.min(self.end);
            if start_pos >= leaf.len() {
                // Cursor parked at the end of the leaf; move on.
                self.cursor.next_leaf();
                continue;
            }
            let b = leaf.as_bytes()[start_pos];
            if self.cursor.pos() + 1 >= visible_end {
                if visible_end < leaf_end {
                    // Interval ends inside this leaf; park the cursor at the end.
                    self.cursor.set(visible_end);
                } else {
                    self.cursor.next_leaf();
                }
            } else {
                self.cursor.set(self.cursor.pos() + 1);
            }
            return Some(b);
        }
    }
}

/// An iterator over the chars of a char-aligned interval of a rope, yielding
/// `char`.
pub struct Chars<'a> {
    cursor: Cursor<'a, RopeInfo>,
    end: usize,
}

impl<'a> Chars<'a> {
    pub(crate) fn new(rope: &'a Rope, start: usize, end: usize) -> Self {
        Chars { cursor: Cursor::new(rope, start), end }
    }
}

impl<'a> Iterator for Chars<'a> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        loop {
            if self.cursor.pos() >= self.end {
                return None;
            }
            let (leaf, start_pos) = self.cursor.get_leaf()?;
            if start_pos >= leaf.len() {
                self.cursor.next_leaf();
                continue;
            }
            let ch = leaf[start_pos..].chars().next()?;
            if start_pos + ch.len_utf8() >= leaf.len() {
                self.cursor.next_leaf();
            } else {
                self.cursor.set(self.cursor.pos() + ch.len_utf8());
            }
            if self.cursor.pos() > self.end {
                // Defensive clamp for callers that pass mid-char ends.
                self.cursor.set(self.end);
            }
            return Some(ch);
        }
    }
}

/// An iterator over the bytes of a byte interval of a rope, yielding
/// `(byte_idx, u8)` with byte indices in the reference coordinate system of
/// the underlying rope.
pub struct ByteIndices<'a> {
    inner: Bytes<'a>,
    index: usize,
}

impl<'a> ByteIndices<'a> {
    /// Byte indices are absolute (relative to the rope).
    pub(crate) fn new(rope: &'a Rope, start: usize, end: usize) -> Self {
        ByteIndices { inner: Bytes::new(rope, start, end), index: start }
    }

    /// Byte indices are relative to `start` (e.g. within a slice view).
    pub(crate) fn new_at(rope: &'a Rope, start: usize, end: usize) -> Self {
        ByteIndices { inner: Bytes::new(rope, start, end), index: 0 }
    }
}

impl<'a> Iterator for ByteIndices<'a> {
    type Item = (usize, u8);

    fn next(&mut self) -> Option<(usize, u8)> {
        let index = self.index;
        let byte = self.inner.next()?;
        self.index += 1;
        Some((index, byte))
    }
}

/// An iterator over the chars of a char-aligned interval of a rope, yielding
/// `(char_idx, char)` with char indices in the reference coordinate system of
/// the underlying rope (absolute by default).
pub struct CharIndices<'a> {
    inner: Chars<'a>,
    index: usize,
}

impl<'a> CharIndices<'a> {
    /// Char indices are absolute (relative to the rope).
    pub(crate) fn new(rope: &'a Rope, start: usize, end: usize) -> Self {
        let index = rope.count::<CharsMetric>(start);
        CharIndices { inner: Chars::new(rope, start, end), index }
    }

    /// Char indices are relative to the interval start (e.g. within a slice
    /// view), starting at 0.
    pub(crate) fn new_at(rope: &'a Rope, start: usize, end: usize) -> Self {
        CharIndices { inner: Chars::new(rope, start, end), index: 0 }
    }
}

impl<'a> Iterator for CharIndices<'a> {
    type Item = (usize, char);

    fn next(&mut self) -> Option<(usize, char)> {
        let index = self.index;
        let ch = self.inner.next()?;
        self.index += 1;
        Some((index, ch))
    }
}

impl Rope {
    /// An iterator over all the chars of the rope.
    pub fn chars(&self) -> Chars<'_> {
        Chars::new(self, 0, self.len())
    }

    /// An iterator over the chars of the rope, starting at `char_idx`.
    ///
    /// If `char_idx == len_chars()`, an iterator at the end of the rope is
    /// created (i.e. `next()` will return `None`).
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `char_idx > len_chars()`.
    pub fn chars_at(&self, char_idx: usize) -> Chars<'_> {
        self.try_chars_at(char_idx).expect("Rope::chars_at callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::chars_at`].
    pub fn try_chars_at(&self, char_idx: usize) -> Result<Chars<'_>, RopeError> {
        if char_idx > self.len_chars() {
            return Err(RopeError::CharOffsetOutOfBounds {
                offset: char_idx,
                len: self.len_chars(),
            });
        }
        let byte_idx = self.count_base_units::<CharsMetric>(char_idx);
        Ok(Chars::new(self, byte_idx, self.len()))
    }

    /// An iterator over all the bytes of the rope.
    pub fn bytes(&self) -> Bytes<'_> {
        Bytes::new(self, 0, self.len())
    }

    /// An iterator over the bytes of the rope, starting at `byte_idx`.
    ///
    /// Unlike the chunk iterator, a mid-char start is allowed and yields the
    /// continuation bytes of the split char. If `byte_idx == len()`, an
    /// iterator at the end of the rope is created (i.e. `next()` will return
    /// `None`).
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `byte_idx > len()`.
    pub fn bytes_at(&self, byte_idx: usize) -> Bytes<'_> {
        self.try_bytes_at(byte_idx).expect("Rope::bytes_at callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::bytes_at`].
    pub fn try_bytes_at(&self, byte_idx: usize) -> Result<Bytes<'_>, RopeError> {
        self.validate_offset(byte_idx)?;
        Ok(Bytes::new(self, byte_idx, self.len()))
    }

    /// An iterator over the logical lines of the rope, starting at `line_idx`.
    ///
    /// Line ending semantics match [`Rope::lines`]. If `line_idx` equals one
    /// plus the number of newlines, an empty iterator is returned.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `line_idx > self.measure::<LinesMetric>() + 1`.
    pub fn lines_at(&self, line_idx: usize) -> Lines<'_> {
        self.try_lines_at(line_idx).expect("Rope::lines_at callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::lines_at`].
    pub fn try_lines_at(&self, line_idx: usize) -> Result<Lines<'_>, RopeError> {
        self.validate_line(line_idx)?;
        let start = if line_idx == self.max_line_index() {
            self.len()
        } else {
            self.count_base_units::<LinesMetric>(line_idx)
        };
        Ok(self.lines(start..))
    }

    /// An iterator over the chunks of the rope.
    pub fn chunks(&self) -> ChunkIter<'_> {
        self.iter_chunks(..)
    }

    /// An iterator over the chunks of the rope, starting at the chunk
    /// containing `byte_idx`.
    ///
    /// Also returns the byte, char, and line indices of the beginning of that
    /// chunk. A one-past-the-end `byte_idx` yields starting at the last chunk.
    ///
    /// The return value is organized as
    /// `(iterator, chunk_byte_idx, chunk_char_idx, chunk_line_idx)`.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `byte_idx > len()`.
    pub fn chunks_at_byte(&self, byte_idx: usize) -> (ChunkIter<'_>, usize, usize, usize) {
        self.try_chunks_at_byte(byte_idx)
            .expect("Rope::chunks_at_byte callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::chunks_at_byte`].
    pub fn try_chunks_at_byte(
        &self,
        byte_idx: usize,
    ) -> Result<(ChunkIter<'_>, usize, usize, usize), RopeError> {
        self.validate_offset(byte_idx)?;
        let (_, byte_start, line_start, _) =
            self.chunk_at_offset(byte_idx).expect("validated byte offset yields a chunk");
        let char_start = self.count::<CharsMetric>(byte_start);
        Ok((self.iter_chunks(byte_start..), byte_start, char_start, line_start))
    }

    /// An iterator over the chunks of the rope, starting at the chunk
    /// containing `char_idx`.
    ///
    /// Also returns the byte, char, and line indices of the beginning of that
    /// chunk. A one-past-the-end `char_idx` yields starting at the last chunk.
    ///
    /// The return value is organized as
    /// `(iterator, chunk_byte_idx, chunk_char_idx, chunk_line_idx)`.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `char_idx > len_chars()`.
    pub fn chunks_at_char(&self, char_idx: usize) -> (ChunkIter<'_>, usize, usize, usize) {
        self.try_chunks_at_char(char_idx)
            .expect("Rope::chunks_at_char callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::chunks_at_char`].
    pub fn try_chunks_at_char(
        &self,
        char_idx: usize,
    ) -> Result<(ChunkIter<'_>, usize, usize, usize), RopeError> {
        let byte_idx = self.try_char_to_byte(char_idx)?;
        self.try_chunks_at_byte(byte_idx)
    }

    /// An iterator over the bytes of the rope, yielding `(byte_idx, u8)` with
    /// absolute byte indices.
    pub fn byte_indices(&self) -> ByteIndices<'_> {
        ByteIndices::new(self, 0, self.len())
    }

    /// An iterator over the bytes of the rope, starting at `byte_idx`,
    /// yielding `(byte_idx, u8)` with absolute byte indices.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `byte_idx > len()`.
    pub fn byte_indices_at(&self, byte_idx: usize) -> ByteIndices<'_> {
        self.try_byte_indices_at(byte_idx)
            .expect("Rope::byte_indices_at callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::byte_indices_at`].
    pub fn try_byte_indices_at(&self, byte_idx: usize) -> Result<ByteIndices<'_>, RopeError> {
        self.validate_offset(byte_idx)?;
        Ok(ByteIndices::new(self, byte_idx, self.len()))
    }

    /// An iterator over the chars of the rope, yielding `(char_idx, char)`
    /// with absolute char indices.
    pub fn char_indices(&self) -> CharIndices<'_> {
        CharIndices::new(self, 0, self.len())
    }

    /// An iterator over the chars of the rope, starting at `char_idx`,
    /// yielding `(char_idx, char)` with absolute char indices.
    ///
    /// Time complexity: O(log n).
    ///
    /// # Panics
    ///
    /// Panics if `char_idx > len_chars()`.
    pub fn char_indices_at(&self, char_idx: usize) -> CharIndices<'_> {
        self.try_char_indices_at(char_idx)
            .expect("Rope::char_indices_at callers must validate bounds")
    }

    /// Non-panicking version of [`Rope::char_indices_at`].
    pub fn try_char_indices_at(&self, char_idx: usize) -> Result<CharIndices<'_>, RopeError> {
        if char_idx > self.len_chars() {
            return Err(RopeError::CharOffsetOutOfBounds {
                offset: char_idx,
                len: self.len_chars(),
            });
        }
        let byte_idx = self.count_base_units::<CharsMetric>(char_idx);
        Ok(CharIndices::new(self, byte_idx, self.len()))
    }
}
