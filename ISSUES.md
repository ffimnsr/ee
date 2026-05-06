# Issues

## New World

### Tooling and CI

- [ ] Add property-based tests (`proptest`) in `crates/xi-rope` for delta application, merging, and CRDT invariants.

### Optional Future Boundary Work

- [ ] Move text-object range resolution from `crates/ee-tui/src/app/mod.rs` into `xi-core-lib` if we want backend-owned semantic text objects across future frontends.
- [ ] Move visual-block delete/change/yank execution from `crates/ee-tui/src/app/mod.rs` into `xi-core-lib` so rectangular selection mutations become backend-owned editor semantics.
- [ ] Re-evaluate visual-block insert setup and replay split between `ee-tui` and `xi-core-lib`; keep frontend workflow glue only, move any remaining selection-truth or mutation semantics backend-side if reused by another frontend.


### Commands

Implement these commands, if there's already a command like this implemented but have
different name alias this. Description for the command can be found here: https://docs.helix-editor.com/commands.html. Don't copy the keybindings in the site, check only for description

- [x] select_regex
- [x] split_selection
- [x] split_selection_on_newline
- [x] merge_selections
- [x] merge_consecutive_selections
- [x] trim_selections
- [x] collapse_selection
- [x] flip_selections
- [x] ensure_selections_forward
- [x] keep_primary_selection
- [x] remove_primary_selection
- [x] copy_selection_on_next_line
- [x] copy_selection_on_prev_line
- [x] rotate_selection_contents_backward
- [x] rotate_selection_contents_forward
- [x] select_all
- [x] delete_word_backward
- [x] delete_word_forward
- [x] kill_to_line_start
- [x] kill_to_line_end
- [x] kill_line - remove the line
- [x] delete_char_backward
- [x] delete_char_forward
- [x] insert_newline
- [x] add_newline_below
- [x] add_newline_above
- [x] code_action
- [x] extend_line_below
- [x] extend_to_line_bounds
- [x] shrink_to_line_bounds
- [x] join_selections
- [x] join_selections_space
- [x] keep_selections
- [x] remove_selections
- [x] expand_selection
- [x] shrink_selection
- [x] select_prev_sibling
- [x] select_next_sibling
- [x] select_all_siblings
- [x] select_all_children
- [x] move_parent_node_end
- [x] move_parent_node_start
- [x] goto_column
- [x] goto_file_start
- [x] goto_last_line
- [x] goto_file
- [x] goto_line_start
- [x] goto_line_end

- [ ] align_selections
- [ ] rotate_selections_backward
- [ ] rotate_selections_forward
- move_line_down
- move_line_up
- match_brackets
- surround_add
- surround_replace
- surround_delete
- select_textobject_around
- select_textobject_inner

- goto_first_nonwhitespace
- goto_window_top
- goto_window_center
- goto_window_bottom
- goto_definition
- goto_type_definition
- goto_reference
- goto_implementation
- goto_last_accessed_file
- goto_last_modified_file
- goto_next_buffer
- goto_previous_buffer
- goto_last_modification
- goto_word
- goto_next_diag
- goto_prev_diag
- goto_last_diag
- goto_first_diag
- goto_next_function
- goto_prev_function
- goto_next_class
- goto_prev_class
- goto_next_parameter
- goto_prev_parameter
- goto_next_comment
- goto_prev_comment
- goto_next_test
- goto_prev_test
- goto_next_paragraph
- goto_prev_paragraph
- goto_next_change
- goto_prev_change
- goto_last_change
- goto_first_change
- rotate_view / cycle_view
- jump_view_left
- jump_view_down
- jump_view_up
- jump_view_right
- swap_view_left
- swap_view_down
- swap_view_up
- swap_view_right
- file_picker
- file_picker_in_current_directory
- buffer_picker
- jumplist_picker
- changed_file_picker
- symbol_picker
- workspace_symbol_picker
- diagnostics_picker
- workspace_diagnostics_picker
- rename_symbol
- select_references_to_symbol_under_cursor
- hover (static command)
- last_picker
- toggle_comments
- toggle_block_comments
- toggle_line_comments
- paste_clipboard_after
- paste_clipboard_before
- yank_to_clipboard
- yank_main_selection_to_clipboard
- replace_selections_with_clipboard
- global_search
- command_palette
- completion (static command)
- commit_undo_checkpoint
- insert_register

View description of commands here https://docs.helix-editor.com/commands.html

