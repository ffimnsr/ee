//! Core Rope edit/slice/metric operations.
use super::impls::ChunkIter;
use super::metrics::{BaseMetric, LinesMetric, Utf16CodeUnitsMetric};
use super::slice::Lines;
use super::*;

impl Rope {
    pub(crate) fn validate_offset(&self, offset: usize) -> Result<(), RopeError> {
        if offset > self.len() {
            Err(RopeError::offset_out_of_bounds(offset, self.len()))
        } else {
            Ok(())
        }
    }

    pub(crate) fn max_line_index(&self) -> usize {
        self.measure::<LinesMetric>() + 1
    }

    pub(crate) fn validate_line(&self, line: usize) -> Result<(), RopeError> {
        let max_line = self.max_line_index();
        if line > max_line { Err(RopeError::line_out_of_bounds(line, max_line)) } else { Ok(()) }
    }

    pub(crate) fn validate_interval<T: IntervalBounds>(
        &self,
        iv: T,
    ) -> Result<Interval, RopeError> {
        let iv = iv.into_interval(self.len());
        if iv.start() > iv.end() {
            return Err(RopeError::reversed_interval(iv));
        }
        if iv.end() > self.len() {
            return Err(RopeError::interval_out_of_bounds(iv, self.len()));
        }
        Ok(iv)
    }

    /// Edit the string, replacing the byte range `start..end` with `new`.
    ///
    /// Time complexity: O(log n)
    #[deprecated(since = "0.3.0", note = "Use Rope::edit instead")]
    pub fn edit_str<T: IntervalBounds>(&mut self, iv: T, new: &str) {
        self.edit(iv, new)
    }

    pub fn try_edit<T, IV>(&mut self, iv: IV, new: T) -> Result<(), RopeError>
    where
        T: Into<Rope>,
        IV: IntervalBounds,
    {
        let iv = self.validate_interval(iv)?;
        self.edit(iv, new);
        Ok(())
    }

    /// Returns a new Rope with the contents of the provided range.
    pub fn slice<T: IntervalBounds>(&self, iv: T) -> Rope {
        self.try_slice(iv).expect("Rope::slice callers must validate bounds")
    }

    pub fn try_slice<T: IntervalBounds>(&self, iv: T) -> Result<Rope, RopeError> {
        Ok(self.subseq(self.validate_interval(iv)?))
    }

