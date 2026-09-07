//! `impl App` command methods: registry.
use super::*;

impl App {
    pub(super) fn command_specs() -> &'static [CommandSpec] {
        static ENRICHED_COMMAND_SPECS: OnceLock<Vec<CommandSpec>> = OnceLock::new();
        ENRICHED_COMMAND_SPECS
            .get_or_init(|| {
                COMMAND_SPECS
                    .iter()
                    .cloned()
                    .map(|mut spec| {
                        let metadata = Self::command_metadata(spec.canonical_id);
                        spec.summary = metadata.summary;
                        spec.usage = metadata.usage;
                        spec.category = Some(metadata.category);
                        spec
                    })
                    .collect()
            })
            .as_slice()
    }
    pub(super) fn ex_command_names() -> &'static [&'static str] {
        static COMMAND_NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
        COMMAND_NAMES
            .get_or_init(|| Self::command_specs().iter().map(|spec| spec.alias).collect())
            .as_slice()
    }
    pub(super) fn resolve_ex_command(head: &str) -> Option<&'static CommandSpec> {
        Self::command_specs().iter().find(|spec| spec.alias == head)
    }
    pub(super) fn canonical_command_spec(canonical_id: &str) -> &'static CommandSpec {
        Self::command_specs()
            .iter()
            .find(|spec| spec.canonical_id == canonical_id)
            .unwrap_or_else(|| panic!("missing canonical command spec for {canonical_id}"))
    }
    pub(super) fn command_help_canonical_ids() -> &'static [&'static str] {
        static CANONICAL_IDS: OnceLock<Vec<&'static str>> = OnceLock::new();
        CANONICAL_IDS
            .get_or_init(|| {
                let mut seen = std::collections::HashSet::new();
                Self::command_specs()
                    .iter()
                    .filter_map(|spec| seen.insert(spec.canonical_id).then_some(spec.canonical_id))
                    .collect()
            })
            .as_slice()
    }
    pub(super) fn ordered_aliases_for(canonical_id: &str) -> Vec<&'static str> {
        let mut aliases = Self::command_specs()
            .iter()
            .filter(|spec| spec.canonical_id == canonical_id)
            .cloned()
            .collect::<Vec<_>>();
        aliases.sort_by(|left, right| {
            let left_rank = usize::from(left.alias != left.dispatch);
            let right_rank = usize::from(right.alias != right.dispatch);
            left_rank
                .cmp(&right_rank)
                .then(left.alias.len().cmp(&right.alias.len()))
                .then(left.alias.cmp(right.alias))
        });
        aliases.into_iter().map(|spec| spec.alias).collect()
    }
    pub(super) fn command_metadata(canonical_id: &str) -> CommandMetadata {
        let category = match canonical_id {
            "help" | "commands" | "keymap" | "command_palette" => "discovery",
            "term"
            | "make"
            | "test"
            | "run"
            | "pipe"
            | "pipe_to"
            | "shell_insert_output"
            | "shell_append_output"
            | "shell_keep_pipe" => "shell",
            "edit"
            | "reload"
            | "reload_all"
            | "new"
            | "split"
            | "vsplit"
            | "tab_edit"
            | "tab_close"
            | "tab_next"
            | "tab_prev"
            | "tabs"
            | "rotate_view"
            | "rotate_view_reverse"
            | "transpose_view"
            | "wclose"
            | "wonly"
            | "jump_view_left"
            | "jump_view_down"
            | "jump_view_up"
            | "jump_view_right"
            | "swap_view_left"
            | "swap_view_down"
            | "swap_view_up"
            | "swap_view_right" => "windows",
            "quit"
            | "quit_force"
            | "quit_all"
            | "quit_all_force"
            | "write"
            | "update"
            | "write_all"
            | "write_quit"
            | "write_quit_all"
            | "read"
            | "move"
            | "change_current_directory"
            | "show_directory"
            | "create_directory"
            | "encoding"
            | "recover"
            | "recoverdel"
            | "edit_config"
            | "logs"
            | "reload_config" => "workspace",
            "buffer_close"
            | "buffer_close_force"
            | "buffer_close_others"
            | "buffer_close_others_force"
            | "buffer_close_all"
            | "buffer_close_all_force"
            | "next_buffer"
            | "previous_buffer"
            | "alternate_buffer"
            | "buffers"
            | "buffer_picker"
            | "changed_file_picker"
            | "files"
            | "file_picker"
            | "file_picker_in_current_directory"
            | "file_explorer"
            | "file_explorer_in_current_buffer_directory"
            | "file_explorer_in_current_directory"
            | "global_search"
            | "grep"
            | "jumplist_picker"
            | "last_picker" => "buffers",
            "goto"
            | "goto_column"
            | "goto_first_nonwhitespace"
            | "goto_last_modification"
            | "goto_declaration"
            | "goto_definition"
            | "goto_type_definition"
            | "goto_reference"
            | "goto_implementation"
            | "goto_window_top"
            | "goto_window_center"
            | "goto_window_bottom"
            | "goto_last_accessed_file"
            | "goto_last_modified_file"
            | "goto_next_diag"
            | "goto_prev_diag"
            | "goto_first_diag"
            | "goto_last_diag"
            | "goto_word"
            | "swift_motion"
            | "goto_next_change"
            | "goto_prev_change"
            | "goto_first_change"
            | "goto_last_change"
            | "goto_next_function"
            | "goto_prev_function"
            | "goto_next_class"
            | "goto_prev_class"
            | "goto_next_parameter"
            | "goto_prev_parameter"
            | "goto_next_comment"
            | "goto_prev_comment"
            | "goto_next_test"
            | "goto_prev_test"
            | "goto_next_paragraph"
            | "goto_prev_paragraph"
            | "goto_file_end" => "navigation",
            "definition"
            | "references"
            | "symbols"
            | "workspace_symbols"
            | "codeaction"
            | "code_action"
            | "rename"
            | "diagnostics"
            | "hover"
            | "lsp_restart"
            | "lsp_stop"
            | "symbol_picker"
            | "workspace_symbol_picker"
            | "diagnostics_picker"
            | "workspace_diagnostics_picker" => "ide",
            "quickfix_open"
            | "quickfix_close"
            | "quickfix_next"
            | "quickfix_prev"
            | "quickfix_first"
            | "quickfix_last"
            | "quickfix_select"
            | "quickfix_list"
            | "location_list_open"
            | "location_list_close"
            | "location_list_next"
            | "location_list_prev"
            | "location_list_first"
            | "location_list_last"
            | "location_list_select" => "lists",
            "agents"
            | "agents_clear"
            | "agents_close"
            | "agents_config"
            | "agents_config_set"
            | "agents_config_toggle"
            | "agents_layout"
            | "agents_thoughts"
            | "agents_mcp"
            | "agents_mode_next"
            | "agents_mode_prev"
            | "agents_new"
            | "agents_threads"
            | "agents_next"
            | "agents_prev"
            | "agents_stop"
            | "agents_resume"
            | "agents_discard"
            | "agents_reconnect" => "agents",
            _ => "editing",
        };

        let (summary, usage) = match canonical_id {
            "help" => (Cow::Borrowed("open searchable editor help"), None),
            "commands" => (Cow::Borrowed("list ex commands and features"), None),
            "keymap" => (Cow::Borrowed("list high-value normal-mode bindings"), None),
            "command_palette" => (Cow::Borrowed("open searchable command reference picker"), None),
            "term" => (
                Cow::Borrowed(
                    "run shell command and open transcript buffer; bang shorthand available",
                ),
                Some("<shell-command>"),
            ),
            "make" => (Cow::Borrowed("run cargo build in transcript buffer"), Some("[args]")),
            "test" => (Cow::Borrowed("run cargo test in transcript buffer"), Some("[args]")),
            "run" => (Cow::Borrowed("run cargo run in transcript buffer"), Some("[args]")),
            "edit" => (Cow::Borrowed("open file in current view"), Some("[path]")),
            "edit_config" => (Cow::Borrowed("open nearest ee config file"), None),
            "reload" => (Cow::Borrowed("reload active buffer from disk"), None),
            "reload_all" => (Cow::Borrowed("reload all open buffers from disk"), None),
            "new" => (Cow::Borrowed("create scratch buffer"), None),
            "split" => (Cow::Borrowed("open file in horizontal split"), Some("[path]")),
            "vsplit" => (Cow::Borrowed("open file in vertical split"), Some("[path]")),
            "goto" => (Cow::Borrowed("jump to 1-based line number"), Some("<line>")),
            "goto_column" => (
                Cow::Borrowed("move cursor to 1-based display column on current line"),
                Some("<column>"),
            ),
            "goto_first_nonwhitespace" => {
                (Cow::Borrowed("jump to first non-whitespace character on current line"), None)
            }
            "goto_last_modification" => {
                (Cow::Borrowed("jump to previous entry in change list"), None)
            }
            "goto_declaration" | "goto_definition" | "goto_type_definition" => {
                (Cow::Borrowed("request LSP navigation at cursor"), None)
            }
            "goto_reference" => (Cow::Borrowed("request backend references at cursor"), None),
            "goto_implementation" => (Cow::Borrowed("request implementation at cursor"), None),
            "next_buffer" => (Cow::Borrowed("cycle to next open buffer"), None),
            "previous_buffer" => (Cow::Borrowed("cycle to previous open buffer"), None),
            "alternate_buffer" => (Cow::Borrowed("jump to alternate buffer"), None),
            "goto_window_top" | "goto_window_center" | "goto_window_bottom" => {
                (Cow::Borrowed("jump cursor inside visible window"), None)
            }
            "goto_last_accessed_file" => {
                (Cow::Borrowed("switch to most recently accessed buffer"), None)
            }
            "goto_last_modified_file" => {
                (Cow::Borrowed("switch to most recently modified buffer"), None)
            }
            "goto_next_diag" | "goto_prev_diag" | "goto_first_diag" | "goto_last_diag" => {
                (Cow::Borrowed("jump active-buffer diagnostics"), None)
            }
            "goto_word" => {
                (Cow::Borrowed("move to next word start using normal word semantics"), None)
            }
            "swift_motion" => {
                (Cow::Borrowed("start visible-window two-char jump with labels"), None)
            }
            "quit" => (Cow::Borrowed("quit app when active buffer pristine"), None),
            "quit_force" => (Cow::Borrowed("force quit current session"), None),
            "quit_all" => (Cow::Borrowed("quit after pristine check across buffers"), None),
            "quit_all_force" => (Cow::Borrowed("force quit whole session"), None),
            "write" => (Cow::Borrowed("save current buffer"), None),
            "update" => (Cow::Borrowed("write current buffer only when dirty"), None),
            "write_all" => (Cow::Borrowed("save all dirty buffers"), None),
            "write_quit" => (Cow::Borrowed("save current buffer then quit"), None),
            "write_quit_all" => (Cow::Borrowed("save dirty buffers then quit"), None),
            "delete" => (Cow::Borrowed("delete addressed line range"), None),
            "substitute" => (
                Cow::Borrowed("replace matches in addressed line range"),
                Some("s/pattern/replacement/[flags]"),
            ),
            "yank" => (Cow::Borrowed("yank addressed line range"), None),
            "paste_clipboard_after" | "paste_clipboard_before" => {
                (Cow::Borrowed("paste system clipboard around current selection"), None)
            }
            "yank_to_clipboard" | "yank_main_selection_to_clipboard" => {
                (Cow::Borrowed("copy selection or addressed lines into system clipboard"), None)
            }
            "replace_selections_with_clipboard" => {
                (Cow::Borrowed("replace selections with system clipboard contents"), None)
            }
            "paste_primary_clipboard_after" | "paste_primary_clipboard_before" => {
                (Cow::Borrowed("paste primary clipboard around current selection"), None)
            }
            "yank_to_primary_clipboard" | "yank_main_selection_to_primary_clipboard" => {
                (Cow::Borrowed("copy selection or addressed lines into primary clipboard"), None)
            }
            "replace_selections_with_primary_clipboard" => {
                (Cow::Borrowed("replace selections with primary clipboard contents"), None)
            }
            "format" => (Cow::Borrowed("format current document through backend formatter"), None),
            "complete" => (Cow::Borrowed("open completion picker from backend suggestions"), None),
            "definition" => (Cow::Borrowed("request definition at cursor"), None),
            "symbols" => (Cow::Borrowed("request document symbols for current buffer"), None),
            "workspace_symbols" => (
                Cow::Borrowed("query workspace symbols with trailing search text"),
                Some("[query]"),
            ),
            "codeaction" => (Cow::Borrowed("request indexed backend code action directly"), None),
            "code_action" => (Cow::Borrowed("open code-action picker"), None),
            "rename" => (Cow::Borrowed("request backend rename at cursor"), Some("<new_name>")),
            "diagnostics" => {
                (Cow::Borrowed("open location list for active-buffer diagnostics"), None)
            }
            "diagnostics_picker" => {
                (Cow::Borrowed("open picker for active-buffer diagnostics"), None)
            }
            "logs" => (Cow::Borrowed("open picker for discovered editor and plugin logs"), None),
            "hover" => (Cow::Borrowed("request LSP hover at cursor"), None),
            "insert_register" => {
                (Cow::Borrowed("insert register contents at cursor"), Some("<register>"))
            }
            "gblame" => (Cow::Borrowed("show git blame metadata for current line"), None),
            "gdiff" => (Cow::Borrowed("open git diff for current buffer in scratch view"), None),
            "ghunkdiff" => (Cow::Borrowed("open git diff for current hunk in scratch view"), None),
            "expandtab" => {
                (Cow::Borrowed("convert tabs to spaces in selection or addressed lines"), None)
            }
            "reindent" => (Cow::Borrowed("run core reindent on current selection or line"), None),
            "reflow" => (
                Cow::Borrowed("hard-wrap selection or addressed lines to a width"),
                Some("<width>"),
            ),
            "renormalize" => (Cow::Borrowed("convert buffer line-ending setting to LF"), None),
            "buffer_picker" => (Cow::Borrowed("open buffer picker"), None),
            "changed_file_picker" => (Cow::Borrowed("open git-changed-file picker"), None),
            "jumplist_picker" => (Cow::Borrowed("open jump history picker"), None),
            "last_picker" => (Cow::Borrowed("reopen previous picker"), None),
            "files" => {
                (Cow::Borrowed("open file picker rooted at current working directory"), None)
            }
            "file_picker" => {
                (Cow::Borrowed("open file picker rooted at current buffer directory"), None)
            }
            "file_picker_in_current_directory" => {
                (Cow::Borrowed("open file picker rooted at current working directory"), None)
            }
            "file_explorer" => (Cow::Borrowed("open explorer rooted at workspace"), None),
            "file_explorer_in_current_buffer_directory" => {
                (Cow::Borrowed("open explorer rooted at current buffer directory"), None)
            }
            "file_explorer_in_current_directory" => {
                (Cow::Borrowed("open explorer rooted at current working directory"), None)
            }
            "workspace_diagnostics_picker" => {
                (Cow::Borrowed("open picker for workspace diagnostics"), None)
            }
            "global_search" => (Cow::Borrowed("open workspace live-grep picker"), None),
            "grep" => (Cow::Borrowed("open live grep picker seeded with query"), Some("<query>")),
            "reload_config" => {
                (Cow::Borrowed("refresh frontend config and keymap overrides"), None)
            }
            "set_language" => {
                (Cow::Borrowed("set or show current syntax name"), Some("[language]"))
            }
            "lsp_restart" => (Cow::Borrowed("restart language-server plugin"), None),
            "lsp_stop" => (Cow::Borrowed("stop language-server plugin"), None),
            "change_current_directory" => {
                (Cow::Borrowed("switch current working directory"), Some("<path>"))
            }
            "show_directory" => (Cow::Borrowed("print current working directory"), None),
            "create_directory" => (
                Cow::Borrowed("create directory tree under current workspace root"),
                Some("<path>"),
            ),
            "read" => (Cow::Borrowed("insert file contents at cursor"), Some("<path>")),
            "move" => (Cow::Borrowed("move current buffer to new path"), Some("<path>")),
            "pipe" => (
                Cow::Borrowed("replace selections with shell command output"),
                Some("<shell-command>"),
            ),
            "pipe_to" => (
                Cow::Borrowed("run shell command for each selection and ignore stdout"),
                Some("<shell-command>"),
            ),
            "shell_insert_output" => {
                (Cow::Borrowed("insert shell output before selections"), Some("<shell-command>"))
            }
            "shell_append_output" => {
                (Cow::Borrowed("append shell output after selections"), Some("<shell-command>"))
            }
            "shell_keep_pipe" => (
                Cow::Borrowed("keep selections whose shell command exits successfully"),
                Some("<shell-command>"),
            ),
            "encoding" => {
                (Cow::Borrowed("show or set current buffer encoding metadata"), Some("[name]"))
            }
            "clear_register" => {
                (Cow::Borrowed("clear one register or all registers"), Some("[register]"))
            }
            "echo" => (Cow::Borrowed("print arguments to status line"), Some("[text...]")),
            "redraw" => (Cow::Borrowed("clear and repaint UI"), None),
            "selection_for_find" => (Cow::Borrowed("lift selection into find"), None),
            "selection_for_replace" => (Cow::Borrowed("lift selection into replace"), None),
            "selection_into_lines" => {
                (Cow::Borrowed("split selection into per-line cursors"), None)
            }
            "select_regex" => {
                (Cow::Borrowed("select regex matches inside current selections"), Some("<pattern>"))
            }
            "split_selection_on_newline" => {
                (Cow::Borrowed("split selections on line boundaries"), None)
            }
            "merge_selections" | "merge_consecutive_selections" => {
                (Cow::Borrowed("combine active selections"), None)
            }
            "trim_selections" => (Cow::Borrowed("trim current selections"), None),
            "collapse_selection" => (Cow::Borrowed("collapse selections to cursor points"), None),
            "align_selections" => (Cow::Borrowed("pad selections into aligned columns"), None),
            "align_it" => (
                Cow::Borrowed("align matched lines tabular-style in selection, range, or block"),
                Some("[N|*|-N]<delimiter>|/regex/ [l1r1l0]"),
            ),
            "flip_selections" => (Cow::Borrowed("flip selection direction"), None),
            "ensure_selections_forward" => {
                (Cow::Borrowed("rewrite selections to forward direction"), None)
            }
            "join_selections" => (Cow::Borrowed("join selected lines"), None),
            "join_selections_space" => (Cow::Borrowed("join selected lines with spaces"), None),
            "select_textobject_inner" => {
                (Cow::Borrowed("select inner text object at cursor"), Some("<spec>"))
            }
            "select_textobject_around" => {
                (Cow::Borrowed("select outer text object at cursor"), Some("<spec>"))
            }
            "surround_add" => (Cow::Borrowed("add surrounding delimiters"), Some("<pair> [spec]")),
            "surround_replace" => (Cow::Borrowed("replace surrounding delimiters"), Some("<pair>")),
            "surround_delete" => (Cow::Borrowed("delete surrounding delimiters"), None),
            "keep_selections" => (Cow::Borrowed("keep selections matching regex"), Some("[regex]")),
            "remove_selections" => {
                (Cow::Borrowed("remove selections matching regex"), Some("[regex]"))
            }
            "keep_primary_selection" => (Cow::Borrowed("keep primary selection only"), None),
            "remove_primary_selection" => (Cow::Borrowed("drop primary selection"), None),
            "expand_selection" => (Cow::Borrowed("grow syntax-node selections"), None),
            "shrink_selection" => (Cow::Borrowed("restore previous syntax-node selections"), None),
            "rotate_selections_backward" => {
                (Cow::Borrowed("cycle primary selection backward"), None)
            }
            "rotate_selections_forward" => (Cow::Borrowed("cycle primary selection forward"), None),
            "commit_undo_checkpoint" => {
                (Cow::Borrowed("split subsequent edits into a fresh undo step"), None)
            }
            "multi_find" => {
                (Cow::Borrowed("run backend multi-find queries"), Some("<term> [term ...]"))
            }
            "set" => {
                (Cow::Borrowed("change editor options inline"), Some("<option>|<option>=<value>"))
            }
            "nohlsearch" => (Cow::Borrowed("clear search highlighting"), None),
            "quickfix_open" => (Cow::Borrowed("open quickfix list"), None),
            "quickfix_close" => (Cow::Borrowed("close quickfix list"), None),
            "quickfix_next" => (Cow::Borrowed("jump to next quickfix entry"), None),
            "quickfix_prev" => (Cow::Borrowed("jump to previous quickfix entry"), None),
            "quickfix_first" => (Cow::Borrowed("jump to first quickfix entry"), None),
            "quickfix_last" => (Cow::Borrowed("jump to last quickfix entry"), None),
            "quickfix_select" => (Cow::Borrowed("jump to quickfix entry"), Some("[N]")),
            "quickfix_list" => (Cow::Borrowed("print quickfix entries to status line"), None),
            "location_list_open" => (Cow::Borrowed("open location list"), None),
            "location_list_close" => (Cow::Borrowed("close location list"), None),
            "location_list_next" => (Cow::Borrowed("jump to next location-list entry"), None),
            "location_list_prev" => (Cow::Borrowed("jump to previous location-list entry"), None),
            "location_list_first" => (Cow::Borrowed("jump to first location-list entry"), None),
            "location_list_last" => (Cow::Borrowed("jump to last location-list entry"), None),
            "location_list_select" => (Cow::Borrowed("jump to location-list entry"), Some("[N]")),
            "agents" => (Cow::Borrowed("open agents pane or report agents-mode status"), None),
            "agents_close" => {
                (Cow::Borrowed("close agents pane without killing the session"), None)
            }
            "agents_config" => {
                (Cow::Borrowed("list advertised agent session config options"), None)
            }
            "agents_config_set" => (
                Cow::Borrowed("set advertised agent session config option"),
                Some("<config_id> <value>"),
            ),
            "agents_config_toggle" => (
                Cow::Borrowed("toggle advertised boolean agent session config option"),
                Some("<config_id>"),
            ),
            "agents_layout" => {
                (Cow::Borrowed("set the agents pane split layout"), Some("right|bottom|full"))
            }
            "agents_thoughts" => (
                Cow::Borrowed("show, hide, or toggle streamed agent thought messages"),
                Some("on|off|toggle"),
            ),
            "agents_mcp" => (
                Cow::Borrowed("browse MCP tools/prompts/resources or show MCP health"),
                Some("tools|prompts|resources|close"),
            ),
            "agents_mode_next" => (Cow::Borrowed("switch to next advertised agent mode"), None),
            "agents_mode_prev" => (Cow::Borrowed("switch to previous advertised agent mode"), None),
            "agents_new" => (Cow::Borrowed("start a new agent session"), None),
            "agents_threads" => (Cow::Borrowed("open picker for agent sessions"), None),
            "agents_next" => (Cow::Borrowed("switch to the next agent thread"), None),
            "agents_prev" => (Cow::Borrowed("switch to the previous agent thread"), None),
            "agents_stop" => (Cow::Borrowed("stop the active agent session"), None),
            "agents_resume" => {
                (Cow::Borrowed("resume a paused recoverable agent turn from its checkpoint"), None)
            }
            "agents_discard" => {
                (Cow::Borrowed("discard a paused recoverable agent turn's checkpoint"), None)
            }
            "agents_reconnect" => (
                Cow::Borrowed(
                    "reconnect the persisted agent session of this workspace (load or resume)",
                ),
                None,
            ),
            "agents_clear" => (Cow::Borrowed("clear the active agent session"), None),
            "buffers" => (Cow::Borrowed("print open buffers to status line"), None),
            "buffer_close" => (Cow::Borrowed("close current buffer"), None),
            "buffer_close_force" => {
                (Cow::Borrowed("force-close current buffer without pristine check"), None)
            }
            "buffer_close_others" => (Cow::Borrowed("close non-active buffers"), None),
            "buffer_close_others_force" => (Cow::Borrowed("force-close non-active buffers"), None),
            "buffer_close_all" => (Cow::Borrowed("close all buffers"), None),
            "buffer_close_all_force" => (Cow::Borrowed("force-close all buffers"), None),
            "tab_edit" => (Cow::Borrowed("open buffer in new tab"), Some("[path]")),
            "tab_close" => (Cow::Borrowed("close current tab"), None),
            "tab_next" => (Cow::Borrowed("cycle to next tab"), None),
            "tab_prev" => (Cow::Borrowed("cycle to previous tab"), None),
            "tabs" => (Cow::Borrowed("print tab list"), None),
            _ => (Cow::Owned(humanize_command_summary(canonical_id)), None),
        };

        CommandMetadata { summary, usage, category }
    }
    pub(super) fn format_command_help_item(canonical_id: &str) -> String {
        let aliases = Self::ordered_aliases_for(canonical_id)
            .into_iter()
            .map(|alias| format!(":{alias}"))
            .collect::<Vec<_>>()
            .join(" / ");
        let spec = Self::canonical_command_spec(canonical_id);
        let category = spec.category.expect("registry metadata missing command category");
        match spec.usage {
            Some(usage) => format!("[{category}] {aliases} {usage} {}", spec.summary),
            None => format!("[{category}] {aliases} {}", spec.summary),
        }
    }
    pub(super) fn rewrite_command_alias(command: &str) -> Cow<'_, str> {
        let trimmed = command.trim_start();
        if trimmed.is_empty() {
            return Cow::Borrowed(command);
        }

        let split_at = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let head = &trimmed[..split_at];
        let tail = trimmed[split_at..].trim_start();
        let Some(spec) = Self::resolve_ex_command(head) else {
            return Cow::Borrowed(command);
        };
        if spec.dispatch == head {
            return Cow::Borrowed(command);
        }

        let mut rewritten = String::from(spec.dispatch);
        if !tail.is_empty() {
            rewritten.push(' ');
            rewritten.push_str(tail);
        }
        Cow::Owned(rewritten)
    }
    pub(super) const LSP_PLUGIN_NAME: &'static str = crate::config::LSP_PLUGIN_NAME;
}
