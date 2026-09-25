//! Render, find-all, and backend-syntax tests.
use super::*;
use crate::span_payload::ScopeTable;

#[test]
fn caret_only_repaint_omits_spans_and_keeps_them_for_renders() {
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust"]);
    let text =
        Rope::from((0..200).map(|index| format!("let value_{index} = 42;\n")).collect::<String>());
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(&text, 80);
    let (client, peer) = recording_client();
    let store = RopeTextStore::new(text.clone(), 0);

    // Cold render carries spans for the rows it renders.
    view.request_lines(&store, &client, 0, 39, true, "rust", true);
    let cold = peer.take_notifications();
    let cold_span_rows = count_span_rows(&cold);
    assert!(cold_span_rows > 0, "cold render should carry spans");

    // Moving the caret keeps text and syntax valid, so the repaint sends
    // cursors only: no `spans` key, and no syntax production behind it.
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(120, 120));
    view.set_selection(&text, selection);
    view.request_lines(&store, &client, 0, 39, true, "rust", true);
    let repaint = peer.take_notifications();

    assert_eq!(count_span_rows(&repaint), 0, "caret repaints must not resend spans");
    assert!(
        repaint.iter().any(|(method, params)| {
            method == "update"
                && params["update"]["ops"].as_array().is_some_and(|ops| {
                    ops.iter().any(|op| op["op"] == "update" && op["lines"].is_array())
                })
        }),
        "caret repaint should still send cursor updates"
    );

    // Span production is not disabled by the omit-spans branch: rows the plan has
    // no valid cache entry for are still rendered with spans, which is how a
    // frontend that lost a row recovers them.
    view.request_lines(&store, &client, 160, 199, true, "rust", true);
    let fresh_window = peer.take_notifications();
    assert!(
        count_span_rows(&fresh_window) > 0,
        "rows without a valid cache entry must carry spans"
    );
}

#[test]
fn selection_drag_repaint_omits_spans() {
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust"]);
    let text =
        Rope::from((0..40).map(|index| format!("let value_{index} = 42;\n")).collect::<String>());
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(&text, 80);
    let (client, peer) = recording_client();
    let store = RopeTextStore::new(text.clone(), 0);

    view.request_lines(&store, &client, 0, 39, true, "rust", true);
    let cold = peer.take_notifications();
    assert!(count_span_rows(&cold) > 0, "cold render should carry spans");

    // A drag is a selection change, not a text or syntax change: restyling the
    // dragged rows would mean re-parsing and re-walking the window per pointer
    // move, which is what the cursor-only branch exists to avoid.
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(0, 120));
    view.set_selection(&text, selection);
    view.request_lines(&store, &client, 0, 39, true, "rust", true);
    let repaint = peer.take_notifications();

    assert_eq!(count_span_rows(&repaint), 0, "drag repaints must not resend spans");
    assert!(
        repaint.iter().any(|(method, params)| {
            method == "update"
                && params["update"]["ops"].as_array().is_some_and(|ops| {
                    ops.iter().any(|op| op["op"] == "update" && op["lines"].is_array())
                })
        }),
        "drag repaint should still send cursor updates"
    );
}

#[test]
fn keep_and_remove_primary_selection_use_primary_region() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("abcdef");
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(0, 1));
    selection.add_region(SelRegion::new(2, 3));
    selection.add_region(SelRegion::new(4, 5));
    view.set_selection(&text, selection.clone());

    view.do_edit(&text, ViewEvent::KeepPrimarySelection);
    assert_eq!(view.sel_regions(), &[SelRegion::new(4, 5)]);

    view.set_selection(&text, selection);
    view.do_edit(&text, ViewEvent::RotateSelectionsBackward);
    view.do_edit(&text, ViewEvent::RemovePrimarySelection);
    assert_eq!(view.sel_regions(), &[SelRegion::new(0, 1), SelRegion::new(4, 5)]);
}

