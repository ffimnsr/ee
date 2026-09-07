//! VLF store tests: viewport.
use super::*;

#[test]
fn scan_viewport_first_covers_all_pages() {
    let content: Vec<u8> = (0..10).flat_map(|i| format!("line{i}\n").into_bytes()).collect();
    let (store, _f) = store_from(&content);
    let viewport = ByteRange::new(0, 20);
    store.scan_viewport_first(viewport, || false).unwrap();
    assert!(store.index().scan_progress().is_complete());
}

#[test]
fn scan_viewport_first_scans_viewport_before_tail() {
    // Content: 10 * "line\n" = 50 bytes.  page_size=10 → 5 pages.
    // Viewport covers page 2 ([20,30)).  After scanning only the first
    // (viewport) step, page 2 must already be scanned.
    let content: Vec<u8> = b"0123456789".repeat(5).to_vec(); // 50 bytes, no newlines
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(&content).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 10, 1024 * 1024).unwrap();

    // Track how many pages are scanned on the viewport pass by stopping
    // after the viewport pages are done (cancel after 1 non-viewport page).
    let viewport = ByteRange::new(20, 30); // page 2
    let mut extra_count = 0u32;
    store
        .scan_viewport_first(viewport, || {
            // Count pages beyond the viewport; stop after 1 expansion step.
            let scanned = store.index().scan_progress().scanned_bytes;
            // After viewport (10 bytes) is scanned, allow one expansion step.
            if scanned > 10 {
                extra_count += 1;
                extra_count > 2
            } else {
                false
            }
        })
        .unwrap();

    // Page 2 (viewport) must be scanned.
    let idx = store.index();
    let desc = idx.page_at_byte(20).expect("page 2 should be scanned");
    assert_eq!(desc.scan_state, ScanState::Scanned);
}

#[test]
fn scan_viewport_first_cancellable() {
    let content: Vec<u8> = b"x".repeat(1024).to_vec();
    let (store, _f) = store_from(&content);
    let viewport = ByteRange::new(0, 64);
    // Cancel immediately after first page.
    let mut count = 0;
    store
        .scan_viewport_first(viewport, || {
            count += 1;
            count > 1
        })
        .unwrap();
    // Only a subset should be scanned.
    assert!(!store.index().scan_progress().is_complete());
}

// ---- VlfMemoryBudget / VlfMemoryStats ---------------------------

#[test]
fn open_with_budget_uses_provided_caps() {
    let content = b"hello world";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();
    let budget = VlfMemoryBudget { raw_page_byte_cap: 4096, decoded_byte_cap: 2048 };
    let store = VlfStore::open_with_budget(f.path(), budget).unwrap();
    // Should open successfully; reads should still work.
    match store.read_byte_range(ByteRange::new(0, 5)) {
        TextChunkResult::Ready(c) => assert_eq!(c.text, "hello"),
        other => panic!("expected Ready, got {:?}", other),
    }
}

#[test]
fn memory_stats_tracks_peak_decoded_bytes() {
    let content = b"hello world, this is some longer text for cache tracking";
    let (store, _f) = store_from(content);
    let before = store.memory_stats().peak_decoded_bytes;
    assert_eq!(before, 0);
    let _ = store.read_byte_range(ByteRange::new(0, content.len() as u64));
    let after = store.memory_stats().peak_decoded_bytes;
    assert!(after > 0, "peak_decoded_bytes should increase after a read");
}

#[test]
fn memory_stats_tracks_descriptor_bytes() {
    let content = b"line one\nline two\nline three\n";
    let (store, _f) = store_from(content);
    assert_eq!(store.memory_stats().descriptor_bytes, 0);
    store.scan_all().unwrap();
    let stats = store.memory_stats();
    // At least one descriptor should have been tracked.
    assert!(stats.descriptor_bytes > 0, "descriptor_bytes must be non-zero after scan_all");
}

#[test]
fn memory_stats_overlay_bytes_zero_in_read_only_milestone() {
    let (store, _f) = store_from(b"data");
    store.scan_all().unwrap();
    let _ = store.read_byte_range(ByteRange::new(0, 4));
    assert_eq!(store.memory_stats().peak_overlay_bytes, 0);
}

