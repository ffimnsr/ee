use super::*;
use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use crate::buffer::BufferId;
use crate::registers::RegisterName;

enum SaveOutcome {
    Saved,
    AwaitingPrivilegeConfirm,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandSpec {
    canonical_id: &'static str,
    alias: &'static str,
    summary: Cow<'static, str>,
    usage: Option<&'static str>,
    category: Option<&'static str>,
    dispatch: &'static str,
}

struct CommandMetadata {
    summary: Cow<'static, str>,
    usage: Option<&'static str>,
    category: &'static str,
}

const fn command_spec(
    alias: &'static str,
    canonical_id: &'static str,
    dispatch: &'static str,
) -> CommandSpec {
    CommandSpec {
        canonical_id,
        alias,
        summary: Cow::Borrowed(canonical_id),
        usage: None,
        category: None,
        dispatch,
    }
}

// Flat alias registry preserves current first-match completion semantics.
// canonical_id groups aliases so execution/help can move off raw strings incrementally.
const COMMAND_SPECS: &[CommandSpec] = &[
    command_spec("b#", "alternate_buffer", "b#"),
    command_spec("bc", "buffer_close", "bc"),
    command_spec("bc!", "buffer_close_force", "bc!"),
    command_spec("bd", "buffer_close", "bc"),
    command_spec("bdelete", "buffer_close", "bc"),
    command_spec("bclose", "buffer_close", "bc"),
    command_spec("bclose!", "buffer_close_force", "bc!"),
    command_spec("bcloseall", "buffer_close_all", "bca"),
    command_spec("bcloseall!", "buffer_close_all_force", "bca!"),
    command_spec("bcloseother", "buffer_close_others", "bco"),
    command_spec("bcloseother!", "buffer_close_others_force", "bco!"),
    command_spec("bca", "buffer_close_all", "bca"),
    command_spec("bca!", "buffer_close_all_force", "bca!"),
    command_spec("bco", "buffer_close_others", "bco"),
    command_spec("bco!", "buffer_close_others_force", "bco!"),
    command_spec("buffer_close!", "buffer_close_force", "bc!"),
    command_spec("buffer_close", "buffer_close", "bc"),
    command_spec("buffer_close_all", "buffer_close_all", "bca"),
    command_spec("buffer_close_all!", "buffer_close_all_force", "bca!"),
    command_spec("buffer_close_others", "buffer_close_others", "bco"),
    command_spec("buffer_close_others!", "buffer_close_others_force", "bco!"),
    command_spec("bn", "next_buffer", "bn"),
    command_spec("bnext", "next_buffer", "bn"),
    command_spec("goto_next_buffer", "next_buffer", "bn"),
    command_spec("bp", "previous_buffer", "bp"),
    command_spec("bprev", "previous_buffer", "bp"),
    command_spec("bprevious", "previous_buffer", "bp"),
    command_spec("goto_previous_buffer", "previous_buffer", "bp"),
    command_spec("buffers", "buffers", "ls"),
    command_spec("cc", "quickfix_select", "cc"),
    command_spec("ccl", "quickfix_close", "cclose"),
    command_spec("cclose", "quickfix_close", "cclose"),
    command_spec("cfirst", "quickfix_first", "cfirst"),
    command_spec("cl", "quickfix_list", "clist"),
    command_spec("clast", "quickfix_last", "clast"),
    command_spec("clist", "quickfix_list", "clist"),
    command_spec("cn", "quickfix_next", "cn"),
    command_spec("cnext", "quickfix_next", "cn"),
    command_spec("cope", "quickfix_open", "copen"),
    command_spec("copen", "quickfix_open", "copen"),
    command_spec("cp", "quickfix_prev", "cp"),
    command_spec("cprev", "quickfix_prev", "cp"),
    command_spec("cprevious", "quickfix_prev", "cp"),
    command_spec("codeaction", "codeaction", "codeaction"),
    command_spec("codeactions", "codeaction", "codeaction"),
    command_spec("code_action", "code_action", "code_action"),
    command_spec("complete", "complete", "complete"),
    command_spec("completion", "complete", "complete"),
    command_spec("config_reload", "reload_config", "reload_config"),
    command_spec("command_palette", "command_palette", "command_palette"),
    command_spec("d", "delete", "d"),
    command_spec("s", "substitute", "s"),
    command_spec("substitute", "substitute", "s"),
    command_spec("def", "definition", "definition"),
    command_spec("definition", "definition", "definition"),
    command_spec("goto_declaration", "goto_declaration", "goto_declaration"),
    command_spec("goto_definition", "goto_definition", "goto_definition"),
    command_spec("goto_type_definition", "goto_type_definition", "goto_type_definition"),
    command_spec("goto_reference", "goto_reference", "goto_reference"),
    command_spec("goto_implementation", "goto_implementation", "goto_implementation"),
    command_spec("delete", "delete", "d"),
    command_spec("diagnostics", "diagnostics", "diagnostics"),
    command_spec("e", "edit", "e"),
    command_spec("goto_last_accessed_file", "goto_last_accessed_file", "goto_last_accessed_file"),
    command_spec("goto_last_modified_file", "goto_last_modified_file", "goto_last_modified_file"),
    command_spec("e!", "reload", "e!"),
    command_spec("edit", "edit", "e"),
    command_spec("edit_config", "edit_config", "edit_config"),
    command_spec("goto_window_bottom", "goto_window_bottom", "goto_window_bottom"),
    command_spec("goto_window_center", "goto_window_center", "goto_window_center"),
    command_spec("goto_window_top", "goto_window_top", "goto_window_top"),
    command_spec("edit!", "reload", "e!"),
    command_spec("expandtab", "expandtab", "expandtab"),
    command_spec("g", "goto", "g"),
    command_spec("commands", "commands", "commands"),
    command_spec("create_directory", "create_directory", "create_directory"),
    command_spec("decrement", "decrement", "decrement"),
    command_spec("delete_char_backward", "delete_char_backward", "delete_char_backward"),
    command_spec("delete_char_forward", "delete_char_forward", "delete_char_forward"),
    command_spec("delete_word_backward", "delete_word_backward", "delete_word_backward"),
    command_spec("delete_word_forward", "delete_word_forward", "delete_word_forward"),
    command_spec("duplicate_line", "duplicate_line", "duplicate_line"),
    command_spec("files", "files", "files"),
    command_spec("file_explorer", "file_explorer", "file_explorer"),
    command_spec(
        "file_explorer_in_current_buffer_directory",
        "file_explorer_in_current_buffer_directory",
        "file_explorer_in_current_buffer_directory",
    ),
    command_spec(
        "file_explorer_in_current_directory",
        "file_explorer_in_current_directory",
        "file_explorer_in_current_directory",
    ),
    command_spec("file_picker", "file_picker", "file_picker"),
    command_spec(
        "file_picker_in_current_directory",
        "file_picker_in_current_directory",
        "file_picker_in_current_directory",
    ),
    command_spec("format", "format", "format"),
    command_spec("grep", "grep", "grep"),
    command_spec("global_search", "global_search", "global_search"),
    command_spec("buffer_picker", "buffer_picker", "buffer_picker"),
    command_spec("changed_file_picker", "changed_file_picker", "changed_file_picker"),
    command_spec("gblame", "gblame", "gblame"),
    command_spec("gdiff", "gdiff", "gdiff"),
    command_spec("ghunkdiff", "ghunkdiff", "ghunkdiff"),
    command_spec("goto", "goto", "g"),
    command_spec("goto_column", "goto_column", "goto_column"),
    command_spec("goto_first_change", "goto_first_change", "goto_first_change"),
    command_spec("goto_first_diag", "goto_first_diag", "goto_first_diag"),
    command_spec(
        "goto_first_nonwhitespace",
        "goto_first_nonwhitespace",
        "goto_first_nonwhitespace",
    ),
    command_spec("goto_last_change", "goto_last_change", "goto_last_change"),
    command_spec("goto_last_diag", "goto_last_diag", "goto_last_diag"),
    command_spec("goto_last_modification", "goto_last_modification", "goto_last_modification"),
    command_spec("goto_next_change", "goto_next_change", "goto_next_change"),
    command_spec("goto_next_class", "goto_next_class", "goto_next_class"),
    command_spec("goto_next_comment", "goto_next_comment", "goto_next_comment"),
    command_spec("goto_next_diag", "goto_next_diag", "goto_next_diag"),
    command_spec("goto_next_function", "goto_next_function", "goto_next_function"),
    command_spec("goto_next_paragraph", "goto_next_paragraph", "goto_next_paragraph"),
    command_spec("goto_next_parameter", "goto_next_parameter", "goto_next_parameter"),
    command_spec("goto_next_test", "goto_next_test", "goto_next_test"),
    command_spec("goto_prev_change", "goto_prev_change", "goto_prev_change"),
    command_spec("goto_prev_class", "goto_prev_class", "goto_prev_class"),
    command_spec("goto_prev_comment", "goto_prev_comment", "goto_prev_comment"),
    command_spec("goto_prev_diag", "goto_prev_diag", "goto_prev_diag"),
    command_spec("goto_prev_function", "goto_prev_function", "goto_prev_function"),
    command_spec("goto_prev_paragraph", "goto_prev_paragraph", "goto_prev_paragraph"),
    command_spec("goto_prev_parameter", "goto_prev_parameter", "goto_prev_parameter"),
    command_spec("goto_prev_test", "goto_prev_test", "goto_prev_test"),
    command_spec("goto_word", "goto_word", "goto_word"),
    command_spec("lang", "set_language", "set_language"),
    command_spec("hs", "split", "sp"),
    command_spec("hsplit", "split", "sp"),
    command_spec("help", "help", "help"),
    command_spec("hover", "hover", "hover"),
    command_spec("increment", "increment", "increment"),
    command_spec("insert_newline", "insert_newline", "insert_newline"),
    command_spec("insert_register", "insert_register", "insert_register"),
    command_spec("insert_tab", "insert_tab", "insert_tab"),
    command_spec("keymap", "keymap", "keymap"),
    command_spec("kill_line", "kill_line", "kill_line"),
    command_spec("kill_to_line_end", "kill_to_line_end", "kill_to_line_end"),
    command_spec("kill_to_line_start", "kill_to_line_start", "kill_to_line_start"),
    command_spec("lcl", "location_list_close", "lclose"),
    command_spec("lclose", "location_list_close", "lclose"),
    command_spec("lfirst", "location_list_first", "lfirst"),
    command_spec("llast", "location_list_last", "llast"),
    command_spec("lsp_restart", "lsp_restart", "lsp_restart"),
    command_spec("lsp_stop", "lsp_stop", "lsp_stop"),
    command_spec("ll", "location_list_select", "ll"),
    command_spec("ln", "location_list_next", "lnext"),
    command_spec("lnext", "location_list_next", "lnext"),
    command_spec("logs", "logs", "logs"),
    command_spec("lop", "location_list_open", "lopen"),
    command_spec("lopen", "location_list_open", "lopen"),
    command_spec("lp", "location_list_prev", "lprev"),
    command_spec("lprev", "location_list_prev", "lprev"),
    command_spec("lprevious", "location_list_prev", "lprev"),
    command_spec("ls", "buffers", "ls"),
    command_spec("multi_find", "multi_find", "multi_find"),
    command_spec("make", "make", "make"),
    command_spec("move", "move", "move"),
    command_spec("move_parent_node_end", "move_parent_node_end", "move_parent_node_end"),
    command_spec("move_parent_node_start", "move_parent_node_start", "move_parent_node_start"),
    command_spec("mv", "move", "move"),
    command_spec("n", "new", "new"),
    command_spec("new", "new", "new"),
    command_spec("noh", "nohlsearch", "noh"),
    command_spec("nohlsearch", "nohlsearch", "noh"),
    command_spec("o", "edit", "e"),
    command_spec("outline", "symbols", "symbols"),
    command_spec("open", "edit", "e"),
    command_spec("pipe", "pipe", "pipe"),
    command_spec("pipe_to", "pipe_to", "pipe_to"),
    command_spec("pwd", "show_directory", "show_directory"),
    command_spec("q", "quit", "q"),
    command_spec("q!", "quit_force", "q!"),
    command_spec("qa", "quit_all", "qa"),
    command_spec("qa!", "quit_all_force", "qa!"),
    command_spec("quit", "quit", "q"),
    command_spec("quit!", "quit_force", "q!"),
    command_spec("quit_all", "quit_all", "qa"),
    command_spec("quit_all!", "quit_all_force", "qa!"),
    command_spec("recover", "recover", "recover"),
    command_spec("recoverdel", "recoverdel", "recoverdel"),
    command_spec("reload_config", "reload_config", "reload_config"),
    command_spec("reset_diff_change", "reset_diff_change", "reset_diff_change"),
    command_spec("reindent", "reindent", "reindent"),
    command_spec("reflow", "reflow", "reflow"),
    command_spec("renormalize", "renormalize", "renormalize"),
    command_spec("rename", "rename", "rename"),
    command_spec("references", "references", "references"),
    command_spec("refs", "references", "references"),
    command_spec("reload", "reload", "e!"),
    command_spec("reload_all", "reload_all", "reload_all"),
    command_spec("rl", "reload", "e!"),
    command_spec("rla", "reload_all", "reload_all"),
    command_spec("r", "read", "read"),
    command_spec("read", "read", "read"),
    command_spec("redraw", "redraw", "redraw"),
    command_spec("run", "run", "run"),
    command_spec("run_shell_command", "term", "term"),
    command_spec("shell_append_output", "shell_append_output", "shell_append_output"),
    command_spec("shell_insert_output", "shell_insert_output", "shell_insert_output"),
    command_spec("shell_keep_pipe", "shell_keep_pipe", "shell_keep_pipe"),
    command_spec("shell_pipe", "pipe", "pipe"),
    command_spec("shell_pipe_to", "pipe_to", "pipe_to"),
    command_spec("selection_for_find", "selection_for_find", "selection_for_find"),
    command_spec("selection_for_replace", "selection_for_replace", "selection_for_replace"),
    command_spec("select_regex", "select_regex", "select_regex"),
    command_spec("selection_into_lines", "selection_into_lines", "selection_into_lines"),
    command_spec("set", "set", "set"),
    command_spec("set_language", "set_language", "set_language"),
    command_spec("sh", "term", "term"),
    command_spec("show_directory", "show_directory", "show_directory"),
    command_spec("split_selection", "selection_into_lines", "selection_into_lines"),
    command_spec(
        "split_selection_on_newline",
        "split_selection_on_newline",
        "split_selection_on_newline",
    ),
    command_spec("merge_selections", "merge_selections", "merge_selections"),
    command_spec(
        "merge_consecutive_selections",
        "merge_consecutive_selections",
        "merge_consecutive_selections",
    ),
    command_spec("trim_selections", "trim_selections", "trim_selections"),
    command_spec("align_selections", "align_selections", "align_selections"),
    command_spec("align_it", "align_it", "align_it"),
    command_spec("collapse_selection", "collapse_selection", "collapse_selection"),
    command_spec("clear_register", "clear_register", "clear_register"),
    command_spec("flip_selections", "flip_selections", "flip_selections"),
    command_spec("echo", "echo", "echo"),
    command_spec("encoding", "encoding", "encoding"),
    command_spec(
        "ensure_selections_forward",
        "ensure_selections_forward",
        "ensure_selections_forward",
    ),
    command_spec("expand_selection", "expand_selection", "expand_selection"),
    command_spec("extend_char_left", "extend_char_left", "extend_char_left"),
    command_spec("extend_char_right", "extend_char_right", "extend_char_right"),
    command_spec("extend_line_above", "extend_line_above", "extend_line_above"),
    command_spec("extend_line_below", "extend_line_below", "extend_line_below"),
    command_spec("extend_line_down", "extend_line_down", "extend_line_down"),
    command_spec("extend_line_up", "extend_line_up", "extend_line_up"),
    command_spec("extend_to_line_bounds", "extend_to_line_bounds", "extend_to_line_bounds"),
    command_spec("extend_to_file_end", "extend_to_file_end", "extend_to_file_end"),
    command_spec("extend_to_file_start", "extend_to_file_start", "extend_to_file_start"),
    command_spec("extend_visual_line_down", "extend_line_down", "extend_line_down"),
    command_spec("extend_visual_line_up", "extend_line_up", "extend_line_up"),
    command_spec("join_selections", "join_selections", "join_selections"),
    command_spec("join_selections_space", "join_selections_space", "join_selections_space"),
    command_spec("jumplist_picker", "jumplist_picker", "jumplist_picker"),
    command_spec("keep_selections", "keep_selections", "keep_selections"),
    command_spec("keep_primary_selection", "keep_primary_selection", "keep_primary_selection"),
    command_spec("last_picker", "last_picker", "last_picker"),
    command_spec("match_brackets", "match_brackets", "match_brackets"),
    command_spec("move_line_down", "move_line_down", "move_line_down"),
    command_spec("move_line_up", "move_line_up", "move_line_up"),
    command_spec("goto_file_end", "goto_file_end", "goto_file_end"),
    command_spec("remove_selections", "remove_selections", "remove_selections"),
    command_spec(
        "remove_primary_selection",
        "remove_primary_selection",
        "remove_primary_selection",
    ),
    command_spec(
        "rotate_selections_backward",
        "rotate_selections_backward",
        "rotate_selections_backward",
    ),
    command_spec(
        "rotate_selections_forward",
        "rotate_selections_forward",
        "rotate_selections_forward",
    ),
    command_spec("select_line_above", "select_line_above", "select_line_above"),
    command_spec("select_line_below", "select_line_below", "select_line_below"),
    command_spec("select_all_children", "select_all_children", "select_all_children"),
    command_spec("select_all_siblings", "select_all_siblings", "select_all_siblings"),
    command_spec("symbol_picker", "symbol_picker", "symbol_picker"),
    command_spec("symbols", "symbols", "symbols"),
    command_spec("swift", "swift_motion", "swift_motion"),
    command_spec("swift_motion", "swift_motion", "swift_motion"),
    command_spec(
        "select_textobject_around",
        "select_textobject_around",
        "select_textobject_around",
    ),
    command_spec("select_textobject_inner", "select_textobject_inner", "select_textobject_inner"),
    command_spec("select_next_sibling", "select_next_sibling", "select_next_sibling"),
    command_spec("select_prev_sibling", "select_prev_sibling", "select_prev_sibling"),
    command_spec("select_references_to_symbol_under_cursor", "goto_reference", "goto_reference"),
    command_spec("shrink_selection", "shrink_selection", "shrink_selection"),
    command_spec("shrink_to_line_bounds", "shrink_to_line_bounds", "shrink_to_line_bounds"),
    command_spec(
        "copy_selection_on_next_line",
        "copy_selection_on_next_line",
        "copy_selection_on_next_line",
    ),
    command_spec(
        "copy_selection_on_prev_line",
        "copy_selection_on_prev_line",
        "copy_selection_on_prev_line",
    ),
    command_spec("diagnostics_picker", "diagnostics_picker", "diagnostics_picker"),
    command_spec("surround_add", "surround_add", "surround_add"),
    command_spec("surround_delete", "surround_delete", "surround_delete"),
    command_spec("surround_replace", "surround_replace", "surround_replace"),
    command_spec(
        "workspace_diagnostics_picker",
        "workspace_diagnostics_picker",
        "workspace_diagnostics_picker",
    ),
    command_spec("workspace_symbol_picker", "workspace_symbol_picker", "workspace_symbol_picker"),
    command_spec("wsymbol", "workspace_symbols", "wsymbols"),
    command_spec("wsymbols", "workspace_symbols", "wsymbols"),
    command_spec("add_newline_above", "add_newline_above", "add_newline_above"),
    command_spec("add_newline_below", "add_newline_below", "add_newline_below"),
    command_spec(
        "reverse_selection_contents",
        "reverse_selection_contents",
        "reverse_selection_contents",
    ),
    command_spec(
        "rotate_selection_contents_backward",
        "rotate_selection_contents_backward",
        "rotate_selection_contents_backward",
    ),
    command_spec(
        "rotate_selection_contents_forward",
        "rotate_selection_contents_forward",
        "rotate_selection_contents_forward",
    ),
    command_spec("select_all", "select_all", "select_all"),
    command_spec("bpick", "buffer_picker", "bpick"),
    command_spec("sp", "split", "sp"),
    command_spec("split", "split", "sp"),
    command_spec("tabc", "tab_close", "tabc"),
    command_spec("tabclose", "tab_close", "tabc"),
    command_spec("tabe", "tab_edit", "tabnew"),
    command_spec("tabedit", "tab_edit", "tabnew"),
    command_spec("tabn", "tab_next", "tabn"),
    command_spec("tabnext", "tab_next", "tabn"),
    command_spec("tabnew", "tab_edit", "tabnew"),
    command_spec("tabp", "tab_prev", "tabp"),
    command_spec("tabprev", "tab_prev", "tabp"),
    command_spec("tabprevious", "tab_prev", "tabp"),
    command_spec("tabs", "tabs", "tabs"),
    command_spec("rotate_view", "rotate_view", "rotate_view"),
    command_spec("cycle_view", "rotate_view", "rotate_view"),
    command_spec("rotate_view_reverse", "rotate_view_reverse", "rotate_view_reverse"),
    command_spec("transpose_view", "transpose_view", "transpose_view"),
    command_spec("wclose", "wclose", "wclose"),
    command_spec("wonly", "wonly", "wonly"),
    command_spec("jump_view_left", "jump_view_left", "jump_view_left"),
    command_spec("jump_view_down", "jump_view_down", "jump_view_down"),
    command_spec("jump_view_up", "jump_view_up", "jump_view_up"),
    command_spec("jump_view_right", "jump_view_right", "jump_view_right"),
    command_spec("swap_view_left", "swap_view_left", "swap_view_left"),
    command_spec("swap_view_down", "swap_view_down", "swap_view_down"),
    command_spec("swap_view_up", "swap_view_up", "swap_view_up"),
    command_spec("swap_view_right", "swap_view_right", "swap_view_right"),
    command_spec("term", "term", "term"),
    command_spec("terminal", "term", "term"),
    command_spec("test", "test", "test"),
    command_spec("transpose", "transpose", "transpose"),
    command_spec("sort", "sort", "sort"),
    command_spec("rsort", "rsort", "rsort"),
    command_spec("uniq", "uniq", "uniq"),
    command_spec("dedup", "uniq", "uniq"),
    command_spec("add_selection_above", "add_selection_above", "add_selection_above"),
    command_spec("add_selection_below", "add_selection_below", "add_selection_below"),
    command_spec("agents", "agents", "agents"),
    command_spec("agents_clear", "agents_clear", "agents_clear"),
    command_spec("agents_close", "agents_close", "agents_close"),
    command_spec("agents_config", "agents_config", "agents_config"),
    command_spec("agents_config_set", "agents_config_set", "agents_config_set"),
    command_spec("agents_config_toggle", "agents_config_toggle", "agents_config_toggle"),
    command_spec("agents_layout", "agents_layout", "agents_layout"),
    command_spec("agents_thoughts", "agents_thoughts", "agents_thoughts"),
    command_spec("agents_mcp", "agents_mcp", "agents_mcp"),
    command_spec("agents_mode_next", "agents_mode_next", "agents_mode_next"),
    command_spec("agents_mode_prev", "agents_mode_prev", "agents_mode_prev"),
    command_spec("agents_new", "agents_new", "agents_new"),
    command_spec("agents_threads", "agents_threads", "agents_threads"),
    command_spec("agents_next", "agents_next", "agents_next"),
    command_spec("agents_prev", "agents_prev", "agents_prev"),
    command_spec("agents_stop", "agents_stop", "agents_stop"),
    command_spec(
        "change_current_directory",
        "change_current_directory",
        "change_current_directory",
    ),
    command_spec("cd", "change_current_directory", "change_current_directory"),
    command_spec("commit_undo_checkpoint", "commit_undo_checkpoint", "commit_undo_checkpoint"),
    command_spec("diffget", "reset_diff_change", "reset_diff_change"),
    command_spec("diffg", "reset_diff_change", "reset_diff_change"),
    command_spec("|", "pipe", "pipe"),
    command_spec("vs", "vsplit", "vs"),
    command_spec("vsplit", "vsplit", "vs"),
    command_spec("w", "write", "w"),
    command_spec("w!", "write", "w"),
    command_spec("wq", "write_quit", "wq"),
    command_spec("wq!", "write_quit", "wq"),
    command_spec("wa", "write_all", "wa"),
    command_spec("wa!", "write_all", "wa"),
    command_spec("write!", "write", "w"),
    command_spec("write_all", "write_all", "wa"),
    command_spec("write_all!", "write_all", "wa"),
    command_spec("write_quit", "write_quit", "wq"),
    command_spec("write_quit!", "write_quit", "wq"),
    command_spec("write_quit_all", "write_quit_all", "wqa"),
    command_spec("write_quit_all!", "write_quit_all", "wqa"),
    command_spec("write", "write", "w"),
    command_spec("wqa", "write_quit_all", "wqa"),
    command_spec("wqa!", "write_quit_all", "wqa"),
    command_spec("u", "update", "u"),
    command_spec("update", "update", "u"),
    command_spec("x", "write_quit", "wq"),
    command_spec("x!", "write_quit", "wq"),
    command_spec("xa", "write_quit_all", "wqa"),
    command_spec("xa!", "write_quit_all", "wqa"),
    command_spec("y", "yank", "y"),
    command_spec("yank", "yank", "y"),
    command_spec("paste_clipboard_after", "paste_clipboard_after", "paste_clipboard_after"),
    command_spec("paste_clipboard_before", "paste_clipboard_before", "paste_clipboard_before"),
    command_spec("yank_to_clipboard", "yank_to_clipboard", "yank_to_clipboard"),
    command_spec(
        "yank_main_selection_to_clipboard",
        "yank_main_selection_to_clipboard",
        "yank_main_selection_to_clipboard",
    ),
    command_spec(
        "replace_selections_with_clipboard",
        "replace_selections_with_clipboard",
        "replace_selections_with_clipboard",
    ),
    command_spec(
        "paste_primary_clipboard_after",
        "paste_primary_clipboard_after",
        "paste_primary_clipboard_after",
    ),
    command_spec(
        "paste_primary_clipboard_before",
        "paste_primary_clipboard_before",
        "paste_primary_clipboard_before",
    ),
    command_spec(
        "yank_to_primary_clipboard",
        "yank_to_primary_clipboard",
        "yank_to_primary_clipboard",
    ),
    command_spec(
        "yank_main_selection_to_primary_clipboard",
        "yank_main_selection_to_primary_clipboard",
        "yank_main_selection_to_primary_clipboard",
    ),
    command_spec(
        "replace_selections_with_primary_clipboard",
        "replace_selections_with_primary_clipboard",
        "replace_selections_with_primary_clipboard",
    ),
];

#[derive(Clone, Copy)]
enum WindowLineTarget {
    Top,
    Center,
    Bottom,
}

mod buffers;
mod execute;
mod help;
mod registry;
mod settings;
mod workspace;

#[cfg(test)]
mod command_registry_tests;
#[cfg(test)]
mod tests;
struct AlignItCommandSpec {
    pattern: String,
    regex: bool,
    occurrence: i64,
    all: bool,
    format: String,
}

fn parse_align_it_spec(spec: &str) -> Result<AlignItCommandSpec, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("align_it: usage: :align_it [N|*|-N]<delimiter>|/regex/ [l1r1l0]".to_owned());
    }

    let (occurrence, all, rest) = parse_align_it_occurrence(spec)?;
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Err("align_it: usage: :align_it [N|*|-N]<delimiter>|/regex/ [l1r1l0]".to_owned());
    }

    let (pattern, regex, format) = if let Some(regex_body) = rest.strip_prefix('/') {
        let Some(end) = find_align_it_regex_end(regex_body) else {
            return Err("align_it: unterminated regex; use /.../".to_owned());
        };
        let pattern = &regex_body[..end];
        if pattern.is_empty() {
            return Err(
                "align_it: usage: :align_it [N|*|-N]<delimiter>|/regex/ [l1r1l0]".to_owned()
            );
        }
        regex::Regex::new(pattern)
            .map_err(|err| format!("align_it: invalid regex `{pattern}`: {err}"))?;
        let format = regex_body[end + 1..].trim();
        (pattern.to_owned(), true, format.to_owned())
    } else {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let pattern = parts.next().unwrap_or_default();
        if pattern.is_empty() {
            return Err(
                "align_it: usage: :align_it [N|*|-N]<delimiter>|/regex/ [l1r1l0]".to_owned()
            );
        }
        let format = parts.next().unwrap_or_default().trim().to_owned();
        (pattern.to_owned(), false, format)
    };

    if !format.is_empty() {
        validate_align_it_format(&format)?;
    }

    Ok(AlignItCommandSpec { pattern, regex, occurrence, all, format })
}

