//! VLF store tests: edit.
use super::*;

#[test]
fn edit_permission_forbidden_before_enable_editing() {
    let content = b"hello world";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    assert!(
        matches!(store.edit_permission(), EditPermission::Forbidden { .. }),
        "should be Forbidden before enable_editing"
    );
}

#[test]
fn edit_permission_allowed_after_enable_editing() {
    let content = b"hello world";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    store.enable_editing();
    assert!(
        matches!(store.edit_permission(), EditPermission::Allowed),
        "should be Allowed after enable_editing"
    );
}

#[test]
fn apply_insert_without_enable_editing_returns_error() {
    use crate::vlf::overlay::OverlayEditContext;
    let content = b"hello";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    let err = store.apply_insert(5, " world", ctx).unwrap_err();
    assert!(
        matches!(err, VlfEditError::EditingNotEnabled),
        "expected EditingNotEnabled, got {err:?}"
    );
}

#[test]
fn apply_insert_after_enable_editing_succeeds() {
    use crate::vlf::overlay::OverlayEditContext;
    let content = b"hello";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(5, " world", ctx).unwrap();
    // signed_byte_delta should reflect the 6 inserted bytes.
    assert_eq!(store.signed_byte_delta(), 6);
}

#[test]
fn apply_delete_after_enable_editing_succeeds() {
    use crate::vlf::overlay::OverlayEditContext;
    let content = b"hello world";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    use crate::text_store::ByteRange;
    store.apply_delete(ByteRange::new(5, 11), ctx).unwrap();
    assert_eq!(store.signed_byte_delta(), -6);
}

#[test]
fn apply_insert_mid_file_preserves_surrounding_content() {
    use crate::vlf::overlay::OverlayEditContext;

    let content = b"alpha\nbeta\ngamma\n";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(10, " needle", ctx).unwrap();

    match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
        TextChunkResult::Ready(chunk) => {
            assert_eq!(chunk.text, "alpha\nbeta needle\ngamma\n");
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn apply_insert_mid_file_with_small_page_size_preserves_surrounding_content() {
    use crate::vlf::overlay::OverlayEditContext;

    let content = b"alpha\nbeta\ngamma\n";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(10, " needle", ctx).unwrap();

    match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
        TextChunkResult::Ready(chunk) => {
            assert_eq!(chunk.text, "alpha\nbeta needle\ngamma\n");
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn apply_insert_mid_file_after_scan_all_preserves_surrounding_content() {
    use crate::vlf::overlay::OverlayEditContext;

    let content = b"alpha\nbeta\ngamma\n";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
    store.scan_all().unwrap();
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(10, " needle", ctx).unwrap();

    match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
        TextChunkResult::Ready(chunk) => {
            assert_eq!(chunk.text, "alpha\nbeta needle\ngamma\n");
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn apply_delete_then_insert_replaces_mid_file_range() {
    use crate::vlf::overlay::OverlayEditContext;

    let content = b"alpha\nbeta\ngamma\n";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_delete(ByteRange::new(6, 10), ctx).unwrap();
    store.apply_insert(6, "BETA!", ctx).unwrap();

    match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
        TextChunkResult::Ready(chunk) => {
            assert_eq!(chunk.text, "alpha\nBETA!\ngamma\n");
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn suggested_save_policy_none_before_enable_editing() {
    let content = b"hello";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    assert!(store.suggested_save_policy().is_none(), "no policy before editing");
}

#[test]
fn suggested_save_policy_tail_shift_after_small_insert() {
    use crate::vlf::overlay::OverlayEditContext;
    let content = b"hello";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(5, " world", ctx).unwrap();
    assert!(
        matches!(
            store.suggested_save_policy(),
            Some(crate::vlf::overlay::VlfSavePolicy::TailShift { .. })
        ),
        "small insert should suggest TailShift"
    );
}

#[test]
fn enable_editing_called_twice_is_noop() {
    let content = b"hello world";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    store.enable_editing();
    store.enable_editing(); // second call must not panic or reset overlay
    assert!(matches!(store.edit_permission(), EditPermission::Allowed));
}

#[test]
fn peak_overlay_bytes_tracked_after_insert() {
    use crate::vlf::overlay::OverlayEditContext;
    let content = b"hello";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open(f.path()).unwrap();
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(5, " world", ctx).unwrap();
    assert!(store.memory_stats().peak_overlay_bytes > 0, "overlay bytes should be > 0");
}
