use super::*;
use std::borrow::Cow;
use std::path::{Component, Path};
use std::sync::OnceLock;

use crate::buffer::BufferId;
use crate::registers::RegisterName;

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

impl App {
    fn command_specs() -> &'static [CommandSpec] {
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

    fn ex_command_names() -> &'static [&'static str] {
        static COMMAND_NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
        COMMAND_NAMES
            .get_or_init(|| Self::command_specs().iter().map(|spec| spec.alias).collect())
            .as_slice()
    }

    fn resolve_ex_command(head: &str) -> Option<&'static CommandSpec> {
        Self::command_specs().iter().find(|spec| spec.alias == head)
    }

    fn canonical_command_spec(canonical_id: &str) -> &'static CommandSpec {
        Self::command_specs()
            .iter()
            .find(|spec| spec.canonical_id == canonical_id)
            .unwrap_or_else(|| panic!("missing canonical command spec for {canonical_id}"))
    }

    fn command_help_canonical_ids() -> &'static [&'static str] {
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

    fn ordered_aliases_for(canonical_id: &str) -> Vec<&'static str> {
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

    fn command_metadata(canonical_id: &str) -> CommandMetadata {
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

    fn format_command_help_item(canonical_id: &str) -> String {
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

    fn rewrite_command_alias(command: &str) -> Cow<'_, str> {
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

    const LSP_PLUGIN_NAME: &'static str = crate::config::LSP_PLUGIN_NAME;

    fn current_workspace_root(&self) -> PathBuf {
        self.backend
            .active()
            .path
            .as_deref()
            .and_then(|path| crate::config::find_git_root(path.parent().unwrap_or(path)))
            .or_else(|| {
                std::env::current_dir().ok().and_then(|cwd| crate::config::find_git_root(&cwd))
            })
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn current_picker_root(&self) -> Option<PathBuf> {
        self.backend.active().path.as_ref().and_then(|path| path.parent().map(Path::to_path_buf))
    }

    fn open_file_picker_at(&mut self, cwd: PathBuf, title: &str) {
        let mut picker = PickerState::new_files(cwd);
        picker.title = title.to_owned();
        self.open_picker(picker);
    }

    pub(crate) fn open_file_picker_for_buffer_directory(&mut self) {
        let cwd = self
            .current_picker_root()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Files");
    }

    pub(crate) fn open_file_picker_in_current_directory(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Files (cwd)");
    }

    pub(crate) fn open_file_explorer(&mut self) {
        self.open_file_picker_at(self.current_workspace_root(), "Explorer");
    }

    pub(crate) fn open_file_explorer_for_buffer_directory(&mut self) {
        let cwd = self
            .current_picker_root()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Explorer (buffer dir)");
    }

    pub(crate) fn open_file_explorer_in_current_directory(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Explorer (cwd)");
    }

    pub(crate) fn open_buffer_picker(&mut self) {
        let entries: Vec<_> = self
            .backend
            .all_bufs()
            .iter()
            .map(|buffer| (buffer.id, buffer.title(), buffer.path.clone()))
            .collect();
        self.open_picker(PickerState::new_buffers(entries));
    }

    fn open_location_picker(
        &mut self,
        title: &str,
        empty_message: &str,
        items: Vec<crate::picker::PickerItem>,
    ) {
        if items.is_empty() {
            self.backend.status_message = Some(empty_message.to_owned());
            return;
        }
        self.open_picker(PickerState::new_locations(title, items));
    }

    pub(crate) fn open_jump_list_picker(&mut self) {
        let items = self
            .jump_list
            .iter()
            .enumerate()
            .map(|(index, (line, col))| crate::picker::PickerItem {
                label: format!(
                    "{}:{}:{} {}",
                    index + 1,
                    line + 1,
                    col + 1,
                    self.backend
                        .active()
                        .get_line(*line)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .unwrap_or("<blank>")
                ),
                detail: None,
                path: None,
                buf_id: None,
                line: Some(*line),
                col: Some(*col),
                choice_index: None,
            })
            .collect();
        self.open_location_picker("Jumplist", "no jumplist entries", items);
    }

    pub(crate) fn open_changed_file_picker(&mut self) {
        let repo_root = self
            .backend
            .active()
            .path
            .as_deref()
            .and_then(|path| crate::config::find_git_root(path.parent().unwrap_or(path)))
            .or_else(|| {
                std::env::current_dir().ok().and_then(|cwd| crate::config::find_git_root(&cwd))
            });
        let Some(repo_root) = repo_root else {
            self.backend.status_message = Some(String::from("changed files: not inside git repo"));
            return;
        };
        match crate::git::changed_files(&repo_root) {
            Ok(files) => {
                let items = files
                    .into_iter()
                    .map(|path| {
                        let label = path
                            .strip_prefix(&repo_root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned();
                        crate::picker::PickerItem {
                            label,
                            detail: None,
                            path: Some(path),
                            buf_id: None,
                            line: None,
                            col: None,
                            choice_index: None,
                        }
                    })
                    .collect();
                self.open_location_picker("Changed Files", "no changed files", items);
            }
            Err(err) => {
                self.backend.status_message = Some(format!("changed files failed: {err}"));
            }
        }
    }

    pub(crate) fn open_diagnostics_picker(&mut self) {
        let active_id = self.backend.active().id;
        let items = self
            .active_diagnostic_items()
            .into_iter()
            .map(|(_, entry)| crate::picker::PickerItem {
                label: entry.display_label(),
                detail: entry.path.as_ref().map(|path| path.to_string_lossy().into_owned()),
                path: entry.path,
                buf_id: Some(active_id),
                line: Some(entry.line),
                col: Some(entry.col),
                choice_index: None,
            })
            .collect();
        self.open_location_picker("Diagnostics", "no diagnostics", items);
    }

    pub(crate) fn open_workspace_diagnostics_picker(&mut self) {
        let items = self
            .backend
            .all_bufs()
            .iter()
            .flat_map(|buffer| {
                let prefix = buffer
                    .path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| buffer.title());
                buffer.diagnostics.iter().map(move |diagnostic| {
                    // Whole-buffer policy-allowed: diagnostic offset→line/col requires full text mirror.
                    let (line, col) = line_col_for_offset(&buffer.lines, diagnostic.range.start);
                    crate::picker::PickerItem {
                        label: format!("{prefix}:{}: {}", line + 1, diagnostic.message),
                        detail: buffer
                            .path
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned()),
                        path: buffer.path.clone(),
                        buf_id: Some(buffer.id),
                        line: Some(line),
                        col: Some(col),
                        choice_index: None,
                    }
                })
            })
            .collect();
        self.open_location_picker("Workspace Diagnostics", "no workspace diagnostics", items);
    }

    pub(super) fn open_global_search(&mut self) {
        let cwd = self.current_workspace_root();
        let mut picker = PickerState::new_grep(String::new(), cwd);
        picker.title = String::from("Global Search");
        self.open_picker(picker);
        self.enter_normal_mode();
    }

    pub(super) fn open_command_palette(&mut self) {
        self.open_help_picker("Command Palette", Self::command_help_items());
    }

    pub(crate) fn reopen_last_picker(&mut self) {
        let Some(picker) = self.last_picker.clone() else {
            self.backend.status_message = Some(String::from("no previous picker"));
            return;
        };
        self.picker = Some(picker);
    }

    pub(super) fn execute_command(&mut self) {
        let _ = self.backend.sync_pending_events();
        self.handle_pending_ui_actions();
        self.handle_pending_locations();
        self.handle_pending_symbols();

        let raw = self.command_buffer.trim().to_owned();

        // Push non-empty commands to history (deduplicate consecutive duplicates).
        if !raw.is_empty() && self.command_history.last().map(|s| s.as_str()) != Some(&raw) {
            self.command_history.push(raw.clone());
            const HISTORY_MAX: usize = 100;
            if self.command_history.len() > HISTORY_MAX {
                self.command_history.remove(0);
            }
        }
        self.history_idx = None;

        // Parse an optional line-address range from the front of the command.
        let cursor_line = self.backend.cursor_line;
        let line_count = self.backend.line_count().max(1);
        let (range, rest) = parse_ex_range(&raw, cursor_line, line_count, &self.marks);
        let command = rest.trim_start();
        let command = Self::rewrite_command_alias(command);
        let command = command.as_ref();

        match crate::terminal::parse_command(command) {
            Ok(Some(shell_command)) => {
                self.run_terminal_command(shell_command);
                self.enter_normal_mode();
                return;
            }
            Err(message) => {
                self.backend.status_message = Some(message.to_owned());
                self.enter_normal_mode();
                return;
            }
            Ok(None) => {}
        }

        // Bare range (e.g. `:5`, `:.`, `:%`) with no following command → jump.
        if command.is_empty() {
            if let Some((start, _end)) = range {
                self.jump_to_line(start);
                self.enter_normal_mode();
                return;
            }
            self.enter_normal_mode();
            return;
        }

        let mut parts = command.split_whitespace();
        let head = parts.next().unwrap_or_default();
        let tail = command[head.len()..].trim_start();
        match head {
            "q" | "quit" => {
                if !self.backend.pristine {
                    self.backend.status_message =
                        Some("unsaved changes (use :w to save or :q! to force)".to_owned());
                    self.enter_normal_mode();
                    return;
                }
                self.should_quit = true;
            }
            "q!" | "quit!" => self.should_quit = true,
            "qa" | "quit_all" => {
                if self.backend.all_bufs().iter().any(|buf| !buf.pristine) {
                    self.backend.status_message =
                        Some("unsaved changes (use :wa to save or :qa! to force)".to_owned());
                    self.enter_normal_mode();
                    return;
                }
                self.should_quit = true;
            }
            "qa!" | "quit_all!" => self.should_quit = true,
            "w" | "write" | "w!" | "write!" => {
                if let Err(message) = self.save_current_buffer() {
                    self.backend.status_message = Some(message);
                }
            }
            "u" | "update" => {
                if !self.backend.active().pristine
                    && let Err(message) = self.save_current_buffer()
                {
                    self.backend.status_message = Some(message);
                }
            }
            "wq" | "x" | "wq!" | "x!" | "write_quit" | "write_quit!" => {
                if let Err(message) = self.save_current_buffer() {
                    self.backend.status_message = Some(message);
                } else {
                    self.should_quit = true;
                }
            }
            "wa" | "wa!" | "write_all" | "write_all!" => {
                if let Err(message) = self.save_all_dirty_buffers() {
                    self.backend.status_message = Some(message);
                }
            }
            "wqa" | "xa" | "wqa!" | "xa!" | "write_quit_all" | "write_quit_all!" => {
                if let Err(message) = self.save_all_dirty_buffers() {
                    self.backend.status_message = Some(message);
                } else {
                    self.should_quit = true;
                }
            }
            cmd if cmd == "s"
                || cmd == "substitute"
                || cmd.starts_with("s/")
                || cmd.starts_with("s!")
                || cmd.starts_with("s|")
                || cmd.starts_with("s,") =>
            {
                let body = if cmd == "s" || cmd == "substitute" {
                    let leftover = parts.collect::<Vec<_>>().join(" ");
                    if leftover.is_empty() {
                        self.backend.status_message =
                            Some("substitute: usage: s/pattern/replacement/[flags]".to_owned());
                        self.enter_normal_mode();
                        return;
                    }
                    leftover
                } else {
                    cmd[1..].to_owned()
                };
                let (start, end) = range.unwrap_or((cursor_line, cursor_line));
                match parse_substitute_cmd(&body) {
                    Some((pattern, replacement, flags)) => {
                        self.execute_substitute(start, end, &pattern, &replacement, &flags);
                    }
                    None => {
                        self.backend.status_message =
                            Some("substitute: usage: s/pattern/replacement/[flags]".to_owned());
                    }
                }
                self.enter_normal_mode();
                return;
            }
            "d" | "delete" => {
                let (start, end) = range.unwrap_or((cursor_line, cursor_line));
                self.delete_line_range(start, end);
            }
            "y" | "yank" => {
                let (start, end) = range.unwrap_or((cursor_line, cursor_line));
                self.yank_line_range(start, end);
            }
            "paste_clipboard_after" => {
                self.paste_from_register(RegisterName::Clipboard, false);
            }
            "paste_clipboard_before" => {
                self.paste_from_register(RegisterName::Clipboard, true);
            }
            "yank_to_clipboard" => {
                if let Some((start, end)) = range {
                    self.yank_line_range_into_register(start, end, RegisterName::Clipboard);
                } else {
                    self.yank_selection_to_register(RegisterName::Clipboard);
                }
            }
            "yank_main_selection_to_clipboard" => {
                self.yank_main_selection_to_register(RegisterName::Clipboard);
            }
            "replace_selections_with_clipboard" => {
                self.replace_selections_with_register(RegisterName::Clipboard);
            }
            "paste_primary_clipboard_after" => {
                self.paste_from_register(RegisterName::PrimaryClipboard, false);
            }
            "paste_primary_clipboard_before" => {
                self.paste_from_register(RegisterName::PrimaryClipboard, true);
            }
            "yank_to_primary_clipboard" => {
                if let Some((start, end)) = range {
                    self.yank_line_range_into_register(start, end, RegisterName::PrimaryClipboard);
                } else {
                    self.yank_selection_to_register(RegisterName::PrimaryClipboard);
                }
            }
            "yank_main_selection_to_primary_clipboard" => {
                self.yank_main_selection_to_register(RegisterName::PrimaryClipboard);
            }
            "replace_selections_with_primary_clipboard" => {
                self.replace_selections_with_register(RegisterName::PrimaryClipboard);
            }
            "format" => {
                if let Err(err) = self.backend.format_document() {
                    self.backend.status_message = Some(format!("format failed: {err}"));
                }
            }
            "complete" | "completion" => {
                if let Err(err) = self.backend.request_completion(None) {
                    self.backend.status_message = Some(format!("completion failed: {err}"));
                }
            }
            "definition" | "def" => {
                if let Err(err) = self.backend.request_definition() {
                    self.backend.status_message = Some(format!("definition failed: {err}"));
                }
            }
            "goto_declaration" => {
                if let Err(err) = self.backend.request_declaration() {
                    self.backend.status_message = Some(format!("declaration failed: {err}"));
                }
            }
            "goto_definition" => {
                if let Err(err) = self.backend.request_definition() {
                    self.backend.status_message = Some(format!("definition failed: {err}"));
                }
            }
            "goto_type_definition" => {
                if let Err(err) = self.backend.request_type_definition() {
                    self.backend.status_message = Some(format!("type definition failed: {err}"));
                }
            }
            "references" | "refs" => {
                if let Err(err) = self.backend.request_references() {
                    self.backend.status_message = Some(format!("references failed: {err}"));
                }
            }
            "goto_reference" | "select_references_to_symbol_under_cursor" => {
                if let Err(err) = self.backend.request_references() {
                    self.backend.status_message = Some(format!("references failed: {err}"));
                }
            }
            "goto_implementation" => {
                if let Err(err) = self.backend.request_implementation() {
                    self.backend.status_message = Some(format!("implementation failed: {err}"));
                }
            }
            "symbols" | "outline" => {
                if let Err(err) = self.backend.request_document_symbols() {
                    self.backend.status_message = Some(format!("symbols failed: {err}"));
                }
            }
            "symbol_picker" => {
                if let Err(err) = self.backend.request_document_symbols() {
                    self.backend.status_message = Some(format!("symbols failed: {err}"));
                }
            }
            "wsymbols" | "wsymbol" => {
                let query = tail.to_owned();
                if let Err(err) = self.backend.request_workspace_symbols(&query) {
                    self.backend.status_message = Some(format!("workspace symbols failed: {err}"));
                }
            }
            "workspace_symbol_picker" => {
                if let Err(err) = self.backend.request_workspace_symbols("") {
                    self.backend.status_message = Some(format!("workspace symbols failed: {err}"));
                }
            }
            "codeaction" | "codeactions" => {
                let action_index = parts.next().and_then(|part| part.parse::<usize>().ok());
                if let Err(err) = self.backend.request_code_actions(action_index) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            "code_action" => {
                if let Err(err) = self.backend.request_code_actions(None) {
                    self.backend.status_message = Some(format!("code action failed: {err}"));
                }
            }
            "goto_column" => {
                let Some(column) = parts.next().and_then(|part| part.parse::<usize>().ok()) else {
                    self.backend.status_message =
                        Some(String::from("goto_column: usage: :goto_column <column>"));
                    self.enter_normal_mode();
                    return;
                };
                self.goto_column(column.saturating_sub(1));
            }
            "goto_first_nonwhitespace" => {
                self.goto_first_nonwhitespace();
            }
            "goto_last_modification" => {
                self.change_list_older();
            }
            "goto_word" => {
                self.move_word_start(true, false);
            }
            "swift_motion" | "swift" => {
                self.start_swift_motion();
                return;
            }
            "goto_window_top" => {
                self.goto_window_top();
            }
            "goto_window_center" => {
                self.goto_window_center();
            }
            "goto_window_bottom" => {
                self.goto_window_bottom();
            }
            "goto_last_accessed_file" => {
                self.goto_last_accessed_file();
            }
            "goto_last_modified_file" => {
                self.goto_last_modified_file();
            }
            "goto_next_diag" => {
                self.goto_adjacent_diagnostic(true);
            }
            "goto_prev_diag" => {
                self.goto_adjacent_diagnostic(false);
            }
            "goto_first_diag" => {
                self.goto_edge_diagnostic(true);
            }
            "goto_last_diag" => {
                self.goto_edge_diagnostic(false);
            }
            "goto_next_function"
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
            | "goto_prev_paragraph" => {
                let _ = self.backend.send_edit(head, json!([]));
            }
            "goto_next_change" => {
                self.jump_to_git_hunk(true);
            }
            "goto_prev_change" => {
                self.jump_to_git_hunk(false);
            }
            "goto_first_change" => {
                self.jump_to_git_hunk_edge(true);
            }
            "goto_last_change" => {
                self.jump_to_git_hunk_edge(false);
            }
            "rename" => {
                let new_name = parts.collect::<Vec<_>>().join(" ");
                if new_name.is_empty() {
                    self.backend.status_message =
                        Some(String::from("rename: usage: :rename new_name"));
                } else if let Err(err) = self.backend.request_rename(&new_name) {
                    self.backend.status_message = Some(format!("rename failed: {err}"));
                }
            }
            "diagnostics" => {
                self.open_diagnostics_location_list();
            }
            "diagnostics_picker" => {
                self.open_diagnostics_picker();
                self.enter_normal_mode();
                return;
            }
            "workspace_diagnostics_picker" => {
                self.open_workspace_diagnostics_picker();
                self.enter_normal_mode();
                return;
            }
            "hover" => {
                let position = Some((self.backend.cursor_line, self.backend.cursor_col));
                if let Err(err) = self.backend.request_hover(position) {
                    self.backend.status_message = Some(format!("hover failed: {err}"));
                }
            }
            "insert_register" => {
                self.backend.status_message = Some(match self.insert_register_command(tail) {
                    Ok(message) => message,
                    Err(message) => message,
                });
            }
            "gblame" => {
                self.show_git_blame();
                self.enter_normal_mode();
                return;
            }
            "gdiff" => {
                self.open_git_diff_view(false);
                self.enter_normal_mode();
                return;
            }
            "ghunkdiff" => {
                self.open_git_diff_view(true);
                self.enter_normal_mode();
                return;
            }
            "expandtab" => {
                let _ = self.backend.send_edit(
                    "expand_tabs",
                    json!({ "range": range.map(|(start, end)| [start as i64, end as i64]) }),
                );
            }
            "reindent" => {
                let _ = self.backend.send_edit("reindent", json!([]));
            }
            "reflow" => {
                let Some(width) = parts
                    .next()
                    .and_then(|part| part.parse::<usize>().ok())
                    .filter(|width| *width > 0)
                else {
                    self.backend.status_message = Some("reflow: usage: :reflow <width>".to_owned());
                    return;
                };
                if parts.next().is_some() {
                    self.backend.status_message = Some("reflow: usage: :reflow <width>".to_owned());
                    return;
                }
                let _ = self.backend.send_edit(
                    "reflow_lines",
                    json!({
                        "width": width,
                        "range": range.map(|(start, end)| [start as i64, end as i64]),
                    }),
                );
            }
            "renormalize" => {
                let _ = self
                    .backend
                    .send_edit("normalize_line_endings", json!({ "line_ending": "\n" }));
            }
            "toggle_comments" => {
                let _ = self.backend.send_edit("toggle_comment", json!([]));
            }
            "toggle_line_comments" => {
                let _ = self.backend.send_edit("toggle_line_comment", json!([]));
            }
            "toggle_block_comments" => {
                let _ = self.backend.send_edit("toggle_block_comment", json!([]));
            }
            "help" => {
                self.open_help_picker("Help", Self::help_items());
                return;
            }
            "commands" => {
                self.open_help_picker("Commands", Self::command_help_items());
                return;
            }
            "keymap" => {
                self.open_help_picker("Keymap", Self::keymap_help_items());
                return;
            }
            "selection_for_find" => {
                let _ =
                    self.backend.send_edit("selection_for_find", json!({ "case_sensitive": true }));
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": true }));
            }
            "selection_for_replace" => {
                let _ = self.backend.send_edit("selection_for_replace", json!([]));
            }
            "transpose" => {
                let _ = self.backend.send_edit("transpose", json!([]));
            }
            "sort" => {
                let _ = self.backend.send_edit(
                    "sort_lines",
                    json!({
                        "descending": false,
                        "range": range.map(|(start, end)| [start as i64, end as i64]),
                    }),
                );
            }
            "rsort" => {
                let _ = self.backend.send_edit(
                    "sort_lines",
                    json!({
                        "descending": true,
                        "range": range.map(|(start, end)| [start as i64, end as i64]),
                    }),
                );
            }
            "uniq" | "dedup" => {
                let result = match range {
                    Some((start, end)) => self.dedup_line_range(start, end),
                    None => self.dedup_selected_or_all_lines(),
                };
                match result {
                    Ok(message) | Err(message) => self.backend.status_message = Some(message),
                }
            }
            "duplicate_line" => {
                let _ = self.backend.send_edit("duplicate_line", json!([]));
            }
            "increment" => {
                let _ = self.backend.send_edit("increase_number", json!([]));
            }
            "decrement" => {
                let _ = self.backend.send_edit("decrease_number", json!([]));
            }
            "multi_find" => {
                let terms = parts.collect::<Vec<_>>();
                if terms.is_empty() {
                    self.backend.status_message =
                        Some("multi_find: usage: :multi_find term [term ...]".to_owned());
                } else {
                    let queries = terms
                        .into_iter()
                        .enumerate()
                        .map(|(index, term)| {
                            json!({
                                "id": index,
                                "chars": term,
                                "case_sensitive": smart_case_sensitive(term),
                                "regex": false,
                                "whole_words": false,
                            })
                        })
                        .collect::<Vec<_>>();
                    let _ = self.backend.send_edit("multi_find", json!({ "queries": queries }));
                }
            }
            "selection_into_lines" | "split_selection" => {
                let _ = self.backend.send_edit("selection_into_lines", json!([]));
            }
            "split_selection_on_newline" => {
                let _ = self.backend.send_edit("selection_into_lines", json!([]));
            }
            "select_regex" => {
                let pattern = parts.collect::<Vec<_>>().join(" ");
                if pattern.is_empty() {
                    self.backend.status_message =
                        Some("select_regex: usage: :select_regex pattern".to_owned());
                } else {
                    let _ = self.backend.send_edit(
                        "select_regex",
                        json!({
                            "chars": pattern,
                            "case_sensitive": false,
                        }),
                    );
                }
            }
            "merge_selections" => {
                let _ = self.backend.send_edit("merge_selections", json!([]));
            }
            "merge_consecutive_selections" => {
                let _ = self.backend.send_edit("merge_consecutive_selections", json!([]));
            }
            "trim_selections" => {
                let _ = self.backend.send_edit("trim_selections", json!([]));
            }
            "align_selections" => {
                let _ = self.backend.send_edit("align_selections", json!([]));
            }
            "align_it" => {
                let spec = tail.trim();
                match parse_align_it_spec(spec) {
                    Ok(spec) => {
                        let params = json!({
                            "pattern": spec.pattern,
                            "regex": spec.regex,
                            "occurrence": spec.occurrence,
                            "all": spec.all,
                            "format": spec.format,
                            "range": range.map(|(start, end)| [start as i64, end as i64]),
                        });
                        let _ = self.backend.send_edit("align_it", params);
                    }
                    Err(message) => {
                        self.backend.status_message = Some(message);
                    }
                }
            }
            "collapse_selection" => {
                let _ = self.backend.send_edit("collapse_selections", json!([]));
            }
            "flip_selections" => {
                let _ = self.backend.send_edit("flip_selections", json!([]));
            }
            "ensure_selections_forward" => {
                let _ = self.backend.send_edit("ensure_selections_forward", json!([]));
            }
            "keep_primary_selection" => {
                let _ = self.backend.send_edit("keep_primary_selection", json!([]));
            }
            "remove_primary_selection" => {
                let _ = self.backend.send_edit("remove_primary_selection", json!([]));
            }
            "rotate_selections_backward" => {
                let _ = self.backend.send_edit("rotate_selections_backward", json!([]));
            }
            "rotate_selections_forward" => {
                let _ = self.backend.send_edit("rotate_selections_forward", json!([]));
            }
            "move_line_down" => {
                if let Err(message) = self.move_current_line_adjacent(true) {
                    self.backend.status_message = Some(message);
                }
            }
            "move_line_up" => {
                if let Err(message) = self.move_current_line_adjacent(false) {
                    self.backend.status_message = Some(message);
                }
            }
            "create_directory" => {
                if tail.is_empty() {
                    self.backend.status_message =
                        Some(String::from("create_directory: usage: :create_directory <path>"));
                } else {
                    self.backend.status_message = Some(
                        self.create_directory_in_workspace(tail).unwrap_or_else(|message| message),
                    );
                }
            }
            "match_brackets" => {
                let _ = self.backend.move_to_matching_bracket(false);
            }
            "surround_add" => {
                let Some(pair) = parts.next() else {
                    self.backend.status_message =
                        Some("surround_add: usage: :surround_add <pair> [textobject]".to_owned());
                    self.enter_normal_mode();
                    return;
                };
                let textobject = parts.next().and_then(|arg| arg.chars().next());
                if let Err(message) = self.surround_add(pair, textobject) {
                    self.backend.status_message = Some(message);
                }
            }
            "surround_replace" => {
                let Some(pair) = parts.next() else {
                    self.backend.status_message =
                        Some("surround_replace: usage: :surround_replace <pair>".to_owned());
                    self.enter_normal_mode();
                    return;
                };
                if let Err(message) = self.surround_replace(pair) {
                    self.backend.status_message = Some(message);
                }
            }
            "surround_delete" => {
                if let Err(message) = self.surround_delete() {
                    self.backend.status_message = Some(message);
                }
            }
            "select_textobject_around" => {
                let Some(spec) = parts.next().and_then(|arg| arg.chars().next()) else {
                    self.backend.status_message = Some(
                        "select_textobject_around: usage: :select_textobject_around <specifier>"
                            .to_owned(),
                    );
                    self.enter_normal_mode();
                    return;
                };
                if let Err(message) = self.select_text_object(true, spec) {
                    self.backend.status_message = Some(message);
                }
            }
            "select_textobject_inner" => {
                let Some(spec) = parts.next().and_then(|arg| arg.chars().next()) else {
                    self.backend.status_message = Some(
                        "select_textobject_inner: usage: :select_textobject_inner <specifier>"
                            .to_owned(),
                    );
                    self.enter_normal_mode();
                    return;
                };
                if let Err(message) = self.select_text_object(false, spec) {
                    self.backend.status_message = Some(message);
                }
            }
            "copy_selection_on_next_line" => {
                let _ = self.backend.send_edit("add_selection_below", json!([]));
            }
            "copy_selection_on_prev_line" => {
                let _ = self.backend.send_edit("add_selection_above", json!([]));
            }
            "rotate_selection_contents_backward" => {
                let _ = self.backend.send_edit("rotate_selection_contents_backward", json!([]));
            }
            "rotate_selection_contents_forward" => {
                let _ = self.backend.send_edit("rotate_selection_contents_forward", json!([]));
            }
            "reverse_selection_contents" => {
                let _ = self.backend.send_edit("reverse_selection_contents", json!([]));
            }
            "select_all" => {
                let _ = self.backend.send_edit("select_all", json!([]));
            }
            "delete_word_backward" => {
                let _ = self.backend.send_edit("delete_word_backward", json!([]));
            }
            "delete_word_forward" => {
                let _ = self.backend.send_edit("delete_word_forward", json!([]));
            }
            "kill_to_line_start" => {
                let _ = self.backend.send_edit("delete_to_beginning_of_line", json!([]));
            }
            "kill_to_line_end" => {
                let _ = self.backend.send_edit("delete_to_end_of_paragraph", json!([]));
            }
            "kill_line" => {
                self.delete_line_range(cursor_line, cursor_line);
            }
            "delete_char_backward" => {
                let _ = self.backend.send_edit("delete_backward", json!([]));
            }
            "delete_char_forward" => {
                if self.try_vlf_delete_forward(1) {
                    return;
                }
                let _ = self.backend.send_edit("delete_forward", json!([]));
            }
            "insert_newline" => {
                let _ = self.backend.send_edit("insert_newline", json!([]));
            }
            "add_newline_below" => {
                self.add_newline_below();
            }
            "add_newline_above" => {
                self.add_newline_above();
            }
            "extend_char_left" => {
                let _ = self.backend.send_edit("move_left_and_modify_selection", json!([]));
            }
            "extend_char_right" => {
                let _ = self.backend.send_edit("move_right_and_modify_selection", json!([]));
            }
            "extend_line_up" | "extend_visual_line_up" => {
                let _ = self.backend.send_edit("move_up_and_modify_selection", json!([]));
            }
            "extend_line_down" | "extend_visual_line_down" => {
                let _ = self.backend.send_edit("move_down_and_modify_selection", json!([]));
            }
            "extend_line_above" => {
                let _ = self.backend.send_edit("extend_line_above", json!([]));
            }
            "extend_line_below" => {
                self.extend_line_below();
            }
            "select_line_above" => {
                let _ = self.backend.send_edit("select_line_above", json!([]));
            }
            "select_line_below" => {
                let _ = self.backend.send_edit("select_line_below", json!([]));
            }
            "extend_to_line_bounds" => {
                self.extend_to_line_bounds();
            }
            "shrink_to_line_bounds" => {
                self.shrink_to_line_bounds();
            }
            "goto_file_end" => {
                let _ = self.backend.send_edit("move_to_end_of_document", json!([]));
            }
            "extend_to_file_start" => {
                let _ = self
                    .backend
                    .send_edit("move_to_beginning_of_document_and_modify_selection", json!([]));
            }
            "extend_to_file_end" => {
                let _ = self
                    .backend
                    .send_edit("move_to_end_of_document_and_modify_selection", json!([]));
            }
            "join_selections" => {
                self.join_selections(false);
            }
            "join_selections_space" => {
                self.join_selections(true);
            }
            "keep_selections" => {
                let pattern = parts.collect::<Vec<_>>().join(" ");
                if pattern.is_empty() {
                    self.filter_selections_from_search(false);
                } else {
                    self.filter_selections(&pattern, false);
                }
            }
            "remove_selections" => {
                let pattern = parts.collect::<Vec<_>>().join(" ");
                if pattern.is_empty() {
                    self.filter_selections_from_search(true);
                } else {
                    self.filter_selections(&pattern, true);
                }
            }
            "expand_selection" => {
                let _ = self.backend.send_edit("expand_selection", json!([]));
            }
            "shrink_selection" => {
                let _ = self.backend.send_edit("shrink_selection", json!([]));
            }
            "select_prev_sibling" => {
                let _ = self.backend.send_edit("select_prev_sibling", json!([]));
            }
            "select_next_sibling" => {
                let _ = self.backend.send_edit("select_next_sibling", json!([]));
            }
            "select_all_siblings" => {
                let _ = self.backend.send_edit("select_all_siblings", json!([]));
            }
            "select_all_children" => {
                let _ = self.backend.send_edit("select_all_children", json!([]));
            }
            "move_parent_node_start" => {
                let _ = self.backend.send_edit("move_parent_node_start", json!([]));
            }
            "move_parent_node_end" => {
                let _ = self.backend.send_edit("move_parent_node_end", json!([]));
            }
            "add_selection_above" => {
                let _ = self.backend.send_edit("add_selection_above", json!([]));
            }
            "add_selection_below" => {
                let _ = self.backend.send_edit("add_selection_below", json!([]));
            }
            "insert_tab" => {
                let _ = self.backend.send_edit("insert_tab", json!([]));
            }
            "e" | "edit" | "o" | "open" => {
                let path = parts.next().map(PathBuf::from);
                match self.backend.open_buffer(path) {
                    Ok(buf_id) => {
                        let _ = self.backend.switch_to_id(buf_id);
                        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                        self.viewport = Viewport::default();
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("open failed: {err}"));
                    }
                }
            }
            "e!" | "edit!" | "rl" | "reload" => {
                let id = self.backend.active().id;
                match self.backend.reload_buffer(id) {
                    Ok(()) => {
                        self.viewport = Viewport::default();
                    }
                    Err(err) => {
                        self.backend.status_message = Some(format!("reload failed: {err}"));
                    }
                }
            }
            "rla" | "reload_all" => {
                if let Err(message) = self.reload_all_buffers() {
                    self.backend.status_message = Some(message);
                } else {
                    self.viewport = Viewport::default();
                }
            }
            "new" | "n" => {
                if let Err(message) = self.open_scratch_buffer() {
                    self.backend.status_message = Some(message);
                }
            }
            "recover" => {
                let recovery_path = self
                    .backend
                    .active()
                    .path
                    .as_ref()
                    .and_then(|p| crate::buffer::recovery_file_path(p));
                match recovery_path {
                    Some(rp) if rp.exists() => match self.backend.open_buffer(Some(rp)) {
                        Ok(buf_id) => {
                            let _ = self.backend.switch_to_id(buf_id);
                            self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
                            self.viewport = Viewport::default();
                        }
                        Err(err) => {
                            self.backend.status_message = Some(format!("recover failed: {err}"));
                        }
                    },
                    Some(_) => {
                        self.backend.status_message = Some("no recovery file found".to_owned());
                    }
                    None => {
                        self.backend.status_message =
                            Some("current buffer has no backing file".to_owned());
                    }
                }
            }
            "set_language" | "lang" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    format!("language: {}", self.current_buffer_language())
                } else {
                    match self.set_current_buffer_language(tail) {
                        Ok(language) => format!("language: {language}"),
                        Err(message) => message,
                    }
                });
            }
            "reload_config" | "config_reload" => {
                self.backend.status_message = Some(match self.reload_runtime_config() {
                    Ok(message) => message,
                    Err(message) => message,
                });
            }
            "lsp_restart" => {
                self.backend.status_message =
                    Some(match self.backend.restart_plugin(Self::LSP_PLUGIN_NAME) {
                        Ok(()) => String::from("lsp restart requested"),
                        Err(err) => format!("lsp restart failed: {err}"),
                    });
            }
            "lsp_stop" => {
                self.backend.status_message =
                    Some(match self.backend.stop_plugin(Self::LSP_PLUGIN_NAME) {
                        Ok(()) => String::from("lsp stop requested"),
                        Err(err) => format!("lsp stop failed: {err}"),
                    });
            }
            "change_current_directory" | "cd" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("cd: usage: :cd path")
                } else {
                    match std::env::set_current_dir(PathBuf::from(tail)) {
                        Ok(()) => match std::env::current_dir() {
                            Ok(path) => format!("cwd: {}", path.display()),
                            Err(err) => format!("cd failed: {err}"),
                        },
                        Err(err) => format!("cd failed: {err}"),
                    }
                });
            }
            "show_directory" | "pwd" => {
                self.backend.status_message = Some(match std::env::current_dir() {
                    Ok(path) => format!("cwd: {}", path.display()),
                    Err(err) => format!("pwd failed: {err}"),
                });
            }
            "pipe" | "|" | "shell_pipe" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    format!("{head}: usage: :{head} shell-command")
                } else {
                    match self.run_shell_command_on_selections(tail, ShellSelectionMode::Replace) {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "pipe_to" | "shell_pipe_to" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    format!("{head}: usage: :{head} shell-command")
                } else {
                    match self
                        .run_shell_command_on_selections(tail, ShellSelectionMode::IgnoreOutput)
                    {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "shell_insert_output" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("shell_insert_output: usage: :shell_insert_output shell-command")
                } else {
                    match self
                        .run_shell_command_on_selections(tail, ShellSelectionMode::InsertBefore)
                    {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "shell_append_output" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("shell_append_output: usage: :shell_append_output shell-command")
                } else {
                    match self
                        .run_shell_command_on_selections(tail, ShellSelectionMode::InsertAfter)
                    {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "shell_keep_pipe" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("shell_keep_pipe: usage: :shell_keep_pipe shell-command")
                } else {
                    match self
                        .run_shell_command_on_selections(tail, ShellSelectionMode::KeepByStatus)
                    {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "reset_diff_change" | "diffget" | "diffg" => {
                self.backend.status_message = Some(match self.restore_git_hunk() {
                    Ok(message) => message,
                    Err(message) => message,
                });
            }
            "read" | "r" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("read: usage: :read path")
                } else {
                    match self.read_file_into_buffer(tail) {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "move" | "mv" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    String::from("move: usage: :move path")
                } else {
                    match self.move_current_buffer(tail) {
                        Ok(message) => message,
                        Err(message) => message,
                    }
                });
            }
            "echo" => {
                self.backend.status_message = Some(tail.to_owned());
            }
            "encoding" => {
                self.backend.status_message = Some(if tail.is_empty() {
                    format!("encoding: {}", self.config.charset)
                } else {
                    self.config.charset = tail.to_owned();
                    format!("encoding: {}", self.config.charset)
                });
            }
            "clear_register" => {
                self.backend.status_message = Some(match self.clear_register_command(tail) {
                    Ok(message) => message,
                    Err(message) => message,
                });
            }
            "redraw" => {
                self.redraw_requested = true;
                self.backend.status_message = Some(String::from("redraw"));
            }
            "recoverdel" => {
                let recovery_path = self
                    .backend
                    .active()
                    .path
                    .as_ref()
                    .and_then(|p| crate::buffer::recovery_file_path(p));
                match recovery_path {
                    Some(rp) if rp.exists() => match std::fs::remove_file(&rp) {
                        Ok(()) => {
                            self.backend.status_message = Some(format!("deleted {}", rp.display()));
                        }
                        Err(err) => {
                            self.backend.status_message = Some(format!("recoverdel failed: {err}"));
                        }
                    },
                    _ => {
                        self.backend.status_message = Some("no recovery file found".to_owned());
                    }
                }
            }
            "copen" | "cope" => {
                self.quickfix_open = true;
                if self.quickfix.as_ref().is_some_and(|q| !q.is_empty()) {
                    self.quickfix_focused = true;
                }
            }
            "cclose" | "ccl" => {
                self.quickfix_open = false;
                self.quickfix_focused = false;
            }
            "cn" | "cnext" => self.qf_next(true),
            "cp" | "cprev" | "cprevious" => self.qf_prev(true),
            "cfirst" => {
                if let Some(qf) = self.quickfix.as_mut() {
                    let entry = qf.first_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "clast" => {
                if let Some(qf) = self.quickfix.as_mut() {
                    let entry = qf.last_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "cc" => {
                let n = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                if let Some(qf) = self.quickfix.as_mut() {
                    let entry = qf.select_one_based(n).cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "clist" | "cl" => {
                let msg = match &self.quickfix {
                    None => "no quickfix list".to_owned(),
                    Some(qf) if qf.is_empty() => "quickfix list is empty".to_owned(),
                    Some(qf) => qf
                        .entries
                        .iter()
                        .enumerate()
                        .map(|(i, e)| {
                            let marker = if i == qf.selected { ">" } else { " " };
                            format!("{marker}{}: {}", i + 1, e.display_label())
                        })
                        .collect::<Vec<_>>()
                        .join("  "),
                };
                self.backend.status_message = Some(msg);
            }
            "lopen" | "lop" => {
                self.location_list_open = true;
                if self.location_list.as_ref().is_some_and(|l| !l.is_empty()) {
                    self.location_list_focused = true;
                }
            }
            "lclose" | "lcl" => {
                self.location_list_open = false;
                self.location_list_focused = false;
            }
            "lnext" | "ln" => self.qf_next(false),
            "lprev" | "lp" | "lprevious" => self.qf_prev(false),
            "lfirst" => {
                if let Some(ll) = self.location_list.as_mut() {
                    let entry = ll.first_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "llast" => {
                if let Some(ll) = self.location_list.as_mut() {
                    let entry = ll.last_entry().cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "ll" => {
                let n = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                if let Some(ll) = self.location_list.as_mut() {
                    let entry = ll.select_one_based(n).cloned();
                    if let Some(e) = entry {
                        self.navigate_to_qf_entry(e);
                    }
                }
            }
            "bn" | "bnext" | "goto_next_buffer" => self.cycle_buffer_command(true),
            "bp" | "bprev" | "bprevious" | "goto_previous_buffer" => {
                self.cycle_buffer_command(false);
            }
            "b#" => match self.backend.switch_alternate() {
                Ok(()) => {
                    let new = self.backend.active().id;
                    self.tabs.focused_windows_mut().set_focused_buffer(new);
                    self.viewport = Viewport::default();
                }
                Err(err) => {
                    self.backend.status_message = Some(format!("{err}"));
                }
            },
            "bd" | "bdelete" | "bc" | "bclose" | "buffer_close" => {
                let id = self.backend.active().id;
                if let Err(message) = self.close_buffers(
                    &[id],
                    false,
                    "unsaved changes (use :write to save or :bc! to force)",
                ) {
                    self.backend.status_message = Some(message);
                }
            }
            "bc!" | "bclose!" | "buffer_close!" => {
                let id = self.backend.active().id;
                if let Err(message) = self.close_buffers(&[id], true, "") {
                    self.backend.status_message = Some(message);
                }
            }
            "buffer_close_others" | "bco" | "bcloseother" => {
                let active_id = self.backend.active().id;
                let ids = self
                    .backend
                    .all_bufs()
                    .iter()
                    .map(|buf| buf.id)
                    .filter(|id| *id != active_id)
                    .collect::<Vec<_>>();
                if let Err(message) = self.close_buffers(
                    &ids,
                    false,
                    "unsaved changes (use :wa to save or :bco! to force)",
                ) {
                    self.backend.status_message = Some(message);
                }
            }
            "bco!" | "bcloseother!" | "buffer_close_others!" => {
                let active_id = self.backend.active().id;
                let ids = self
                    .backend
                    .all_bufs()
                    .iter()
                    .map(|buf| buf.id)
                    .filter(|id| *id != active_id)
                    .collect::<Vec<_>>();
                if let Err(message) = self.close_buffers(&ids, true, "") {
                    self.backend.status_message = Some(message);
                }
            }
            "buffer_close_all" | "bca" | "bcloseall" => {
                if let Err(message) = self.close_all_buffers(false) {
                    self.backend.status_message = Some(message);
                }
            }
            "bca!" | "bcloseall!" | "buffer_close_all!" => {
                if let Err(message) = self.close_all_buffers(true) {
                    self.backend.status_message = Some(message);
                }
            }
            "ls" | "buffers" => {
                let list = self.backend.list_buffers_str();
                self.backend.status_message = Some(list);
            }
            "sp" | "split" | "hs" | "hsplit" => {
                let path = parts.next().map(PathBuf::from);
                let buf_id = if let Some(p) = path {
                    match self.backend.open_buffer(Some(p)) {
                        Ok(id) => id,
                        Err(err) => {
                            self.backend.status_message = Some(format!("open failed: {err}"));
                            self.enter_normal_mode();
                            return;
                        }
                    }
                } else {
                    self.backend.active().id
                };
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Horizontal,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
                let _ = self.backend.switch_to_id(buf_id);
            }
            "vs" | "vsplit" => {
                let path = parts.next().map(PathBuf::from);
                let buf_id = if let Some(p) = path {
                    match self.backend.open_buffer(Some(p)) {
                        Ok(id) => id,
                        Err(err) => {
                            self.backend.status_message = Some(format!("open failed: {err}"));
                            self.enter_normal_mode();
                            return;
                        }
                    }
                } else {
                    self.backend.active().id
                };
                let (_, new_vp) = self.tabs.focused_windows_mut().split(
                    SplitDir::Vertical,
                    buf_id,
                    self.viewport,
                );
                self.viewport = new_vp;
                let _ = self.backend.switch_to_id(buf_id);
            }
            "goto" | "g" => {
                let Some(line) = parts.next().and_then(|part| part.parse::<usize>().ok()) else {
                    self.backend.status_message = Some("goto: usage: :goto line-number".to_owned());
                    self.enter_normal_mode();
                    return;
                };
                self.jump_to_line(line.saturating_sub(1));
            }
            "tabnew" | "tabe" | "tabedit" => {
                let path = parts.next().map(PathBuf::from);
                let buf_id = match self.backend.open_buffer(path) {
                    Ok(id) => id,
                    Err(err) => {
                        self.backend.status_message = Some(format!("open failed: {err}"));
                        self.enter_normal_mode();
                        return;
                    }
                };
                let new_vp = self.tabs.new_tab(buf_id, self.viewport);
                self.viewport = new_vp;
                let _ = self.backend.switch_to_id(buf_id);
            }
            "tabc" | "tabclose" => {
                if let Some(new_vp) = self.tabs.close_tab(self.viewport) {
                    self.viewport = new_vp;
                    let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                    let _ = self.backend.switch_to_id(new_buf);
                } else {
                    self.should_quit = true;
                }
            }
            "tabn" | "tabnext" => {
                let new_vp = self.tabs.focus_next(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            "tabp" | "tabprev" | "tabprevious" => {
                let new_vp = self.tabs.focus_prev(self.viewport);
                self.viewport = new_vp;
                let new_buf = self.tabs.focused_windows().focused_window().buffer_id;
                let _ = self.backend.switch_to_id(new_buf);
            }
            "rotate_view" | "cycle_view" => {
                self.rotate_view();
            }
            "rotate_view_reverse" => {
                self.rotate_view_reverse();
            }
            "transpose_view" => {
                self.transpose_view();
            }
            "wclose" => {
                self.close_view();
            }
            "wonly" => {
                self.close_other_views();
            }
            "jump_view_left" => {
                self.jump_view(crate::window::ViewDirection::Left);
            }
            "jump_view_down" => {
                self.jump_view(crate::window::ViewDirection::Down);
            }
            "jump_view_up" => {
                self.jump_view(crate::window::ViewDirection::Up);
            }
            "jump_view_right" => {
                self.jump_view(crate::window::ViewDirection::Right);
            }
            "swap_view_left" => {
                self.swap_view(crate::window::ViewDirection::Left);
            }
            "swap_view_down" => {
                self.swap_view(crate::window::ViewDirection::Down);
            }
            "swap_view_up" => {
                self.swap_view(crate::window::ViewDirection::Up);
            }
            "swap_view_right" => {
                self.swap_view(crate::window::ViewDirection::Right);
            }
            "commit_undo_checkpoint" => {
                let _ = self.backend.send_edit("commit_undo_checkpoint", json!([]));
            }
            "tabs" => {
                let info = (0..self.tabs.tab_count())
                    .map(|i| {
                        let marker = if i == self.tabs.focused_idx() { '>' } else { ' ' };
                        format!("{marker} Tab {}", i + 1)
                    })
                    .collect::<Vec<_>>()
                    .join("  ");
                self.backend.status_message = Some(info);
            }
            "files" => {
                self.open_file_picker_in_current_directory();
                self.enter_normal_mode();
                return;
            }
            "file_explorer" => {
                self.open_file_explorer();
                self.enter_normal_mode();
                return;
            }
            "file_explorer_in_current_buffer_directory" => {
                self.open_file_explorer_for_buffer_directory();
                self.enter_normal_mode();
                return;
            }
            "file_explorer_in_current_directory" => {
                self.open_file_explorer_in_current_directory();
                self.enter_normal_mode();
                return;
            }
            "file_picker" => {
                self.open_file_picker_for_buffer_directory();
                self.enter_normal_mode();
                return;
            }
            "file_picker_in_current_directory" => {
                self.open_file_picker_in_current_directory();
                self.enter_normal_mode();
                return;
            }
            "bpick" => {
                self.open_buffer_picker();
                self.enter_normal_mode();
                return;
            }
            "buffer_picker" => {
                self.open_buffer_picker();
                self.enter_normal_mode();
                return;
            }
            "jumplist_picker" => {
                self.open_jump_list_picker();
                self.enter_normal_mode();
                return;
            }
            "changed_file_picker" => {
                self.open_changed_file_picker();
                self.enter_normal_mode();
                return;
            }
            "last_picker" => {
                self.reopen_last_picker();
                self.enter_normal_mode();
                return;
            }
            "global_search" => {
                self.open_global_search();
                return;
            }
            "command_palette" => {
                self.open_command_palette();
                return;
            }
            "grep" => {
                let query = parts.collect::<Vec<_>>().join(" ");
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                self.open_picker(PickerState::new_grep(query, cwd));
                self.enter_normal_mode();
                return;
            }
            "set" => {
                let opt = parts.next().unwrap_or_default();
                self.apply_set_option(opt);
            }
            "noh" | "nohlsearch" => {
                self.search_pattern = None;
                let _ = self.backend.send_edit("highlight_find", json!({ "visible": false }));
                self.backend.status_message = Some("search highlight cleared".to_owned());
            }
            other if !other.is_empty() => {
                self.backend.status_message = Some(format!("unknown command: {other}"));
            }
            _ => {}
        }
        self.enter_normal_mode();
    }

    fn apply_set_option(&mut self, opt: &str) {
        use crate::config::{NumberStyle, StatuslineFormat};

        if let Some((key, val)) = opt.split_once('=') {
            match key {
                "scrolloff" | "so" => {
                    if let Ok(n) = val.parse::<usize>() {
                        self.config.scroll_offset = n;
                    }
                }
                "colorcolumn" | "cc" => {
                    self.config.color_column = val.parse::<usize>().ok().filter(|&n| n > 0);
                }
                "statusline" | "stl" => match val {
                    "default" => self.config.statusline_format = StatuslineFormat::Default,
                    "minimal" => self.config.statusline_format = StatuslineFormat::Minimal,
                    _ => {}
                },
                "number" | "nu" | "nonu" | "nonumber" => {}
                _ => {
                    self.backend.status_message = Some(format!("unknown option: {key}"));
                    return;
                }
            }
        } else {
            match opt {
                "number" | "nu" => self.config.number_style = NumberStyle::Absolute,
                "nonumber" | "nonu" => self.config.number_style = NumberStyle::Absolute,
                "relativenumber" | "rnu" => {
                    self.config.number_style = NumberStyle::Relative;
                }
                "norelativenumber" | "nornu" => {
                    self.config.number_style = NumberStyle::Absolute;
                }
                "relativenumberabsolute" | "rnua" => {
                    self.config.number_style = NumberStyle::RelativeAbsolute;
                }
                "wrap" => self.config.wrap_lines = true,
                "nowrap" => self.config.wrap_lines = false,
                "cursorline" | "cul" => self.config.cursor_line = true,
                "nocursorline" | "nocul" => self.config.cursor_line = false,
                "list" => self.config.show_visible_whitespace = true,
                "nolist" => self.config.show_visible_whitespace = false,
                "signcolumn" | "smc" => self.config.sign_column = true,
                "nosigncolumn" | "nosmc" => self.config.sign_column = false,
                other => {
                    self.backend.status_message = Some(format!("unknown option: {other}"));
                    return;
                }
            }
        }
        self.backend.status_message = Some(format!("set: {opt}"));
    }

    pub(super) fn history_older(&mut self) {
        if self.command_history.is_empty() {
            return;
        }
        let new_idx = match self.history_idx {
            None => {
                self.history_draft = self.command_buffer.clone();
                self.command_history.len().saturating_sub(1)
            }
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
        };
        self.history_idx = Some(new_idx);
        self.command_buffer = self.command_history[new_idx].clone();
    }

    pub(super) fn history_newer(&mut self) {
        let Some(idx) = self.history_idx else { return };
        if idx + 1 >= self.command_history.len() {
            self.history_idx = None;
            self.command_buffer = self.history_draft.clone();
        } else {
            let new_idx = idx + 1;
            self.history_idx = Some(new_idx);
            self.command_buffer = self.command_history[new_idx].clone();
        }
    }

    pub(super) fn complete_command(&mut self) {
        let prefix = self.command_buffer.clone();
        let candidates: Vec<&&str> =
            Self::ex_command_names().iter().filter(|c| c.starts_with(&*prefix)).collect();
        if let Some(&&first) = candidates.first() {
            self.command_buffer = first.to_owned();
        }
    }

    fn open_help_picker(&mut self, title: &str, items: Vec<String>) {
        self.open_picker(PickerState::new_help(title, items));
        self.enter_normal_mode();
    }

    pub(super) fn current_buffer_language(&self) -> String {
        let buf = self.backend.active();
        self.syntax_overrides
            .get(&buf.id)
            .cloned()
            .or_else(|| {
                buf.path
                    .as_deref()
                    .and_then(xi_core_lib::tree_sitter_support::language_name_for_path)
            })
            .or_else(|| self.highlighter.syntax_name_for_path(buf.path.as_deref()))
            .unwrap_or_else(|| String::from("Plain Text"))
    }

    fn cycle_buffer_command(&mut self, forward: bool) {
        let old = self.backend.active().id;
        if forward {
            self.backend.next_buffer();
        } else {
            self.backend.prev_buffer();
        }
        let new = self.backend.active().id;
        if old != new {
            self.tabs.focused_windows_mut().set_focused_buffer(new);
            self.viewport = Viewport::default();
        }
    }

    pub(super) fn goto_last_accessed_file(&mut self) {
        match self.backend.switch_last_accessed() {
            Ok(()) => {
                let new = self.backend.active().id;
                self.tabs.focused_windows_mut().set_focused_buffer(new);
                self.viewport = Viewport::default();
            }
            Err(err) => {
                self.backend.status_message = Some(err.to_string());
            }
        }
    }

    pub(super) fn goto_last_modified_file(&mut self) {
        match self.backend.switch_last_modified() {
            Ok(()) => {
                let new = self.backend.active().id;
                self.tabs.focused_windows_mut().set_focused_buffer(new);
                self.viewport = Viewport::default();
            }
            Err(err) => {
                self.backend.status_message = Some(err.to_string());
            }
        }
    }

    pub(super) fn goto_window_top(&mut self) {
        self.goto_window_line(WindowLineTarget::Top);
    }

    pub(super) fn goto_window_center(&mut self) {
        self.goto_window_line(WindowLineTarget::Center);
    }

    pub(super) fn goto_window_bottom(&mut self) {
        self.goto_window_line(WindowLineTarget::Bottom);
    }

    fn goto_window_line(&mut self, target: WindowLineTarget) {
        let total_lines = self.backend.line_count().max(1);
        let visible_height = self.last_editor_height.max(1);
        let count =
            usize::try_from(self.input_state.count()).unwrap_or(usize::MAX).saturating_sub(1);
        let scrolloff = self.config.scroll_offset.min(visible_height.saturating_sub(1) / 2);
        let last_visible_line = visible_height.saturating_sub(1);
        let target_line = match target {
            WindowLineTarget::Top => self.viewport.top_line + scrolloff + count,
            WindowLineTarget::Center => self.viewport.top_line + (last_visible_line / 2),
            WindowLineTarget::Bottom => {
                self.viewport.top_line + last_visible_line.saturating_sub(scrolloff + count)
            }
        }
        .min(total_lines.saturating_sub(1));
        self.push_jump();
        self.move_cursor_to(target_line, 0);
    }

    fn active_diagnostic_items(&self) -> Vec<(usize, QfEntry)> {
        let buf = self.backend.active();
        buf.diagnostics
            .iter()
            .map(|diagnostic| {
                // Whole-buffer policy-allowed: diagnostic offset→line/col requires full text mirror.
                let (line, col) = line_col_for_offset(&buf.lines, diagnostic.range.start);
                let severity = match diagnostic.severity {
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Error => "error",
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Warning => "warning",
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Information => "info",
                    xi_core_lib::plugin_rpc::DiagnosticSeverity::Hint => "hint",
                };
                (
                    diagnostic.range.start,
                    QfEntry {
                        path: buf.path.clone(),
                        line,
                        col,
                        message: format!("[{severity}] {}", diagnostic.message),
                    },
                )
            })
            .collect()
    }

    pub(super) fn populate_diagnostics_location_list(&mut self, selected: usize) -> bool {
        let items = self.active_diagnostic_items();
        if items.is_empty() {
            self.backend.status_message = Some(String::from("no diagnostics"));
            return false;
        }
        let entries = items.into_iter().map(|(_, entry)| entry).collect::<Vec<_>>();
        let mut list = QfList::new("Diagnostics", entries);
        let _ = list.select_one_based(selected + 1);
        self.location_list = Some(list);
        true
    }

    pub(super) fn active_cursor_offset(&self) -> usize {
        let buf = self.backend.active();
        let line = self.backend.cursor_line.min(buf.line_count().saturating_sub(1));
        // Bounded: reads only up to cursor line, not full buffer.
        let prefix = buf.line_start_offset(line).unwrap_or(0);
        let col = buf.get_line(line).map(|l| self.backend.cursor_col.min(l.len())).unwrap_or(0);
        prefix + col
    }

    fn goto_adjacent_diagnostic(&mut self, forward: bool) {
        let items = self.active_diagnostic_items();
        if items.is_empty() {
            self.backend.status_message = Some(String::from("no diagnostics"));
            return;
        }

        let cursor_offset = self.active_cursor_offset();
        let target = if forward {
            items.iter().position(|(start, _)| *start > cursor_offset)
        } else {
            items.iter().rposition(|(start, _)| *start < cursor_offset)
        };

        let Some(selected) = target else {
            self.backend.status_message = Some(if forward {
                String::from("no next diagnostic")
            } else {
                String::from("no previous diagnostic")
            });
            return;
        };

        let entry = items[selected].1.clone();
        let _ = self.populate_diagnostics_location_list(selected);
        self.move_cursor_to(entry.line, entry.col);
    }

    fn goto_edge_diagnostic(&mut self, first: bool) {
        let items = self.active_diagnostic_items();
        if items.is_empty() {
            self.backend.status_message = Some(String::from("no diagnostics"));
            return;
        }

        let selected = if first { 0 } else { items.len().saturating_sub(1) };
        let entry = items[selected].1.clone();
        let _ = self.populate_diagnostics_location_list(selected);
        self.move_cursor_to(entry.line, entry.col);
    }

    fn set_current_buffer_language(&mut self, requested: &str) -> Result<String, String> {
        let language = xi_core_lib::tree_sitter_support::canonical_language_name(requested)
            .ok_or_else(|| format!("set_language: unknown language `{requested}`"))?;
        self.syntax_overrides.insert(self.backend.active().id, language.clone());
        Ok(language)
    }

    fn reload_runtime_config(&mut self) -> Result<String, String> {
        let active_path = self.backend.active().path.clone();
        self.config = crate::config::load_config(active_path.as_deref());
        self.key_bindings = crate::keymap::bindings_for(&self.config.keymap);
        self.key_sequences = crate::keymap::sequence_bindings_for(&self.config.keymap);
        self.backend
            .reload_editor_config()
            .map_err(|err| format!("config reload failed: {err}"))?;
        Ok(String::from("config reloaded"))
    }

    fn resolve_workspace_path(&self, target: &str) -> Result<PathBuf, String> {
        let target = target.trim();
        if target.is_empty() {
            return Err(String::from("path cannot be empty"));
        }

        let workspace_root = self.current_workspace_root();
        let relative = if Path::new(target).is_absolute() {
            Path::new(target).strip_prefix(&workspace_root).map_err(|_| {
                format!("path must stay under workspace {}", workspace_root.display())
            })?
        } else {
            Path::new(target)
        };

        let mut resolved = workspace_root.clone();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => resolved.push(part),
                Component::ParentDir => {
                    if resolved == workspace_root {
                        return Err(format!(
                            "path must stay under workspace {}",
                            workspace_root.display()
                        ));
                    }
                    resolved.pop();
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!(
                        "path must stay under workspace {}",
                        workspace_root.display()
                    ));
                }
            }
        }

        Ok(resolved)
    }

    fn create_directory_in_workspace(&mut self, target: &str) -> Result<String, String> {
        let workspace_root = self.current_workspace_root();
        let path = self
            .resolve_workspace_path(target)
            .map_err(|message| format!("create_directory: {message}"))?;

        if path.exists() && !path.is_dir() {
            return Err(format!(
                "create_directory failed: {} exists and is not a directory",
                path.display()
            ));
        }

        std::fs::create_dir_all(&path).map_err(|err| format!("create_directory failed: {err}"))?;
        let display = path.strip_prefix(&workspace_root).unwrap_or(&path);
        Ok(format!("created {}", display.display()))
    }

    fn read_file_into_buffer(&mut self, path: &str) -> Result<String, String> {
        let path = PathBuf::from(path);
        let content =
            std::fs::read_to_string(&path).map_err(|err| format!("read failed: {err}"))?;
        self.backend
            .send_edit("insert", json!({ "chars": content }))
            .map_err(|err| format!("read failed: {err}"))?;
        Ok(format!("read {}", path.display()))
    }

    fn move_current_buffer(&mut self, target: &str) -> Result<String, String> {
        let Some(source) = self.backend.active().path.clone() else {
            return Err(String::from("move: current buffer has no backing file"));
        };
        let target = PathBuf::from(target);
        if source == target {
            return Err(String::from("move: source and destination are the same"));
        }
        if target.exists() {
            return Err(format!("move failed: {} already exists", target.display()));
        }
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| format!("move failed: {err}"))?;
        }

        let buffer_id = self.backend.active().id;
        let pristine =
            self.backend.buffer_pristine(buffer_id).map_err(|err| format!("move failed: {err}"))?;
        if pristine {
            std::fs::rename(&source, &target).map_err(|err| format!("move failed: {err}"))?;
        }
        self.backend
            .set_buffer_path(buffer_id, target.clone())
            .map_err(|err| format!("move failed: {err}"))?;
        self.backend.save_buffer(buffer_id).map_err(|err| format!("move failed: {err}"))?;
        if !pristine {
            std::fs::remove_file(&source).map_err(|err| format!("move failed: {err}"))?;
        }
        Ok(format!("moved {} -> {}", source.display(), target.display()))
    }

    fn clear_register_command(&mut self, target: &str) -> Result<String, String> {
        let target = target.trim();
        if target.is_empty() {
            self.registers.clear(None);
            return Ok(String::from("registers cleared"));
        }
        let mut chars = target.chars();
        let Some(name) = chars.next() else {
            return Err(String::from("clear_register: usage: :clear_register [register]"));
        };
        if chars.next().is_some() {
            return Err(String::from("clear_register: usage: :clear_register [register]"));
        }
        let register = RegisterName::from_char(name)
            .ok_or_else(|| format!("clear_register: invalid register `{name}`"))?;
        self.registers.clear(Some(&register));
        Ok(format!("register {name} cleared"))
    }

    fn insert_register_command(&mut self, target: &str) -> Result<String, String> {
        let target = target.trim();
        let mut chars = target.chars();
        let Some(name) = chars.next() else {
            return Err(String::from("insert_register: usage: :insert_register <register>"));
        };
        if chars.next().is_some() {
            return Err(String::from("insert_register: usage: :insert_register <register>"));
        }
        let register = RegisterName::from_char(name)
            .ok_or_else(|| format!("insert_register: invalid register `{name}`"))?;
        let text = self.registers.get(&register);
        if text.is_empty() {
            return Ok(format!("register {name} empty"));
        }
        self.backend
            .send_edit("insert", json!({ "chars": text }))
            .map_err(|err| format!("insert_register failed: {err}"))?;
        Ok(format!("inserted register {name}"))
    }

    fn save_current_buffer(&mut self) -> Result<(), String> {
        self.backend.save().map_err(|err| format!("save failed: {err}"))
    }

    fn save_all_dirty_buffers(&mut self) -> Result<(), String> {
        use std::collections::HashSet;

        self.backend.flush_all_pending_edits().map_err(|err| format!("save failed: {err}"))?;
        let mut seen_paths = HashSet::new();
        let candidate_ids = self
            .backend
            .all_bufs()
            .iter()
            .rev()
            .filter_map(|buf| {
                let path = buf.path.as_ref()?;
                let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                seen_paths.insert(key).then_some(buf.id)
            })
            .collect::<Vec<_>>();
        for id in candidate_ids {
            if self.backend.buffer_pristine(id).map_err(|err| format!("save failed: {err}"))? {
                continue;
            }
            self.backend.save_buffer(id).map_err(|err| format!("save failed: {err}"))?;
        }
        Ok(())
    }

    fn reload_all_buffers(&mut self) -> Result<(), String> {
        let ids = self.backend.all_bufs().iter().map(|buf| buf.id).collect::<Vec<_>>();
        for id in ids {
            self.backend.reload_buffer(id).map_err(|err| format!("reload failed: {err}"))?;
        }
        Ok(())
    }

    fn open_scratch_buffer(&mut self) -> Result<(), String> {
        let buf_id = self.backend.open_buffer(None).map_err(|err| format!("open failed: {err}"))?;
        self.backend.switch_to_id(buf_id).map_err(|err| format!("open failed: {err}"))?;
        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
        self.viewport = Viewport::default();
        Ok(())
    }

    fn close_all_buffers(&mut self, force: bool) -> Result<(), String> {
        if !force && self.backend.all_bufs().iter().any(|buf| !buf.pristine) {
            return Err("unsaved changes (use :wa to save or :bca! to force)".to_owned());
        }

        let keep_id = if self.backend.buf_count() == 1 && self.backend.active().path.is_none() {
            self.backend.active().id
        } else {
            let buf_id =
                self.backend.open_buffer(None).map_err(|err| format!("open failed: {err}"))?;
            self.backend.switch_to_id(buf_id).map_err(|err| format!("open failed: {err}"))?;
            self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
            self.viewport = Viewport::default();
            buf_id
        };

        let ids = self
            .backend
            .all_bufs()
            .iter()
            .map(|buf| buf.id)
            .filter(|id| *id != keep_id)
            .collect::<Vec<_>>();
        self.close_buffers(&ids, true, "")
    }

    fn close_buffers(
        &mut self,
        ids: &[BufferId],
        force: bool,
        unsaved_message: &str,
    ) -> Result<(), String> {
        if !force
            && ids.iter().copied().any(|id| {
                self.backend
                    .all_bufs()
                    .iter()
                    .find(|buf| buf.id == id)
                    .is_some_and(|buf| !buf.pristine)
            })
        {
            return Err(unsaved_message.to_owned());
        }

        let active_id = self.backend.active().id;
        let closed_active = ids.contains(&active_id);
        for id in ids {
            if self.backend.all_bufs().iter().any(|buf| buf.id == *id) {
                self.backend.close_buffer(*id).map_err(|err| format!("close failed: {err}"))?;
            }
        }

        let fallback = self.backend.active().id;
        let valid_buffers = self
            .backend
            .all_bufs()
            .iter()
            .map(|buf| buf.id)
            .collect::<std::collections::HashSet<_>>();
        self.tabs.retarget_invalid_buffers(&valid_buffers, fallback);
        self.tabs.focused_windows_mut().set_focused_buffer(fallback);
        if closed_active {
            self.viewport = Viewport::default();
        }
        Ok(())
    }

    fn help_items() -> Vec<String> {
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

    fn command_help_items() -> Vec<String> {
        Self::command_help_canonical_ids()
            .iter()
            .copied()
            .map(Self::format_command_help_item)
            .collect()
    }

    #[cfg(test)]
    fn command_registry_aliases() -> Vec<&'static str> {
        Self::ex_command_names().to_vec()
    }

    #[cfg(test)]
    fn documented_command_aliases() -> Vec<String> {
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
    fn claimed_command_aliases() -> Vec<String> {
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

    fn command_brief(canonical_id: &str) -> String {
        let alias =
            Self::ordered_aliases_for(canonical_id).into_iter().next().unwrap_or(canonical_id);
        let spec = Self::canonical_command_spec(canonical_id);
        match spec.usage {
            Some(usage) => format!(":{alias} {usage} {}", spec.summary),
            None => format!(":{alias} {}", spec.summary),
        }
    }

    fn command_name(canonical_id: &str) -> String {
        let alias =
            Self::ordered_aliases_for(canonical_id).into_iter().next().unwrap_or(canonical_id);
        format!(":{alias}")
    }

    fn keymap_help_items() -> Vec<String> {
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

#[cfg(test)]
mod command_registry_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn command_registry_keeps_aliases_unique() {
        let aliases = App::command_registry_aliases();
        assert_eq!(aliases.len(), App::command_specs().len());
        let unique = aliases.iter().copied().collect::<HashSet<_>>();
        assert_eq!(aliases.len(), unique.len());
    }

    #[test]
    fn command_registry_resolves_aliases_to_canonical_dispatch() {
        let completion = App::resolve_ex_command("completion").unwrap();
        assert_eq!(completion.canonical_id, "complete");
        assert_eq!(completion.dispatch, "complete");

        let terminal = App::resolve_ex_command("terminal").unwrap();
        assert_eq!(terminal.canonical_id, "term");
        assert_eq!(terminal.dispatch, "term");

        let pipe = App::resolve_ex_command("|").unwrap();
        assert_eq!(pipe.canonical_id, "pipe");
        assert_eq!(pipe.dispatch, "pipe");
    }

    #[test]
    fn command_registry_rewrite_preserves_tail() {
        assert_eq!(
            App::rewrite_command_alias("terminal cargo test -p ee-cli").as_ref(),
            "term cargo test -p ee-cli"
        );
        assert_eq!(App::rewrite_command_alias("completion").as_ref(), "complete");
        assert_eq!(App::rewrite_command_alias("goto_next_function").as_ref(), "goto_next_function");
    }

    #[test]
    fn command_registry_preserves_existing_completion_order_for_prefixes() {
        let aliases = App::command_registry_aliases();
        let wr = aliases.iter().copied().find(|alias| alias.starts_with("wr")).unwrap();
        assert_eq!(wr, "write!");

        let bp = aliases.iter().copied().find(|alias| alias.starts_with("bp")).unwrap();
        assert_eq!(bp, "bp");

        let ed = aliases.iter().copied().find(|alias| alias.starts_with("ed")).unwrap();
        assert_eq!(ed, "edit");
    }

    #[test]
    fn command_help_items_document_phase_three_aliases() {
        let help = App::command_help_items().join("\n");
        assert!(help.contains(":config_reload"));
        assert!(help.contains(":bpick"));
        assert!(help.contains(":outline"));
        assert!(help.contains(":wsymbols"));
        assert!(help.contains(":set"));
    }

    #[test]
    fn completion_now_covers_new_registry_aliases() {
        let mut app = App::from_path(None).unwrap();

        app.command_buffer = String::from("conf");
        app.complete_command();
        assert_eq!(app.command_buffer, "config_reload");

        app.command_buffer = String::from("nohl");
        app.complete_command();
        assert_eq!(app.command_buffer, "nohlsearch");

        app.command_buffer = String::from("wsy");
        app.complete_command();
        assert_eq!(app.command_buffer, "wsymbol");
    }

    #[test]
    fn completable_aliases_all_resolve_through_registry() {
        let unresolved: Vec<_> = App::command_registry_aliases()
            .into_iter()
            .filter(|alias| App::resolve_ex_command(alias).is_none())
            .collect();
        assert!(unresolved.is_empty(), "completion aliases missing from registry: {unresolved:?}");
    }

    #[test]
    fn command_help_rows_only_claim_resolvable_aliases() {
        let unresolved: Vec<_> = App::claimed_command_aliases()
            .into_iter()
            .filter(|alias| App::resolve_ex_command(alias).is_none())
            .collect();
        assert!(unresolved.is_empty(), "help rows mention unknown aliases: {unresolved:?}");
    }

    #[test]
    fn command_help_rows_include_category_usage_and_grouped_aliases() {
        let tab_row = App::format_command_help_item("tab_edit");
        assert!(tab_row.starts_with("[windows] "));
        assert!(tab_row.contains(":tabnew / :tabe / :tabedit [path]"));

        let grep_row = App::format_command_help_item("grep");
        assert!(grep_row.starts_with("[buffers] "));
        assert!(grep_row.contains(":grep <query>"));

        let complete_row = App::format_command_help_item("complete");
        assert!(complete_row.contains(":complete / :completion"));

        let substitute_row = App::format_command_help_item("substitute");
        assert!(substitute_row.starts_with("[editing] "));
        assert!(substitute_row.contains(":s / :substitute s/pattern/replacement/[flags]"));
    }

    #[test]
    fn help_items_partially_reuse_registry_briefs() {
        let help = App::help_items();
        assert!(help[0].contains(":commands list ex commands and features"));
        assert!(help[0].contains(":keymap list high-value normal-mode bindings"));
        assert!(help[4].contains(":hover request LSP hover at cursor"));
        assert!(
            help[8].contains(":term <shell-command> run shell command and open transcript buffer")
        );
    }

    #[test]
    fn help_items_command_mentions_resolve_through_registry() {
        let help = App::help_items();
        let unresolved: Vec<_> = help
            .iter()
            .flat_map(|line| {
                line.split_whitespace().filter_map(|token| {
                    token
                        .trim_matches(|ch: char| ch == '|' || ch == ',')
                        .strip_prefix(':')
                        .filter(|alias| !alias.is_empty())
                })
            })
            .filter(|alias| App::resolve_ex_command(alias).is_none())
            .collect();
        assert!(unresolved.is_empty(), "general help mentions unknown aliases: {unresolved:?}");
    }
}

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

#[cfg(test)]
mod tests {
    use super::App;

    #[test]
    fn command_help_items_cover_all_ex_commands() {
        let help = App::command_help_items().join("\n");
        let missing: Vec<_> = App::ex_command_names()
            .iter()
            .copied()
            .filter(|command| !help.contains(&format!(":{command}")))
            .collect();
        assert!(missing.is_empty(), "missing commands from command palette: {missing:?}");
    }

    #[test]
    fn documented_aliases_are_completable() {
        let completable =
            App::command_registry_aliases().into_iter().collect::<std::collections::HashSet<_>>();
        let missing: Vec<_> = App::documented_command_aliases()
            .into_iter()
            .filter(|alias| !completable.contains(alias.as_str()))
            .collect();
        assert!(missing.is_empty(), "documented aliases missing from completion: {missing:?}");
    }
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
