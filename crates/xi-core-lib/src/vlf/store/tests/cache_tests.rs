//! VLF store tests: cache.
use super::*;

#[test]
fn viewport_state_initialized_to_zero_window() {
    let (store, _f) = store_from(b"hello");
    let vp = store.viewport_state();
    assert_eq!(vp.window_start.0, 0);
    assert_eq!(vp.window_end.0, 0);
    assert_eq!(vp.original_encoded_len, 0);
    assert!(!vp.dirty);
    assert_eq!(vp.batch_size, DEFAULT_BATCH_SIZE);
}

#[test]
fn set_viewport_updates_window_state() {
    let content = b"hello world";
    let (store, _f) = store_from(content);
    store.set_viewport(ByteOffset(0), ByteOffset(5));
    let vp = store.viewport_state();
    assert_eq!(vp.window_start.0, 0);
    assert_eq!(vp.window_end.0, 5);
    assert_eq!(vp.original_encoded_len, 5);
    assert!(!vp.dirty);
}

#[test]
fn set_batch_size_reflected_in_subsequent_set_viewport() {
    let content = b"hello world";
    let (mut store, _f) = store_from(content);
    store.set_batch_size(512);
    store.set_viewport(ByteOffset(0), ByteOffset(5));
    let vp = store.viewport_state();
    assert_eq!(vp.batch_size, 512);
}

// ---- Decoded text cache -----------------------------------------

#[test]
fn decoded_cache_populated_on_read() {
    let content = b"hello world";
    let (store, _f) = store_from(content);
    assert_eq!(store.decoded_cache_used_bytes(), 0);
    let _ = store.read_byte_range(ByteRange::new(0, 5));
    assert!(store.decoded_cache_used_bytes() > 0, "cache should be populated after read");
}

#[test]
fn decoded_cache_hit_avoids_redundant_decode() {
    let content = b"hello world";
    let (store, _f) = store_from(content);
    // First read populates cache.
    let r1 = store.read_byte_range(ByteRange::new(0, 5));
    let used_after_first = store.decoded_cache_used_bytes();
    // Second read hits cache; used bytes must not grow.
    let r2 = store.read_byte_range(ByteRange::new(0, 5));
    assert_eq!(store.decoded_cache_used_bytes(), used_after_first);
    // Both reads return identical text.
    if let (TextChunkResult::Ready(c1), TextChunkResult::Ready(c2)) = (r1, r2) {
        assert_eq!(c1.text, c2.text);
    } else {
        panic!("expected two Ready results");
    }
}

#[test]
fn decoded_cache_evicts_background_before_viewport() {
    // Content: "abcdef" (6 bytes); page_size=3 → pages [0,3)="abc", [3,6)="def".
    // decoded cache cap=5 bytes; batch_size=0 so overscan==viewport, making [3,6) Background.
    let content = b"abcdef";
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, content).unwrap();
    std::io::Write::flush(&mut f).unwrap();
    let store = VlfStore {
        pager: FilePager::open_with_config(f.path(), 1024 * 1024, DEFAULT_MAX_READ_SIZE).unwrap(),
        index: RefCell::new(PageIndex::new(content.len() as u64)),
        page_size: 3,
        viewport: RefCell::new(VlfViewportState::new(0)),
        decoded_cache: RefCell::new(DecodedTextCache::new(5)),
        syntax_cache: RefCell::new(ViewportSyntaxCache::new()),
        batch_size: 0, // overscan == viewport, so [3,6) is Background when viewport=[0,3)
        stats: RefCell::new(VlfMemoryStats::default()),
        first_viewport_set: Cell::new(false),
        scan_rx: RefCell::new(None),
        bg_cancel: Arc::new(AtomicBool::new(false)),
        approx_line_floor: Cell::new(0),
        exact_line_count: Cell::new(None),
        overlay: RefCell::new(None),
    };

    // Prime a background entry at [3,6) (before viewport is set).
    let _ = store.read_with_seam(ByteRange::new(3, 6)).unwrap();
    let used_bg = store.decoded_cache_used_bytes();
    assert!(used_bg > 0, "cache should have the background entry");

    // Set viewport over [0,3); with batch_size=0, [3,6) becomes Background.
    store.set_viewport(ByteOffset(0), ByteOffset(3));

    // Reading [0,3) should evict the background [3,6) entry to stay within cap=5.
    let _ = store.read_with_seam(ByteRange::new(0, 3)).unwrap();

    // Cache must not exceed cap.
    assert!(
        store.decoded_cache_used_bytes() <= 5,
        "cache exceeded cap: {} bytes",
        store.decoded_cache_used_bytes()
    );
}

// ---- Visible-syntax-span memo --------------------------------------

#[test]
fn syntax_cache_computes_once_for_same_window() {
    let (store, _f) = store_from(b"fn main() {}\nlet x = 1;\n");
    let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let c = std::rc::Rc::clone(&calls);
    let compute_a = move || {
        c.set(c.get() + 1);
        vec![Vec::new()]
    };
    let c = std::rc::Rc::clone(&calls);
    let compute_b = move || {
        c.set(c.get() + 1);
        vec![Vec::new()]
    };
    let r1 = store.cached_visible_syntax_spans("fn main() {}\n", "rust", compute_a);
    let r2 = store.cached_visible_syntax_spans("fn main() {}\n", "rust", compute_b);
    assert_eq!(r1, r2);
    assert_eq!(calls.get(), 1, "identical window should compute spans once");
}

#[test]
fn syntax_cache_recomputes_on_window_or_language_change() {
    let (store, _f) = store_from(b"fn main() {}\n");
    let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let mk = |calls: std::rc::Rc<std::cell::Cell<usize>>| {
        move || {
            calls.set(calls.get() + 1);
            vec![Vec::new()]
        }
    };
    let _ = store.cached_visible_syntax_spans("window a", "rust", mk(std::rc::Rc::clone(&calls)));
    let _ = store.cached_visible_syntax_spans("window a", "rust", mk(std::rc::Rc::clone(&calls)));
    let _ = store.cached_visible_syntax_spans("window b", "rust", mk(std::rc::Rc::clone(&calls)));
    let _ = store.cached_visible_syntax_spans("window b", "c", mk(std::rc::Rc::clone(&calls)));
    assert_eq!(calls.get(), 3, "hit once, then window change and language change each recompute");
}

#[test]
fn syntax_cache_bypasses_when_overlay_editing_active() {
    let (store, _f) = store_from(b"fn main() {}\n");
    store.enable_editing();
    if let Some(overlay) = store.overlay.borrow_mut().as_mut() {
        overlay.set_read_byte_range_ready();
    }
    assert!(store.overlay_read_enabled(), "test precondition: overlay reads on");

    let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let mk = |calls: std::rc::Rc<std::cell::Cell<usize>>| {
        move || {
            calls.set(calls.get() + 1);
            vec![Vec::new()]
        }
    };
    let _ = store.cached_visible_syntax_spans("same", "rust", mk(std::rc::Rc::clone(&calls)));
    let _ = store.cached_visible_syntax_spans("same", "rust", mk(std::rc::Rc::clone(&calls)));
    assert_eq!(calls.get(), 2, "overlay windows must bypass the memo");
}
