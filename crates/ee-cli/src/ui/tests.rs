//! UI rendering tests.
use super::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[cfg(feature = "agents")]
#[test]
fn agents_system_notices_wrap_inside_transcript_width() {
    let item = crate::app::TranscriptItem::System {
        text: String::from(
            "commands: /compact — Summarize session history, /discard — Discard paused work",
        ),
        at: std::time::SystemTime::UNIX_EPOCH,
    };

    let lines = transcript_lines(&item, 24, false);
    assert!(lines.len() > 1, "long notice must wrap");
    assert!(lines.iter().all(|line| line.width() <= 24));
    assert_eq!(lines[0].spans[0].content, "-!- ");
    assert_eq!(lines[1].spans[0].content, "    ");
}

#[test]
fn apply_annotation_overlay_styles_target_range() {
    let spans = vec![Span::styled("alpha", Style::default().fg(theme::BORDER_PICKER_RESULTS))];
    let visual = annotation_visual("find");

    let out = apply_annotation_overlay(spans, 1, 3, 0, visual);

    assert_eq!(out.len(), 3);
    assert_eq!(out[0].content, "a");
    assert_eq!(out[1].content, "lp");
    assert_eq!(out[2].content, "ha");
    assert_eq!(
        out[1].style,
        Style::default().fg(theme::BG_APP).bg(visual.bg).add_modifier(Modifier::BOLD)
    );
}

#[test]
fn apply_core_annotations_maps_byte_ranges_to_display_cols() {
    let spans = vec![Span::styled("abcdef", Style::default())];
    let annotations = vec![CoreAnnotation {
        annotation_type: String::from("other"),
        ranges: vec![[0, 2, 0, 5]],
        payloads: None,
    }];

    let out = apply_core_annotations(spans, "abcdef", 0, &annotations, 0);

    assert_eq!(out.len(), 3);
    assert_eq!(out[0].content, "ab");
    assert_eq!(out[1].content, "cde");
    assert_eq!(
        out[1].style,
        Style::default().bg(theme::BG_ANNOTATION).add_modifier(Modifier::UNDERLINED)
    );
}

#[test]
fn collect_line_annotation_segments_merges_same_priority_overlaps() {
    let annotations = vec![
        CoreAnnotation {
            annotation_type: String::from("lint"),
            ranges: vec![[0, 1, 0, 3]],
            payloads: None,
        },
        CoreAnnotation {
            annotation_type: String::from("lint"),
            ranges: vec![[0, 2, 0, 5]],
            payloads: None,
        },
    ];

    let segments = collect_line_annotation_segments("abcdef", 0, &annotations);

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].start_display, 1);
    assert_eq!(segments[0].end_display, 5);
}

#[test]
fn collect_line_annotation_segments_sorts_by_priority() {
    let annotations = vec![
        CoreAnnotation {
            annotation_type: String::from("find"),
            ranges: vec![[0, 1, 0, 2]],
            payloads: None,
        },
        CoreAnnotation {
            annotation_type: String::from("selection"),
            ranges: vec![[0, 1, 0, 2]],
            payloads: None,
        },
    ];

    let segments = collect_line_annotation_segments("abcdef", 0, &annotations);

    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].priority, annotation_priority("selection"));
    assert_eq!(segments[1].priority, annotation_priority("find"));
}

#[test]
fn annotation_marker_for_line_prefers_payload_backed_plugin_annotations() {
    let buf = BufState {
        id: 1,
        path: None,
        display_name: None,
        view_id: String::new(),
        editor_config_synced: true,
        pending_line_request: false,
        line_cache: Vec::new(),
        lines: vec![String::from("alpha")],
        cursor_line: 0,
        cursor_col: 0,
        pristine: true,
        save_complete: true,
        last_save_generation: 0,
        completed_save_generation: 0,
        last_save_result_generation: 0,
        last_save_succeeded: true,
        last_save_permission_denied: false,
        last_save_error_message: None,
        status_message: None,
        last_scroll: None,
        mtime: None,
        externally_modified: false,
        diagnostics: Vec::new(),
        annotations: vec![CoreAnnotation {
            annotation_type: String::from("lint"),
            ranges: vec![[0, 0, 0, 3]],
            payloads: Some(vec![serde_json::Value::String(String::from("todo"))]),
        }],
        is_vlf: false,
        vlf_cache_start_line: 0,
        vlf_previous_viewport: None,
        vlf_generation: 0,
        vlf_approx_line_count: 0,
        vlf_line_count_exact: false,
        pending_vlf_tail_jump: false,
        vlf_search_ranges: Vec::new(),
    };

    assert_eq!(annotation_marker_for_line(&buf, 0), Some(('T', theme::FG_MARKER_HINT)));
}

#[test]
fn status_messages_render_as_top_right_toasts_not_prompt_text() {
    let mut app = App::from_path(None).unwrap();
    app.backend.status_message = Some(String::from("saved /tmp/example.rs"));
    app.sync_status_toast();

    let width = 80;
    let height = 10;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();
    let rect = toast_rect(Rect::new(0, 0, width, height), "saved /tmp/example.rs").unwrap();
    let row_text = |y| {
        (rect.x + 1..rect.right() - 1)
            .map(|x| buffer.cell((x, y)).unwrap().symbol())
            .collect::<String>()
    };
    let content_row = row_text(rect.y + 1);
    let prompt_row =
        (0..width).map(|x| buffer.cell((x, height - 1)).unwrap().symbol()).collect::<String>();

    assert!(content_row.contains("saved /tmp/example.rs"), "toast row: {content_row:?}");
    assert_eq!(buffer.cell((rect.x + 1, rect.y + 1)).unwrap().symbol(), " ");
    assert_eq!(buffer.cell((rect.right() - 2, rect.y + 1)).unwrap().symbol(), " ");
    assert!(prompt_row.trim().is_empty(), "prompt row: {prompt_row:?}");
    assert_eq!(buffer.cell((rect.x + 1, rect.y + 1)).unwrap().bg, theme::BG_CHROME);
}

