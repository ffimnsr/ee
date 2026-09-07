//! Event-context tests: edit.
use super::*;

#[test]
fn edit_type_to_string_matches_wire_names() {
    assert_eq!(edit_type_to_string(EditType::InsertChars), "insert");
    assert_eq!(edit_type_to_string(EditType::InsertNewline), "newline");
    assert_eq!(edit_type_to_string(EditType::Other), "other");
}

#[test]
fn delete_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\
        this is a string\n\
        that has three\n\
        lines.";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });

    ctx.do_edit(EditNotification::MoveRight);
    assert_eq!(
        harness.debug_render(),
        "\
        t|his is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::DeleteBackward);
    assert_eq!(
        harness.debug_render(),
        "\
        |his is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::DeleteForward);
    assert_eq!(
        harness.debug_render(),
        "\
        |is is a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::MoveWordRight);
    ctx.do_edit(EditNotification::DeleteWordForward);
    assert_eq!(
        harness.debug_render(),
        "\
        is| a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::DeleteWordBackward);
    assert_eq!(
        harness.debug_render(),
        "| \
        a string\n\
        that has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::MoveToRightEndOfLine);
    ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
    assert_eq!(
        harness.debug_render(),
        "\
        |\nthat has three\n\
        lines."
    );

    ctx.do_edit(EditNotification::DeleteToEndOfParagraph);
    ctx.do_edit(EditNotification::DeleteToEndOfParagraph);
    assert_eq!(
        harness.debug_render(),
        "\
        |\nlines."
    );
}

#[test]
fn multiline_indentation_test() {
    use crate::rpc::{EditNotification, GestureType::*};
    let initial_text = "\
    this is a string\n\
    that has three\n\
    lines.";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();

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

    // Simple multi line indent/outdent test
    ctx.do_edit(EditNotification::Indent);
    assert_eq!(
        harness.debug_render(),
        "    \
    this |is a string\n    \
    that |has three\n\
    lines."
    );

    ctx.do_edit(EditNotification::Outdent);
    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(
        harness.debug_render(),
        "\
    this |is a string\n\
    that |has three\n\
    lines."
    );

    // Different position indent/outdent test
    // Shouldn't change cursor position
    ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: ToggleSel });
    ctx.do_edit(EditNotification::Gesture { line: 1, col: 10, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
    this |is a string\n\
    that has t|hree\n\
    lines."
    );

    ctx.do_edit(EditNotification::Indent);
    assert_eq!(
        harness.debug_render(),
        "    \
    this |is a string\n    \
    that has t|hree\n\
    lines."
    );

    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(
        harness.debug_render(),
        "\
    this |is a string\n\
    that has t|hree\n\
    lines."
    );

    // Multi line selection test
    ctx.do_edit(EditNotification::Gesture { line: 1, col: 10, ty: ToggleSel });
    ctx.do_edit(EditNotification::MoveToEndOfDocumentAndModifySelection);
    ctx.do_edit(EditNotification::Indent);
    assert_eq!(
        harness.debug_render(),
        "    \
    this [is a string\n    \
    that has three\n    \
    lines.|]"
    );

    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(
        harness.debug_render(),
        "\
    this [is a string\n\
    that has three\n\
    lines.|]"
    );

    // Multi cursor different line indent test
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
    ctx.do_edit(EditNotification::Gesture { line: 2, col: 0, ty: ToggleSel });
    assert_eq!(
        harness.debug_render(),
        "\
    |this is a string\n\
    that has three\n\
    |lines."
    );

    ctx.do_edit(EditNotification::Indent);
    assert_eq!(
        harness.debug_render(),
        "    \
    |this is a string\n\
    that has three\n    \
    |lines."
    );

    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(
        harness.debug_render(),
        "\
    |this is a string\n\
    that has three\n\
    |lines."
    );
}

#[test]
fn simple_indentation_test() {
    use crate::rpc::{EditNotification, GestureType::*};

    let harness = ContextHarness::new("");
    let mut ctx = harness.make_context();
    // Single indent and outdent test
    ctx.do_edit(EditNotification::Insert { chars: "hello".into() });
    ctx.do_edit(EditNotification::Indent);
    assert_eq!(harness.debug_render(), "    hello|");
    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(harness.debug_render(), "hello|");

    // Test when outdenting with less than 4 spaces
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 0, ty: PointSelect });
    ctx.do_edit(EditNotification::Insert { chars: "  ".into() });
    assert_eq!(harness.debug_render(), "  |hello");
    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(harness.debug_render(), "|hello");

    // Non-selection one line indent and outdent test
    ctx.do_edit(EditNotification::MoveToEndOfDocument);
    ctx.do_edit(EditNotification::Indent);
    ctx.do_edit(EditNotification::InsertNewline);
    ctx.do_edit(EditNotification::Insert { chars: "world".into() });
    assert_eq!(harness.debug_render(), "    hello\n    world|");

    ctx.do_edit(EditNotification::MoveWordLeft);
    ctx.do_edit(EditNotification::MoveToBeginningOfDocumentAndModifySelection);
    ctx.do_edit(EditNotification::Indent);
    assert_eq!(harness.debug_render(), "    [|    hello\n        ]world");

    ctx.do_edit(EditNotification::Outdent);
    assert_eq!(harness.debug_render(), "[|    hello\n    ]world");

    ctx.do_edit(EditNotification::SelectAll);
    ctx.do_edit(EditNotification::DeleteBackward);
    ctx.do_edit(EditNotification::Insert { chars: "hello".into() });
    ctx.do_edit(EditNotification::SelectAll);
    ctx.do_edit(EditNotification::InsertTab);
    assert_eq!(harness.debug_render(), "    |");
}

#[test]
fn number_change_tests() {
    use crate::rpc::{EditNotification, GestureType::*};

    let harness = ContextHarness::new("");
    let mut ctx = harness.make_context();
    // Single indent and outdent test
    ctx.do_edit(EditNotification::Insert { chars: "1234".into() });
    ctx.do_edit(EditNotification::IncreaseNumber);
    assert_eq!(harness.debug_render(), "1235|");

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 2, ty: PointSelect });
    ctx.do_edit(EditNotification::IncreaseNumber);
    assert_eq!(harness.debug_render(), "1236|");

    ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
    ctx.do_edit(EditNotification::Insert { chars: "-42".into() });
    ctx.do_edit(EditNotification::IncreaseNumber);
    assert_eq!(harness.debug_render(), "-41|");

    // Cursor is on the 3
    ctx.do_edit(EditNotification::MoveToEndOfDocument);
    ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
    ctx.do_edit(EditNotification::Insert { chars: "this is a 336 text example".into() });
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
    ctx.do_edit(EditNotification::DecreaseNumber);
    assert_eq!(harness.debug_render(), "this is a 335| text example");

    // Cursor is on of the 3
    ctx.do_edit(EditNotification::MoveToEndOfDocument);
    ctx.do_edit(EditNotification::DeleteToBeginningOfLine);
    ctx.do_edit(EditNotification::Insert { chars: "this is a -336 text example".into() });
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 11, ty: PointSelect });
    ctx.do_edit(EditNotification::DecreaseNumber);
    assert_eq!(harness.debug_render(), "this is a -337| text example");
}

