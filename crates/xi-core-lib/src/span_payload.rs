//! Span payload encoding: scope interning plus the packed record layout.
//!
//! Phase 1 of `docs/upgrades/line-payload-binary-framing.md` replaces per-span
//! JSON objects with interned scope ids and flat span arrays. `ScopeTable` is
//! the production interning table used by `View::encode_line_spans`.
//!
//! The fixed-width record layout (`SpanRecord`, `SpanRecordU16`, pack/unpack) is
//! the Phase 3 binary-frame carrier. It measures better than flat JSON on bytes
//! and decode cost but needs the byte-frame transport, so it stays test-gated
//! alongside the perf probe that measures it.

use std::collections::HashMap;

/// Saturating `usize` -> `u32` for wire offsets. Only a single line longer than
/// 4 GiB could saturate; clamping keeps the payload well-formed instead of
/// panicking.
pub(crate) fn clamp_offset(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Interned scope names for one update payload.
#[derive(Debug, Default)]
pub(crate) struct ScopeTable {
    names: Vec<String>,
    index: HashMap<String, u32>,
}

impl ScopeTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the stable index for `scope`, assigning a new one when unseen.
    pub(crate) fn intern(&mut self, scope: &str) -> u32 {
        if let Some(&id) = self.index.get(scope) {
            return id;
        }
        let id = clamp_offset(self.names.len());
        self.names.push(scope.to_owned());
        self.index.insert(scope.to_owned(), id);
        id
    }

    /// True when nothing was interned, so the payload can omit the table.
    pub(crate) fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    /// Consumes the table, yielding the scope names in id order.
    pub(crate) fn into_names(self) -> Vec<String> {
        self.names
    }

    /// Bytes a JSON scope table would take, counting `["name",...]` with commas.
    #[cfg(test)]
    pub(crate) fn json_bytes(&self) -> usize {
        let names: usize = self.names.iter().map(|name| name.len() + 3).sum();
        names + self.names.len().saturating_sub(1)
    }
}

#[cfg(test)]
pub(crate) use records::{SPAN_RECORD_BYTES, SPAN_RECORD_U16_BYTES};
#[cfg(test)]
pub(crate) use records::{decode_records, decode_records_u16, encode, encode_u16};

#[cfg(test)]
mod records {
    use zerocopy::byteorder::{LE, U16, U32};
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

    use super::{ScopeTable, clamp_offset};
    use crate::tree_sitter_support::VisibleSyntaxSpan;

    /// Size of one packed span record in bytes.
    pub(crate) const SPAN_RECORD_BYTES: usize = 12;

    /// Size of one packed 16-bit span record in bytes.
    pub(crate) const SPAN_RECORD_U16_BYTES: usize = 6;

