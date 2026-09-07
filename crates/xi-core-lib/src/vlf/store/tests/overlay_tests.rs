//! VLF store tests: overlay.
use super::*;

#[test]
fn edit_permission_is_forbidden_for_vlf() {
    let (store, _f) = store_from(b"hello");
    assert_eq!(store.edit_permission(), EditPermission::Forbidden { reason: VLF_READ_ONLY_REASON });
}

#[test]
fn copy_and_search_work_regardless_of_edit_permission() {
    // read_byte_range, iter_chunks, and line/byte lookups must succeed even
    // when edit_permission() returns Forbidden.
    let content = b"line one\nline two\nline three\n";
    let (store, _f) = store_from(content);
    store.scan_all().unwrap();

    assert_eq!(store.edit_permission(), EditPermission::Forbidden { reason: VLF_READ_ONLY_REASON });
    // Navigation/search still works.
    assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    match store.read_byte_range(ByteRange::new(0, 8)) {
        TextChunkResult::Ready(c) => assert_eq!(c.text, "line one"),
        other => panic!("expected Ready, got {:?}", other),
    }
    let chunks: Vec<_> = store.iter_chunks(ByteRange::new(0, 8)).collect();
    assert!(!chunks.is_empty());
}

#[test]
fn overlay_reads_and_line_lookups_include_inserted_text() {
    let (store, _f) = store_from(b"alpha\nbeta\n");
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(2, "XYZ", ctx).unwrap();

    assert_eq!(store.len_bytes(), 14);

    match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
        TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, "alXYZpha\nbeta\n"),
        other => panic!("expected Ready, got {:?}", other),
    }

    assert_eq!(store.line_to_byte(LogicalLine(0)), LineLookup::Exact(ByteOffset(0)));
    assert_eq!(store.line_to_byte(LogicalLine(1)), LineLookup::Exact(ByteOffset(9)));
    assert_eq!(store.byte_to_line(ByteOffset(4)), Some(LogicalLine(0)));
    assert_eq!(store.byte_to_line(ByteOffset(11)), Some(LogicalLine(1)));

    let chunks: Vec<_> = store.iter_chunks(ByteRange::new(0, store.len_bytes())).collect();
    assert_eq!(chunks.len(), 1);
    match &chunks[0] {
        TextChunkResult::Ready(chunk) => assert_eq!(chunk.text, "alXYZpha\nbeta\n"),
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[test]
fn overlay_search_reads_include_inserted_text() {
    let (store, _f) = store_from(b"alpha\nbeta\n");
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(5, " plus", ctx).unwrap();

    let token = store.pager.current_generation();
    let chunk = store.read_search_range(ByteRange::new(0, store.len_bytes()), token).unwrap();
    assert_eq!(chunk.text, "alpha plus\nbeta\n");
}
