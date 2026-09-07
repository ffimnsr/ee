//! Event-context tests: motion.
use super::*;

#[test]
fn goto_line_out_of_bounds_alerts_instead_of_panicking() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("hello\nworld\n");
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::GotoLine { line: 99 });

    let notifications = harness.take_notifications();
    let alert = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected alert notification");
    assert_eq!(alert.1["msg"], json!("goto_line: line 99 beyond last line 3"));
    assert_eq!(harness.debug_render(), "|hello\nworld\n");
}

#[test]
fn gesture_out_of_bounds_alerts_instead_of_panicking() {
    use crate::rpc::{EditNotification, GestureType::PointSelect};

    let harness = ContextHarness::new("hello\nworld\n");
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 99, col: 0, ty: PointSelect });

    let notifications = harness.take_notifications();
    let alert = notifications
        .iter()
        .find(|(method, _)| method == "alert")
        .expect("expected alert notification");
    assert_eq!(alert.1["msg"], json!("gesture: line 99 beyond last line 3"));
    assert_eq!(harness.debug_render(), "|hello\nworld\n");
}

#[test]
fn toggle_line_comment_edits_current_line() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("fn main() {}\n");
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("rust");

    ctx.do_edit(EditNotification::ToggleLineComment);

    assert_eq!(harness.debug_render(), "// |fn main() {}\n");
}

#[test]
fn toggle_block_comment_edits_current_line_when_language_has_no_line_comment() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("div { color: red; }\n");
    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("CSS");

    ctx.do_edit(EditNotification::ToggleBlockComment);

    assert_eq!(harness.debug_render(), "/* |div { color: red; } */\n");
}

#[test]
fn reindent_unsupported_language_avoids_background_task_and_panics() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("<div>\n<span>hi</span>\n</div>\n");
    harness.take_notifications();

    let mut ctx = harness.make_context();
    ctx.language = LanguageId::from("HTML");

    ctx.do_edit(EditNotification::Reindent);

    assert!(!harness.editor.borrow().whole_scan_task.is_in_progress());
    let notifications = harness.take_notifications();
    assert!(notifications.iter().all(|(method, _)| method != "alert"));
    assert_eq!(harness.debug_render(), "|<div>\n<span>hi</span>\n</div>\n");
}

#[test]
fn goto_column_uses_display_width_and_can_extend_selection() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("日本x");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::GotoColumn { display_col: 2, modify_selection: false });
    assert_eq!(harness.debug_render(), "日|本x");

    ctx.do_edit(EditNotification::GotoColumn { display_col: 0, modify_selection: false });
    ctx.do_edit(EditNotification::GotoColumn { display_col: 2, modify_selection: true });
    assert_eq!(harness.debug_render(), "[日|]本x");
}

#[test]
fn goto_column_uses_logical_column_even_when_view_is_wrapped() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("abcdef");
    {
        let text = harness.editor.borrow().get_buffer().clone();
        harness.view.borrow_mut().debug_force_rewrap_cols(&text, 2);
    }

    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::GotoColumn { display_col: 4, modify_selection: false });

    assert_eq!(harness.debug_render(), "abcd|ef");
}

#[test]
fn goto_next_paragraph_moves_to_next_nonblank_block() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\nbeta\n\ncharlie\n\ndelta\n");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::GotoNextParagraph);
    assert_eq!(harness.debug_render(), "alpha\nbeta\n\n|charlie\n\ndelta\n");

    ctx.do_edit(EditNotification::GotoNextParagraph);
    assert_eq!(harness.debug_render(), "alpha\nbeta\n\ncharlie\n\n|delta\n");
}

#[test]
fn goto_prev_paragraph_moves_to_previous_nonblank_block() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\n\nbeta\ngamma\n\ndelta\n");
    {
        let text = harness.editor.borrow().get_buffer().clone();
        harness.view.borrow_mut().set_selection(
            &text,
            crate::selection::Selection::new_simple(SelRegion::caret(
                crate::line_offset::LogicalLines.offset_of_line(&text, 5),
            )),
        );
    }
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::GotoPrevParagraph);
    assert_eq!(harness.debug_render(), "alpha\n\n|beta\ngamma\n\ndelta\n");

    ctx.do_edit(EditNotification::GotoPrevParagraph);
    assert_eq!(harness.debug_render(), "|alpha\n\nbeta\ngamma\n\ndelta\n");
}

#[test]
fn add_newline_commands_insert_blank_lines_around_current_line() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\nbeta");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::AddNewlineBelow);
    assert_eq!(harness.debug_render(), "alpha\n|\nbeta");

    let harness = ContextHarness::new("alpha\nbeta");
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::MoveDown);
    ctx.do_edit(EditNotification::AddNewlineAbove);
    assert_eq!(harness.debug_render(), "alpha\n|\nbeta");
}

#[test]
fn join_selections_joins_current_and_next_line() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("abc\n    def\nxyz");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::JoinSelections { select_space: false });

    assert_eq!(harness.debug_render(), "abc def|xyz");
}

#[test]
fn join_selections_space_selects_inserted_space() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("abc\n    def\nxyz");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::JoinSelections { select_space: true });

    assert_eq!(harness.debug_render(), "abc[ |]defxyz");
}

#[test]
fn join_selections_handles_multiple_regions() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("aa\n  bb\ncc\n  dd\nend");
    {
        let text = harness.editor.borrow().get_buffer().clone();
        let mut selection = crate::selection::Selection::new();
        selection.add_region(SelRegion::new(text.offset_of_line(0), text.offset_of_line(2)));
        selection.add_region(SelRegion::new(text.offset_of_line(2), text.offset_of_line(4)));
        harness.view.borrow_mut().set_selection(&text, selection);
    }

    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::JoinSelections { select_space: true });

    assert_eq!(harness.debug_render(), "aa[ |]bbcc[ |]ddend");
}
