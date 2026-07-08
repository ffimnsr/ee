# Auto Indent + Smart Indent Phase 4

## Scope

Implement backend syntax-aware smart-indent evaluation on top of Phase 3 indent-query contract.

## Implemented

### New backend indent engine

Added `crates/xi-core-lib/src/indent.rs`.

This module now owns syntax-aware newline indent evaluation.

Main pieces:

- `SyntaxIndentContext`
- `IndentOutcome`
- `syntax_indent_outcome(...)`

Current MVP outcomes:

- `Inherit`
- `IndentOneLevel`
- `DedentOneLevel`

### Runtime-query consumption

Phase 4 now consumes Phase 3 runtime indent queries through shared loader path.

Flow:

1. editor/event layer passes language, file-path, and document mode into newline handling
2. indent engine asks runtime loader for compiled `indents.scm`
3. engine parses current buffer with timeout
4. engine evaluates query captures and maps them to bounded indent outcomes
5. newline text mutation applies outcome in backend using existing indent policy helpers

No frontend parser ownership added.

### Fail-closed behavior

Syntax-aware path returns `None` and falls back cleanly when:

- document mode disables whole-document syntax work (`ConstrainedNormal`, `Vlf`)
- runtime indent query missing
- language unsupported
- parse/query path unavailable or times out
- selection spans multiple logical lines

Fallback order now:

1. plain newline when `auto_indent = false`
2. copied-indent baseline when `auto_indent = true` but syntax/heuristic layers unavailable
3. syntax-aware query result when available
4. heuristic smart-indent fallback when syntax-aware path unavailable and `smart_indent = true`

### Wiring

Editor path now passes syntax/runtime context into newline command without moving parser ownership outside backend.

Changed pieces:

- `crates/xi-core-lib/src/event_context.rs`
  - special-cases `BufferEvent::InsertNewline`
  - builds `SyntaxIndentContext` from backend-owned language/file/mode state
- `crates/xi-core-lib/src/editor.rs`
  - added `do_insert_newline_with_context(...)`
- `crates/xi-core-lib/src/edit_ops.rs`
  - added `insert_newline_with_context(...)`
  - keeps text mutation separate from syntax evaluation

## Notes on current query semantics

Phase 4 evaluator intentionally keeps semantics narrow.

- query captures are consumed as bounded line-local signals
- backend still owns final indent application
- if syntax-aware evaluator returns no signal, existing heuristic layer remains fallback

This keeps newline editing stable while runtime `indents.scm` assets are still immature.

## Regression coverage added

### `crates/xi-core-lib/src/indent.rs`

- syntax-aware indent outcome from indent capture
- syntax-aware dedent outcome from dedent capture
- missing query falls closed
- constrained mode falls closed

### `crates/xi-core-lib/src/edit_ops.rs`

- syntax-aware newline path uses query outcome
- syntax-aware path falls back to heuristic when query missing
- constrained mode falls closed to heuristic/baseline path

## Deferred

Still deferred beyond Phase 4:

- real language-specific bundled `indents.scm` assets
- richer capture vocabulary beyond bounded MVP
- query-driven alignment / hanging indent rules
- large-file mode policy tuning for future syntax-aware Enter behavior
