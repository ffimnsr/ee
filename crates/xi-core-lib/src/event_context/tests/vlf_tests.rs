//! Event-context tests: vlf.
use super::*;
use crate::config::ConfigDomain;

#[test]
fn vlf_render_emits_update_ops() {
    // Stage A Phase 2+: the unified render path emits `update` op streams for
    // VLF buffers through `RenderSource` (the `vlf_chunks` channel is gone).
    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\ndelta\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();
    {
        // The harness's empty-rope init pre-rendered one ALL_VALID line; real
        // opens mark the view dirty before the first render. Do the same so
        // the VLF editor's real content is rendered, not shadow-copied.
        let ed = ctx.editor.borrow();
        let store = ed.vlf_store.as_ref().expect("vlf store");
        ctx.view.borrow_mut().set_dirty(store.as_ref());
    }

    ctx.render();

    let notifications = harness.take_notifications();
    let (_, params) =
        notifications.iter().find(|(m, _)| m == "update").expect("expected update notification");
    let lines = params["update"]["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .flat_map(|op| op["lines"].as_array().cloned().unwrap_or_default())
        .collect::<Vec<_>>();
    let texts = lines.iter().filter_map(|line| line["text"].as_str()).collect::<Vec<_>>();
    // Rope-parity wire shape: line intervals include the terminator; a
    // trailing newline yields one empty final line.
    assert_eq!(texts, vec!["alpha\n", "beta\n", "gamma\n", "delta\n", ""]);
    // The update carries the VLF total-line metadata (Phase 3 payload).
    let total = &params["update"]["vlf_total_lines"];
    assert_eq!(total["count"], 5u64);
    assert_eq!(total["exact"], true);
}

#[test]
fn vlf_render_wraps_when_vlf_wrap_enabled() {
    // `vlf_wrap: true` (+ byte wrap width) turns on the windowed width pass:
    // the update carries wrapped rows and `ln` only on logical-line heads.
    let content = b"alpha beta gamma delta epsilon\nzeta eta\n";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
    store.scan_all().unwrap();

    let mut harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    let buffer_id = harness.view.borrow().get_buffer_id();
    harness
        .config_manager
        .set_user_config(
            ConfigDomain::UserOverride(buffer_id),
            serde_json::json!({ "vlf_wrap": true }).as_object().expect("config table").clone(),
        )
        .expect("config update");
    // The view's byte wrap width (the harness renders with no width); mirror
    // what a real `update_wrap_settings` config push sets.
    harness.view.borrow_mut().update_wrap_settings(&Rope::from(""), 4, false);
    harness.take_notifications();
    let mut ctx = harness.make_context();
    {
        let ed = ctx.editor.borrow();
        let store = ed.vlf_store.as_ref().expect("vlf store");
        ctx.view.borrow_mut().set_dirty(store.as_ref());
    }

    ctx.render();

    let notifications = harness.take_notifications();
    let (_, params) = notifications.iter().find(|(m, _)| m == "update").expect("expected update");
    let rows = params["update"]["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .flat_map(|op| op["lines"].as_array().cloned().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(rows.len() > 2, "wrapped render must emit more rows than logical lines");
    assert_eq!(rows[0]["text"], "alpha ");
    assert_eq!(rows[0]["ln"], 0u64, "first row carries the logical line");
    assert!(
        rows.iter().skip(1).any(|row| row.get("ln").is_none()),
        "continuation rows must omit ln"
    );
}

#[test]
fn vlf_render_recovers_when_index_lands_between_repaints() {
    // Render before the index exists (Unknown -> height 0, span-less shadow)
    // must not wedge the shadow into a permanent copy: once the index lands,
    // the next repaint renders real text.

    let content = b"alpha\nbeta\ngamma\ndelta\n";
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
    // NOTE: no scan_all — the index has not started.

    let harness = ContextHarness::new("");
    *harness.editor.borrow_mut() = Editor::with_vlf_store(store);
    harness.take_notifications();
    let mut ctx = harness.make_context();

    // First repaint: empty window (no panic, no content yet).
    ctx.render();
    let first = harness.take_notifications();
    assert!(
        first.iter().any(|(m, _)| m == "update"),
        "unscanned render must still emit an update (empty window)"
    );

    // Index lands; the next repaint must render the full window.
    {
        let ed = ctx.editor.borrow();
        ed.vlf_store.as_ref().expect("vlf store").scan_all().unwrap();
    }
    ctx.render();

    let notifications = harness.take_notifications();
    let (_, params) =
        notifications.iter().find(|(m, _)| m == "update").expect("expected update notification");
    let lines = params["update"]["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .flat_map(|op| op["lines"].as_array().cloned().unwrap_or_default())
        .collect::<Vec<_>>();
    let texts = lines.iter().filter_map(|line| line["text"].as_str()).collect::<Vec<_>>();
    assert_eq!(texts, vec!["alpha\n", "beta\n", "gamma\n", "delta\n", ""]);
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
fn render_if_needed_renders_vlf_through_store_not_placeholder_rope() {
    let (harness, _f) = vlf_harness(b"alpha\nbeta\ngamma\n");
    harness.view.borrow_mut().set_vlf_selection(SelRegion::caret(10));
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.render_if_needed();

    let notifications = harness.take_notifications();
    let (_, params) =
        notifications.iter().find(|(m, _)| m == "update").expect("update emitted for VLF");
    let texts = params["update"]["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .flat_map(|op| op["lines"].as_array().cloned().unwrap_or_default())
        .filter_map(|line| line["text"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        texts.iter().any(|text| text == "alpha\n"),
        "render must carry VLF store content, not the placeholder rope: {texts:?}"
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

    // The legacy `vlf_viewport` fetch is gone: repaint via the unified render
    // path and assert the edited line arrives on the `update` channel.
    {
        let ed = ctx.editor.borrow();
        let store = ed.vlf_store.as_ref().expect("vlf store");
        ctx.view.borrow_mut().set_dirty(store.as_ref());
    }
    ctx.render();

    let notifications = harness.take_notifications();
    let (_, update) = notifications
        .iter()
        .find(|(method, _)| method == "update")
        .expect("expected update after VLF edit");
    let texts = update["update"]["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .flat_map(|op| op["lines"].as_array().cloned().unwrap_or_default())
        .filter_map(|line| line["text"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        texts.iter().any(|text| text == "beta needle\n"),
        "unified render must carry the edited line: {texts:?}"
    );
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
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let source = b"fn main() { foo(bar); }\n";
    let (harness, _f) = vlf_harness(source);
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    // The unified render seeds the store's semantic window (legacy
    // `vlf_viewport` fetch is gone).
    {
        let ed = ctx.editor.borrow();
        let store = ed.vlf_store.as_ref().expect("vlf store");
        ctx.view.borrow_mut().set_dirty(store.as_ref());
    }
    ctx.render();
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
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    // 16 short functions; the harness viewport (height 10) renders lines
    // 0..12, so `f12` on line 12 stays outside the parsed window.
    let content = (0..16).map(|i| format!("fn f{i:02}() {{}}\n")).collect::<String>();
    let (harness, _f) = vlf_harness(content.as_bytes());
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    // The unified render seeds the semantic window (legacy `vlf_viewport`
    // fetch is gone): first_line..first_line + height + 2, i.e. lines 0..12.
    {
        let ed = ctx.editor.borrow();
        let store = ed.vlf_store.as_ref().expect("vlf store");
        ctx.view.borrow_mut().set_dirty(store.as_ref());
    }
    ctx.render();
    harness.take_notifications();

    let window_end = "fn f00() {}\n".len() * 12;
    {
        let editor = ctx.editor.borrow();
        let store = editor.vlf_store.as_ref().expect("vlf store");
        let window_range = ctx.current_vlf_semantic_range(store).expect("semantic window");
        let window_text = ctx
            .current_vlf_semantic_window_text(store, window_range, "rust", None)
            .expect("semantic window text");
        assert_eq!(window_text, &content[..window_end]);
        assert_eq!(window_range.start.0 as usize, 0);
        assert_eq!(window_range.end.0 as usize, window_end);
    }

    // Caret at the start of line 12 (`f12`): its function is outside the
    // parsed window, so navigation must alert.
    harness.view.borrow_mut().set_vlf_selection(SelRegion::caret(window_end));

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
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let (harness, _f) = vlf_harness(b"fn alpha() {}\nfn beta() {}\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    // Unified render crops the semantic window to the whole file here; the
    // navigation target (beta) is inside it.
    {
        let ed = ctx.editor.borrow();
        let store = ed.vlf_store.as_ref().expect("vlf store");
        ctx.view.borrow_mut().set_dirty(store.as_ref());
    }
    ctx.render();
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
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    let (harness, _f) = vlf_harness(b"fn alpha() { beta(gamma); }\nfn delta() {}\n");
    harness.take_notifications();
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    {
        let ed = ctx.editor.borrow();
        let store = ed.vlf_store.as_ref().expect("vlf store");
        ctx.view.borrow_mut().set_dirty(store.as_ref());
    }
    ctx.render();
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