fn parse_align_it_occurrence(spec: &str) -> Result<(i64, bool, &str), String> {
    if let Some(rest) = spec.strip_prefix('*') {
        return Ok((1, true, rest));
    }

    if let Some(rest) = spec.strip_prefix('-') {
        let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
        if digits == 0 {
            return Ok((-1, false, rest));
        }
        let value: i64 = rest[..digits]
            .parse()
            .map_err(|_| "align_it: invalid occurrence selector".to_owned())?;
        if value == 0 {
            return Err("align_it: occurrence selector cannot be 0".to_owned());
        }
        return Ok((-value, false, &rest[digits..]));
    }

    let digits = spec.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 {
        return Ok((1, false, spec));
    }
    let value: i64 =
        spec[..digits].parse().map_err(|_| "align_it: invalid occurrence selector".to_owned())?;
    if value == 0 {
        return Err("align_it: occurrence selector cannot be 0".to_owned());
    }
    Ok((value, false, &spec[digits..]))
}

fn find_align_it_regex_end(spec: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, ch) in spec.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '/' => return Some(index),
            _ => {}
        }
    }
    None
}

fn validate_align_it_format(spec: &str) -> Result<(), String> {
    let bytes = spec.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'l' | b'r' | b'c' => {}
            _ => {
                return Err(format!(
                    "align_it: invalid format `{spec}`; use repeated l|r|c followed by digits"
                ));
            }
        }
        index += 1;
        let digit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if digit_start == index {
            return Err(format!(
                "align_it: invalid format `{spec}`; use repeated l|r|c followed by digits"
            ));
        }
    }
    Ok(())
}

fn humanize_command_summary(canonical_id: &str) -> String {
    if let Some(rest) = canonical_id.strip_prefix("goto_next_") {
        return format!("jump to next {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("goto_prev_") {
        return format!("jump to previous {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("goto_first_") {
        return format!("jump to first {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("goto_last_") {
        return format!("jump to last {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("goto_") {
        return format!("jump to {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("select_") {
        return format!("select {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("move_") {
        return format!("move {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("extend_") {
        return format!("extend {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("rotate_") {
        return format!("rotate {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("toggle_") {
        return format!("toggle {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("add_") {
        return format!("add {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("paste_") {
        return format!("paste {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("yank_") {
        return format!("yank {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("keep_") {
        return format!("keep {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("remove_") {
        return format!("remove {}", rest.replace('_', " "));
    }
    if let Some(rest) = canonical_id.strip_prefix("delete_") {
        return format!("delete {}", rest.replace('_', " "));
    }
    canonical_id.replace('_', " ")
}
