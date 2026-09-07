//! Render, find-all, and backend-syntax tests.
use super::*;

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

    let line = VisualLine { interval: Interval::new(0, 10), line_num: Some(1) };
    let encoded = view.encode_line(line, Some(&text), &syntax_spans, text.len());
    let syntax = encoded["syntax_spans"].as_array().expect("missing syntax spans");

    assert_eq!(syntax.len(), 2);
    assert_eq!(syntax[0]["start_byte"], 0);
    assert_eq!(syntax[0]["end_byte"], 3);
    assert_eq!(syntax[0]["scope"], "keyword.control.rust");
    assert_eq!(syntax[1]["start_byte"], 8);
    assert_eq!(syntax[1]["end_byte"], 9);
    assert_eq!(syntax[1]["scope"], "constant.numeric.decimal.rust");
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

    let line = VisualLine { interval: Interval::new(6, 16), line_num: Some(2) };
    let encoded = view.encode_line(line, Some(&text), &syntax_spans, text.len());
    let syntax = encoded["syntax_spans"].as_array().expect("missing syntax spans");

    assert_eq!(syntax.len(), 1);
    assert_eq!(syntax[0]["start_byte"], 0);
    assert_eq!(syntax[0]["end_byte"], 6);
    assert_eq!(syntax[0]["scope"], "entity.name.function.c");
}

#[test]
fn encode_line_omits_syntax_spans_when_backend_has_no_data() {
    let view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("plain text\n");
    let line = VisualLine { interval: Interval::new(0, 10), line_num: Some(1) };

    let encoded = view.encode_line(line, Some(&text), &[], text.len());

    assert!(encoded.get("syntax_spans").is_none());
}

#[test]
fn render_if_dirty_emits_backend_syntax_spans_without_plugin_update() {
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust"]);
    let mut view = View::new(1.into(), BufferId::new(2));
    let editor = crate::editor::Editor::with_text("let x = 1;\n");
    let (client, peer) = recording_client();
    view.debug_force_rewrap_cols(editor.get_buffer(), 80);

    view.render_if_dirty(editor.get_buffer(), &client, true, "rust", true);
    let notifications = peer.take_notifications();

    let syntax_refresh = notifications.iter().any(|(method, params)| {
        method == "update"
            && params["update"]["ops"].as_array().is_some_and(|ops| {
                ops.iter().any(|op| {
                    op["lines"].as_array().is_some_and(|lines| {
                        lines.iter().any(|line| line.get("syntax_spans").is_some())
                    })
                })
            })
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
        editor.get_buffer(),
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

    view.render_if_dirty(editor.get_buffer(), &client, true, "Plain Text", true);

    let notifications = peer.take_notifications();
    let syntax_refresh = notifications.iter().any(|(method, params)| {
        method == "update"
            && params["update"]["ops"].as_array().is_some_and(|ops| {
                ops.iter().any(|op| {
                    op["lines"].as_array().is_some_and(|lines| {
                        lines.iter().any(|line| line.get("syntax_spans").is_some())
                    })
                })
            })
    });

    assert!(!syntax_refresh, "unsupported languages should render without backend syntax");
}

#[test]
#[ignore = "manual perf probe for syntax payload size and render latency"]
fn syntax_span_render_perf_probe() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text =
        Rope::from((0..200).map(|index| format!("let value_{index} = 42;\n")).collect::<String>());

    let baseline_client = Client::new(Box::new(RecordingPeer::default()));
    let (syntax_client, syntax_peer) = recording_client();

    view.request_lines(&text, &baseline_client, 0, 199, true, "rust", false);

    let started = Instant::now();
    view.request_lines(&text, &syntax_client, 0, 199, true, "rust", true);
    let elapsed = started.elapsed();

    let syntax_bytes: usize = syntax_peer
        .take_notifications()
        .into_iter()
        .filter(|(method, _)| method == "update")
        .map(|(_, params)| serde_json::to_vec(&params).expect("serialize update payload").len())
        .sum();

    eprintln!(
        "syntax perf probe: visible_lines=200 payload_bytes={} render_us={}",
        syntax_bytes,
        elapsed.as_micros()
    );

    assert!(syntax_bytes > 0, "probe should emit a measured update payload");
}
