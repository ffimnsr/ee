//! Edit-ops tests: newline and smart-indent behavior.
use super::*;

#[test]
fn insert_newline_plain_when_auto_indent_disabled() {
    let text: Rope = "    hello".into();
    let mut config = test_config();
    config.auto_indent = false;

    let delta = insert_newline(&text, &[SelRegion::caret(text.len())], &config);

    assert_eq!(String::from(delta.apply(&text)), "    hello\n");
}

#[test]
fn insert_newline_copies_full_leading_whitespace_at_or_after_content() {
    let cases = [
        ("    hello", 9, "    hello\n    "),
        ("\thello", 6, "\thello\n\t"),
        (" \thello", 7, " \thello\n \t"),
        ("    hello", 6, "    he\n    llo"),
    ];

    let config = test_config();
    for (input, offset, expected) in cases {
        let text: Rope = input.into();
        let delta = insert_newline(&text, &[SelRegion::caret(offset)], &config);
        assert_eq!(String::from(delta.apply(&text)), expected, "input={input:?}, offset={offset}");
    }
}

#[test]
fn insert_newline_inside_indentation_copies_prefix_only() {
    let text: Rope = "    hello".into();
    let config = test_config();

    let delta = insert_newline(&text, &[SelRegion::caret(2)], &config);

    assert_eq!(String::from(delta.apply(&text)), "  \n    hello");
}

#[test]
fn insert_newline_replaces_selection_using_start_line_indent() {
    let text: Rope = "    hello world".into();
    let config = test_config();

    let delta = insert_newline(&text, &[SelRegion::new(6, 11)], &config);

    assert_eq!(String::from(delta.apply(&text)), "    he\n    orld");
}

#[test]
fn insert_newline_replaces_multiline_selection_using_start_line_indent() {
    let text: Rope = "    alpha\n  beta".into();
    let config = test_config();

    let delta = insert_newline(&text, &[SelRegion::new(6, text.len())], &config);

    assert_eq!(String::from(delta.apply(&text)), "    al\n    ");
}

#[test]
fn insert_newline_handles_multiple_cursors_deterministically() {
    let text: Rope = "    one\n\ttwo".into();
    let config = test_config();
    let regions = [SelRegion::caret(7), SelRegion::caret(text.len())];

    let delta = insert_newline(&text, &regions, &config);

    assert_eq!(String::from(delta.apply(&text)), "    one\n    \n\ttwo\n\t");
}

#[test]
fn insert_newline_preserves_configured_line_ending() {
    let text: Rope = "  hi".into();
    let mut config = test_config();
    config.line_ending = "\r\n".to_owned();

    let delta = insert_newline(&text, &[SelRegion::caret(text.len())], &config);

    assert_eq!(String::from(delta.apply(&text)), "  hi\r\n  ");
}

#[test]
fn insert_newline_after_opener_adds_one_indent_level() {
    let text: Rope = "if ready {".into();
    let config = test_config();

    let delta = insert_newline(&text, &[SelRegion::caret(text.len())], &config);

    assert_eq!(String::from(delta.apply(&text)), "if ready {\n\t");
}

#[test]
fn insert_newline_after_opener_uses_space_indent_policy() {
    let text: Rope = "if ready {".into();
    let mut config = test_config();
    config.translate_tabs_to_spaces = true;

    let delta = insert_newline(&text, &[SelRegion::caret(text.len())], &config);

    assert_eq!(String::from(delta.apply(&text)), "if ready {\n    ");
}

#[test]
fn insert_newline_before_closer_dedents_one_level() {
    let text: Rope = "    }".into();
    let config = test_config();

    let delta = insert_newline(&text, &[SelRegion::caret(4)], &config);

    assert_eq!(String::from(delta.apply(&text)), "    \n}");
}

#[test]
fn insert_newline_between_braces_keeps_opener_indent_without_brace_expansion() {
    let text: Rope = "{}".into();
    let config = test_config();

    let delta = insert_newline(&text, &[SelRegion::caret(1)], &config);

    assert_eq!(String::from(delta.apply(&text)), "{\n\t}");
}

#[test]
fn insert_newline_heuristics_disable_cleanly_when_smart_indent_disabled() {
    let text: Rope = "if ready {".into();
    let mut config = test_config();
    config.smart_indent = false;

    let delta = insert_newline(&text, &[SelRegion::caret(text.len())], &config);

    assert_eq!(String::from(delta.apply(&text)), "if ready {\n");
}

