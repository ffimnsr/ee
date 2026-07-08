# Auto Indent + Smart Indent Phase 2

## Scope

Add bounded heuristic smart-indent fallback without tree-sitter dependency.

## Implemented

### New config

Added `smart_indent` to backend buffer config in `crates/xi-core-lib/src/config.rs`.

Behavior:

- `auto_indent = false` => plain newline only
- `auto_indent = true`, `smart_indent = false` => Phase 1 baseline copied-indent only
- `auto_indent = true`, `smart_indent = true` => Phase 1 baseline plus bounded heuristics

Current default is `true` in:

- `crates/xi-core-lib/src/config.rs::BufferItems::default`
- `crates/ee-cli/src/config.rs::EditorSettings::to_xi_config_table`

Serde default also set so missing older config payloads still deserialize with `smart_indent = true`.

### Heuristic newline behavior

Updated `crates/xi-core-lib/src/edit_ops.rs::insert_newline`.

Heuristics stay syntax-agnostic and line-local.

#### Indent-after-opener

If trimmed text before newline ends with one of:

- `{`
- `[`
- `(`

then newline indentation adds one extra indent level using existing indent policy:

- spaces when `translate_tabs_to_spaces = true`
- tab when `translate_tabs_to_spaces = false`

#### Dedent-before-closer

If newline is inserted while still in line-leading indentation and remaining trimmed text starts with one of:

- `}`
- `]`
- `)`

then newline indentation removes one indent level from copied baseline indentation.

This keeps behavior narrow and predictable. It does not attempt formatter-style alignment or language-specific rules.

### Explicit fallback boundaries

Heuristics only layer on top of baseline copied indentation.

Fallback cases:

- no opener/closer match => baseline copied indent only
- `smart_indent = false` => baseline copied indent only
- multi-line selection => baseline copied indent only

## Regression coverage added

Added focused tests in `crates/xi-core-lib/src/edit_ops.rs` for:

- opener adds one indent level
- opener respects space-indent policy
- closer dedents one level
- `{|}` becomes `{" + newline + indent + "}` without brace-expansion behavior
- `smart_indent = false` disables heuristics cleanly
- multi-line selection skips heuristic layer and falls back to baseline behavior

Also updated plugin test config payloads to include `smart_indent`.

## Deferred

Still not implemented here:

- tree-sitter-powered newline indentation
- language-specific alignment rules
- brace-pair expansion into two new lines
- syntax-query runtime integration for Enter key