#[test]
fn bytes_before_first_viewport_counts_reads_before_set_viewport() {
    // bytes_before_first_viewport must accumulate pager reads made before
    // set_viewport is called, then stop once the viewport is set.
    let (store, _f) = store_from(b"hello world");

    // Pre-viewport read: 11 bytes.
    let _ = store.read_byte_range(ByteRange::new(0, 11));
    let before = store.memory_stats().bytes_before_first_viewport;
    assert!(before > 0, "expected non-zero pre-viewport byte count, got {before}");

    // Set the viewport; counter must stop.
    store.set_viewport(ByteOffset(0), ByteOffset(11));

    // Post-viewport read.
    let _ = store.read_byte_range(ByteRange::new(0, 11));
    let after = store.memory_stats().bytes_before_first_viewport;
    assert_eq!(before, after, "bytes_before_first_viewport should not grow after set_viewport");
}

#[test]
fn ten_gib_sparse_fixture_stays_within_configured_budget() {
    let ten_gib = 10u64 * 1024 * 1024 * 1024;
    let mut f = NamedTempFile::new().unwrap();
    f.as_file().set_len(ten_gib).unwrap();
    f.write_all(b"alpha\nbeta\n").unwrap();
    f.flush().unwrap();

    let budget = VlfMemoryBudget { raw_page_byte_cap: 64 * 1024, decoded_byte_cap: 16 * 1024 };
    let store = VlfStore::open_with_budget(f.path(), budget.clone()).unwrap();

    match store.read_byte_range(ByteRange::new(0, 8 * 1024)) {
        TextChunkResult::Ready(chunk) => assert!(chunk.text.starts_with("alpha\nbeta\n")),
        other => panic!("expected Ready, got {:?}", other),
    }

    let stats = store.memory_stats();
    assert_eq!(store.len_bytes(), ten_gib);
    assert!(
        stats.peak_raw_bytes <= budget.raw_page_byte_cap,
        "raw cache {} exceeded cap {}",
        stats.peak_raw_bytes,
        budget.raw_page_byte_cap
    );
    assert!(
        stats.peak_decoded_bytes <= budget.decoded_byte_cap,
        "decoded cache {} exceeded cap {}",
        stats.peak_decoded_bytes,
        budget.decoded_byte_cap
    );
}

#[test]
fn first_viewport_read_does_not_require_full_scan() {
    let content = (0..20_000).map(|i| format!("line {i}\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 256, 64 * 1024).unwrap();

    store.set_viewport(ByteOffset(0), ByteOffset(256));

    match store.read_byte_range(ByteRange::new(0, 256)) {
        TextChunkResult::Ready(chunk) => assert!(chunk.text.starts_with("line 0\nline 1\n")),
        other => panic!("expected Ready, got {:?}", other),
    }

    assert_eq!(store.known_line_count(), KnownLineCount::Unknown);
    assert_eq!(store.index().len(), 0, "first viewport read must not force page-index scan");
    assert!(
        matches!(store.line_to_byte(LogicalLine(10_000)), LineLookup::Pending),
        "unscanned tail should stay unresolved after first viewport read"
    );
}

#[test]
fn decoded_cache_stays_within_budget_cap() {
    // Small decoded cap: 20 bytes.  Content is 50 bytes spanning 5 × 10-byte pages.
    // Reading all 50 bytes should not exceed the cap.
    let content: Vec<u8> = b"0123456789".repeat(5).to_vec();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(&content).unwrap();
    f.flush().unwrap();
    let budget = VlfMemoryBudget {
        raw_page_byte_cap: 1024 * 1024,
        decoded_byte_cap: 20, // only 2 pages can fit
    };
    let store = VlfStore::open_with_budget(f.path(), budget).unwrap();
    for start in (0u64..50).step_by(10) {
        let _ = store.read_byte_range(ByteRange::new(start, start + 10));
    }
    assert!(
        store.decoded_cache_used_bytes() <= 20,
        "decoded cache exceeded budget: {} bytes",
        store.decoded_cache_used_bytes()
    );
}