    /// Returns borrowed read-only view over provided range.
    pub fn slice_view<T: IntervalBounds>(&self, iv: T) -> RopeSlice<'_> {
        RopeSlice::new(self, iv)
    }

    // encourage callers to use Cursor instead?

    /// Determine whether `offset` lies on a codepoint boundary.
    pub fn is_codepoint_boundary(&self, offset: usize) -> bool {
        let mut cursor = Cursor::new(self, offset);
        cursor.is_boundary::<BaseMetric>()
    }

    /// Return the offset of the codepoint before `offset`.
    pub fn prev_codepoint_offset(&self, offset: usize) -> Option<usize> {
        let mut cursor = Cursor::new(self, offset);
        cursor.prev::<BaseMetric>()
    }

    /// Return the offset of the codepoint after `offset`.
    pub fn next_codepoint_offset(&self, offset: usize) -> Option<usize> {
        let mut cursor = Cursor::new(self, offset);
        cursor.next::<BaseMetric>()
    }

    /// Returns `offset` if it lies on a codepoint boundary. Otherwise returns
    /// the codepoint after `offset`.
    pub fn at_or_next_codepoint_boundary(&self, offset: usize) -> Option<usize> {
        if self.is_codepoint_boundary(offset) {
            Some(offset)
        } else {
            self.next_codepoint_offset(offset)
        }
    }

    /// Returns `offset` if it lies on a codepoint boundary. Otherwise returns
    /// the codepoint before `offset`.
    pub fn at_or_prev_codepoint_boundary(&self, offset: usize) -> Option<usize> {
        if self.is_codepoint_boundary(offset) {
            Some(offset)
        } else {
            self.prev_codepoint_offset(offset)
        }
    }

    pub fn prev_grapheme_offset(&self, offset: usize) -> Option<usize> {
        let mut cursor = Cursor::new(self, offset);
        cursor.prev_grapheme()
    }

    pub fn next_grapheme_offset(&self, offset: usize) -> Option<usize> {
        let mut cursor = Cursor::new(self, offset);
        cursor.next_grapheme()
    }

    /// Return the line number corresponding to the byte index `offset`.
    ///
    /// The line number is 0-based, thus this is equivalent to the count of newlines
    /// in the slice up to `offset`.
    ///
    /// Time complexity: O(log n)
    ///
    /// # Panics
    ///
    /// This function will panic if `offset > self.len()`. Callers are expected to
    /// validate their input.
    pub fn line_of_offset(&self, offset: usize) -> usize {
        self.try_line_of_offset(offset).expect("Rope::line_of_offset callers must validate bounds")
    }

    pub fn try_line_of_offset(&self, offset: usize) -> Result<usize, RopeError> {
        self.validate_offset(offset)?;
        Ok(self.count::<LinesMetric>(offset))
    }

    /// Return the byte offset corresponding to the line number `line`.
    /// If `line` is equal to one plus the current number of lines,
    /// this returns the offset of the end of the rope. Arguments higher
    /// than this will panic.
    ///
    /// The line number is 0-based.
    ///
    /// Time complexity: O(log n)
    ///
    /// # Panics
    ///
    /// This function will panic if `line > self.measure::<LinesMetric>() + 1`.
    /// Callers are expected to validate their input.
    pub fn offset_of_line(&self, line: usize) -> usize {
        self.try_offset_of_line(line).expect("Rope::offset_of_line callers must validate bounds")
    }

    pub fn try_offset_of_line(&self, line: usize) -> Result<usize, RopeError> {
        self.validate_line(line)?;
        Ok(match line.cmp(&self.max_line_index()) {
            Ordering::Equal => self.len(),
            Ordering::Less => self.count_base_units::<LinesMetric>(line),
            Ordering::Greater => unreachable!("validate_line rejects oversized indices"),
        })
    }

    /// Returns chunk containing `byte_offset` and chunk start metrics.
    pub fn chunk_at_offset(&self, byte_offset: usize) -> Option<(&str, usize, usize, usize)> {
        if byte_offset > self.len() {
            return None;
        }
        let cursor = Cursor::new(self, byte_offset);
        let (leaf, _) = cursor.get_leaf()?;
        Some((
            leaf.as_str(),
            cursor.leaf_start_offset()?,
            cursor.leaf_start_measure::<LinesMetric>()?,
            cursor.leaf_start_measure::<Utf16CodeUnitsMetric>()?,
        ))
    }

    /// Returns chunk containing line boundary `line` and chunk start metrics.
    pub fn chunk_at_line(&self, line: usize) -> Option<(&str, usize, usize, usize)> {
        let max_line = self.measure::<LinesMetric>() + 1;
        if line > max_line {
            return None;
        }
        self.chunk_at_offset(self.count_base_units::<LinesMetric>(line))
    }

    /// Returns chunk containing UTF-16 boundary `utf16_offset` and chunk start metrics.
    pub fn chunk_at_utf16(&self, utf16_offset: usize) -> Option<(&str, usize, usize, usize)> {
        let max_utf16 = self.measure::<Utf16CodeUnitsMetric>();
        if utf16_offset > max_utf16 {
            return None;
        }
        self.chunk_at_offset(self.count_base_units::<Utf16CodeUnitsMetric>(utf16_offset))
    }

    /// Returns an iterator over chunks of the rope.
    ///
    /// Each chunk is a `&str` slice borrowed from the rope's storage. The size
    /// of the chunks is indeterminate but for large strings will generally be
    /// in the range of 511-1024 bytes.
    ///
    /// The empty string will yield a single empty slice. In all other cases, the
    /// slices will be nonempty.
    ///
    /// Time complexity: technically O(n log n), but the constant factor is so
    /// tiny it is effectively O(n). This iterator does not allocate.
    pub fn iter_chunks<T: IntervalBounds>(&self, range: T) -> ChunkIter<'_> {
        let Interval { start, end } = range.into_interval(self.len());

        ChunkIter { cursor: Cursor::new(self, start), end }
    }

    /// An iterator over the raw lines. The lines, except the last, include the
    /// terminating newline.
    ///
    /// The return type is a `Cow<str>`, and in most cases the lines are slices
    /// borrowed from the rope.
    pub fn lines_raw<T: IntervalBounds>(&self, range: T) -> LinesRaw<'_> {
        LinesRaw { inner: self.iter_chunks(range), fragment: "" }
    }

    /// An iterator over the lines of a rope.
    ///
    /// Lines are ended with either Unix (`\n`) or MS-DOS (`\r\n`) style line endings.
    /// The line ending is stripped from the resulting string. The final line ending
    /// is optional.
    ///
    /// The return type is a `Cow<str>`, and in most cases the lines are slices borrowed
    /// from the rope.
    ///
    /// The semantics are intended to match `str::lines()`.
    pub fn lines<T: IntervalBounds>(&self, range: T) -> Lines<'_> {
        Lines { inner: self.lines_raw(range) }
    }

    // callers should be encouraged to use cursor instead
    pub fn byte_at(&self, offset: usize) -> u8 {
        let cursor = Cursor::new(self, offset);
        let (leaf, pos) = cursor.get_leaf().unwrap();
        leaf.as_bytes()[pos]
    }

    pub fn slice_to_cow<T: IntervalBounds>(&self, range: T) -> Cow<'_, str> {
        let mut iter = self.iter_chunks(range);
        let first = iter.next();
        let second = iter.next();

        match (first, second) {
            (None, None) => Cow::from(""),
            (Some(s), None) => Cow::from(s),
            (Some(one), Some(two)) => {
                let mut result = [one, two].concat();
                for chunk in iter {
                    result.push_str(chunk);
                }
                Cow::from(result)
            }
            (None, Some(_)) => unreachable!(),
        }
    }

    /// Stream rope contents into a byte writer without flattening first.
    pub fn write_to<W: io::Write>(&self, mut writer: W) -> io::Result<()> {
        for chunk in self.iter_chunks(..) {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }
}
