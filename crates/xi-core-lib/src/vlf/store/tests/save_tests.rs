//! VLF store tests: save.
use super::*;

#[test]
fn doc_status_mode_name_is_vlf() {
    let (store, _f) = store_from(b"hello");
    let status = store.doc_status();
    assert_eq!(status.mode_name, "vlf");
}

#[test]
fn doc_status_file_size_matches_content() {
    let (store, _f) = store_from(b"hello world");
    let status = store.doc_status();
    assert_eq!(status.file_size_bytes, 11);
}

#[test]
fn doc_status_disabled_features_excludes_search() {
    let (store, _f) = store_from(b"hi");
    let status = store.doc_status();
    assert!(!status.disabled_features.contains(&"search"), "search should be available in VLF");
    assert!(status.disabled_features.contains(&"editing"), "editing must be disabled");
    assert!(status.disabled_features.contains(&"save"), "save must be disabled");
    assert!(status.disabled_features.contains(&"undo"), "undo must be disabled");
    assert!(status.disabled_features.contains(&"lsp"), "lsp must be disabled");
}

#[test]
fn doc_status_editable_vlf_reports_edit_and_save_enabled() {
    let (store, _f) = store_from(b"hi");
    store.enable_editing();

    let status = store.doc_status();
    assert!(!status.disabled_features.contains(&"editing"), "editing should be enabled");
    assert!(!status.disabled_features.contains(&"save"), "save should be enabled");
    assert!(status.disabled_features.contains(&"undo"), "undo should stay disabled");
    assert!(status.disabled_features.contains(&"lsp"), "other VLF restrictions remain");
    assert!(store.is_editing_enabled());
    assert!(store.is_save_enabled());
}

#[test]
fn refresh_after_save_rebases_overlay_without_leaving_vlf_mode() {
    let (store, file) = store_from(b"alpha\n");
    store.enable_editing();
    let ctx = OverlayEditContext { revision_id: 1, undo_group: 1 };
    store.apply_insert(6, "beta\n", ctx).unwrap();

    std::fs::write(file.path(), b"alpha\nbeta\n").unwrap();

    let mut store = store;
    store.refresh_after_save(file.path()).unwrap();

    assert!(store.is_editing_enabled());
    assert!(store.is_save_enabled());
    assert_eq!(store.signed_byte_delta(), 0);
    assert!(matches!(store.suggested_save_policy(), Some(VlfSavePolicy::SameSizeInPlaceOverwrite)));
    assert_eq!(store.known_line_count(), KnownLineCount::Unknown);
}

#[test]
fn refresh_after_save_restarts_background_indexing_only_when_already_active() {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(b"alpha\nbeta\n").unwrap();
    file.flush().unwrap();

    let mut without_indexer = VlfStore::open_with_config(file.path(), 64, 1024 * 1024).unwrap();
    without_indexer.refresh_after_save(file.path()).unwrap();
    assert!(
        without_indexer.scan_rx.borrow().is_none(),
        "refresh should not start background indexing for stores that never enabled it"
    );

    let mut with_indexer = VlfStore::open_with_config(file.path(), 64, 1024 * 1024).unwrap();
    with_indexer.start_background_indexing();
    assert!(with_indexer.scan_rx.borrow().is_some(), "precondition: scanner should be active");

    with_indexer.refresh_after_save(file.path()).unwrap();
    assert!(
        with_indexer.scan_rx.borrow().is_some(),
        "refresh should restart background indexing when it was already active"
    );
}

#[test]
fn doc_status_indexing_progress_zero_before_scan() {
    let (store, _f) = store_from(b"abc\ndef\n");
    let status = store.doc_status();
    // Nothing has been scanned yet.
    assert_eq!(status.indexing_progress, 0.0);
}

#[test]
fn doc_status_indexing_progress_one_after_full_scan() {
    let (store, _f) = store_from(b"abc\ndef\n");
    store.scan_all().unwrap();
    let status = store.doc_status();
    assert!((status.indexing_progress - 1.0).abs() < f64::EPSILON);
}

// ---- Background indexing ----------------------------------------

