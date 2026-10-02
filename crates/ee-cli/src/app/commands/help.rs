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
    /// Rows for `:keymap`: every bound key and sequence from the merged
    /// default+config map, tagged `[config]` when the row comes from (or is
    /// unbound by) the user keymap config.
    pub(super) fn keymap_help_items(&self) -> Vec<String> {
        use crate::keymap::{BindingKey, KeyPress, format_binding_mode, format_key_press};

        let mut config_keys = std::collections::HashSet::new();
        for operation in &self.config.keymap.operations {
            match operation {
                crate::keymap::KeymapOperation::Bind { binding, .. } => {
                    config_keys.insert(*binding);
                }
                crate::keymap::KeymapOperation::Unbind(binding) => {
                    config_keys.insert(*binding);
                }
            }
        }

        let mode_rank = |mode: Mode| -> usize {
            match mode {
                Mode::Normal => 0,
                Mode::Insert => 1,
                Mode::Replace => 2,
                Mode::Visual => 3,
                Mode::VisualLine => 4,
                Mode::VisualBlock => 5,
                Mode::OperatorPending => 6,
                Mode::CommandLine => 7,
                Mode::Search => 8,
                Mode::Picker => 9,
                Mode::Quickfix => 10,
                Mode::LocationList => 11,
                Mode::SubstituteConfirm => 12,
                Mode::Agent => 13,
                Mode::PrivilegeConfirm => 14,
            }
        };

        fn key_label(binding: &BindingKey) -> String {
            let key = format_key_press(KeyPress { key: binding.key, modifiers: binding.modifiers });
            match binding.prefix {
                Some(prefix) => format!("{prefix}{key}"),
                None => key,
            }
        }

        let mut rows: Vec<(usize, String)> = Vec::new();

        // Merged single-key bindings (defaults + config overrides).
        for (binding, action) in &self.key_bindings {
            let tag = if config_keys.contains(binding) { "  [config]" } else { "" };
            rows.push((
                mode_rank(binding.mode),
                format!(
                    "{:<9} {:<9} {:<34} {}{}",
                    format_binding_mode(binding.mode),
                    key_label(binding),
                    crate::keymap::format_action_spec(action),
                    crate::keymap::action_hint_description(action),
                    tag,
                ),
            ));
        }

        // User-config unbinds: show the key with its fate so the dump reflects
        // the config even for rows absent from the merged map.
        for operation in &self.config.keymap.operations {
            if let crate::keymap::KeymapOperation::Unbind(binding) = operation {
                rows.push((
                    mode_rank(binding.mode),
                    format!(
                        "{:<9} {:<9} {:<34} (unbound by config)",
                        format_binding_mode(binding.mode),
                        key_label(binding),
                        "unbind",
                    ),
                ));
            }
        }

        // Sequence bindings: defaults, then config overrides (later wins).
        type SequenceMapKey = (usize, Mode, Vec<KeyPress>);
        type SequenceMapRow = (crate::keymap::Action, String, bool);
        let mut sequences: std::collections::HashMap<SequenceMapKey, SequenceMapRow> =
            std::collections::HashMap::new();
        for binding in crate::keymap::default_sequence_bindings() {
            sequences.insert(
                (mode_rank(binding.mode), binding.mode, binding.sequence.clone()),
                (binding.action.clone(), binding.description.clone(), false),
            );
        }
        for binding in &self.config.keymap.sequence_bindings {
            sequences.insert(
                (mode_rank(binding.mode), binding.mode, binding.sequence.clone()),
                (binding.action.clone(), binding.description.clone(), true),
            );
        }
        for ((_, mode, keys), (action, description, from_config)) in sequences {
            let key = keys.iter().map(|key| format_key_press(*key)).collect::<Vec<_>>().join(" ");
            let tag = if from_config { "  [config]" } else { "" };
            rows.push((
                mode_rank(mode),
                format!(
                    "{:<9} {:<9} {:<34} {}{}",
                    format_binding_mode(mode),
                    key,
                    crate::keymap::format_action_spec(&action),
                    description,
                    tag,
                ),
            ));
        }

        rows.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        rows.into_iter().map(|(_, row)| row).collect()
    }
}
