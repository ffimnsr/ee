//! Default binding surfaces, hint entries, and key formatting.
use super::defaults::default_sequence_bindings;
use super::spec::parse_key_press_spec;
use super::vim::build_vim_bindings;
use super::*;

pub(crate) fn bindings() -> &'static HashMap<BindingKey, Action> {
    static BINDINGS: OnceLock<HashMap<BindingKey, Action>> = OnceLock::new();
    BINDINGS.get_or_init(build_vim_bindings)
}

pub(crate) fn bindings_for(settings: &KeymapSettings) -> HashMap<BindingKey, Action> {
    let mut map = if settings.inherit_defaults { bindings().clone() } else { HashMap::new() };

    for operation in &settings.operations {
        match operation {
            KeymapOperation::Unbind(binding) => {
                map.remove(binding);
            }
            KeymapOperation::Bind { binding, action } => {
                map.insert(*binding, action.clone());
            }
        }
    }

    map
}

pub(crate) fn sequence_bindings_for(settings: &KeymapSettings) -> SequenceBindings {
    let mut roots = HashMap::new();
    let bindings = if settings.inherit_defaults {
        let mut bindings = default_sequence_bindings().clone();
        bindings.extend(settings.sequence_bindings.iter().cloned());
        bindings
    } else {
        settings.sequence_bindings.clone()
    };

    for binding in &bindings {
        roots
            .entry(binding.mode)
            .or_insert_with(|| SequenceNode {
                action: None,
                description: None,
                children: HashMap::new(),
            })
            .insert(&binding.sequence, binding.action.clone(), binding.description.clone());
    }
    SequenceBindings { roots }
}

pub(crate) fn key_press_from_event(key: KeyEvent) -> KeyPress {
    KeyPress { key: key.code, modifiers: key.modifiers }
}

pub(crate) fn parse_key_sequence_spec(specs: &[String]) -> Result<Vec<KeyPress>, String> {
    if specs.is_empty() {
        return Err(String::from("keys must contain at least one entry"));
    }
    specs.iter().map(|spec| parse_key_press_spec(spec)).collect()
}

pub(crate) fn format_key_sequence(sequence: &[KeyPress]) -> String {
    sequence.iter().map(|key| format_key_press(*key)).collect::<Vec<_>>().join(" ")
}

pub(crate) fn format_key_press(key: KeyPress) -> String {
    let mut parts = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".to_owned());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("Alt".to_owned());
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("Shift".to_owned());
    }

    let key_name = match key.key {
        KeyCode::Left => String::from("Left"),
        KeyCode::Right => String::from("Right"),
        KeyCode::Up => String::from("Up"),
        KeyCode::Down => String::from("Down"),
        KeyCode::Enter => String::from("Enter"),
        KeyCode::Backspace => String::from("Backspace"),
        KeyCode::Tab => String::from("Tab"),
        KeyCode::BackTab => String::from("BackTab"),
        KeyCode::Esc => String::from("Esc"),
        KeyCode::Char(' ') => String::from("SPC"),
        KeyCode::Char(ch) => ch.to_string(),
        other => format!("{other:?}"),
    };

    if parts.is_empty() {
        key_name
    } else {
        parts.push(key_name);
        parts.join("+")
    }
}

