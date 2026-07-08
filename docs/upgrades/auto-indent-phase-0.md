# Auto Indent + Smart Indent Phase 0

## Purpose

Freeze behavior contract before Phase 1 implementation.

## Current state findings

### Config already exists

- `crates/xi-core-lib/src/config.rs` defines `BufferItems.auto_indent` and defaults it to `true`.
- `crates/ee-cli/src/config.rs` already writes `auto_indent` into xi config table.
- No `smart_indent` config exists yet.

### Newline path ignores `auto_indent`

- `crates/xi-core-lib/src/editor.rs::Editor::do_insert_newline` calls `edit_ops::insert_newline`.
- `crates/xi-core-lib/src/edit_ops.rs::insert_newline` currently delegates straight to `insert(base, regions, &config.line_ending)`.
- Result: Enter only inserts configured line ending. No indent carry-forward. No smart-indent adjustment.

### Existing reuse points already available

- `crates/xi-core-lib/src/edit_ops.rs::insert_tab` already respects `tab_size` and `translate_tabs_to_spaces` via `get_tab_text`.
- `crates/xi-core-lib/src/edit_ops.rs` already has logical-line helpers used by other edit operations:
  - `logical_line_contents`
  - `previous_char_boundary`
  - `next_char_boundary`
  - `line_content_end`
- `crates/xi-core-lib/src/lang_features.rs::reindent` already performs whole-line reindent from shared indentation levels.
- `crates/xi-core-lib/src/tree_sitter_support.rs` already computes indentation levels and warms `RuntimeQueryKind::Indents`.
- `crates/xi-core-lib/src/runtime_loader.rs` already recognizes `indents.scm` as runtime query asset.

### Architectural gap

Tree-sitter indentation infrastructure already exists for explicit reindent operations, but newline insertion does not use any of it yet. Phase 1 should wire baseline auto-indent into newline path first. Later smart-indent phases should reuse existing runtime/query plumbing instead of creating new loader path.

## Phase 0 decisions

## 1. Baseline newline behavior

### `auto_indent = false`

Keep current behavior unchanged.

- Enter replaces each selection with exactly `config.line_ending`.
- No whitespace carry-forward.
- No smart-indent adjustment.

### `auto_indent = true`

Enter inserts `config.line_ending` plus carried indentation derived from pre-edit buffer state.

#### Indentation source

For each selection region, derive indentation from logical line containing `region.min()` in pre-edit buffer.

Reason:

- deterministic for forward and backward selections
- deterministic for multi-cursor edits
- stable when selection spans multiple lines

#### Carry-forward rule

- If caret/selection start is inside line leading whitespace, copy only whitespace prefix before caret.
- If caret/selection start is at first non-whitespace column or later, copy full logical-line leading whitespace.
- If line has no leading whitespace, carry-forward indent is empty.

Reason:

Copying only prefix while caret sits inside indentation preserves user-intended partial indent and avoids jumping deeper than cursor column.

#### Position cases

- Caret at end of line: new line inherits full leading whitespace from current logical line.
- Caret in middle of non-whitespace text: split line, new line inherits full leading whitespace from current logical line.
- Caret inside indentation: split line, new line inherits indentation prefix before caret only.
- Active non-caret selection: replace selection with one newline plus carried indent computed from selection start rule above.
- Multi-line selection: still collapses to one newline plus carried indent from start line.
- Multi-cursor: evaluate each region independently from same pre-edit snapshot.

### Line ending and tab policy

- Preserve `config.line_ending` exactly.
- Preserve existing selection replacement semantics.
- Reuse existing tab/space settings when later phases add indentation depth adjustments.
- Baseline Phase 1 carry-forward copies existing whitespace bytes verbatim from source line. It does not normalize tabs to spaces or spaces to tabs.

Reason:

Phase 1 goal is missing behavior parity, not reformatting.

## 2. Smart-indent boundary

### Config surface

Add new backend-owned `smart_indent` boolean config.

Contract:

- `auto_indent` stays baseline newline-indent toggle.
- `smart_indent` is additive only.
- `smart_indent` has no effect when `auto_indent = false`.

Reason:

Separate toggles keep existing `auto_indent` meaning clean and give explicit opt-out for syntax-aware behavior.

### Fallback contract

Smart-indent must never block Enter.

Evaluation order:

1. If `auto_indent = false`, insert plain newline.
2. If `auto_indent = true` and no smart-indent adjustment available, use carried-indent baseline only.
3. If `smart_indent = true`, optional heuristic or tree-sitter layer may adjust baseline result.
4. Any parser, query, runtime, timeout, unsupported-language, or degraded-mode failure falls back closed to baseline auto-indent, not failed edit.

### Initial supported smart-indent behavior

Initial MVP scope stays narrow:

- indent one level after trailing opener tokens such as `{`, `[`, `(`
- dedent one level when newline is inserted before closing tokens such as `}`, `]`, `)`

Out of scope for first rollout:

- alignment rules
- hanging indent rules
- language-specific formatter behavior
- brace-pair expansion or auto-inserted extra lines
- workspace/project-wide indentation inference

Reason:

Narrow opener/dedent behavior gives predictable value without turning Enter into formatter.

## 3. Reuse contract

Later phases should reuse existing backend pieces before adding new abstractions.

- Newline mutation stays backend-owned in `crates/xi-core-lib/src/edit_ops.rs`.
- Smart-indent decision logic stays backend-owned in `xi-core-lib`.
- Existing runtime query path for `indents.scm` stays canonical source for syntax-aware indentation assets.
- Existing whole-document indentation code in `lang_features.rs` and indentation-level computation in `tree_sitter_support.rs` should inform smart-indent implementation where practical.
- Frontend should continue sending newline edit intent only. Frontend must not own indentation policy.

## 4. Test contract for follow-up phases

Phase 1 minimum regression set should cover:

- `auto_indent = false` preserves plain newline behavior
- spaces, tabs, and mixed leading whitespace carry forward correctly
- caret inside indentation copies prefix only
- mid-line and end-of-line Enter remain deterministic
- non-caret selections collapse to newline plus indent deterministically
- multi-cursor newline remains deterministic
- configured `line_ending` still preserved

Future smart-indent phases must also cover:

- `smart_indent = false` disables additive behavior cleanly
- unsupported language and missing query assets fall back to baseline auto-indent
- parser/runtime timeout falls back closed
- VLF/degraded modes avoid expensive synchronous parse work on Enter

## Summary

Phase 0 conclusion:

- `auto_indent` already exists but currently does nothing on Enter.
- Phase 1 should implement pure carry-forward indentation in newline path first.
- Smart indent should ship behind separate `smart_indent` toggle.
- Smart indent MVP should stay bounded to one-level opener/dedent behavior.
- Syntax-aware indentation should reuse existing `indents.scm` runtime/query pipeline, not create new path.