#[test]
fn start_background_indexing_eventually_completes() {
    // Create content with several pages' worth of data.
    let content: Vec<u8> = (0..20).flat_map(|i| format!("line {i:04}\n").into_bytes()).collect();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(&content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open_with_config(f.path(), 16, 1024 * 1024).unwrap();
    store.start_background_indexing();

    // Poll with a timeout to wait for background scan to complete.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        store.drain_incoming();
        if store.index().scan_progress().is_complete() {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("background indexing did not complete within 5 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Line count must be exact after full scan.
    match store.known_line_count() {
        KnownLineCount::Exact(_) => {}
        other => panic!("expected Exact line count after full scan, got {:?}", other),
    }
}

#[test]
fn start_background_indexing_idempotent() {
    // Calling start_background_indexing twice must not panic or spawn two threads.
    let (store, _f) = store_from(b"hello\nworld\n");
    store.start_background_indexing();
    store.start_background_indexing(); // second call is a no-op
}

#[test]
fn background_indexing_produces_correct_newline_count() {
    let content = b"a\nb\nc\nd\ne\n";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    // page_size=4 → pages overlap different newlines.
    let store = VlfStore::open_with_config(f.path(), 4, 1024 * 1024).unwrap();
    store.start_background_indexing();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        store.drain_incoming();
        if store.index().scan_progress().is_complete() {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("background indexing did not complete within 5 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    // Content has 5 newlines → 6 lines.
    match store.known_line_count() {
        KnownLineCount::Exact(n) => assert_eq!(n, 6, "expected 6 lines"),
        other => panic!("expected Exact, got {:?}", other),
    }
}

// ---- Stable line numbers (approx_line_floor) --------------------

#[test]
fn approximate_line_count_never_decreases() {
    // Construct a file where the first page is denser with newlines than
    // the rest, so the extrapolated estimate would drop as more pages are
    // scanned.  The floor must prevent any decrease.
    //
    // Page 0 (8 bytes): "a\nb\nc\n\n" — 4 newlines in 8 bytes (dense).
    // Page 1 (8 bytes): "xxxxxxxx" — 0 newlines in 8 bytes (sparse).
    // Total = 16 bytes, 4 newlines → Exact(5) after full scan.
    // After page 0 only: estimate = 4/8 * 16 = 8 lines (over-estimate).
    // After pages 0+1: scan is complete → Exact(5).
    // The floor ensures the Approximate value only increased until exact.
    let content = b"a\nb\nc\n\nxxxxxxxx";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open_with_config(f.path(), 8, 1024 * 1024).unwrap();

    // Scan only the first page and observe the approximate count.
    store.scan_page_at(0).unwrap();
    let first_approx = match store.known_line_count() {
        KnownLineCount::Approximate(n) => n,
        // If the single-page scan already produced Exact, the floor test
        // is vacuous — the index was fully covered in one pass.
        KnownLineCount::Exact(_) | KnownLineCount::Unknown => return,
    };

    // Scan the second page; the returned value must be >= first_approx
    // **unless** we now have an Exact value (which is always authoritative).
    store.scan_page_at(8).unwrap();
    match store.known_line_count() {
        KnownLineCount::Approximate(n) => {
            assert!(
                n >= first_approx,
                "Approximate line count decreased from {first_approx} to {n}"
            );
        }
        // Exact is always authoritative; no floor assertion needed.
        KnownLineCount::Exact(_) | KnownLineCount::Unknown => {}
    }
}

#[test]
fn floor_preserved_across_multiple_drain_calls() {
    // After establishing a floor via known_line_count, subsequent calls
    // must not return a lower value even before the scan is complete.
    let content: Vec<u8> = b"line\n".repeat(100);
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(&content).unwrap();
    f.flush().unwrap();

    let store = VlfStore::open_with_config(f.path(), 10, 1024 * 1024).unwrap();
    store.scan_page_at(0).unwrap(); // scan first page only

    let first = match store.known_line_count() {
        KnownLineCount::Approximate(n) => n,
        KnownLineCount::Exact(_) => return, // already done; test not applicable
        KnownLineCount::Unknown => return,
    };

    // Second call must return >= first (floor is preserved).
    match store.known_line_count() {
        KnownLineCount::Approximate(n) => {
            assert!(n >= first, "second call returned {n} < {first}");
        }
        // Exact is authoritative; no floor assertion needed.
        KnownLineCount::Exact(_) | KnownLineCount::Unknown => {}
    }
}