#[test]
fn select_regex_only_matches_inside_existing_selections() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("foo1 bar foo2 baz");
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(0, 8));
    selection.add_region(SelRegion::new(9, text.len()));
    view.set_selection(&text, selection);

    view.do_edit(
        &text,
        ViewEvent::SelectRegex { chars: String::from(r"foo\d"), case_sensitive: true },
    );

    assert_eq!(view.sel_regions(), &[SelRegion::new(0, 4), SelRegion::new(9, 13)]);
}

#[test]
fn multi_queries_find_next() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("hello hello world\n hello!");
    let query1 = FindQuery {
        id: None,
        chars: "hello".to_string(),
        case_sensitive: false,
        regex: false,
        whole_words: false,
    };
    let query2 = FindQuery {
        id: None,
        chars: "o world".to_string(),
        case_sensitive: false,
        regex: false,
        whole_words: false,
    };
    view.do_edit(&text, ViewEvent::MultiFind { queries: vec![query1, query2] });
    view.do_find(&text);
    view.do_find_next(&text, false, true, false, &SelectionModifier::Set);
    assert_eq!(view.sel_regions().first(), Some(&SelRegion::new(0, 5)));
    view.do_find_next(&text, false, true, false, &SelectionModifier::Set);
    assert_eq!(view.sel_regions().first(), Some(&SelRegion::new(6, 11)));
    view.do_find_next(&text, false, true, false, &SelectionModifier::Set);
    assert_eq!(view.sel_regions().first(), Some(&SelRegion::new(10, 17)));
}

#[test]
fn multi_queries_find_all() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("hello hello world\n hello!");
    let query1 = FindQuery {
        id: None,
        chars: "hello".to_string(),
        case_sensitive: false,
        regex: false,
        whole_words: false,
    };
    let query2 = FindQuery {
        id: None,
        chars: "world".to_string(),
        case_sensitive: false,
        regex: false,
        whole_words: false,
    };
    view.do_edit(&text, ViewEvent::MultiFind { queries: vec![query1, query2] });
    view.do_find(&text);
    view.do_find_all(&text);
    assert_eq!(view.sel_regions().len(), 4);
}

#[test]
fn encode_line_includes_backend_syntax_spans_with_byte_ranges() {
    let view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("let x = 1;\n");
    let syntax_spans = vec![
        VisibleSyntaxSpan {
            start_byte: 0,
            end_byte: 3,
            scope: String::from("keyword.control.rust"),
        },
        VisibleSyntaxSpan {
            start_byte: 8,
            end_byte: 9,
            scope: String::from("constant.numeric.decimal.rust"),
        },
    ];

    let mut scopes = ScopeTable::new();
    let line = VisualLine { interval: Interval::new(0, 10), line_num: Some(1) };
    let encoded = view.encode_line(
        line,
        Some(&RopeTextStore::new(text.clone(), 0)),
        &syntax_spans,
        &mut scopes,
        text.len(),
        0,
        None,
    );
    let spans: Vec<u32> = encoded["spans"]
        .as_array()
        .expect("missing spans")
        .iter()
        .map(|value| u32::try_from(value.as_u64().expect("span entries are integers")).unwrap())
        .collect();

    assert_eq!(encoded["ln"], 0, "logical line number is always emitted");
    assert_eq!(spans, [0, 3, 0, 8, 9, 1], "flat [start, end, scope_id] triples");
    assert_eq!(scopes.names(), ["keyword.control.rust", "constant.numeric.decimal.rust"]);
}

#[test]
fn encode_line_keeps_line_relative_syntax_spans() {
    let view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("first\nsecond();\n");
    let syntax_spans = vec![VisibleSyntaxSpan {
        start_byte: 0,
        end_byte: 6,
        scope: String::from("entity.name.function.c"),
    }];

    let mut scopes = ScopeTable::new();
    let line = VisualLine { interval: Interval::new(6, 16), line_num: Some(2) };
    let encoded = view.encode_line(
        line,
        Some(&RopeTextStore::new(text.clone(), 0)),
        &syntax_spans,
        &mut scopes,
        text.len(),
        1,
        None,
    );
    let spans = encoded["spans"].as_array().expect("missing spans");

    assert_eq!(spans.len(), 3);
    assert_eq!(spans[0], 0);
    assert_eq!(spans[1], 6);
    assert_eq!(scopes.names(), ["entity.name.function.c"]);
}

