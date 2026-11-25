use super::*;

// ---------------------------------------------------------------------------
// InsertBuffer
// ---------------------------------------------------------------------------

/// An append-only buffer holding inserted text.
///
/// Text is UTF-8 validated on entry; invalid bytes are rejected.  The buffer
/// can only grow; individual bytes are never removed.  Old pieces that
/// reference an earlier slice of the buffer remain valid indefinitely.
///
/// Inserted bytes are **never** copied into the [`crate::vlf::pager::FilePager`]
/// page cache; they are separate from the original file.
pub struct InsertBuffer {
    /// Stable identifier.
    id: BufferId,
    /// UTF-8 validated bytes.
    pub(super) bytes: Vec<u8>,
}

impl InsertBuffer {
    pub(super) fn new(id: BufferId) -> Self {
        InsertBuffer { id, bytes: Vec::new() }
    }

    /// Identifier for this buffer.
    pub fn id(&self) -> BufferId {
        self.id
    }

    /// Append `text` to the buffer.
    ///
    /// Returns the [`InsertRange`] that addresses the newly appended bytes.
    ///
    /// Fails if `text` is not valid UTF-8 (though `&str` guarantees this at
    /// the type level).
    pub fn append(&mut self, text: &str) -> InsertRange {
        let start = self.bytes.len() as u64;
        self.bytes.extend_from_slice(text.as_bytes());
        let end = self.bytes.len() as u64;
        InsertRange { start, end }
    }

    /// Read a slice from the buffer.
    ///
    /// Returns `None` if `range` is out of bounds.
    pub fn slice(&self, range: InsertRange) -> Option<&[u8]> {
        let start = range.start as usize;
        let end = range.end as usize;
        self.bytes.get(start..end)
    }

    /// Total byte length of the buffer.
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Returns `true` if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}
