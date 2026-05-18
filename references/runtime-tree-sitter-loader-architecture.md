# Runtime Tree-Sitter Loader Architecture

## Ownership

- `tree-sitter-loader` owns parser-directory discovery, `tree-sitter.json` parsing, grammar compilation/loading, standard query-path parsing, and file-type/content-regex/injection-regex matching.
- `crates/xi-core-lib/src/runtime_loader.rs` owns ee runtime-root policy, merged language-config projection, trusted-workspace gating for workspace overrides, ee shebang/glob metadata, alias resolution, file-type precedence checks, cache invalidation hooks, and translation into backend-facing `RuntimeLanguage` / `GrammarHandle` views.
- `crates/xi-core-lib/src/tree_sitter_support.rs` remains canonical parse/query consumer. It keeps parser creation, query compilation, visible-range budgets, and feature gating on top of runtime assets.
- `crates/xi-core-lib/src/lang_features.rs` remains edit-feature dispatcher that consumes runtime-backed capability data.
- `crates/ee-cli/` stays diagnostics-only for runtime health. It must not parse manifests or compile queries directly.

## Runtime Layout

- User runtime root is `dirs::data_dir()/ee/`. On Linux this maps to `$XDG_DATA_HOME/ee/`.
- Compiled shared libraries live under `grammars/` as platform-native dynamic libraries.
- Query files live under `queries/<language>/` and use file-backed `.scm` assets such as `highlights.scm`, `injections.scm`, `locals.scm`, `tags.scm`, `textobjects.scm`, `indents.scm`, `folds.scm`, and `rainbows.scm`.
- Runtime metadata keeps one canonical language id plus aliases, file types, optional globs, optional shebangs, grammar id, grammar library name, optional query-language override, and supported query kinds.

## Merge Rules

- Built-in editor language definitions seed runtime language state.
- User runtime overrides apply after built-ins.
- Workspace runtime overrides apply last only when workspace is trusted.
- Aliases resolve to one canonical runtime language. Duplicate alias ownership is rejected.
- File-type conflicts are rejected unless one language declares higher explicit `match_priority`.
- Unknown-language buffers with a known but conflicted extension fail closed: config merge rejects equal-priority file-type ownership, so path-only detection returns no language until precedence is made explicit.
- Grammar source selection stays keyed by merged ee language entries; parser directories remain loader input for build/discovery workflows, not a second selection source.

## Cache Contract

- Grammar handles are cached by canonical library path plus observed modified time.
- Loader-derived language metadata cache stays separate from ee query-artifact cache.
- Runtime reload/startup refresh uses explicit invalidation hooks instead of silent stale reuse.

## Final Flow

1. Runtime roots resolve in precedence order: bundled base, then user overlay, then trusted workspace overlay.
2. `tree-sitter-loader` discovers parser metadata from configured parser directories, parses `tree-sitter.json`, and resolves standard query paths.
3. `crates/xi-core-lib/src/runtime_loader.rs` merges ee language config onto loader metadata, canonicalizes language ids and aliases, selects runtime asset roots, and caches grammar handles plus file-backed query artifacts.
4. `crates/xi-core-lib/src/tree_sitter_support.rs` consumes that cache for parser acquisition, visible-range/VLF budgets, whole-buffer parse helpers, and feature-scoped query compilation.
5. `crates/xi-core-lib/src/lang_features.rs` and object/navigation consumers read the same runtime-backed language metadata and query availability.
6. `crates/ee-cli/` surfaces frontend diagnostics from runtime health reports without becoming a second grammar/query loader.
