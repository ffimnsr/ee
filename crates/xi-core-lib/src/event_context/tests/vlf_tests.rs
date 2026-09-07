//! Event-context tests: vlf.
use super::*;

#[test]
fn vlf_viewport_sends_correct_lines_for_scanned_file() {
    use crate::rpc::EditNotification;

    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\ndelta\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 1, generation: 1 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");

    assert_eq!(params["generation"], 1u64);
    assert_eq!(params["line_start"], 0u64);
    let lines = params["lines"].as_array().expect("lines must be array");
    assert_eq!(lines.len(), 2, "should return exactly the requested line count");
    assert_eq!(lines[0].as_str(), Some("alpha"));
    assert_eq!(lines[1].as_str(), Some("beta"));
}

#[test]
fn vlf_viewport_sends_full_crlf_line_range() {
    use crate::rpc::EditNotification;

    let content = (0..200).map(|i| format!("line {i}\r\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();
    store.scan_all().unwrap();
    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.editor.borrow_mut().enable_vlf_editing();
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 100, line_end: 140, generation: 12 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(params["line_start"], 100u64);
    assert_eq!(lines.len(), 41, "CRLF viewport should return full requested range");
    assert_eq!(lines.first().and_then(|line| line.as_str()), Some("line 100\r"));
    assert_eq!(lines.last().and_then(|line| line.as_str()), Some("line 140\r"));
}

#[test]
fn vlf_selected_text_reads_from_text_store() {
    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\n");
    harness
        .view
        .borrow_mut()
        .set_selection(&Rope::from("alpha\nbeta\ngamma\n"), SelRegion::new(1, 8));
    let mut ctx = harness.make_context();

    assert_eq!(ctx.preview_selected_text(false), "lpha\nbe");
    assert_eq!(ctx.preview_selected_text(true), "alpha\nbeta\n");
}

#[test]
fn vlf_viewport_uses_prefix_fallback_for_pending_index() {
    use crate::rpc::EditNotification;

    let mut f = NamedTempFile::new().unwrap();
    let content =
        (0..200).map(|i| format!("line {i} {}\n", "x".repeat(128 * 1024))).collect::<String>();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 20, line_end: 25, generation: 42 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification even for pending index");
    assert_eq!(params["generation"], 42u64);
    let lines = params["lines"].as_array().unwrap();
    assert_eq!(params["line_start"], 20u64);
    assert_eq!(lines.len(), 6, "prefix fallback should satisfy near-top viewport");
    assert!(
        lines
            .first()
            .and_then(|line| line.as_str())
            .is_some_and(|line| line.starts_with("line 20 "))
    );
    assert!(
        lines
            .last()
            .and_then(|line| line.as_str())
            .is_some_and(|line| line.starts_with("line 25 "))
    );
}

#[test]
fn vlf_viewport_estimates_unknown_line_count_from_decoded_chunk() {
    use crate::rpc::EditNotification;

    let content = (0..200).map(|i| format!("line {i}\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 2, generation: 7 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");

    let approximate = params["approximate_line_count"].as_u64().unwrap();
    assert!(approximate > 103, "estimate should not crawl by line_end + 100, got {approximate}");
    assert!(!params["line_count_exact"].as_bool().unwrap());
}

#[test]
fn vlf_viewport_near_approx_end_returns_tail_lines() {
    use crate::rpc::EditNotification;

    let content = (0..200).map(|i| format!("line {i}\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 190, line_end: 210, generation: 8 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let response_line_start = params["line_start"].as_u64().unwrap();
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(response_line_start, 190);
    assert_eq!(lines.first().and_then(|line| line.as_str()), Some("line 190"));
    assert!(lines.iter().any(|line| line.as_str() == Some("line 199")));
}

#[test]
fn vlf_viewport_tail_sentinel_returns_file_tail_without_index() {
    use crate::rpc::EditNotification;

    let content = (0..200).map(|i| format!("line {i}\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: u64::MAX, line_end: 4, generation: 9 });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(params["generation"], 9u64);
    assert_eq!(params["approximate_line_count"], 201u64);
    assert!(params["line_count_exact"].as_bool().unwrap());
    assert_eq!(params["line_start"], 196u64);
    assert!(lines.iter().any(|line| line.as_str() == Some("line 199")));
}

#[test]
fn vlf_viewport_tail_sentinel_returns_tail_without_exact_line_count_scan() {
    use crate::rpc::EditNotification;

    let line = format!("{}\n", "x".repeat(1023));
    let content = line.repeat(33 * 1024);
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport {
        line_start: u64::MAX,
        line_end: 4,
        generation: 10,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");

    assert_eq!(params["generation"], 10u64);
    assert!(!params["line_count_exact"].as_bool().unwrap());
    let lines = params["lines"].as_array().expect("lines must be array");
    assert_eq!(lines.len(), 5);
    assert!(params["approximate_line_count"].as_u64().unwrap() >= 5);
}

#[test]
fn vlf_viewport_tail_sentinel_can_request_full_viewport_without_index() {
    use crate::rpc::EditNotification;

    let line = format!("{}\n", "x".repeat(1023));
    let content = line.repeat(33 * 1024);
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport {
        line_start: u64::MAX,
        line_end: 20,
        generation: 11,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(params["generation"], 11u64);
    assert!(!params["line_count_exact"].as_bool().unwrap());
    assert_eq!(lines.len(), 21);
}

#[test]
fn vlf_viewport_approximate_anchor_returns_lines_without_exact_index() {
    use crate::rpc::EditNotification;

    let content = (0..40_000).map(|i| format!("line {i}\n")).collect::<String>();
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 4096, 1024 * 1024).unwrap();
    store.scan_page_at(0).unwrap();
    assert!(matches!(store.line_to_byte(LogicalLine(20_000)), LineLookup::Approximate(_)));

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport {
        line_start: 20_000,
        line_end: 20_010,
        generation: 12,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(m, _)| m == "vlf_chunks")
        .expect("expected vlf_chunks notification");
    let lines = params["lines"].as_array().expect("lines must be array");

    assert_eq!(params["generation"], 12u64);
    assert_eq!(params["line_start"], 20_000u64);
    assert!(!params["line_count_exact"].as_bool().unwrap());
    assert!(!lines.is_empty());
}

#[test]
fn vlf_viewport_ignored_for_normal_buffer() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("hello\nworld\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 1, generation: 1 });

    let notifications = harness.take_notifications();
    assert!(
        !notifications.iter().any(|(m, _)| m == "vlf_chunks"),
        "vlf_chunks must not be sent for normal (non-VLF) buffers: {notifications:?}"
    );
}

#[test]
fn vlf_find_emits_search_status_with_ranges() {
    use crate::rpc::EditNotification;

    let (harness, _f) = vlf_harness(b"alpha\nbeta needle\ngamma needle\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Find {
        chars: String::from("needle"),
        case_sensitive: true,
        regex: false,
        whole_words: false,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "vlf_search_status")
        .expect("expected vlf_search_status notification");

    assert_eq!(params["query"], "needle");
    assert_eq!(params["stored_match_count"], 2u64);
    assert_eq!(params["complete"], true);
    let ranges = params["ranges"].as_array().expect("ranges array");
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0]["line"], 1u64);
    assert_eq!(ranges[0]["start_col"], 5u64);
    assert_eq!(ranges[0]["end_col"], 11u64);
}

#[test]
fn render_if_needed_skips_vlf_placeholder_rope_selection_offsets() {
    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\n");
    harness.view.borrow_mut().set_vlf_selection(SelRegion::caret(10));
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.render_if_needed();

    let notifications = harness.take_notifications();
    assert!(
        !notifications.iter().any(|(method, _)| method == "update"),
        "VLF render path must not try to render placeholder rope state"
    );
}

#[test]
fn vlf_replace_range_notification_updates_text_then_search_and_viewport() {
    use crate::rpc::EditNotification;
    use crate::text_store::{ByteRange, TextChunkResult};

    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\n");
    harness.editor.borrow_mut().enable_vlf_editing();
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::VlfReplaceRange {
        start_line: 1,
        start_col: 4,
        end_line: 1,
        end_col: 4,
        text: String::from(" needle"),
    });

    {
        let editor = harness.editor.borrow();
        let store = editor.vlf_store.as_ref().expect("expected VLF store");
        match store.read_byte_range(ByteRange::new(0, store.len_bytes())) {
            TextChunkResult::Ready(chunk) => {
                assert_eq!(chunk.text, "alpha\nbeta needle\ngamma\n");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    let notifications = harness.take_notifications();
    let (_, scroll) = notifications
        .iter()
        .find(|(method, _)| method == "scroll_to")
        .expect("expected scroll_to after VLF edit");
    assert_eq!(scroll["line"], 1u64);
    assert_eq!(scroll["col"], 11u64);

    ctx.do_edit(EditNotification::Find {
        chars: String::from("needle"),
        case_sensitive: true,
        regex: false,
        whole_words: false,
    });

    let notifications = harness.take_notifications();
    let (_, status) = notifications
        .iter()
        .find(|(method, _)| method == "vlf_search_status")
        .expect("expected vlf_search_status after edit");
    assert_eq!(status["stored_match_count"], 1u64);
    assert_eq!(status["ranges"][0]["line"], 1u64);
    assert_eq!(status["ranges"][0]["start_col"], 5u64);
    assert_eq!(status["ranges"][0]["end_col"], 11u64);

    ctx.do_edit(EditNotification::VlfViewport { line_start: 1, line_end: 1, generation: 77 });

    let notifications = harness.take_notifications();
    let (_, viewport) = notifications
        .iter()
        .find(|(method, _)| method == "vlf_chunks")
        .expect("expected vlf_chunks after edit");
    assert_eq!(viewport["generation"], 77u64);
    let lines = viewport["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].as_str(), Some("beta needle"));
}

#[test]
fn vlf_find_next_scrolls_to_first_known_match() {
    use crate::rpc::EditNotification;

    let (harness, _f) = vlf_harness(b"alpha\nbeta needle\ngamma needle\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Find {
        chars: String::from("needle"),
        case_sensitive: true,
        regex: false,
        whole_words: false,
    });
    harness.take_notifications();

    ctx.do_edit(EditNotification::FindNext {
        wrap_around: true,
        allow_same: false,
        modify_selection: SelectionModifier::Set,
    });

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "scroll_to")
        .expect("expected scroll_to notification");

    assert_eq!(params["line"], 1u64);
    assert_eq!(params["col"], 5u64);
}

#[test]
fn vlf_syntax_selection_uses_visible_range_parse() {
    use crate::rpc::EditNotification;

    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let source = b"fn main() { foo(bar); }\n";
    let (harness, _f) = vlf_harness(source);
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 0, generation: 1 });
    harness.take_notifications();

    let source = String::from_utf8(source.to_vec()).unwrap();
    {
        let editor = ctx.editor.borrow();
        let store = editor.vlf_store.as_ref().expect("vlf store");
        let window_range = ctx.current_vlf_semantic_range(store).expect("semantic window");
        let window_text = ctx
            .current_vlf_semantic_window_text(store, window_range, "rust", None)
            .expect("semantic window text");
        assert_eq!(window_text, source);
        assert_eq!(window_range.start.0 as usize, 0);
        assert_eq!(window_range.end.0 as usize, source.len());
    }
    let start = source.find("bar").unwrap();
    let end = start + 3;
    harness.view.borrow_mut().set_vlf_selection(SelRegion::new(start, end));

    ctx.do_syntax_selection(crate::object::SyntaxSelectionAction::Expand);

    let notifications = harness.take_notifications();
    assert!(
        notifications.iter().all(|(method, _)| method != "alert"),
        "VLF semantic selection should not alert when current node is inside parsed window: {notifications:?}"
    );

    let selection = harness.view.borrow().sel_regions()[0];
    assert!(selection.min() <= start);
    assert!(selection.max() >= end);
    assert!(selection.min() < start || selection.max() > end);
}

#[test]
fn vlf_syntax_navigation_stays_bounded_to_visible_range() {
    use crate::rpc::EditNotification;

    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let (harness, _f) = vlf_harness(b"fn alpha() {}\nfn beta() {}\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 0, generation: 1 });
    harness.take_notifications();

    {
        let editor = ctx.editor.borrow();
        let store = editor.vlf_store.as_ref().expect("vlf store");
        let window_range = ctx.current_vlf_semantic_range(store).expect("semantic window");
        let window_text = ctx
            .current_vlf_semantic_window_text(store, window_range, "rust", None)
            .expect("semantic window text");
        assert_eq!(window_text, "fn alpha() {}\n");
        assert_eq!(window_range.start.0 as usize, 0);
        assert_eq!(window_range.end.0 as usize, "fn alpha() {}\n".len());
    }

    ctx.do_syntax_navigation(crate::object::SyntaxNavigationAction::new(
        SyntaxNavigationTarget::Function,
        true,
    ));

    let notifications = harness.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected alert notification");
    assert_eq!(params["msg"].as_str(), Some("goto_next_function: outside current parsed range"));
}

#[test]
fn vlf_syntax_navigation_uses_visible_range_parse() {
    use crate::rpc::EditNotification;

    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let (harness, _f) = vlf_harness(b"fn alpha() {}\nfn beta() {}\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 2, generation: 1 });
    harness.take_notifications();

    ctx.do_syntax_navigation(crate::object::SyntaxNavigationAction::new(
        SyntaxNavigationTarget::Function,
        true,
    ));

    let selection = harness.view.borrow().sel_regions()[0];
    assert!(selection.is_caret());
    assert_eq!(selection.min(), "fn alpha() {}\n".len());
    assert!(
        harness.take_notifications().iter().all(|(method, _)| method != "alert"),
        "VLF semantic navigation should not alert when target is inside parsed window"
    );
}

#[test]
fn vlf_syntax_commands_reuse_visible_parse_cache() {
    use crate::rpc::EditNotification;

    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let (harness, _f) = vlf_harness(b"fn alpha() { beta(gamma); }\nfn delta() {}\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::VlfViewport { line_start: 0, line_end: 1, generation: 1 });
    harness.take_notifications();

    let start = "fn alpha() { beta(".len();
    let end = start + "gamma".len();
    harness.view.borrow_mut().set_vlf_selection(SelRegion::new(start, end));

    ctx.do_syntax_selection(crate::object::SyntaxSelectionAction::Expand);
    assert_eq!(harness.view.borrow().semantic_parse_cache_parse_count(), 1);

    ctx.do_syntax_navigation(crate::object::SyntaxNavigationAction::new(
        SyntaxNavigationTarget::Function,
        true,
    ));
    assert_eq!(harness.view.borrow().semantic_parse_cache_parse_count(), 1);
    assert!(
        harness.take_notifications().iter().all(|(method, _)| method != "alert"),
        "reused VLF semantic parse should keep commands functional"
    );
}
