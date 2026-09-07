//! `impl App` command methods: help.
use super::*;

impl App {
    pub(super) fn help_items() -> Vec<String> {
        vec![
            format!(
                "Discovery: {} | {}",
                Self::command_brief("commands"),
                Self::command_brief("keymap")
            ),
            "Modes: i insert | v visual | V visual-line | Ctrl-V visual-block | : command"
                .to_owned(),
            "Move: h j k l | w b e | gg G | % | * # | n N".to_owned(),
            "Edit: d c y operators | p/P register paste | u undo | Ctrl-R redo | . repeat"
                .to_owned(),
            format!(
                "IDE: {} | {} | {} | {} | {} | {} | {}",
                Self::command_brief("hover"),
                Self::command_brief("complete"),
                Self::command_brief("codeaction"),
                Self::command_brief("definition"),
                Self::command_brief("references"),
                Self::command_brief("rename"),
                Self::command_brief("diagnostics")
            ),
            format!(
                "Backend ops: {} {} {} {} {}",
                Self::command_name("transpose"),
                Self::command_name("duplicate_line"),
                Self::command_name("increment"),
                Self::command_name("decrement"),
                Self::command_name("reindent")
            ),
            format!(
                "Selections: {} {} {} {} {}",
                Self::command_name("select_regex"),
                Self::command_name("selection_into_lines"),
                Self::command_name("trim_selections"),
                Self::command_name("collapse_selection"),
                Self::command_name("select_all")
            ),
            format!("Search sets: {}", Self::command_brief("multi_find")),
            format!(
                "Shell: {} | !command shorthand shell runner | {} | {} | {}",
                Self::command_brief("term"),
                Self::command_brief("make"),
                Self::command_brief("test"),
                Self::command_brief("run")
            ),
            format!(
                "Workspace: {} {} {} {} {} {} {} {} {}",
                Self::command_name("file_picker"),
                Self::command_name("file_picker_in_current_directory"),
                Self::command_name("buffer_picker"),
                Self::command_name("changed_file_picker"),
                Self::command_name("symbol_picker"),
                Self::command_name("workspace_symbol_picker"),
                Self::command_name("diagnostics_picker"),
                Self::command_name("workspace_diagnostics_picker"),
                Self::command_name("last_picker")
            ),
            format!(
                "Explorer: {} {} {}",
                Self::command_name("file_explorer"),
                Self::command_name("file_explorer_in_current_buffer_directory"),
                Self::command_name("file_explorer_in_current_directory")
            ),
        ]
    }
    pub(super) fn command_help_items() -> Vec<String> {
        Self::command_help_canonical_ids()
            .iter()
            .copied()
            .map(Self::format_command_help_item)
            .collect()
    }
    #[cfg(test)]
    pub(super) fn command_registry_aliases() -> Vec<&'static str> {
        Self::ex_command_names().to_vec()
    }
    #[cfg(test)]
    pub(super) fn documented_command_aliases() -> Vec<String> {
        Self::command_help_items()
            .iter()
            .flat_map(|line| {
                line.split_whitespace().filter_map(|token| {
                    token
                        .strip_prefix(':')
                        .map(|alias| alias.trim_end_matches('/').trim_end_matches(',').to_owned())
                })
            })
            .filter(|alias| Self::resolve_ex_command(alias).is_some())
            .collect()
    }
    #[cfg(test)]
    pub(super) fn claimed_command_aliases() -> Vec<String> {
        Self::command_help_items()
            .iter()
            .flat_map(|line| {
                line.split_whitespace().filter_map(|token| {
                    token
                        .strip_prefix(':')
                        .map(|alias| alias.trim_end_matches('/').trim_end_matches(',').to_owned())
                })
            })
            .collect()
    }
    pub(super) fn command_brief(canonical_id: &str) -> String {
        let alias =
            Self::ordered_aliases_for(canonical_id).into_iter().next().unwrap_or(canonical_id);
        let spec = Self::canonical_command_spec(canonical_id);
        match spec.usage {
            Some(usage) => format!(":{alias} {usage} {}", spec.summary),
            None => format!(":{alias} {}", spec.summary),
        }
    }
    pub(super) fn command_name(canonical_id: &str) -> String {
        let alias =
            Self::ordered_aliases_for(canonical_id).into_iter().next().unwrap_or(canonical_id);
        format!(":{alias}")
    }
    pub(super) fn keymap_help_items() -> Vec<String> {
        vec![
            "K request hover".to_owned(),
            "gb show git blame for current line".to_owned(),
            "gD open git diff scratch view".to_owned(),
            "Ctrl-A increase number under cursor".to_owned(),
            "Ctrl-X decrease number under cursor".to_owned(),
            "Ctrl-Up add selection above".to_owned(),
            "Ctrl-Down add selection below".to_owned(),
            "gd duplicate current line or selection".to_owned(),
            "* / # selection-for-find forward/backward".to_owned(),
            "gt / gT next and previous tab".to_owned(),
            "]h / [h git hunk next and previous".to_owned(),
            "]q / [q quickfix next and previous".to_owned(),
            "]Q / [Q location list next and previous".to_owned(),
            "z a o c R M fold toggle/open/close/open-all/close-all".to_owned(),
            "Ctrl-O / Tab jump list older/newer".to_owned(),
            "g; / g, change list older/newer".to_owned(),
        ]
    }
}