pub(crate) fn prefix_hint_entries(
    bindings: &HashMap<BindingKey, Action>,
    mode: Mode,
    prefix: char,
) -> Vec<KeyHintEntry> {
    let mut entries = bindings
        .iter()
        .filter(|(binding, _)| binding.mode == mode && binding.prefix == Some(prefix))
        .map(|(binding, action)| KeyHintEntry {
            key: format_key_press(KeyPress { key: binding.key, modifiers: binding.modifiers }),
            description: action_hint_description(action),
            is_group: false,
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

pub(crate) fn register_hint_entries() -> Vec<KeyHintEntry> {
    vec![
        KeyHintEntry {
            key: String::from("\""),
            description: String::from("unnamed register"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("0"),
            description: String::from("last yank"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("1-9"),
            description: String::from("delete history"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("a-z / A-Z"),
            description: String::from("named register / append"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("_"),
            description: String::from("black hole"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("/"),
            description: String::from("search register"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("+"),
            description: String::from("system clipboard"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("*"),
            description: String::from("primary clipboard"),
            is_group: false,
        },
    ]
}

pub(crate) fn mark_set_hint_entries() -> Vec<KeyHintEntry> {
    vec![KeyHintEntry {
        key: String::from("a-z"),
        description: String::from("named mark"),
        is_group: false,
    }]
}

pub(crate) fn mark_jump_hint_entries(line_start: bool) -> Vec<KeyHintEntry> {
    vec![
        KeyHintEntry {
            key: String::from("a-z"),
            description: String::from(if line_start {
                "named mark line"
            } else {
                "named mark exact"
            }),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("'"),
            description: String::from(if line_start {
                "previous jump line"
            } else {
                "previous jump exact"
            }),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("`"),
            description: String::from(if line_start {
                "previous jump line"
            } else {
                "previous jump exact"
            }),
            is_group: false,
        },
    ]
}

pub(crate) fn macro_record_hint_entries() -> Vec<KeyHintEntry> {
    vec![KeyHintEntry {
        key: String::from("a-z"),
        description: String::from("macro register"),
        is_group: false,
    }]
}

pub(crate) fn macro_replay_hint_entries() -> Vec<KeyHintEntry> {
    vec![
        KeyHintEntry {
            key: String::from("a-z"),
            description: String::from("named macro"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("@"),
            description: String::from("last macro"),
            is_group: false,
        },
    ]
}

pub(crate) fn replace_char_hint_entries() -> Vec<KeyHintEntry> {
    vec![KeyHintEntry {
        key: String::from("char"),
        description: String::from("replacement character"),
        is_group: false,
    }]
}

pub(crate) fn with_cancel_hint(mut entries: Vec<KeyHintEntry>) -> Vec<KeyHintEntry> {
    if entries.iter().all(|entry| entry.key != "Esc") {
        entries.insert(
            0,
            KeyHintEntry {
                key: String::from("Esc"),
                description: String::from("cancel"),
                is_group: false,
            },
        );
    }
    entries
}

pub(crate) fn window_command_hint_entries() -> Vec<KeyHintEntry> {
    let mut entries = vec![
        KeyHintEntry {
            key: String::from("s"),
            description: String::from("split horizontally"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("v"),
            description: String::from("split vertically"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("w"),
            description: String::from("next window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("W"),
            description: String::from("previous window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("p"),
            description: String::from("previous window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("h"),
            description: String::from("focus left"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("j"),
            description: String::from("focus down"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("k"),
            description: String::from("focus up"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("l"),
            description: String::from("focus right"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("t"),
            description: String::from("transpose windows"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("H"),
            description: String::from("swap left"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("J"),
            description: String::from("swap down"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("K"),
            description: String::from("swap up"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("L"),
            description: String::from("swap right"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("c"),
            description: String::from("close window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("q"),
            description: String::from("close window"),
            is_group: false,
        },
        KeyHintEntry {
            key: String::from("o"),
            description: String::from("only window"),
            is_group: false,
        },
    ];
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

pub(crate) fn action_hint_description(action: &Action) -> String {
    match action {
        Action::NoOp => String::from("prefix"),
        Action::AgentHistoryPrevious => String::from("agent history previous"),
        Action::AgentHistoryNext => String::from("agent history next"),
        Action::AgentHistorySearchReverse => String::from("agent history reverse search"),
        Action::AgentDraftStash => String::from("agent draft stash"),
        Action::AgentDraftRestore => String::from("agent draft restore"),
        Action::AgentDraftExternalEdit => String::from("agent draft external edit"),
        Action::AgentToggleTranscriptDetails => String::from("agent transcript details"),
        Action::AgentToggleTranscriptRaw => String::from("agent transcript raw view"),
        Action::EnterMode(mode) => format!("enter {}", mode_hint_description(*mode)),
        Action::Edit(method) => edit_hint_description(method),
        Action::PrefillCommandLine(prefix) => {
            format!("command: {}", prefix.trim())
        }
        Action::PendingCharFind { forward, inclusive } => {
            let direction = if *forward { "forward" } else { "backward" };
            let kind = if *inclusive { "find char" } else { "till char" };
            format!("{kind} {direction}")
        }
        Action::MoveWordStart { forward, long_word } => {
            let direction = if *forward { "next" } else { "previous" };
            let family = if *long_word { "long word" } else { "word" };
            format!("{direction} {family} start")
        }
        Action::MoveWordEnd { long_word } => {
            let family = if *long_word { "long word" } else { "word" };
            format!("next {family} end")
        }
        Action::SetOperator(operator) => match operator {
            Operator::Delete => String::from("delete operator"),
            Operator::Change => String::from("change operator"),
            Operator::Yank => String::from("yank operator"),
            Operator::Indent => String::from("indent operator"),
            Operator::Outdent => String::from("outdent operator"),
            Operator::Uppercase => String::from("uppercase operator"),
            Operator::Lowercase => String::from("lowercase operator"),
            Operator::CaseToggle => String::from("toggle case operator"),
        },
        Action::MarkJumpPrefix { line_start } => {
            String::from(if *line_start { "jump to mark line" } else { "jump to exact mark" })
        }
        Action::DeleteSelection { yank, enter_insert } => match (*yank, *enter_insert) {
            (true, false) => String::from("delete selection"),
            (false, false) => String::from("delete selection no yank"),
            (true, true) => String::from("change selection"),
            (false, true) => String::from("change selection no yank"),
        },
        Action::GotoFileStart => String::from("file start"),
        Action::GotoLastLine => String::from("last line"),
        Action::GotoFile => String::from("goto file"),
        Action::RequestDocumentSymbols => String::from("document symbols"),
        Action::RequestWorkspaceSymbols => String::from("workspace symbols"),
        Action::RestoreLastVisual => String::from("restore last visual"),
        Action::ChangeListOlder => String::from("older change"),
        Action::ChangeListNewer => String::from("newer change"),
        Action::TabNext => String::from("next tab"),
        Action::TabPrev => String::from("previous tab"),
        Action::QfNext => String::from("next quickfix"),
        Action::QfPrev => String::from("previous quickfix"),
        Action::LocNext => String::from("next location"),
        Action::LocPrev => String::from("previous location"),
        Action::GitNextHunk => String::from("next hunk"),
        Action::GitPrevHunk => String::from("previous hunk"),
        Action::GitBlame => String::from("git blame"),
        Action::GitDiff => String::from("git diff"),
        Action::FoldToggle => String::from("toggle fold"),
        Action::FoldOpen => String::from("open fold"),
        Action::FoldClose => String::from("close fold"),
        Action::FoldOpenAll => String::from("open all folds"),
        Action::FoldCloseAll => String::from("close all folds"),
        Action::SearchWordUnderCursor { forward } => {
            String::from(if *forward { "search word forward" } else { "search word backward" })
        }
        Action::SearchSelection { detect_word_boundaries } => {
            String::from(if *detect_word_boundaries {
                "search selection with word boundaries"
            } else {
                "search selection"
            })
        }
        other => humanize_action_debug(&format!("{other:?}")),
    }
}

pub(crate) fn edit_hint_description(method: &str) -> String {
    match method {
        "move_to_left_end_of_line" => String::from("line start"),
        "move_to_right_end_of_line" => String::from("line end"),
        "duplicate_line" => String::from("duplicate line"),
        other => humanize_identifier(other),
    }
}

pub(crate) fn mode_hint_description(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal mode",
        Mode::Insert => "insert mode",
        Mode::Visual => "visual mode",
        Mode::VisualLine => "visual line mode",
        Mode::VisualBlock => "visual block mode",
        Mode::OperatorPending => "operator pending",
        Mode::CommandLine => "command line",
        Mode::Search => "search",
        Mode::Picker => "picker",
        Mode::Quickfix => "quickfix",
        Mode::LocationList => "location list",
        Mode::SubstituteConfirm => "substitute confirm",
        Mode::PrivilegeConfirm => "privileged save confirm",
        Mode::Agent => "agents pane",
    }
}

pub(crate) fn humanize_action_debug(debug: &str) -> String {
    let name = debug.split(['(', ' ', '{']).next().unwrap_or(debug);
    humanize_identifier(name)
}

pub(crate) fn humanize_identifier(identifier: &str) -> String {
    let mut out = String::new();
    let mut prev_is_lower_or_digit = false;
    for ch in identifier.chars() {
        if ch == '_' {
            out.push(' ');
            prev_is_lower_or_digit = false;
            continue;
        }
        if ch.is_ascii_uppercase() && prev_is_lower_or_digit {
            out.push(' ');
        }
        out.push(ch.to_ascii_lowercase());
        prev_is_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    out
}