#[test]
fn insert_newline_multiline_selection_falls_back_to_baseline_indent() {
    let text: Rope = "if ready {\n    work();\n}".into();
    let config = test_config();

    let delta = insert_newline(&text, &[SelRegion::new(3, text.len() - 1)], &config);

    assert_eq!(String::from(delta.apply(&text)), "if \n}");
}

#[test]
fn insert_newline_markdown_continues_bullet_marker() {
    let text: Rope = "- foo".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("markdown", None, DocumentMode::Normal);

    let delta = insert_newline_with_context(
        &text,
        &[SelRegion::caret(text.len())],
        &config,
        Some(&context),
    );

    assert_eq!(String::from(delta.apply(&text)), "- foo\n- ");
}

#[test]
fn insert_newline_markdown_keeps_nested_list_indentation() {
    let text: Rope = "  - foo".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("markdown", None, DocumentMode::Normal);

    let delta = insert_newline_with_context(
        &text,
        &[SelRegion::caret(text.len())],
        &config,
        Some(&context),
    );

    assert_eq!(String::from(delta.apply(&text)), "  - foo\n  - ");
}

#[test]
fn insert_newline_markdown_continues_blockquote_and_nested_markers() {
    let cases = [
        ("  1. one", "  1. one\n  2. "), // ordered increments
        ("> quoted", "> quoted\n> "),    // blockquote
        ("> > deep", "> > deep\n> > "),  // nested quotes
        ("> - foo", "> - foo\n> - "),    // quote + bullet
        ("* star", "* star\n* "),        // alternate bullet
    ];

    let config = test_config();
    for (input, expected) in cases {
        let text: Rope = input.into();
        let context = SyntaxIndentContext::new("markdown", None, DocumentMode::Normal);
        let delta = insert_newline_with_context(
            &text,
            &[SelRegion::caret(text.len())],
            &config,
            Some(&context),
        );
        assert_eq!(String::from(delta.apply(&text)), expected, "input={input:?}");
    }
}

#[test]
fn insert_newline_markdown_ordered_markers_increment() {
    let cases = [
        ("1. one", "1. one\n2. "),      // basic increment
        ("10) item", "10) item\n11) "), // paren style
        ("0. zero", "0. zero\n1. "),    // zero-based
        ("> 1. x", "> 1. x\n> 2. "),    // quote + ordered
        ("9. ", "9. \n"),               // bare number marker ends the list
    ];

    let config = test_config();
    for (input, expected) in cases {
        let text: Rope = input.into();
        let context = SyntaxIndentContext::new("markdown", None, DocumentMode::Normal);
        let delta = insert_newline_with_context(
            &text,
            &[SelRegion::caret(text.len())],
            &config,
            Some(&context),
        );
        assert_eq!(String::from(delta.apply(&text)), expected, "input={input:?}");
    }
}

#[test]
fn insert_newline_markdown_task_items_continue_as_unchecked_checkbox() {
    let cases = [
        ("- [ ] task", "- [ ] task\n- [ ] "), // unchecked continues unchecked
        ("- [x] done", "- [x] done\n- [ ] "), // checked resets to unchecked
        ("- [X] done", "- [X] done\n- [ ] "), // uppercase checked
        ("- [ ] ", "- [ ] \n"),               // bare checkbox ends the list
    ];

    let config = test_config();
    for (input, expected) in cases {
        let text: Rope = input.into();
        let context = SyntaxIndentContext::new("markdown", None, DocumentMode::Normal);
        let delta = insert_newline_with_context(
            &text,
            &[SelRegion::caret(text.len())],
            &config,
            Some(&context),
        );
        assert_eq!(String::from(delta.apply(&text)), expected, "input={input:?}");
    }
}

#[test]
fn insert_newline_markdown_empty_item_ends_list_or_quote() {
    // A bare marker ends the block: no marker on the new line. Nested items
    // keep their line's leading indentation (carried below the marker).
    let cases = [("- ", "- \n"), ("> ", "> \n"), ("  - ", "  - \n  ")];

    let config = test_config();
    for (input, expected) in cases {
        let text: Rope = input.into();
        let context = SyntaxIndentContext::new("markdown", None, DocumentMode::Normal);
        let delta = insert_newline_with_context(
            &text,
            &[SelRegion::caret(text.len())],
            &config,
            Some(&context),
        );
        assert_eq!(String::from(delta.apply(&text)), expected, "input={input:?}");
    }
}