#[test]
fn test_exact_position() {
    use crate::rpc::{EditNotification, GestureType::*};

    let initial_text = "\
        this is a string\n\
        that has three\n\
        \n\
        lines.\n\
        And lines with very different length.";
    let harness = ContextHarness::new(initial_text);
    let mut ctx = harness.make_context();
    ctx.do_edit(EditNotification::Gesture { line: 1, col: 5, ty: PointSelect });
    ctx.do_edit(EditNotification::AddSelectionAbove);
    assert_eq!(
        harness.debug_render(),
        "\
        this |is a string\n\
        that |has three\n\
        \n\
        lines.\n\
        And lines with very different length."
    );

    ctx.do_edit(EditNotification::CollapseSelections);
    ctx.do_edit(EditNotification::Gesture { line: 0, col: 5, ty: PointSelect });
    ctx.do_edit(EditNotification::AddSelectionBelow);
    assert_eq!(
        harness.debug_render(),
        "\
        this |is a string\n\
        that |has three\n\
        \n\
        lines.\n\
        And lines with very different length."
    );

    ctx.do_edit(EditNotification::CollapseSelections);
    ctx.do_edit(EditNotification::Gesture { line: 4, col: 10, ty: PointSelect });
    ctx.do_edit(EditNotification::AddSelectionAbove);
    assert_eq!(
        harness.debug_render(),
        "\
        this is a string\n\
        that has t|hree\n\
        \n\
        lines.\n\
        And lines |with very different length."
    );
}

#[test]
fn test_illegal_plugin_edit() {
    use crate::plugins::PluginPid;
    use crate::plugins::rpc::{PluginEdit, PluginNotification};
    use xi_rope::DeltaBuilder;

    let text = "text";
    let harness = ContextHarness::new(text);
    let mut ctx = harness.make_context();
    let rev_token = ctx.editor.borrow().get_head_rev_token();

    let iv = Interval::new(1, 1);
    let mut builder = DeltaBuilder::new(0); // wrong length
    builder.replace(iv, "1".into());

    let edit_one = PluginEdit {
        rev: rev_token,
        delta: builder.build(),
        priority: 55,
        after_cursor: false,
        undo_group: None,
        author: "plugin_one".into(),
    };

    ctx.do_plugin_cmd(PluginPid(1), PluginNotification::Edit { edit: edit_one });
    let new_rev_token = ctx.editor.borrow().get_head_rev_token();
    assert_eq!(rev_token, new_rev_token);
}

#[test]
fn empty_transpose() {
    use crate::rpc::EditNotification;

    let harness = ContextHarness::new("");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Transpose);

    assert_eq!(harness.debug_render(), "|");
}

#[test]
fn eol_multicursor_transpose() {
    use crate::rpc::{EditNotification, GestureType::*};

    let harness = ContextHarness::new("word\n");
    let mut ctx = harness.make_context();

    ctx.do_edit(EditNotification::Gesture { line: 0, col: 4, ty: PointSelect }); // end of first line
    ctx.do_edit(EditNotification::AddSelectionBelow); // add cursor below that, at eof
    ctx.do_edit(EditNotification::Transpose);

    assert_eq!(harness.debug_render(), "wor\nd|");
}