#[test]
fn encode_line_reuses_interned_scope_ids_across_lines() {
    let view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("let x = 1;\nlet y = 2;\n");
    let first = VisibleSyntaxSpan {
        start_byte: 0,
        end_byte: 3,
        scope: String::from("keyword.control.rust"),
    };
    let second = VisibleSyntaxSpan {
        start_byte: 0,
        end_byte: 3,
        scope: String::from("keyword.control.rust"),
    };

    let mut scopes = ScopeTable::new();
    let store = RopeTextStore::new(text.clone(), 0);
    let _ = view.encode_line(
        VisualLine { interval: Interval::new(0, 11), line_num: Some(1) },
        Some(&store),
        std::slice::from_ref(&first),
        &mut scopes,
        text.len(),
        0,
        None,
    );
    let encoded = view.encode_line(
        VisualLine { interval: Interval::new(11, 22), line_num: Some(2) },
        Some(&store),
        std::slice::from_ref(&second),
        &mut scopes,
        text.len(),
        1,
        None,
    );

    assert_eq!(encoded["spans"][2], 0, "repeated scope reuses id 0");
    assert_eq!(scopes.names().len(), 1, "one interned scope for both lines");
}

#[test]
fn encode_line_omits_syntax_spans_when_backend_has_no_data() {
    let view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("plain text\n");
    let line = VisualLine { interval: Interval::new(0, 10), line_num: Some(1) };

    let encoded = view.encode_line(
        line,
        Some(&RopeTextStore::new(text.clone(), 0)),
        &[],
        &mut ScopeTable::new(),
        text.len(),
        0,
        None,
    );

    assert!(encoded.get("spans").is_none());
}

#[test]
fn render_if_dirty_emits_backend_syntax_spans_without_plugin_update() {
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust"]);
    let mut view = View::new(1.into(), BufferId::new(2));
    let editor = crate::editor::Editor::with_text("let x = 1;\n");
    let (client, peer) = recording_client();
    view.debug_force_rewrap_cols(editor.get_buffer(), 80);
    let store = editor.text_store_snapshot();

    view.render_if_dirty(&store, &client, true, "rust", true);
    let notifications = peer.take_notifications();

    let syntax_refresh = notifications.iter().any(|(method, params)| {
        method == "update"
            && params["update"]["ops"].as_array().is_some_and(|ops| {
                ops.iter().any(|op| {
                    op["lines"]
                        .as_array()
                        .is_some_and(|lines| lines.iter().any(|line| line.get("spans").is_some()))
                })
            })
            && params["update"]["scopes"].as_array().is_some_and(|scopes| !scopes.is_empty())
    });

    assert!(syntax_refresh, "backend render should emit syntax-bearing line updates");
}

#[test]
fn backend_syntax_spans_keep_yaml_keys_after_block_scalar_with_forward_context() {
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["yaml", "bash"]);
    let yaml = "\
tasks:
  install:
    commands:
      - |
        printf 'install\\n'
    description: Install editor.
  config-nearest:
    commands:
      - |
        printf 'config\\n'
";
    let start_line = yaml
        .lines()
        .position(|line| line.contains("printf 'install"))
        .expect("fixture includes install block scalar");
    let description_line = yaml
        .lines()
        .position(|line| line.trim_start().starts_with("description:"))
        .expect("fixture includes description key");
    let config_nearest_line = yaml
        .lines()
        .position(|line| line.trim_start().starts_with("config-nearest:"))
        .expect("fixture includes config-nearest key");
    let line_count = config_nearest_line - start_line + 1;
    let mut view = View::new(1.into(), BufferId::new(2));
    let editor = crate::editor::Editor::with_text(yaml);
    view.debug_force_rewrap_cols(editor.get_buffer(), 120);

    let spans = view.backend_syntax_spans_for_segment(
        &editor.text_store_snapshot(),
        start_line,
        line_count,
        "yaml",
        true,
    );

    let description = description_line - start_line;
    let config_nearest = config_nearest_line - start_line;
    assert!(
        spans[description]
            .iter()
            .any(|span| span.scope.starts_with("variable") || span.scope.starts_with("property"))
    );
    assert!(
        spans[config_nearest]
            .iter()
            .any(|span| span.scope.starts_with("variable") || span.scope.starts_with("property"))
    );
    assert!(spans[0].iter().any(|span| span.scope.starts_with("keyword")
        || span.scope.starts_with("string")
        || span.scope.starts_with("function")
        || span.scope.starts_with("property")));
}