#[test]
fn insert_newline_markdown_does_not_continue_marker_mid_line_or_plain_text() {
    let cases = [
        ("- foo", 2, "- \nfoo"), // caret inside the item: plain split
        ("- foo", 3, "- f\noo"), // caret after the marker: still plain
        ("plain", 5, "plain\n"), // no marker at all
    ];

    let config = test_config();
    for (input, offset, expected) in cases {
        let text: Rope = input.into();
        let context = SyntaxIndentContext::new("markdown", None, DocumentMode::Normal);
        let delta = insert_newline_with_context(
            &text,
            &[SelRegion::caret(offset)],
            &config,
            Some(&context),
        );
        assert_eq!(String::from(delta.apply(&text)), expected, "input={input:?} offset={offset}");
    }
}

#[test]
fn insert_newline_marker_continuation_is_markdown_only() {
    let text: Rope = "- foo".into();
    let config = test_config();
    // Non-markdown languages keep the plain carried indent.
    let context = SyntaxIndentContext::new("rust", None, DocumentMode::Normal);

    let delta = insert_newline_with_context(
        &text,
        &[SelRegion::caret(text.len())],
        &config,
        Some(&context),
    );

    assert_eq!(String::from(delta.apply(&text)), "- foo\n");
}

#[test]
fn insert_newline_with_syntax_align_outcome_aligns_to_anchor_column() {
    let _guard = runtime_loader_test_guard();
    let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
    install_indent_query("rust", "(call_expression (identifier) @anchor) @align");

    let text: Rope = "  foo()".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("rust", None, DocumentMode::Normal);
    let anchor = text.len();

    let delta =
        insert_newline_with_context(&text, &[SelRegion::caret(anchor)], &config, Some(&context));

    // The new line is aligned to the anchor identifier's column (2) instead of
    // carrying the opening line's indent.
    assert_eq!(String::from(delta.apply(&text)), "  foo()\n  ");
}

#[test]
fn insert_newline_with_syntax_align_outcome_supports_space_indent_policy() {
    let _guard = runtime_loader_test_guard();
    let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
    install_indent_query("rust", "(call_expression (identifier) @anchor) @align");

    let text: Rope = "    foo()".into();
    let mut config = test_config();
    config.translate_tabs_to_spaces = true;
    let context = SyntaxIndentContext::new("rust", None, DocumentMode::Normal);
    let anchor = text.len();

    let delta =
        insert_newline_with_context(&text, &[SelRegion::caret(anchor)], &config, Some(&context));

    assert_eq!(String::from(delta.apply(&text)), "    foo()\n    ");
}

#[test]
fn insert_newline_with_syntax_context_uses_indent_query_outcome() {
    let _guard = runtime_loader_test_guard();
    let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
    install_indent_query("rust", "(_) @indent");

    let text: Rope = "fn main() {}".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("rust", None, DocumentMode::Normal);
    let anchor = "fn main() {".len();

    let delta =
        insert_newline_with_context(&text, &[SelRegion::caret(anchor)], &config, Some(&context));

    assert_eq!(String::from(delta.apply(&text)), "fn main() {\n\t}");
}

#[test]
fn insert_newline_with_syntax_context_falls_back_to_heuristic_when_query_missing() {
    let _guard = runtime_loader_test_guard();
    let _override_guard = RuntimeLoaderOverrideGuard::install_with_query_language(
        &["rust"],
        Some("rust-missing-indent-query"),
    );

    let text: Rope = "if ready {".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("rust", None, DocumentMode::Normal);

    let delta = insert_newline_with_context(
        &text,
        &[SelRegion::caret(text.len())],
        &config,
        Some(&context),
    );

    assert_eq!(String::from(delta.apply(&text)), "if ready {\n\t");
}

