# Auto Indent + Smart Indent Phase 1

## Scope

Implement baseline `auto_indent` newline behavior from Phase 0 contract.

## Implemented

### Backend newline path

Updated `crates/xi-core-lib/src/edit_ops.rs::insert_newline`.

Behavior now:

- `auto_indent = false`
  - unchanged behavior
  - insert only configured `line_ending`
- `auto_indent = true`
  - insert configured `line_ending`
  - carry forward indentation from current logical line

### Carry-forward rules implemented

Per selection region:

- use `region.min()` as deterministic anchor
- derive indentation from logical line containing anchor
- if anchor sits inside leading whitespace, copy only whitespace prefix before anchor
- otherwise copy full leading whitespace for line
- preserve selection replacement semantics
- preserve configured line ending, including `\r\n`
- preserve remaining line content after split
- handle each cursor independently for multi-cursor edits

## Reused existing backend helpers

Implementation reuses existing logical-line helpers in `edit_ops.rs`:

- `logical_line_contents`
- `line_of_offset`
- `Interval`/`DeltaBuilder`

Phase 1 intentionally does **not** normalize indentation bytes. It copies existing leading whitespace as-is. That matches Phase 0 baseline contract and keeps this phase separate from later smart-indent depth adjustments.

## Regression coverage added

Added focused tests in `crates/xi-core-lib/src/edit_ops.rs` for:

- plain newline when `auto_indent = false`
- copying spaces, tabs, and mixed indentation
- caret inside indentation copying prefix only
- selection replacement from start-line indentation
- multi-line selection replacement from start-line indentation
- deterministic multi-cursor newline behavior
- preserving configured line ending

Updated `crates/xi-core-lib/src/event_context.rs::simple_indentation_test` expectations to reflect new baseline carry-forward behavior.

## Deferred to later phases

Not implemented here:

- `smart_indent` config
- opener/closer heuristic indentation
- tree-sitter newline indentation
- alignment or brace expansion behavior
