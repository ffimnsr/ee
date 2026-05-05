# Issues

## New World

### Tooling and CI

- [ ] Add property-based tests (`proptest`) in `crates/xi-rope` for delta application, merging, and CRDT invariants.


### Commands

Implement these commands, if there's already a command like this implemented but have
different name alias this. Description for the command can be found here: https://docs.helix-editor.com/keymap.html#select--extend-mode. Don't copy the keybindings in the site, check only for description

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

### Backlog

- [ ] align_selections
- [ ] rotate_selections_backward
- [ ] rotate_selections_forward

- extend_line_below
- extend_to_line_bounds
- shrink_to_line_bounds
- join_selections
- join_selections_space
- keep_selections
- remove_selections
- toggle_comments
- expand_selection
- shrink_selection
- select_prev_sibling
- select_next_sibling
- select_all_siblings
- select_all_children
- move_parent_node_end
- move_parent_node_start
- goto_file_start
- goto_column
- goto_last_line
- goto_file
- goto_line_start
- goto_line_end
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
- move_line_down
- move_line_up
- match_brackets
- surround_add
- surround_replace
- surround_delete
- select_textobject_around
- select_textobject_inner
- vsplit
- hsplit
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
- code_action
- select_references_to_symbol_under_cursor
- hover
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
- completion
- commit_undo_checkpoint
- insert_register
- delete_word_backward
- delete_word_forward
- kill_to_line_start
- kill_to_line_end
- delete_char_backward
- delete_char_forward
- insert_newline
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
- add_newline_below
- add_newline_above