#[test]
fn render_if_dirty_omits_syntax_spans_for_unsupported_language() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let editor = crate::editor::Editor::with_text("plain text\n");
    let (client, peer) = recording_client();
    view.debug_force_rewrap_cols(editor.get_buffer(), 80);
    let store = editor.text_store_snapshot();

    view.render_if_dirty(&store, &client, true, "Plain Text", true);

    let notifications = peer.take_notifications();
    let syntax_refresh = notifications.iter().any(|(method, params)| {
        method == "update"
            && params["update"]["ops"].as_array().is_some_and(|ops| {
                ops.iter().any(|op| {
                    op["lines"]
                        .as_array()
                        .is_some_and(|lines| lines.iter().any(|line| line.get("spans").is_some()))
                })
            })
    });

    assert!(!syntax_refresh, "unsupported languages should render without backend syntax");
}

#[test]
#[ignore = "manual perf probe for syntax payload size and render latency"]
fn syntax_span_render_perf_probe() {
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust", "yaml"]);
    let text =
        Rope::from((0..200).map(|index| format!("let value_{index} = 42;\n")).collect::<String>());

    let (plain_bytes, plain_us, plain_serialize_us) = syntax_probe_render(&text, false);
    let (syntax_bytes, syntax_us, syntax_serialize_us) = syntax_probe_render(&text, true);
    let (spans_per_line, production_us, production_segmented_us) =
        syntax_probe_span_production(&text);
    let parse_us = syntax_probe_parse_only(&text);
    let caret = syntax_probe_caret_move(&text);
    let jump = syntax_probe_caret_jump(&text);
    let drag = syntax_probe_drag_render(&text);
    let scroll_back = syntax_probe_scroll_back(&text);
    let (yaml_first_us, yaml_mid_us, yaml_late_us) = syntax_probe_yaml_offset_scale();
    let span_count: usize = spans_per_line.iter().map(Vec::len).sum();
    let encodings = syntax_probe_encodings(&spans_per_line);
    let span_bytes = syntax_bytes.saturating_sub(plain_bytes);
    let span_share = span_bytes as f64 * 100.0 / syntax_bytes.max(1) as f64;

    eprintln!(
        "syntax payload probe: text_bytes={} plain_payload_bytes={plain_bytes} \
         syntax_payload_bytes={syntax_bytes} span_payload_bytes={span_bytes} \
         span_share={span_share:.1}% spans={span_count} \
         plain_render_us={plain_us} syntax_render_us={syntax_us} \
         plain_serialize_us={plain_serialize_us} syntax_serialize_us={syntax_serialize_us} \
         span_production_us={production_us} span_production_segmented_us={production_segmented_us}",
        text.len(),
    );
    eprintln!(
        "syntax repaint probe: parse_only_us={parse_us} \
         caret_move_bytes={} caret_move_us={} caret_move_span_lines={} \
         caret_jump_bytes={} caret_jump_us={} caret_jump_span_lines={} \
         drag_render_bytes={} drag_render_us={} drag_render_span_lines={} \
         scroll_back_bytes={} scroll_back_us={} scroll_back_span_lines={} \
         yaml_window_first_us={yaml_first_us} yaml_window_mid_us={yaml_mid_us} \
         yaml_window_late_us={yaml_late_us}",
        caret.bytes,
        caret.us,
        caret.span_lines,
        jump.bytes,
        jump.us,
        jump.span_lines,
        drag.bytes,
        drag.us,
        drag.span_lines,
        scroll_back.bytes,
        scroll_back.us,
        scroll_back.span_lines,
    );
    eprintln!(
        "span encoding probe: spans={span_count} flat_bytes={} flat_us={} flat_scopes={} \
         records_bytes={} records_us={} records_decode_us={} records_decoded={} \
         records_u16_fits={} records_u16_bytes={} records_u16_us={} records_u16_decode_us={}",
        encodings.flat_bytes,
        encodings.flat_us,
        encodings.flat_scopes,
        encodings.records_bytes,
        encodings.records_us,
        encodings.records_decode_us,
        encodings.records_decoded_len,
        encodings.records_u16_fits,
        encodings.records_u16_bytes,
        encodings.records_u16_us,
        encodings.records_u16_decode_us,
    );

    assert!(plain_bytes > 0, "probe should emit a measured baseline payload");
    assert!(
        syntax_bytes > plain_bytes,
        "syntax-enabled render should carry more payload than the baseline"
    );
    assert_eq!(encodings.records_decoded_len, span_count, "records must round-trip");
    assert!(encodings.records_u16_fits, "fixture should fit in 16-bit records");
    assert!(
        encodings.records_u16_bytes < encodings.flat_bytes,
        "16-bit records should beat the flat JSON carrier on bytes"
    );
}