    /// One line-relative span with its scope name replaced by an interned index.
    #[derive(
        Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned,
    )]
    #[repr(C)]
    pub(crate) struct SpanRecord {
        pub(crate) start: U32<LE>,
        pub(crate) end: U32<LE>,
        pub(crate) scope: U32<LE>,
    }

    /// 16-bit span record. Line-relative offsets and scope ids fit in 16 bits for
    /// any realistic window, which halves the record size; [`encode_u16`] reports
    /// `None` when a line or scope table outgrows that so the caller can fall back.
    #[derive(
        Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned,
    )]
    #[repr(C)]
    pub(crate) struct SpanRecordU16 {
        pub(crate) start: U16<LE>,
        pub(crate) end: U16<LE>,
        pub(crate) scope: U16<LE>,
    }

    impl SpanRecord {
        fn new(start: usize, end: usize, scope: u32) -> Self {
            SpanRecord {
                start: U32::new(clamp_offset(start)),
                end: U32::new(clamp_offset(end)),
                scope: U32::new(scope),
            }
        }
    }

    impl SpanRecordU16 {
        fn new(start: usize, end: usize, scope: u16) -> Option<Self> {
            Some(SpanRecordU16 {
                start: U16::new(u16::try_from(start).ok()?),
                end: U16::new(u16::try_from(end).ok()?),
                scope: U16::new(scope),
            })
        }
    }

    /// Flat span records plus a per-line offset index.
    ///
    /// `line_offsets[i]` is the number of records before line `i`, and the final
    /// entry is the total record count, so every line's slice is recoverable.
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(crate) struct SpanBlob {
        pub(crate) records: Vec<SpanRecord>,
        pub(crate) line_offsets: Vec<u32>,
    }

    impl SpanBlob {
        /// Bytes for the packed records plus the line offset index.
        pub(crate) fn byte_len(&self) -> usize {
            self.records.len() * SPAN_RECORD_BYTES
                + self.line_offsets.len() * std::mem::size_of::<u32>()
        }

        /// Records as little-endian bytes, followed by the little-endian offsets.
        pub(crate) fn to_bytes(&self) -> Vec<u8> {
            let mut bytes = Vec::with_capacity(self.byte_len());
            bytes.extend_from_slice(self.records.as_bytes());
            for offset in &self.line_offsets {
                bytes.extend_from_slice(&offset.to_le_bytes());
            }
            bytes
        }
    }

    /// Packs per-line spans into one blob, interning scope names as it goes.
    pub(crate) fn encode(
        spans_per_line: &[Vec<VisibleSyntaxSpan>],
        table: &mut ScopeTable,
    ) -> SpanBlob {
        let total: usize = spans_per_line.iter().map(Vec::len).sum();
        let mut blob = SpanBlob {
            records: Vec::with_capacity(total),
            line_offsets: Vec::with_capacity(spans_per_line.len() + 1),
        };

        for spans in spans_per_line {
            blob.line_offsets.push(clamp_offset(blob.records.len()));
            for span in spans {
                if span.end_byte <= span.start_byte {
                    continue;
                }
                let scope = table.intern(&span.scope);
                blob.records.push(SpanRecord::new(span.start_byte, span.end_byte, scope));
            }
        }
        blob.line_offsets.push(clamp_offset(blob.records.len()));
        blob
    }

    /// Borrows records straight out of a byte buffer. `None` when `bytes` is not a
    /// whole number of records or does not meet `SpanRecord`'s (alignment 1) layout.
    pub(crate) fn decode_records(bytes: &[u8]) -> Option<&[SpanRecord]> {
        <[SpanRecord]>::ref_from_bytes(bytes).ok()
    }

    /// Flat 16-bit records plus a `u32` line offset index.
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(crate) struct SpanBlobU16 {
        pub(crate) records: Vec<SpanRecordU16>,
        pub(crate) line_offsets: Vec<u32>,
    }

    impl SpanBlobU16 {
        pub(crate) fn byte_len(&self) -> usize {
            self.records.len() * SPAN_RECORD_U16_BYTES
                + self.line_offsets.len() * std::mem::size_of::<u32>()
        }

        pub(crate) fn to_bytes(&self) -> Vec<u8> {
            let mut bytes = Vec::with_capacity(self.byte_len());
            bytes.extend_from_slice(self.records.as_bytes());
            for offset in &self.line_offsets {
                bytes.extend_from_slice(&offset.to_le_bytes());
            }
            bytes
        }
    }

    /// Packs per-line spans into 16-bit records, or `None` when a line-relative
    /// offset or the scope table does not fit in `u16`.
    pub(crate) fn encode_u16(
        spans_per_line: &[Vec<VisibleSyntaxSpan>],
        table: &mut ScopeTable,
    ) -> Option<SpanBlobU16> {
        let total: usize = spans_per_line.iter().map(Vec::len).sum();
        let mut blob = SpanBlobU16 {
            records: Vec::with_capacity(total),
            line_offsets: Vec::with_capacity(spans_per_line.len() + 1),
        };

        for spans in spans_per_line {
            blob.line_offsets.push(clamp_offset(blob.records.len()));
            for span in spans {
                if span.end_byte <= span.start_byte {
                    continue;
                }
                let scope = table.intern(&span.scope);
                let scope = u16::try_from(scope).ok()?;
                blob.records.push(SpanRecordU16::new(span.start_byte, span.end_byte, scope)?);
            }
        }
        blob.line_offsets.push(clamp_offset(blob.records.len()));
        Some(blob)
    }

    /// Borrows 16-bit records straight out of a byte buffer.
    pub(crate) fn decode_records_u16(bytes: &[u8]) -> Option<&[SpanRecordU16]> {
        <[SpanRecordU16]>::ref_from_bytes(bytes).ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::mem::{align_of, size_of};

        fn span(start: usize, end: usize, scope: &str) -> VisibleSyntaxSpan {
            VisibleSyntaxSpan { start_byte: start, end_byte: end, scope: scope.to_owned() }
        }

        #[test]
        fn records_are_twelve_bytes_and_unaligned() {
            assert_eq!(size_of::<SpanRecord>(), SPAN_RECORD_BYTES);
            assert_eq!(align_of::<SpanRecord>(), 1);
        }

        #[test]
        fn records_store_little_endian_bytes() {
            let record = SpanRecord::new(0x0102_0304, 0x0506_0708, 0x0000_0009);
            let bytes = record.as_bytes();
            assert_eq!(bytes, &[0x04, 0x03, 0x02, 0x01, 0x08, 0x07, 0x06, 0x05, 0x09, 0, 0, 0]);
        }

        #[test]
        fn scope_table_dedupes_names() {
            let mut table = ScopeTable::new();
            let first = table.intern("keyword.control.rust");
            let second = table.intern("variable.other.rust");
            let repeat = table.intern("keyword.control.rust");
            assert_eq!(first, repeat);
            assert_ne!(first, second);
            assert_eq!(table.names(), ["keyword.control.rust", "variable.other.rust"]);
        }

        #[test]
        fn encode_indexes_lines_and_skips_empty_spans() {
            let mut table = ScopeTable::new();
            let lines = vec![
                vec![span(0, 3, "keyword"), span(8, 9, "number")],
                vec![span(5, 5, "empty")],
                vec![span(0, 4, "keyword")],
            ];
            let blob = encode(&lines, &mut table);

            assert_eq!(blob.records.len(), 3);
            assert_eq!(blob.line_offsets, [0, 2, 2, 3]);
            assert_eq!(table.names().len(), 2, "repeated scope interns once");
            assert_eq!(blob.records[2].scope.get(), 0, "third record reuses keyword id");
            assert_eq!(blob.byte_len(), 3 * SPAN_RECORD_BYTES + 4 * size_of::<u32>());
        }

        #[test]
        fn byte_round_trip_recovers_records() {
            let mut table = ScopeTable::new();
            let lines = vec![vec![span(1, 4, "a"), span(6, 9, "b")], vec![span(0, 2, "a")]];
            let blob = encode(&lines, &mut table);
            let bytes = blob.to_bytes();

            let records = decode_records(&bytes[..blob.records.len() * SPAN_RECORD_BYTES])
                .expect("aligned record run should decode");
            assert_eq!(records, blob.records.as_slice());
            assert_eq!(records[0].start.get(), 1);
            assert_eq!(records[2].scope.get(), 0);
        }

        #[test]
        fn decode_rejects_partial_records() {
            let bytes = [0u8; SPAN_RECORD_BYTES + 1];
            assert!(decode_records(&bytes).is_none());
        }

        #[test]
        fn decode_accepts_unaligned_buffer() {
            let mut table = ScopeTable::new();
            let blob = encode(&[vec![span(0, 3, "keyword")]], &mut table);
            let mut buffer = vec![0xAAu8];
            buffer.extend_from_slice(&blob.to_bytes());
            let record_bytes = blob.records.len() * SPAN_RECORD_BYTES;
            let records = decode_records(&buffer[1..1 + record_bytes])
                .expect("alignment 1 records decode anywhere");
            assert_eq!(records[0].end.get(), 3);
        }

        #[test]
        fn u16_records_are_six_bytes_and_round_trip() {
            assert_eq!(size_of::<SpanRecordU16>(), SPAN_RECORD_U16_BYTES);
            assert_eq!(align_of::<SpanRecordU16>(), 1);

            let mut table = ScopeTable::new();
            let lines = vec![
                vec![span(0, 3, "keyword"), span(8, 9, "number")],
                vec![span(2, 6, "keyword")],
            ];
            let blob = encode_u16(&lines, &mut table).expect("fixture fits in u16 records");
            assert_eq!(blob.records.len(), 3);
            assert_eq!(blob.line_offsets, [0, 2, 3]);

            let raw = blob.to_bytes();
            let records = decode_records_u16(&raw[..blob.records.len() * SPAN_RECORD_U16_BYTES])
                .expect("u16 records decode");
            assert_eq!(records.len(), 3);
            assert_eq!(records[1].start.get(), 8);
            assert_eq!(records[2].scope.get(), 0);
        }

        #[test]
        fn u16_records_reject_oversized_offsets() {
            let mut table = ScopeTable::new();
            let lines = vec![vec![span(0, usize::from(u16::MAX) + 1, "keyword")]];
            assert!(encode_u16(&lines, &mut table).is_none(), "oversized offset falls back");
        }

        #[test]
        fn scope_table_json_bytes_counts_brackets_and_commas() {
            let mut table = ScopeTable::new();
            table.intern("abc");
            table.intern("de");
            // ["abc","de"] -> 1 + 5 + 1 + 4 + 1
            assert_eq!(table.json_bytes(), 12);
        }
    }
}
