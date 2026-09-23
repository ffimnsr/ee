//! Conversions, chunk iterator, formatting, and rope cursors.
use super::*;

impl FromStr for Rope {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Rope, Self::Err> {
        let mut b = RopeBuilder::new();
        b.push_str(s);
        Ok(b.finish())
    }
}

impl Rope {
    /// Creates a `Rope` by streaming UTF-8 data from a reader.
    ///
    /// Data is appended to a [`RopeBuilder`] in bounded chunks as it is read,
    /// so large inputs are never materialized as a single `String`. Runs in
    /// O(N) time.
    ///
    /// # Errors
    ///
    /// - If the reader returns an error, `from_reader` stops and returns that
    ///   error.
    /// - If non-UTF-8 data is encountered, an [`io::Error`] with kind
    ///   `InvalidData` is returned.
    ///
    /// Note: some data from the reader is likely consumed even on error.
    pub fn from_reader<T: io::Read>(mut reader: T) -> io::Result<Rope> {
        const BUFFER_SIZE: usize = MAX_LEAF * 2;
        let mut builder = RopeBuilder::new();
        let mut buffer = [0u8; BUFFER_SIZE];
        let mut fill_idx = 0; // How much of `buffer` currently holds valid data.
        loop {
            match reader.read(&mut buffer[fill_idx..]) {
                Ok(read_count) => {
                    fill_idx += read_count;

                    // Determine how much of the buffer is valid utf8.
                    let valid_count = match str::from_utf8(&buffer[..fill_idx]) {
                        Ok(_) => fill_idx,
                        Err(e) => e.valid_up_to(),
                    };

                    // Append the valid part of the buffer to the rope.
                    if valid_count > 0 {
                        // SAFETY is proven here: `valid_count` comes from a
                        // validated utf8 prefix, so the slice cannot split a
                        // multi-byte char.
                        builder.push_str(
                            str::from_utf8(&buffer[..valid_count])
                                .expect("bytes validated as utf8 by valid_up_to above"),
                        );
                    }

                    // Shift the un-read part of the buffer to the front.
                    if valid_count < fill_idx {
                        buffer.copy_within(valid_count..fill_idx, 0);
                    }
                    fill_idx -= valid_count;

                    if fill_idx == BUFFER_SIZE {
                        // Buffer is full and none of it could be consumed;
                        // utf8 code points are at most 4 bytes, so this
                        // cannot be valid text.
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "stream did not contain valid UTF-8",
                        ));
                    }

                    // If we're done reading.
                    if read_count == 0 {
                        if fill_idx > 0 {
                            // We couldn't consume all data.
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "stream contained invalid UTF-8",
                            ));
                        } else {
                            return Ok(builder.finish());
                        }
                    }
                }

                Err(e) => {
                    // Read error.
                    return Err(e);
                }
            }
        }
    }

    /// Returns true if this rope and `other` point to precisely the same
    /// in-memory data.
    ///
    /// This holds when one is a clone of the other and neither has been
    /// modified since. Clones initially share all data, so this can detect
    /// whether two ropes have diverged (e.g. whether a buffer has been edited
    /// since an async save snapshot was taken). It is distinct from equality:
    /// equal-content ropes stored separately are not instances.
    ///
    /// Runs in O(1) time.
    pub fn is_instance(&self, other: &Rope) -> bool {
        self.ptr_eq(other)
    }
}

pub struct ChunkIter<'a> {
    pub(crate) cursor: Cursor<'a, RopeInfo>,
    pub(crate) end: usize,
}

impl<'a> Iterator for ChunkIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.cursor.pos() >= self.end {
            return None;
        }
        let (leaf, start_pos) = self.cursor.get_leaf().unwrap();
        let len = min(self.end - self.cursor.pos(), leaf.len() - start_pos);
        self.cursor.next_leaf();
        Some(&leaf[start_pos..start_pos + len])
    }
}

impl<T: AsRef<str>> From<T> for Rope {
    fn from(s: T) -> Rope {
        Rope::from_str(s.as_ref()).unwrap()
    }
}

impl From<Rope> for String {
    // maybe explore grabbing leaf? would require api in tree
    fn from(r: Rope) -> String {
        String::from(&r)
    }
}

impl<'a> From<RopeSlice<'a>> for Rope {
    /// Converts a view into an owned rope, sharing as much backing storage as
    /// possible (`RopeSlice::to_rope` semantics); only leaves at the view
    /// edges are copied.
    ///
    /// Runs in O(log n) time.
    fn from(slice: RopeSlice<'a>) -> Rope {
        slice.to_rope()
    }
}

impl Rope {
    /// Total size of the rope's backing text buffer space, in bytes.
    ///
    /// Sums the allocated capacity of every backing leaf `String`, including
    /// unoccupied over-allocation. The unoccupied space is
    /// `capacity() - len()`.
    ///
    /// Runs in O(n) time.
    pub fn capacity(&self) -> usize {
        let mut cursor = Cursor::new(self, 0);
        let mut total = 0;
        while let Some((leaf, _)) = cursor.get_leaf() {
            total += leaf.capacity();
            if cursor.next_leaf().is_none() {
                break;
            }
        }
        total
    }

