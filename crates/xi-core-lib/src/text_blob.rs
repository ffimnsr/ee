// Copyright 2026 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Scratch buffer and frame format for the binary line-text carrier.
//!
//! One `update` appends the text of every row it re-sends into a single frame; the
//! ops that carry those rows set a `blob` flag and the frame follows the
//! notification. Text therefore crosses the boundary once: store chunk -> frame ->
//! frontend cache, with no per-row JSON string and no per-row JSON metadata.
//!
//! # Frame layout
//!
//! ```text
//! varint row_count
//! varint len_0
//! ...
//! varint len_{row_count - 1}
//! row bytes, concatenated in the same order
//! ```
//!
//! Lengths are LEB128 varints, so a source-like row costs one header byte instead
//! of the ~15 bytes a JSON `[off, len]` pair costs. Rows arrive in op order, which
//! is what lets the frontend hand them out positionally without any offsets.
//!
//! The frame is a scratch buffer, not document state: it is built per render, and a
//! row that overflows the cap falls back to the per-line text carrier (which is
//! what the frontend did before this carrier existed), so an oversized window
//! degrades instead of failing.

/// Upper bound for one update's frame.
///
/// Four mebibytes is far above a window of rows (a 200-row window of 2 KiB lines is
/// 400 KiB) and far below what a `usize` length can address, so the cap is a safety
/// valve for pathological windows rather than a routine case.
pub const MAX_BLOB_BYTES: usize = 4 * 1024 * 1024;

/// Append-only row buffer for one update's text.
#[derive(Debug, Default)]
pub struct TextBlob {
    bytes: Vec<u8>,
    lengths: Vec<usize>,
    /// Set once the cap is reached; further pushes report `false` so callers fall
    /// back to the per-line carrier instead of truncating a row.
    full: bool,
}

impl TextBlob {
    pub fn new() -> Self {
        TextBlob::default()
    }

    /// Appends one row's bytes.
    ///
    /// `false` when the frame is at [`MAX_BLOB_BYTES`]; the caller must then send
    /// that row's text through the per-line carrier.
    pub fn push_bytes(&mut self, bytes: &[u8]) -> bool {
        if self.full || self.bytes.len() + bytes.len() > MAX_BLOB_BYTES {
            self.full = true;
            return false;
        }
        self.lengths.push(bytes.len());
        self.bytes.extend_from_slice(bytes);
        true
    }

    /// Number of rows appended so far.
    pub fn row_count(&self) -> usize {
        self.lengths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lengths.is_empty()
    }

    /// Encodes the frame: row count, per-row lengths, then the bytes.
    pub fn into_frame(self) -> Vec<u8> {
        let mut frame = Vec::with_capacity(self.bytes.len() + self.lengths.len() + 4);
        write_varint(&mut frame, self.lengths.len());
        for length in &self.lengths {
            write_varint(&mut frame, *length);
        }
        frame.extend_from_slice(&self.bytes);
        frame
    }
}

/// Decodes a text frame into its rows.
///
/// Fails closed on anything that does not describe the frame exactly: a truncated
/// header, a length that overruns the payload, or trailing bytes the lengths do not
/// account for.
pub fn decode_frame(frame: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut cursor = 0usize;
    let count = read_varint(frame, &mut cursor)
        .ok_or_else(|| String::from("text frame: truncated row count"))?;
    let mut lengths = Vec::with_capacity(count.min(4096));
    for index in 0..count {
        let length = read_varint(frame, &mut cursor)
            .ok_or_else(|| format!("text frame: truncated length for row {index}"))?;
        lengths.push(length);
    }

    let mut rows = Vec::with_capacity(lengths.len());
    let mut offset = cursor;
    for (index, length) in lengths.iter().enumerate() {
        let end = offset
            .checked_add(*length)
            .ok_or_else(|| format!("text frame: row {index} length overflows"))?;
        let row = frame
            .get(offset..end)
            .ok_or_else(|| format!("text frame: row {index} runs past the frame"))?;
        rows.push(row);
        offset = end;
    }
    if offset != frame.len() {
        return Err(format!(
            "text frame: {} trailing bytes not described by the lengths",
            frame.len() - offset
        ));
    }
    Ok(rows)
}

/// Writes `value` as a LEB128 varint.
fn write_varint(out: &mut Vec<u8>, value: usize) {
    let mut value = value as u64;
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Reads a LEB128 varint at `*cursor`, advancing it past the value.
fn read_varint(bytes: &[u8], cursor: &mut usize) -> Option<usize> {
    let mut value: u64 = 0;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return usize::try_from(value).ok();
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_of(rows: &[&[u8]]) -> Vec<u8> {
        let mut blob = TextBlob::new();
        for row in rows {
            assert!(blob.push_bytes(row), "fixture should fit");
        }
        blob.into_frame()
    }

    #[test]
    fn frame_round_trips_rows() {
        let rows: [&[u8]; 3] = [b"alpha\n", "β\n".as_bytes(), b""];
        let frame = frame_of(&rows);
        let decoded = decode_frame(&frame).expect("round trip");

        assert_eq!(decoded.len(), rows.len());
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(decoded[index], *row, "row {index}");
        }
    }

    #[test]
    fn empty_frame_decodes_to_no_rows() {
        let frame = TextBlob::new().into_frame();
        assert_eq!(decode_frame(&frame).expect("empty frame"), Vec::<&[u8]>::new());
        assert!(frame.len() == 1, "an empty frame is just a zero count: {frame:?}");
    }

    #[test]
    fn header_costs_one_byte_per_source_like_row() {
        // The point of the varint header: a 26-byte row costs 1 header byte, where
        // a JSON `[off, len]` pair cost about 15.
        let rows: Vec<Vec<u8>> = (0..200).map(|_| b"let value = compute();\n".to_vec()).collect();
        let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        let frame = frame_of(&refs);
        let payload: usize = rows.iter().map(Vec::len).sum();

        assert_eq!(
            frame.len() - payload,
            rows.len() + 2,
            "200 is a two-byte count; one byte per row"
        );
    }

    #[test]
    fn decode_fails_closed_on_malformed_frames() {
        let frame = frame_of(&[b"alpha", b"beta"]);
        assert!(decode_frame(&frame[..1]).is_err(), "truncated header");
        assert!(decode_frame(&frame[..frame.len() - 1]).is_err(), "truncated payload");
        assert!(decode_frame(&[]).is_err(), "empty frame has no count");

        let mut trailing = frame.clone();
        trailing.push(b'x');
        assert!(decode_frame(&trailing).is_err(), "trailing bytes");

        let mut overrun = vec![1u8, 0xFF, 0x7F]; // count 1, length 16383, no payload
        overrun.push(0);
        assert!(decode_frame(&overrun).is_err(), "length past the frame");
    }

    #[test]
    fn cap_falls_back_instead_of_truncating() {
        let mut blob = TextBlob::new();
        let filler = "x".repeat(MAX_BLOB_BYTES);
        assert!(blob.push_bytes(filler.as_bytes()), "the cap itself fits");
        assert_eq!(blob.row_count(), 1);

        // At the cap the blob reports `false` for every later row, and the bytes
        // already stored are untouched (the caller sends those rows per-line).
        assert!(!blob.push_bytes(b"next"));
        assert!(!blob.push_bytes(b"next"));
        assert_eq!(blob.row_count(), 1);
        assert_eq!(blob.into_frame().len(), MAX_BLOB_BYTES + 4 + 1);
    }
}