- [ ] lsp_restart
- [ ] lsp_stop
- [ ] pipe / |
- [ ] pipe_to
- [ ] reset_diff_change / diffget / diffg
- [ ] change_current_directory / cd and show_directory / pwd
- [ ] extend_char_left / extend_char_right
- [ ] extend_visual_line_up / extend_visual_line_down
- [ ] extend_line_up / extend_select_line_aboveline_down
- [ ] extend_line_above /  / select_line_below
- [ ] goto_file_end / extend_to_file_start / extend_to_file_end
- [ ] goto_declaration
- [ ] file_picker_in_current_buffer_directory
- [ ] file_explorer / file_explorer_in_current_buffer_directory / file_explorer_in_current_directory
- [ ] paste_primary_clipboard_after / paste_primary_clipboard_before
- [ ] yank_to_primary_clipboard / yank_main_selection_to_primary_clipboard
- [ ] replace_selections_with_primary_clipboard
- [ ] reverse_selection_contents
- [ ] rotate_view_reverse / transpose_view / wclose / wonly
- [ ] shell_pipe / shell_pipe_to / shell_insert_output / shell_append_output / shell_keep_pipe

### Already Implemented But Missing From Tracker

- [x] q / quit / q! / quit!
- [x] w / write / wq / x
- [x] d / delete
- [x] y / yank
- [x] e / edit / e! / edit!
- [x] format
- [x] complete
- [x] definition / def
- [x] references / refs
- [x] symbols / outline
- [x] wsymbols / wsymbol
- [x] codeaction / codeactions
- [x] rename
- [x] diagnostics
- [x] hover
- [x] help / commands / keymap
- [x] gblame / gdiff / ghunkdiff
- [x] reindent / transpose / duplicate_line
- [x] increment / decrement
- [x] selection_for_find / selection_for_replace
- [x] multi_find
- [x] insert_tab
- [x] files / Files / grep / Grep / bpick
- [x] recover / recoverdel
- [x] set / noh / nohlsearch
- [x] bn / bnext / bp / bprev / bprevious / b# / bd / bdelete
- [x] ls / buffers / Buffers
- [x] sp / split / vs / vsplit
- [x] tabnew / tabe / tabedit / tabc / tabclose / tabn / tabnext / tabp / tabprev / tabprevious / tabs
- [x] copen / cope / cclose / ccl / cn / cnext / cp / cprev / cprevious / cfirst / clast / cc / clist / cl
- [x] lopen / lop / lclose / lcl / lnext / ln / lprev / lp / lprevious / lfirst / llast / ll
- [x] search / reverse_search / rsearch / search_next / search_prev
- [x] search_selection / search_selection_detect_word_boundaries
- [x] goto_line / goto_line_start / goto_line_end
- [x] page_up / page_down / page_cursor_half_up / page_cursor_half_down
- [x] jump_forward / jump_backward / save_selection / repeat_last_motion
- [x] replace / replace_with_yanked
- [x] switch_case / switch_to_lowercase / switch_to_uppercase
- [x] insert_mode / append_mode / select_mode / visual_mode / command_mode
- [x] open_below / open_above / insert_at_line_start / insert_at_line_end
- [x] move_next_word_start / move_prev_word_start / move_next_word_end
- [x] move_next_long_word_start / move_prev_long_word_start / move_next_long_word_end
- [x] find_next_char / find_till_char / find_prev_char / till_prev_char
- [x] yank / paste_after / paste_before / indent / unindent / format_selections
- [x] delete_selection / delete_selection_noyank / change_selection / change_selection_noyank
- [x] select_register / undo / redo / earlier / later

### Missing From Helix Tracker

View description of commands here https://docs.helix-editor.com/commands.html

- [x] open / o alias for edit
- [x] hsplit typable aliases: hsplit / hs
- [x] new / n
- [x] write! / w!
- [x] write_quit alias family: write_quit / write_quit! / wq! / x!
- [x] write_all / wa and write_all! / wa!
- [x] write_quit_all / wqa / xa and write_quit_all! / wqa! / xa!
- [x] quit_all / qa and quit_all! / qa!
- [x] buffer_close aliases: buffer_close / bc / bclose and buffer_close! / bc! / bclose!
- [x] buffer_close_others aliases: buffer_close_others / bco / bcloseother and force variants
- [x] buffer_close_all aliases: buffer_close_all / bca / bcloseall and force variants
- [x] update / u
- [x] reload / rl and reload_all / rla
- [x] goto typable alias: goto / g
- [x] set_language / lang
- [x] reload_config
- [x] read / r
- [x] move / mv
- [x] echo
- [x] run_shell_command / sh / ! alias family
- [x] no_op -- this will be useful for masking the keymap to no operation
- [x] move_char_left / move_char_right
- [x] move_visual_line_up / move_visual_line_down
- [x] encoding
- [x] clear_register
- [x] redraw
