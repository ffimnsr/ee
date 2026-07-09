use crossterm::event::{KeyCode, KeyModifiers};

use crate::app::Mode;
use crate::keymap::{
    Action, BindingKey, KeymapSettings, bindings, bindings_for, format_action_spec,
    parse_action_spec,
};

// Extracted from committed tests.rs lines 3855–4492.
// Do not modify — regenerated from git source.

#[test]
fn default_keymap_binds_page_keys_to_editor_scroll() {
    let bindings = bindings_for(&KeymapSettings::default());

    assert_eq!(
        bindings.get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
            prefix: None,
        }),
        Some(&Action::Edit("scroll_page_down"))
    );
    assert_eq!(
        bindings.get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
            prefix: None,
        }),
        Some(&Action::Edit("scroll_page_up"))
    );
}

#[test]
fn bindings_table_has_normal_hjkl() {
    let b = bindings();
    let lookup = |key| {
        b.get(&BindingKey { mode: Mode::Normal, key, modifiers: KeyModifiers::NONE, prefix: None })
            .cloned()
    };
    assert_eq!(lookup(KeyCode::Char('h')), Some(Action::Edit("move_left")));
    assert_eq!(lookup(KeyCode::Char('l')), Some(Action::Edit("move_right")));
    assert_eq!(lookup(KeyCode::Char('k')), Some(Action::Edit("move_up")));
    assert_eq!(lookup(KeyCode::Char('j')), Some(Action::Edit("move_down")));
}

