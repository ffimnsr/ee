# Auto Indent + Smart Indent Phase 5

## Scope

Validate behavior, config semantics, and mode-specific fallbacks before treating smart-indent as complete for current rollout.

## Added coverage

### Baseline auto-indent coverage

Added regression coverage for plain-text / non-code behavior:

- plain text buffer keeps baseline copied-indent only
- no syntax artifacts appear when no parser/query support exists

### Smart-indent language coverage

Added bounded cross-language tests for:

- Rust
- JSON
- Python

These tests use runtime-loaded indent queries and confirm backend syntax-aware path works across multiple language ids, including one indentation-sensitive language when query semantics are available.

### Failure-path coverage

Added tests proving clean fallback behavior for:

- missing parser / unknown language
- missing indent query
- malformed indent query
- disabled `smart_indent`
- constrained mode where whole-document syntax work is disallowed

Expected fallback behavior:

- `auto_indent = false` -> plain newline only
- `auto_indent = true`, parser/query unavailable -> copied-indent baseline, then heuristic smart-indent if enabled
- `smart_indent = false` -> no syntax-aware or heuristic additive indentation

### Determinism coverage

Added deterministic smart-indent tests for:

- multi-cursor newline path
- selection replacement path

## Mode / performance semantics

### Normal mode

- syntax-aware indent allowed
- query-backed smart-indent may run
- if syntax path unavailable, fallback to heuristic or baseline

### ConstrainedNormal mode

- syntax-aware newline indent intentionally disabled
- Enter avoids whole-buffer syntax work
- fallback stays heuristic smart-indent or baseline auto-indent only

### VLF mode

- newline editing unsupported because VLF editing path is not active in normal command dispatch
- no syntax-aware Enter evaluation attempted
- user gets existing unsupported-edit behavior instead of slow best-effort parsing

### Parser-disabled / unsupported language

- no explicit error needed for Enter
- fallback stays baseline auto-indent, plus heuristic smart-indent only when enabled
- fail closed: no panic, no stale indentation artifact, no partial syntax state leak into output

## User-facing config semantics

Current behavior contract:

- `auto_indent`
  - authoritative baseline newline-indent toggle
  - when `false`, Enter inserts only configured line ending
  - when `true`, Enter at least carries forward line indentation

- `smart_indent`
  - additive indentation layer on top of `auto_indent`
  - prefers syntax-aware query result in Normal mode when runtime indent query is available
  - otherwise falls back to bounded heuristic opener/dedent logic
  - when `false`, additive indentation is disabled completely

## Summary

Phase 5 closes current rollout:

- baseline behavior covered
- syntax-aware path covered across multiple language ids
- failure containment verified
- constrained / unsupported mode behavior defined
- config semantics documented

Future work can broaden real bundled `indents.scm` assets and richer language-specific query semantics without changing this fallback contract.
