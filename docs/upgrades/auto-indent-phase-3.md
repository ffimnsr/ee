# Auto Indent + Smart Indent Phase 3

## Scope

Design and load tree-sitter indent query assets using existing runtime loader architecture.

## Findings from earlier phases

From Phase 0 through Phase 2:

- runtime query infrastructure for `indents.scm` already existed in `crates/xi-core-lib/src/runtime_loader.rs`
- `tree_sitter_support.rs` already warmed `RuntimeQueryKind::Indents` before tree-sitter reindent work
- newline smart indent in Phase 2 is still heuristic-only and does not consume query captures yet

So Phase 3 focus was not a second loader path. Focus was freezing contract and making runtime loading explicit and validated.

## Implemented

### Frozen indent-query contract

Defined minimal capture vocabulary in `crates/xi-core-lib/src/runtime_loader.rs`:

- `@indent`
- `@dedent`

Contract meaning:

- `@indent` marks syntax nodes whose interior lines should gain one indent level
- `@dedent` marks syntax nodes whose own line should lose one indent level

This keeps Phase 3 narrow and aligns with planned Phase 4 MVP outcomes:

- inherit-indent
- indent-one-level
- dedent-one-level

### Runtime loader API made explicit

Added typed wrappers in `RuntimeLoader`:

- `resolve_indent_query_source(...)`
- `compile_indent_query(...)`

These still use the same shared runtime loader, cache, and query precedence rules. No second grammar path. No second query loader.

### Contract validation on compile

Indent queries now validate capture names during compilation.

- valid captures compile normally
- unknown captures return explicit runtime error
- missing `indents.scm` still returns `Ok(None)` and remains isolated from unrelated syntax features

This means malformed runtime indent assets fail clearly instead of silently being accepted with undefined semantics.

## Architecture preserved

Phase 3 kept existing boundaries intact:

- same canonical `RuntimeQueryKind::Indents`
- same `queries/<language>/indents.scm` discovery
- same bundled -> user -> trusted workspace overlay order
- same compiled query cache in `RuntimeLoader`
- same tree-sitter consumer entrypoint in `tree_sitter_support.rs`

## Regression coverage added

Added tests in `crates/xi-core-lib/src/runtime_loader.rs` for:

- indent capture contract name mapping
- explicit `compile_indent_query()` path using shared runtime loader/caching
- invalid indent capture reporting clear error

## Deferred to Phase 4

Still not implemented here:

- actual newline indentation driven by query captures
- query-driven line-level indentation evaluation
- language-specific runtime `indents.scm` assets checked into runtime roots

Phase 3 freezes contract and loading path first so Phase 4 can consume them safely.