/// Byte and time cost of the three span carriers for one window.
struct SyntaxProbeEncodings {
    flat_bytes: usize,
    flat_us: u128,
    flat_scopes: usize,
    records_bytes: usize,
    records_us: u128,
    records_decode_us: u128,
    records_decoded_len: usize,
    records_u16_bytes: usize,
    records_u16_us: u128,
    records_u16_decode_us: u128,
    records_u16_fits: bool,
}

/// Compares the production flat-array span carrier against the fixed-width
/// zerocopy record carrier (the Phase 3 binary-frame projection).
///
/// Every carrier includes its scope table, so byte counts compare like for like.
fn syntax_probe_encodings(spans_per_line: &[Vec<VisibleSyntaxSpan>]) -> SyntaxProbeEncodings {
    // 1. Production carrier: interned scope ids in flat per-line arrays.
    let mut table = ScopeTable::new();
    let started = Instant::now();
    let encoded: Vec<Value> = spans_per_line
        .iter()
        .map(|spans| match crate::view::render::encode_line_spans(spans, &mut table) {
            Some(spans) => spans,
            None => Value::Array(Vec::new()),
        })
        .collect();
    let flat_payload = serde_json::to_vec(&Value::Array(encoded)).expect("flat spans");
    let flat_bytes = flat_payload.len() + table.json_bytes();
    let flat_us = started.elapsed().as_micros();
    let flat_scopes = table.names().len();

    // 2. Fixed-width records, borrowed back by pointer cast.
    let mut record_table = crate::span_payload::ScopeTable::new();
    let started = Instant::now();
    let blob = crate::span_payload::encode(spans_per_line, &mut record_table);
    let records_us = started.elapsed().as_micros();
    let records_bytes = blob.byte_len() + record_table.json_bytes();

    let raw = blob.to_bytes();
    let record_bytes = blob.records.len() * crate::span_payload::SPAN_RECORD_BYTES;
    let started = Instant::now();
    let decoded = crate::span_payload::decode_records(&raw[..record_bytes])
        .expect("packed records should decode");
    let records_decode_us = started.elapsed().as_micros();

    // 3. 16-bit records: same layout, half the size when offsets fit.
    let mut table_u16 = crate::span_payload::ScopeTable::new();
    let started = Instant::now();
    let u16_blob = crate::span_payload::encode_u16(spans_per_line, &mut table_u16);
    let records_u16_us = started.elapsed().as_micros();
    let records_u16_fits = u16_blob.is_some();
    let (records_u16_bytes, records_u16_decode_us) = match u16_blob {
        Some(blob) => {
            let bytes = blob.byte_len() + table_u16.json_bytes();
            let raw = blob.to_bytes();
            let record_bytes = blob.records.len() * crate::span_payload::SPAN_RECORD_U16_BYTES;
            let started = Instant::now();
            let decoded = crate::span_payload::decode_records_u16(&raw[..record_bytes])
                .expect("16-bit records decode");
            assert_eq!(decoded.len(), blob.records.len(), "u16 records must round-trip");
            (bytes, started.elapsed().as_micros())
        }
        None => (0, 0),
    };

    SyntaxProbeEncodings {
        flat_bytes,
        flat_us,
        flat_scopes,
        records_bytes,
        records_us,
        records_decode_us,
        records_decoded_len: decoded.len(),
        records_u16_bytes,
        records_u16_us,
        records_u16_decode_us,
        records_u16_fits,
    }
}