#[test]
fn static_mode_prompts_are_blank() {
    let mut app = App::from_path(None).unwrap();
    let modes = [
        Mode::Insert,
        Mode::Visual,
        Mode::VisualLine,
        Mode::VisualBlock,
        Mode::Picker,
        Mode::Quickfix,
        Mode::LocationList,
        Mode::OperatorPending,
        Mode::Agent,
    ];

    for mode in modes {
        app.mode = mode;
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui(frame, &app)).unwrap();
        let prompt_row = (0..80)
            .map(|x| terminal.backend().buffer().cell((x, 9)).unwrap().symbol())
            .collect::<String>();

        assert!(prompt_row.trim().is_empty(), "{mode:?} prompt: {prompt_row:?}");
    }
}

#[test]
fn toast_rect_is_top_right_and_content_sized() {
    let area = Rect { x: 10, y: 4, width: 120, height: 40 };
    let rect = toast_rect(area, "saved /tmp/example.rs").expect("space for toast");

    assert_eq!(rect.x, 104);
    assert_eq!(rect.y, 5);
    assert_eq!(rect.width, 25);
    assert_eq!(rect.height, 3);
}

#[test]
fn toast_hard_wraps_long_paths_and_accounts_for_horizontal_padding() {
    let area = Rect { x: 0, y: 0, width: 58, height: 20 };
    let message = "saved\n/home/pastel/Projects/ee/test_assets/sample-program/sample-program.rs";
    let rect = toast_rect(area, message).expect("space for toast");
    let lines =
        toast_content_lines(message, rect.width, rect.height.saturating_sub(TOAST_FRAME_HEIGHT));

    assert_eq!(rect.x, 1);
    assert_eq!(rect.y, 1);
    assert_eq!(rect.width, 56);
    assert_eq!(rect.height, lines.len() as u16 + TOAST_FRAME_HEIGHT);
    assert_eq!(lines[0], "saved");
    assert_eq!(lines[1..].concat(), message.lines().nth(1).unwrap());
}

#[test]
fn vlf_search_ranges_render_with_find_highlight() {
    let mut app = App::from_path(None).unwrap();
    app.backend.is_vlf = true;
    app.backend.line_cache = vec![LineSlot::Known(crate::backend::CachedLine {
        text: String::from("alpha needle omega"),
        cursors: vec![],
        syntax_spans: vec![],
    })];
    app.backend.vlf_search_ranges = vec![VlfSearchRange { line: 0, start_col: 6, end_col: 12 }];

    let width: u16 = 40;
    let height: u16 = 8;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui(frame, &app)).unwrap();
    let buf = terminal.backend().buffer();

    let find_bg = theme::BG_FIND;
    let gutter_width: u16 = 5;
    let highlighted =
        (gutter_width + 6..gutter_width + 12).any(|x| buf.cell((x, 0)).unwrap().bg == find_bg);

    assert!(highlighted, "VLF search range should render using find highlight");
    assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "•");
}

#[test]
fn vlf_cursor_position_uses_line_cache_text() {
    let mut app = App::from_path(None).unwrap();
    app.backend.is_vlf = true;
    app.backend.cursor_line = 0;
    app.backend.cursor_col = 3;
    app.backend.line_cache = vec![LineSlot::Known(crate::backend::CachedLine {
        text: String::from("abcdef"),
        cursors: vec![],
        syntax_spans: vec![],
    })];

    let pos = cursor_position_for(
        app.backend.active(),
        app.viewport,
        &app,
        buffer_content_area(Rect { x: 5, y: 2, width: 20, height: 4 }),
        Rect { x: 0, y: 7, width: 20, height: 1 },
    );

    assert_eq!(pos, Position::new(9, 2));
}

#[cfg(feature = "agents")]
#[test]
fn plan_modal_floats_at_top_right() {
    let area = Rect { x: 10, y: 4, width: 120, height: 40 };

    let modal = plan_modal_rect(area, 2);

    assert_eq!(modal, Rect { x: 56, y: 6, width: 72, height: 6 });
}

#[test]
fn rendered_spans_pad_to_viewport_width() {
    let spans = vec![Span::styled("short", Style::default().fg(Color::Green))];
    let padded = pad_spans_to_width(spans, 8, Style::default().bg(Color::Black));
    let joined = padded.iter().map(|span| span.content.as_ref()).collect::<String>();

    assert_eq!(joined, "short   ");
    assert_eq!(padded.last().unwrap().style.bg, Some(Color::Black));
}

#[test]
fn rendered_spans_expand_tabs_to_spaces() {
    let spans = vec![Span::styled("ab\tcd", Style::default().fg(Color::Green))];
    let expanded = expand_tabs_in_spans(spans, 4);
    let joined = expanded.iter().map(|span| span.content.as_ref()).collect::<String>();

    assert_eq!(joined, "ab  cd");
    assert_eq!(expanded[0].style.fg, Some(Color::Green));
}