#[test]
fn bindings_table_maps_caret_to_first_non_whitespace() {
    let lookup = bindings()
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('^'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();

    assert_eq!(lookup, Some(Action::GotoFirstNonWhitespace));
}

#[test]
fn overlay_binding_tables_have_defaults() {
    let b = bindings();

    let picker_close = b
        .get(&BindingKey {
            mode: Mode::Picker,
            key: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();
    let quickfix_down = b
        .get(&BindingKey {
            mode: Mode::Quickfix,
            key: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();
    let location_close = b
        .get(&BindingKey {
            mode: Mode::LocationList,
            key: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();
    let substitute_apply = b
        .get(&BindingKey {
            mode: Mode::SubstituteConfirm,
            key: KeyCode::Char('y'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();

    assert_eq!(picker_close, Some(Action::PickerClose));
    assert_eq!(quickfix_down, Some(Action::QuickfixMoveDown));
    assert_eq!(location_close, Some(Action::LocationListClose));
    assert_eq!(substitute_apply, Some(Action::SubstituteConfirmApply));
}

#[test]
fn bindings_table_has_requested_goto_prefix_bindings() {
    let b = bindings();
    let lookup = |key| {
        b.get(&BindingKey {
            mode: Mode::Normal,
            key,
            modifiers: KeyModifiers::NONE,
            prefix: Some('g'),
        })
        .cloned()
    };

    assert_eq!(lookup(KeyCode::Char('g')), Some(Action::GotoFileStart));
    assert_eq!(lookup(KeyCode::Char('e')), Some(Action::GotoLastLine));
    assert_eq!(lookup(KeyCode::Char('f')), Some(Action::GotoFile));
    assert_eq!(lookup(KeyCode::Char('h')), Some(Action::Edit("move_to_left_end_of_line")));
    assert_eq!(lookup(KeyCode::Char('l')), Some(Action::Edit("move_to_right_end_of_line")));
}

#[test]
fn capital_k_binding_requests_hover() {
    let b = bindings();
    let lookup = b
        .get(&BindingKey {
            mode: Mode::Normal,
            key: KeyCode::Char('K'),
            modifiers: KeyModifiers::NONE,
            prefix: None,
        })
        .cloned();

    assert_eq!(lookup, Some(Action::RequestHover));
}

#[test]
fn insert_ctrl_bindings_cover_register_and_completion() {
    let b = bindings();

    let ctrl_key = |key| {
        b.get(&BindingKey {
            mode: Mode::Insert,
            key,
            modifiers: KeyModifiers::CONTROL,
            prefix: None,
        })
        .cloned()
    };

    // ctrl-r -> insert from register
    assert_eq!(ctrl_key(KeyCode::Char('r')), Some(Action::InsertRegister));
    // ctrl-x -> request completion
    assert_eq!(ctrl_key(KeyCode::Char('x')), Some(Action::RequestCompletion));
}

#[test]
fn parse_action_spec_accepts_requested_motion_aliases() {
    assert_eq!(
        parse_action_spec("move_next_word_start").unwrap(),
        Action::MoveWordStart { forward: true, long_word: false }
    );
    assert_eq!(
        parse_action_spec("goto_word").unwrap(),
        Action::MoveWordStart { forward: true, long_word: false }
    );
    assert_eq!(
        parse_action_spec("move_prev_word_start").unwrap(),
        Action::MoveWordStart { forward: false, long_word: false }
    );
    assert_eq!(
        parse_action_spec("move_next_word_end").unwrap(),
        Action::MoveWordEnd { long_word: false }
    );
    assert_eq!(
        parse_action_spec("move_next_long_word_start").unwrap(),
        Action::MoveWordStart { forward: true, long_word: true }
    );
    assert_eq!(
        parse_action_spec("move_prev_long_word_start").unwrap(),
        Action::MoveWordStart { forward: false, long_word: true }
    );
    assert_eq!(
        parse_action_spec("move_next_long_word_end").unwrap(),
        Action::MoveWordEnd { long_word: true }
    );
}

#[test]
fn parse_action_spec_accepts_requested_find_aliases() {
    assert_eq!(
        parse_action_spec("find_next_char").unwrap(),
        Action::PendingCharFind { forward: true, inclusive: true }
    );
    assert_eq!(
        parse_action_spec("find_till_char").unwrap(),
        Action::PendingCharFind { forward: true, inclusive: false }
    );
    assert_eq!(
        parse_action_spec("find_prev_char").unwrap(),
        Action::PendingCharFind { forward: false, inclusive: true }
    );
    assert_eq!(
        parse_action_spec("till_prev_char").unwrap(),
        Action::PendingCharFind { forward: false, inclusive: false }
    );
}

#[test]
fn keymap_action_formatter_roundtrips_representative_actions() {
    let actions = [
        Action::RequestHover,
        Action::PrefillCommandLine("rename "),
        Action::DeleteSelection { yank: false, enter_insert: true },
        Action::PendingCharFind { forward: false, inclusive: true },
        Action::MoveWordStart { forward: true, long_word: false },
        Action::SetPrefix('g'),
        Action::Edit("move_up"),
    ];
    for action in &actions {
        let spec = format_action_spec(action);
        let parsed = parse_action_spec(&spec)
            .unwrap_or_else(|_| panic!("should parse formatted action: {spec:?}"));
        assert_eq!(&parsed, action, "roundtrip failed for action {action:?} -> {spec:?}");
    }
}

#[test]
fn parse_action_spec_accepts_requested_command_aliases() {
    assert_eq!(
        parse_action_spec("find_next_char").unwrap(),
        Action::PendingCharFind { forward: true, inclusive: true }
    );
    assert_eq!(
        parse_action_spec("find_till_char").unwrap(),
        Action::PendingCharFind { forward: true, inclusive: false }
    );

    let assertions = [
        ("request_hover", Action::RequestHover),
        ("hover", Action::RequestHover),
        ("completion", Action::RequestCompletion),
        ("code_action", Action::RequestCodeActions),
        ("register_prefix", Action::RegisterPrefix),
        ("insert_register", Action::InsertRegister),
        ("delete_char_forward", Action::Edit("delete_forward")),
        ("delete_word_forward", Action::Edit("delete_word_forward")),
        ("kill_line", Action::DeleteCurrentLine),
        ("insert_newline", Action::Edit("insert_newline")),
        ("add_newline_below", Action::AddNewlineBelow),
        ("add_newline_above", Action::AddNewlineAbove),
        ("save_selection", Action::SaveSelection),
        ("repeat_last_motion", Action::RepeatLastMotion),
        ("fold_open", Action::FoldOpen),
        ("fold_close", Action::FoldClose),
        ("fold_open_all", Action::FoldOpenAll),
        ("fold_close_all", Action::FoldCloseAll),
    ];
    for (alias, expected) in assertions {
        assert_eq!(parse_action_spec(alias).unwrap(), expected, "action alias {alias:?}");
    }

    assert_eq!(parse_action_spec("no_op").unwrap(), Action::NoOp);
}

#[test]
fn parse_action_spec_accepts_move_line_aliases() {
    assert_eq!(parse_action_spec("move_line_down").unwrap(), Action::Edit("move_down"));
    assert_eq!(parse_action_spec("move_line_up").unwrap(), Action::Edit("move_up"));
}

#[test]
fn parse_action_spec_accepts_match_brackets_alias() {
    assert_eq!(parse_action_spec("match_brackets").unwrap(), Action::MatchingPair);
}

#[test]
fn parse_action_spec_accepts_view_command_names() {
    assert_eq!(parse_action_spec("rotate_view").unwrap(), Action::RotateView);
    assert_eq!(parse_action_spec("cycle_view").unwrap(), Action::RotateView);
    assert_eq!(parse_action_spec("rotate_view_reverse").unwrap(), Action::RotateViewReverse);
    assert_eq!(parse_action_spec("transpose_view").unwrap(), Action::TransposeView);
    assert_eq!(parse_action_spec("wclose").unwrap(), Action::WindowClose);
    assert_eq!(parse_action_spec("wonly").unwrap(), Action::WindowOnly);
    assert_eq!(parse_action_spec("jump_view_left").unwrap(), Action::JumpViewLeft);
    assert_eq!(parse_action_spec("jump_view_down").unwrap(), Action::JumpViewDown);
    assert_eq!(parse_action_spec("jump_view_up").unwrap(), Action::JumpViewUp);
    assert_eq!(parse_action_spec("jump_view_right").unwrap(), Action::JumpViewRight);
    assert_eq!(parse_action_spec("swap_view_left").unwrap(), Action::SwapViewLeft);
    assert_eq!(parse_action_spec("swap_view_down").unwrap(), Action::SwapViewDown);
    assert_eq!(parse_action_spec("swap_view_up").unwrap(), Action::SwapViewUp);
    assert_eq!(parse_action_spec("swap_view_right").unwrap(), Action::SwapViewRight);
}

#[test]
fn capitalized_command_aliases_are_rejected() {
    assert!(parse_action_spec("Sort").is_err());
    assert!(parse_action_spec("Rsort").is_err());
    assert!(parse_action_spec("Reflow").is_err());
    assert!(parse_action_spec("Expandtab").is_err());
    assert!(parse_action_spec("Renormalize").is_err());
    assert!(parse_action_spec("Dedup").is_err());
}