/// Parse-only cost for the window, to split production into parse versus query
/// walk. Uses the same chunk and language as [`syntax_probe_span_production`].
fn syntax_probe_parse_only(text: &Rope) -> u128 {
    use tree_sitter::{ParseOptions, Parser};

    let chunk = text.slice_to_cow(0..text.len()).into_owned();
    let Some(language) = crate::tree_sitter_support::ts_language_for_name("rust") else {
        return 0;
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return 0;
    }

    let bytes = chunk.as_bytes();
    let mut read = |offset: usize, _: tree_sitter::Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: None };
    let started = Instant::now();
    let tree = parser.parse_with_options(&mut read, None, Some(options));
    let elapsed_us = started.elapsed().as_micros();
    assert!(tree.is_some(), "probe fixture should parse");
    elapsed_us
}

/// One repaint after a pure caret move: only `CURSOR_VALID` is invalidated, so
/// the render plan takes the `update` op path over the caret's lines.
fn syntax_probe_caret_move(text: &Rope) -> SyntaxProbeRepaint {
    let (mut view, client, peer, store) = syntax_probe_ready_view(text);
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(40, 40));
    view.set_selection(text, selection);

    let started = Instant::now();
    view.request_lines(&store, &client, 0, 199, true, "rust", true);
    let us = started.elapsed().as_micros();
    syntax_probe_repaint_stats(peer.take_notifications(), us)
}

/// One repaint after a caret jump deep into the window.
fn syntax_probe_caret_jump(text: &Rope) -> SyntaxProbeRepaint {
    let (mut view, client, peer, store) = syntax_probe_ready_view(text);
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(1900, 1900));
    view.set_selection(text, selection);

    let started = Instant::now();
    view.request_lines(&store, &client, 0, 199, true, "rust", true);
    let us = started.elapsed().as_micros();
    syntax_probe_repaint_stats(peer.take_notifications(), us)
}

/// One repaint after selecting 100 lines in the middle of the window.
fn syntax_probe_drag_render(text: &Rope) -> SyntaxProbeRepaint {
    let (mut view, client, peer, store) = syntax_probe_ready_view(text);
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(0, 1900));
    view.set_selection(text, selection);

    let started = Instant::now();
    view.request_lines(&store, &client, 0, 199, true, "rust", true);
    let us = started.elapsed().as_micros();
    syntax_probe_repaint_stats(peer.take_notifications(), us)
}

/// Scrolling away and back: the first window falls outside the plan and is
/// discarded, so returning to it re-renders and re-walks those rows.
fn syntax_probe_scroll_back(text: &Rope) -> SyntaxProbeRepaint {
    let (mut view, client, peer, store) = syntax_probe_ready_view(text);

    view.request_lines(&store, &client, 400, 599, true, "rust", true);
    let _ = peer.take_notifications();

    let started = Instant::now();
    view.request_lines(&store, &client, 0, 199, true, "rust", true);
    let us = started.elapsed().as_micros();
    syntax_probe_repaint_stats(peer.take_notifications(), us)
}

