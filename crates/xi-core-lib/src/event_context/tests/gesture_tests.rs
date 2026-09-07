//! Event-context tests: gesture.
use super::*;

#[test]
fn counted_vertical_motion_moves_relative_to_backend_cursor() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("zero\none\ntwo\nthree");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::MoveVertical { lines: 2, up: false, modify_selection: false });
    assert_eq!(harness.debug_render(), "zero\none\n|two\nthree");

    ctx.do_edit(EditNotification::MoveVertical { lines: 1, up: true, modify_selection: false });
    assert_eq!(harness.debug_render(), "zero\n|one\ntwo\nthree");
}

#[test]
fn test_gestures() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\
        this is a string\n\
        that has three\n\
        lines.";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::MoveDown);
    ctx.do_edit(EditNotification::MoveDown);
    ctx.do_edit(EditNotification::MoveToEndOfParagraph);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that has three\n\
        lines.|"
    );

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
    ctx.do_edit(EditNotification::MoveToEndOfParagraphAndModifySelection);
    assert_eq!(
        harness.debug_render(),
        "\
        [this is a string|]\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::MoveToEndOfParagraph);
    ctx.do_edit(EditNotification::MoveToBeginningOfParagraphAndModifySelection);
    assert_eq!(
        harness.debug_render(),
        "\
        [|this is a string]\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
    assert_eq!(
        harness.debug_render(),
        "\
        |this is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 5, ty: PointSelect });
    assert_eq!(
        harness.debug_render(),
        "\
        this |is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
        this |is a string\n\
        that |has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::MoveToRightEndOfLineAndModifySelection);
    assert_eq!(
        harness.debug_render(),
        "\
        this [is a string|]\n\
        that [has three|]\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: MultiWordSelect });
    assert_eq!(
        harness.debug_render(),
        "\
        this [is a string|]\n\
        that [has three|]\n\
        [lines|]."
    );

    ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
        this [is a string|]\n\
        that [has three|]\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
        this [is a string|]\n\
        that [has three|]\n\
        li|nes."
    );

    ctx.do_edit(EditNotification::MoveToLeftEndOfLine);
    assert_eq!(
        harness.debug_render(),
        "\
        |this is a string\n\
        |that has three\n\
        |lines."
    );

    ctx.do_edit(EditNotification::MoveWordRight);
    assert_eq!(
        harness.debug_render(),
        "\
        this| is a string\n\
        that| has three\n\
        lines|."
    );

    ctx.do_edit(EditNotification::MoveToLeftEndOfLineAndModifySelection);
    assert_eq!(
        harness.debug_render(),
        "\
        [|this] is a string\n\
        [|that] has three\n\
        [|lines]."
    );

    ctx.do_edit(EditNotification::CollapseSelections);
    ctx.do_edit(EditNotification::MoveToRightEndOfLine);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string|\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::Gesture { line: 2, col: 2, ty: MultiLineSelect });
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string|\n\
        that has three\n\
        [lines.|]"
    );

    ctx.do_edit(EditNotification::SelectAll);
    assert_eq!(
        harness.debug_render(),
        "\
        [this is a string\n\
        that has three\n\
        lines.|]"
    );

    ctx.do_edit(EditNotification::CollapseSelections);
    ctx.do_edit(EditNotification::AddSelectionAbove);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that h|as three\n\
        lines.|"
    );

    ctx.do_edit(EditNotification::MoveRight);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that ha|s three\n\
        lines.|"
    );

    ctx.do_edit(EditNotification::MoveLeft);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that h|as three\n\
        lines|."
    );
}
