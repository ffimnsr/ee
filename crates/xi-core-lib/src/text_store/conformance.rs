//! Shared `TextStore` conformance checks.
//!
//! Every store must agree with every other store on the same document: the
//! borrowed carrier ([`ChunkBytes`]) has to match the owned carrier
//! ([`TextChunkResult`]) byte for byte, including pending, cancelled, and
//! unsupported outcomes. Store tests call into this module instead of
//! re-deriving the expectations per backend.

use super::{ByteRange, ChunkBytes, TextChunkResult, TextStore};

/// Picks a codepoint-safe byte offset near `target` in `content`.
///
/// Rope-backed reads slice backing chunks directly, so a range that splits a
/// codepoint is a caller error and panics; the shared set only uses safe
/// offsets.
fn safe_offset(content: &str, target: usize) -> u64 {
    let target = target.min(content.len());
    if content.is_char_boundary(target) {
        return target as u64;
    }
    content
        .char_indices()
        .map(|(offset, _)| offset)
        .take_while(|offset| *offset <= target)
        .last()
        .unwrap_or(0) as u64
}

/// Asserts the borrowed carrier matches the owned carrier for `range`.
pub(crate) fn assert_matches_owned(store: &dyn TextStore, range: ByteRange) {
    match (store.read_byte_range(range), store.read_chunk_bytes(range)) {
        (TextChunkResult::Ready(chunk), ChunkBytes::Ready { bytes, byte_range }) => {
            assert_eq!(bytes.as_ref(), chunk.text.as_bytes(), "bytes differ for {range:?}");
            assert_eq!(byte_range, chunk.byte_range, "byte range differs for {range:?}");
        }
        (TextChunkResult::Pending, ChunkBytes::Pending)
        | (TextChunkResult::Cancelled, ChunkBytes::Cancelled)
        | (TextChunkResult::Unsupported, ChunkBytes::Unsupported) => {}
        (owned, borrowed) => {
            panic!("carrier mismatch for {range:?}: owned={owned:?} borrowed={borrowed:?}")
        }
    }
}

/// Runs the shared conformance set for a store holding `content`.
///
/// Covers: whole document, empty ranges (at both ends and in the interior),
/// out-of-bounds ranges, a codepoint-safe interior split that crosses
/// backing-chunk boundaries, a multibyte range, repeated reads, and carrier
/// agreement across every range `iter_chunks` reports.
///
/// Only carrier agreement is asserted, never a verdict: what a store decides for
/// an out-of-bounds or inverted range is store-specific (rope answers
/// `Unsupported`, VLF answers an empty `Ready`), and pinning one of those here
/// would pin the wrong store's behavior.
pub(crate) fn assert_chunk_bytes_conformance(store: &dyn TextStore, content: &str) {
    let len = content.len() as u64;

    assert_matches_owned(store, ByteRange::new(0, len));
    assert_matches_owned(store, ByteRange::new(0, 0));
    assert_matches_owned(store, ByteRange::new(len, len));
    assert_matches_owned(store, ByteRange::new(len + 1, len + 1));
    assert_matches_owned(store, ByteRange::new(0, len + 1));
    let mid = safe_offset(content, len as usize / 2);
    assert_matches_owned(store, ByteRange::new(mid, mid));
    assert_matches_owned(
        store,
        ByteRange::new(safe_offset(content, 1), safe_offset(content, len as usize / 2)),
    );
    assert_matches_owned(
        store,
        ByteRange::new(
            safe_offset(content, len as usize / 3),
            safe_offset(content, len as usize * 2 / 3),
        ),
    );
    assert_matches_owned(
        store,
        ByteRange::new(
            safe_offset(content, 0),
            safe_offset(content, (len as usize).saturating_sub(1)),
        ),
    );

    // Repeated reads must be stable (no state left behind by the first read).
    for _ in 0..3 {
        assert_matches_owned(store, ByteRange::new(0, len));
    }

    // Every chunk the iterator reports must agree across both carriers, and a
    // fully `Ready` iteration must cover the whole document. Chunk shape itself
    // is store-specific (the VLF iterator decodes page steps without seam
    // adjustment, while range reads do adjust), so no text-level cross-check is
    // asserted beyond carrier agreement.
    let mut covered = 0u64;
    let mut any_pending = false;
    for item in store.iter_chunks(ByteRange::new(0, len)) {
        match item {
            TextChunkResult::Ready(chunk) => {
                assert_matches_owned(store, chunk.byte_range);
                covered += chunk.byte_range.len();
            }
            _ => any_pending = true,
        }
    }
    if !any_pending {
        assert_eq!(covered, len, "iter_chunks must cover the whole document");
    }
}