#[test]
fn insert_newline_with_syntax_context_fails_closed_when_mode_disallows_whole_doc_ops() {
    let _guard = runtime_loader_test_guard();
    let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
    install_indent_query("rust", "(_) @indent");

    let text: Rope = "fn main() {}".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("rust", None, DocumentMode::ConstrainedNormal);
    let anchor = "fn main() {".len();

    let delta =
        insert_newline_with_context(&text, &[SelRegion::caret(anchor)], &config, Some(&context));

    assert_eq!(String::from(delta.apply(&text)), "fn main() {\n\t}");
}

#[test]
fn insert_newline_plain_text_buffer_uses_baseline_auto_indent_only() {
    let text: Rope = "    note".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("Plain Text", None, DocumentMode::Normal);

    let delta = insert_newline_with_context(
        &text,
        &[SelRegion::caret(text.len())],
        &config,
        Some(&context),
    );

    assert_eq!(String::from(delta.apply(&text)), "    note\n    ");
}

#[test]
fn insert_newline_json_buffer_supports_syntax_indent_query() {
    let _guard = runtime_loader_test_guard();
    let _override_guard = RuntimeLoaderOverrideGuard::install(&["json"]);
    install_indent_query("json", "(_) @indent");

    let text: Rope = "{}".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("json", None, DocumentMode::Normal);

    let delta = insert_newline_with_context(&text, &[SelRegion::caret(1)], &config, Some(&context));

    assert_eq!(String::from(delta.apply(&text)), "{\n\t}");
}

#[test]
fn insert_newline_python_buffer_supports_syntax_indent_query_when_ready() {
    let _guard = runtime_loader_test_guard();
    let _override_guard = RuntimeLoaderOverrideGuard::install(&["python"]);
    install_indent_query("python", "(_) @indent");

    let text: Rope = "if ok:\n    pass\n".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("python", None, DocumentMode::Normal);
    let anchor = "if ok:".len();

    let delta =
        insert_newline_with_context(&text, &[SelRegion::caret(anchor)], &config, Some(&context));

    assert_eq!(String::from(delta.apply(&text)), "if ok:\n\t\n    pass\n");
}

#[test]
fn insert_newline_with_syntax_context_falls_back_when_parser_missing() {
    let text: Rope = "if ready {".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("totally-unknown-language", None, DocumentMode::Normal);

    let delta = insert_newline_with_context(
        &text,
        &[SelRegion::caret(text.len())],
        &config,
        Some(&context),
    );

    assert_eq!(String::from(delta.apply(&text)), "if ready {\n\t");
}

#[test]
fn insert_newline_with_syntax_context_falls_back_when_indent_query_malformed() {
    let _guard = runtime_loader_test_guard();
    let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
    install_indent_query("rust", "(");

    let text: Rope = "if ready {".into();
    let config = test_config();
    let context = SyntaxIndentContext::new("rust", None, DocumentMode::Normal);

    let delta = insert_newline_with_context(
        &text,
        &[SelRegion::caret(text.len())],
        &config,
        Some(&context),
    );

    assert_eq!(String::from(delta.apply(&text)), "if ready {\n\t");
}

#[test]
fn insert_newline_with_syntax_context_respects_disabled_smart_indent_on_query_miss() {
    let _guard = runtime_loader_test_guard();
    let _override_guard = RuntimeLoaderOverrideGuard::install_with_query_language(
        &["rust"],
        Some("rust-missing-indent-query"),
    );

    let text: Rope = "if ready {".into();
    let mut config = test_config();
    config.smart_indent = false;
    let context = SyntaxIndentContext::new("rust", None, DocumentMode::Normal);

    let delta = insert_newline_with_context(
        &text,
        &[SelRegion::caret(text.len())],
        &config,
        Some(&context),
    );

    assert_eq!(String::from(delta.apply(&text)), "if ready {\n");
}

#[test]
fn insert_newline_smart_indent_multicursor_remains_deterministic() {
    let text: Rope = "{\n[".into();
    let config = test_config();
    let regions = [SelRegion::caret(1), SelRegion::caret(text.len())];

    let delta = insert_newline(&text, &regions, &config);

    assert_eq!(String::from(delta.apply(&text)), "{\n\t\n[\n\t");
}

#[test]
fn insert_newline_smart_indent_selection_replacement_remains_deterministic() {
    let text: Rope = "{alpha}".into();
    let config = test_config();

    let delta = insert_newline(&text, &[SelRegion::new(1, 6)], &config);

    assert_eq!(String::from(delta.apply(&text)), "{\n\t}");
}