/// YAML production cost for the same 200-line window at three document offsets,
/// measured through the real render path after a warm-up call.
///
/// `parse_from_document_start` (view/render.rs) gives YAML windows a context
/// start of line 0, so later windows parse a longer prefix. Measuring via
/// `backend_syntax_spans_for_segment` keeps that behavior in the loop; calling
/// `chunk_syntax_spans` directly would only ever see the window. The warm-up
/// call absorbs first-use grammar loading so the sweep shows steady state.
fn syntax_probe_yaml_offset_scale() -> (u128, u128, u128) {
    let yaml = Rope::from(
        (0..5_000).map(|index| format!("key_{index}: value_{index}\n")).collect::<String>(),
    );
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(&yaml, 0);
    let store = RopeTextStore::new(yaml.clone(), 0);

    let window_cost = |start_line: usize| -> u128 {
        let started = Instant::now();
        let spans = view.backend_syntax_spans_for_segment(&store, start_line, 200, "yaml", true);
        let elapsed_us = started.elapsed().as_micros();
        assert_eq!(spans.len(), 200, "one span list per rendered line");
        elapsed_us
    };

    let _warmup = window_cost(0);
    (window_cost(0), window_cost(2_400), window_cost(4_800))
}

/// Cold render so the frontend cache holds spans for the whole probe window,
/// then hand back the parts a repaint probe needs.
///
/// The cold pass uses `request_lines` over the full range: `render_if_dirty`
/// only covers the viewport height, and rows outside it would make the
/// "repaint" probe measure first-time rendering instead of a cached repaint.
fn syntax_probe_ready_view(text: &Rope) -> (View, Client, RecordingPeer, RopeTextStore) {
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(text, 80);
    let (client, peer) = recording_client();
    let store = RopeTextStore::new(text.clone(), 0);
    view.request_lines(&store, &client, 0, 199, true, "rust", true);
    let cold = peer.take_notifications();
    assert!(!cold.is_empty(), "cold render should emit an update");
    (view, client, peer, store)
}

fn syntax_probe_repaint_stats(notifications: Vec<(String, Value)>, us: u128) -> SyntaxProbeRepaint {
    let update_count = notifications.iter().filter(|(method, _)| method == "update").count();
    let bytes = notifications
        .iter()
        .filter(|(method, _)| method == "update")
        .map(|(_, params)| serde_json::to_vec(params).expect("serialize update payload").len())
        .sum();
    let span_lines = notifications
        .iter()
        .filter(|(method, _)| method == "update")
        .flat_map(|(_, params)| params["update"]["ops"].as_array().cloned().unwrap_or_default())
        .filter_map(|op| op["lines"].as_array().cloned())
        .flatten()
        .filter(|line| line.get("spans").is_some())
        .count();
    let _ = update_count;
    SyntaxProbeRepaint { bytes, us, span_lines }
}

/// Repaint payload, wall time, and how many rows re-sent syntax spans.
struct SyntaxProbeRepaint {
    bytes: usize,
    us: u128,
    span_lines: usize,
}

/// Times backend span production alone (parse plus highlight/injection query
/// walk), separate from line encoding and payload serialization.
///
/// Returns the per-line spans plus `(whole_chunk_us, per_line_segments_us)`. The
/// two timings use the same chunk and limits; only the segment list differs,
/// which shows how much of the cost is query-walk filtering versus the parse.
fn syntax_probe_span_production(text: &Rope) -> (Vec<Vec<VisibleSyntaxSpan>>, u128, u128) {
    let chunk = text.slice_to_cow(0..text.len()).into_owned();
    let limits =
        VisibleSyntaxLimits { timeout: BACKEND_SYNTAX_TIMEOUT, ..VisibleSyntaxLimits::default() };

    let whole_segment: Vec<std::ops::Range<usize>> = std::iter::once(0..chunk.len()).collect();

    let started = Instant::now();
    let whole = chunk_syntax_spans("rust", &chunk, &whole_segment, limits);
    let whole_us = started.elapsed().as_micros();

    let segments = syntax_probe_line_segments(&chunk);
    let started = Instant::now();
    let per_line = chunk_syntax_spans("rust", &chunk, &segments, limits);
    let per_line_us = started.elapsed().as_micros();

    let spans = whole.iter().map(Vec::len).sum::<usize>();
    assert_eq!(
        spans,
        per_line.iter().map(Vec::len).sum::<usize>(),
        "segment split must not change span count"
    );
    (per_line, whole_us, per_line_us)
}