    /// Shrinks every backing leaf `String` to fit its content exactly.
    ///
    /// Content and all metrics are unchanged. **NOTE:** calling this on a
    /// clone breaks shared storage with its other clones, which can
    /// _increase_ total memory usage despite shrinking this rope's own
    /// capacity.
    ///
    /// Runs in O(n) time.
    pub fn shrink_to_fit(&mut self) {
        let leaves = {
            let mut cursor = Cursor::new(self, 0);
            let mut leaves = Vec::new();
            while let Some((leaf, _)) = cursor.get_leaf() {
                let mut leaf = leaf.clone();
                leaf.shrink_to_fit();
                leaves.push(leaf);
                if cursor.next_leaf().is_none() {
                    break;
                }
            }
            leaves
        };
        let mut builder = TreeBuilder::new();
        for leaf in leaves {
            builder.push_leaf(leaf);
        }
        *self = builder.build();
    }
}

impl From<&Rope> for String {
    fn from(r: &Rope) -> String {
        r.slice_to_cow(..).into_owned()
    }
}

impl fmt::Display for Rope {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for s in self.iter_chunks(..) {
            write!(f, "{}", s)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Rope {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if f.alternate() {
            write!(f, "{}", String::from(self))
        } else {
            write!(f, "Rope({:?})", String::from(self))
        }
    }
}

impl Add for Rope {
    type Output = Rope;
    fn add(self, rhs: Rope) -> Rope {
        let mut b = TreeBuilder::new();
        b.push(self);
        b.push(rhs);
        b.build()
    }
}

//additional cursor features

impl<'a> Cursor<'a, RopeInfo> {
    /// Get previous codepoint before cursor position, and advance cursor backwards.
    pub fn prev_codepoint(&mut self) -> Option<char> {
        self.prev::<BaseMetric>();
        if let Some((l, offset)) = self.get_leaf() { l[offset..].chars().next() } else { None }
    }

    /// Get next codepoint after cursor position, and advance cursor.
    pub fn next_codepoint(&mut self) -> Option<char> {
        if let Some((l, offset)) = self.get_leaf() {
            self.next::<BaseMetric>();
            l[offset..].chars().next()
        } else {
            None
        }
    }

    /// Get the next codepoint after the cursor position, without advancing
    /// the cursor.
    pub fn peek_next_codepoint(&self) -> Option<char> {
        self.get_leaf().and_then(|(l, off)| l[off..].chars().next())
    }

    pub fn next_grapheme(&mut self) -> Option<usize> {
        let (mut l, mut offset) = self.get_leaf()?;
        let mut pos = self.pos();
        while offset < l.len() && !l.is_char_boundary(offset) {
            pos -= 1;
            offset -= 1;
        }
        let mut leaf_offset = pos - offset;
        let mut c = GraphemeCursor::new(pos, self.total_len(), true);
        let mut next_boundary = c.next_boundary(l, leaf_offset);
        while let Err(incomp) = next_boundary {
            if let GraphemeIncomplete::PreContext(_) = incomp {
                let (pl, poffset) = self.prev_leaf()?;
                c.provide_context(pl, self.pos() - poffset);
            } else if incomp == GraphemeIncomplete::NextChunk {
                self.set(pos);
                let (nl, noffset) = self.next_leaf()?;
                l = nl;
                leaf_offset = self.pos() - noffset;
                pos = leaf_offset + nl.len();
            } else {
                return None;
            }
            next_boundary = c.next_boundary(l, leaf_offset);
        }
        next_boundary.unwrap_or(None)
    }

    pub fn prev_grapheme(&mut self) -> Option<usize> {
        let (mut l, mut offset) = self.get_leaf()?;
        let mut pos = self.pos();
        while offset < l.len() && !l.is_char_boundary(offset) {
            pos += 1;
            offset += 1;
        }
        let mut leaf_offset = pos - offset;
        let mut c = GraphemeCursor::new(pos, l.len() + leaf_offset, true);
        let mut prev_boundary = c.prev_boundary(l, leaf_offset);
        while let Err(incomp) = prev_boundary {
            if let GraphemeIncomplete::PreContext(_) = incomp {
                let (pl, poffset) = self.prev_leaf()?;
                c.provide_context(pl, self.pos() - poffset);
            } else if incomp == GraphemeIncomplete::PrevChunk {
                self.set(pos);
                let (pl, poffset) = self.prev_leaf()?;
                l = pl;
                leaf_offset = self.pos() - poffset;
                pos = leaf_offset + pl.len();
            } else {
                return None;
            }
            prev_boundary = c.prev_boundary(l, leaf_offset);
        }
        prev_boundary.unwrap_or(None)
    }
}
