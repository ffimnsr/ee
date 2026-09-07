//! Event-context tests: filter.
use super::*;

#[test]
fn preview_filter_selections_keeps_matching_regions() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta alps");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
            SelectionRange { start: 11, end: 15 },
        ],
    });

    let filtered =
        ctx.preview_filter_selections("^a", false).expect("filter preview should succeed");

    assert_eq!(
        filtered,
        vec![SelectionRange { start: 0, end: 5 }, SelectionRange { start: 11, end: 15 }]
    );
}

#[test]
fn preview_filter_selections_removes_matching_regions() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta alps");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![
            SelectionRange { start: 0, end: 5 },
            SelectionRange { start: 6, end: 10 },
            SelectionRange { start: 11, end: 15 },
        ],
    });

    let filtered =
        ctx.preview_filter_selections("^a", true).expect("filter preview should succeed");

    assert_eq!(filtered, vec![SelectionRange { start: 6, end: 10 }]);
}

#[test]
fn set_selections_replaces_current_selection_regions() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta alps");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![SelectionRange { start: 6, end: 10 }],
    });

    assert_eq!(harness.debug_render(), "alpha [beta|] alps");
}

#[test]
fn extend_line_below_expands_to_next_line_start() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 2, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::ExtendLineBelow { count: 1 });

    assert_eq!(harness.debug_render(), "[alpha\n|]beta\ngamma");
}

#[test]
fn extend_line_above_selects_current_line_then_previous_line() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 1, col: 2, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::ExtendLineAbove);
    assert_eq!(harness.debug_render(), "alpha\n[beta\n|]gamma");

    ctx.do_edit(EditNotification::ExtendLineAbove);
    assert_eq!(harness.debug_render(), "[alpha\nbeta\n|]gamma");
}

#[test]
fn select_line_commands_adjust_active_edge_from_anchor() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 1, col: 2, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::SelectLineBelow);
    assert_eq!(harness.debug_render(), "alpha\n[beta\n|]gamma");

    ctx.do_edit(EditNotification::SelectLineBelow);
    assert_eq!(harness.debug_render(), "alpha\n[beta\ngamma|]");

    ctx.do_edit(EditNotification::SelectLineAbove);
    assert_eq!(harness.debug_render(), "alpha\n[beta\n|]gamma");

    ctx.do_edit(EditNotification::SelectLineAbove);
    assert_eq!(harness.debug_render(), "[|alpha\nbeta\n]gamma");
}

#[test]
fn extend_to_line_bounds_selects_entire_lines() {
    use crate::rpc::{EditNotification, GestureType, SelectionGranularity};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 1, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::Gesture {
        line: 1,
        col: 2,
        ty: GestureType::SelectExtend { granularity: SelectionGranularity::Point },
    });
    ctx.do_edit(EditNotification::ExtendToLineBounds);

    assert_eq!(harness.debug_render(), "[alpha\nbeta\n|]gamma");
}

#[test]
fn move_word_start_uses_backend_vim_semantics() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::MoveWordStart {
        forward: true,
        long_word: false,
        modify_selection: false,
    });
    assert_eq!(harness.debug_render(), "alpha |beta");

    ctx.do_edit(EditNotification::MoveWordStart {
        forward: false,
        long_word: false,
        modify_selection: false,
    });
    assert_eq!(harness.debug_render(), "|alpha beta");
}

#[test]
fn move_word_end_extends_selection_when_requested() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha beta");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::MoveWordEnd { long_word: false, modify_selection: true });

    assert_eq!(harness.debug_render(), "[alph|]a beta");
}

#[test]
fn find_char_moves_with_inclusive_and_exclusive_variants() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("abcabc");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::FindChar {
        target: 'b',
        forward: true,
        inclusive: true,
        modify_selection: false,
    });
    assert_eq!(harness.debug_render(), "a|bcabc");

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 6, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::FindChar {
        target: 'b',
        forward: false,
        inclusive: false,
        modify_selection: true,
    });
    assert_eq!(harness.debug_render(), "abcab[|c]");
}

#[test]
fn move_to_matching_bracket_handles_nested_multiline_pairs() {
    use crate::rpc::{EditNotification, GestureType};

    let harness = ContextHarness::new("fn main() {\n    (alpha + [beta])\n}\n");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 10, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::MoveToMatchingBracket { modify_selection: false });
    assert_eq!(harness.debug_render(), "fn main() {\n    (alpha + [beta])\n|}\n");

    ctx.do_edit(EditNotification::Gesture { line: 1, col: 4, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::MoveToMatchingBracket { modify_selection: true });
    assert_eq!(harness.debug_render(), "fn main() {\n    [(alpha + [beta]|])\n}\n");
}

#[test]
fn preview_select_chars_respects_multibyte_boundaries() {
    let harness = ContextHarness::new("aéb");
    let mut ctx = harness.make_context();

    let selection = ctx.preview_select_chars(2);

    assert_eq!(selection, vec![SelectionRange { start: 0, end: 3 }]);
}

#[test]
fn preview_selected_text_uses_backend_selection_truth() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\nbeta");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![SelectionRange { start: 1, end: 8 }],
    });

    assert_eq!(ctx.preview_selected_text(false), "lpha\nbe");
    assert_eq!(ctx.preview_selected_text(true), "alpha\nbeta\n");
}

#[test]
fn preview_selected_text_uses_text_store_for_constrained_normal() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("alpha\nbeta");
    harness.editor.borrow_mut().set_document_mode(DocumentMode::ConstrainedNormal);
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::SetSelections {
        selections: vec![SelectionRange { start: 1, end: 8 }],
    });

    assert_eq!(ctx.preview_selected_text(false), "lpha\nbe");
    assert_eq!(ctx.preview_selected_text(true), "alpha\nbeta\n");
}

#[test]
fn preview_block_text_respects_requested_rectangle() {
    let harness = ContextHarness::new("abcd\nefgh\nijk");
    let mut ctx = harness.make_context();

    assert_eq!(ctx.preview_block_text(0, 2, 1, 3), "bc\nfg\njk\n");
}

#[test]
fn shrink_to_line_bounds_drops_partial_outer_lines() {
    use crate::rpc::{EditNotification, GestureType, SelectionGranularity};

    let harness = ContextHarness::new("alpha\nbeta\ngamma");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 1, ty: GestureType::PointSelect });
    ctx.do_edit(EditNotification::Gesture {
        line: 2,
        col: 2,
        ty: GestureType::SelectExtend { granularity: SelectionGranularity::Point },
    });
    ctx.do_edit(EditNotification::ShrinkToLineBounds);

    assert_eq!(harness.debug_render(), "alpha\n[beta\n|]gamma");
}