/// Half-open byte ranges of each line in `chunk`, line breaks included.
fn syntax_probe_line_segments(chunk: &str) -> Vec<std::ops::Range<usize>> {
    let mut segments = Vec::new();
    let mut start = 0;
    for (index, _) in chunk.match_indices('\n') {
        segments.push(start..index + 1);
        start = index + 1;
    }
    if start < chunk.len() {
        segments.push(start..chunk.len());
    }
    segments
}

/// Measures one full-window render of `text`, returning
/// `(payload_bytes, render_us, serialize_us)`.
///
/// Wrap breaks must be seeded before rendering: a fresh view holds empty breaks
/// while the text is non-empty, and `MergedBreaks::new` asserts on that mismatch.
/// `render_us` includes the recording peer's payload clone (test-harness cost),
/// so treat it as an upper bound for backend render work.
fn syntax_probe_render(text: &Rope, syntax_enabled: bool) -> (usize, u128, u128) {
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(text, 80);
    let (client, peer) = recording_client();
    let store = RopeTextStore::new(text.clone(), 0);

    let started = Instant::now();
    view.request_lines(&store, &client, 0, 199, true, "rust", syntax_enabled);
    let elapsed_us = started.elapsed().as_micros();

    let notifications = peer.take_notifications();
    let started = Instant::now();
    let bytes = notifications
        .iter()
        .filter(|(method, _)| method == "update")
        .map(|(_, params)| serde_json::to_vec(params).expect("serialize update payload").len())
        .sum();
    let serialize_us = started.elapsed().as_micros();
    (bytes, elapsed_us, serialize_us)
}

#[test]
fn update_lines_carry_repeated_logical_ln_for_wrapped_rows() {
    let text = Rope::from("one two three four\nfive six seven\neight nine ten eleven\n");
    let mut view = View::new(1.into(), BufferId::new(2));
    view.height = 40;
    // Narrow byte wrap: every logical line spans several visual rows.
    view.debug_force_rewrap_cols(&text, 4);

    let (client, peer) = recording_client();
    let store = RopeTextStore::new(text.clone(), 0);
    view.render_if_dirty(&store, &client, true, "", false);

    let updates = peer.take_notifications();
    let update_json = updates
        .iter()
        .find(|(method, _)| method == "update")
        .map(|(_, params)| params.clone())
        .expect("update notification");
    let lines = update_json["update"]["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .flat_map(|op| op["lines"].as_array().cloned().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(!lines.is_empty(), "wrapped rows must be emitted");

    let ln: Vec<Option<usize>> =
        lines.iter().map(|l| l["ln"].as_u64().map(|v| v as usize)).collect();
    assert_eq!(ln.first(), Some(&Some(0)), "first row starts at logical 0");
    // `ln` is carried only on the first visual row of each logical line
    // (0..=3, the trailing newline adds an empty 4th logical line); wrapped
    // continuation rows omit it entirely.
    let mut expected = 0;
    for (row, value) in ln.iter().enumerate() {
        if let Some(n) = value {
            assert_eq!(*n, expected, "row {row} must open logical line {expected}: {ln:?}");
            expected += 1;
        }
    }
    assert_eq!(expected, 4, "missing logical lines in {ln:?}");
    // Wrapped rows stay enumerated by their logical line, with continuation
    // rows carrying no `ln`.
    assert!(ln.len() > 4, "expected wrapping, got {ln:?}");
    assert!(ln.contains(&None), "continuation rows must omit ln: {ln:?}");
}

#[test]
fn word_wrap_waits_for_reported_view_size() {
    let text = Rope::from("one two three four five six seven eight nine ten");
    let mut view = View::new(1.into(), BufferId::new(2));

    // No frontend-reported size yet: word wrap must not break at width 0.
    view.update_wrap_settings(&text, 0, true);
    let (start, end) = view.lines.logical_line_range(&text, 0);
    assert_eq!((start, end), (0, text.len()), "no wrap before a size arrives");
    assert!(view.lines.is_converged(), "no pending wrap work at size 0");

    // Once the frontend reports its size, word wrap wraps at that width.
    view.set_size(Size { width: 10.0, height: 24.0 });
    view.update_wrap_settings(&text, 0, true);
    assert!(!view.lines.is_converged(), "word wrap is active once a size is reported");
}
