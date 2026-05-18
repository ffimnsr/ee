# Issues

## New World

### Large File Support: VLF Mode (Very Large Files)

- [ ] Improve large-buffer quit latency.
  - [ ] Exit event loop immediately after `handle_event` sets `should_quit`, before another expensive frame, scroll notification, source-control refresh, or external-change scan.
  - [ ] Add explicit fast app-shutdown path for `BufferManager` that stops reader/core threads without best-effort `close_view` cleanup work.
  - [ ] Keep interactive buffer-close commands (`:bc`, window close, tab close) on normal close semantics; use fast shutdown only for whole-app exit.
  - [ ] Avoid full-buffer close/render/plugin cleanup for large `ConstrainedNormal` buffers during app exit.
  - [ ] Re-evaluate exact-threshold behavior for 8 MiB fixtures: keep constrained-normal and ensure teardown stays non-blocking.
  - [ ] Add regression coverage proving `:q` on pristine large buffers does not save, does not close-buffer synchronously, and exits within quit budget.

Real next jump likely needs architectural change: first render from decoded prefix/line count while full rope + CRDT engine finishes after first paint. Current synchronous new_view_rpc still dominates.

- [ ] Improve large normal/constrained startup first render latency.
  - [ ] Add prefix-first startup path for large normal and `ConstrainedNormal` files: decode enough leading text to render first viewport, return `new_view`/first update, then finish full rope + CRDT engine construction after first paint.
  - [ ] Preserve current editing semantics after hydration: undo/redo, save, whole-document operations, LSP full sync gates, file metadata, and advisory lock behavior must match normal rope-backed buffers once full load completes.
  - [ ] Keep frontend update protocol explicit: first render may expose a bounded line cache/pending-load status, but cursor, scroll, and subsequent edits must wait for or safely join hydration before mutating full document state.
  - [ ] Ensure startup render does not run whole-buffer wrap, syntax, line-cache, or plugin work before first paint; only visible-prefix render-critical work belongs on the synchronous path.
  - [ ] Add perf regression coverage for 20 MiB long-line fixture on macOS: target warm open-to-first-render <250ms, with separate noise ceiling only for CI variance.
  - [ ] Add correctness tests for invalid UTF-8, UTF-8 BOM, mixed line endings, long first line truncation/rendering, and edit/save attempted before hydration completes.

### Runtime Tree-Sitter Grammar + Query Loading

- Rules:
  - Integrate external `tree-sitter-loader` crate for grammar discovery, grammar build/load orchestration, file-type/content-regex detection, and standard query-path parsing. Keep adapter surface backend-owned, but do not reimplement upstream loader mechanics already provided by crate.
  - Keep editor-facing grammar selection config-driven. Runtime grammar loading must be rooted in ee language config merge rules, not direct parser-directory discovery bypassing editor config.
  - Runtime tree-sitter must become canonical backend source for grammar resolution. Do not keep compile-time-only language registry as second source of truth once runtime loader lands.
  - Query files must stay file-backed under runtime directories, not embedded into Rust source. Support `.scm` files per language under `queries/<language>/`.
  - Query loading must support deterministic overlay order and explicit fallback rules in ee backend adapter. Missing optional query kinds disable only that feature; they must not crash syntax loading for unrelated features.
  - Treat upstream loader query support as baseline only: standard `highlights`, `injections`, `locals`, and `tags` may come from loader metadata, but ee-owned support remains required for `indents`, `textobjects`, `rainbows`, `folds`, and `; inherits:` composition.
  - Same runtime loader result must serve normal, constrained, and VLF syntax features so feature gating, query compilation, and language metadata stay consistent across modes.
  - Dynamic library loading must stay explicit and bounded: canonical paths only, known runtime roots only, cache handles, and surface actionable errors for ABI mismatch, missing symbol, or unreadable query files.
  - Every phase must land with regression tests and at least one failure-path test.

- [ ] Phase 0: freeze runtime loader architecture, runtime layout, and ownership boundaries.
  - [ ] Why: dynamic grammar loading cuts across config, runtime asset layout, backend syntax APIs, and distribution; lock architecture before code lands so later phases do not duplicate registries or query rules.
  - [ ] Document target ownership.
    - [ ] External `tree-sitter-loader` dependency owns parser-directory discovery, `tree-sitter.json` parsing, grammar compilation/loading, standard query-path parsing, and file-type/content-regex/injection-regex selection.
    - [ ] `crates/xi-core-lib/` or small backend-only adapter crate owns ee-specific runtime root policy, merged language-config loading, bridge from ee config/runtime layout into loader inputs, shebang/glob matching that upstream loader does not cover, query inheritance resolution, extra query kinds, capability mapping, cache invalidation policy, and translation from loader results into backend syntax APIs.
    - [ ] `crates/xi-core-lib/src/tree_sitter_support.rs` owns parser creation, query compilation, visible-range/VLF parse budgets, and language-feature gating on top of loader-provided runtime assets.
    - [ ] `crates/xi-core-lib/src/lang_features.rs` owns edit-feature dispatch that consumes loader-backed indentation, textobject, comment-style, and semantic capability data.
    - [ ] `crates/ee-tui/` owns runtime-health surfacing and user-facing diagnostics only; it must not parse grammar manifests or compile queries directly.
  - [ ] Define runtime directory contract.
    - [ ] `grammars/` stores compiled parser shared libraries (`.so`, `.dylib`, `.dll`).
    - [ ] `queries/<language>/` stores `.scm` files such as `highlights.scm`, `injections.scm`, `locals.scm`, `textobjects.scm`, `indents.scm`, `tags.scm`, `folds.scm`, and `rainbows.scm`.
    - [ ] Language metadata file declares canonical language id, file extensions/globs, optional aliases, optional shebang markers, grammar library name, and query-language mapping when grammar id differs from display name.
  - [ ] Define completion criteria.
    - [ ] Opening file with runtime-installed grammar uses shared library and query files without recompiling `ee`.
    - [ ] Missing grammar disables syntax features with explicit diagnostic instead of silent fallback to stale built-in registry.
    - [ ] Missing optional standard query file disables only affected standard feature set; missing ee-only query file disables only that ee-owned feature set.
    - [ ] Query inheritance and overlay rules are documented and testable.

- [ ] Phase 1: integrate `tree-sitter-loader` and define ee backend adapter model.
  - [ ] Why: runtime grammar support needs one stable backend API for discovery, caches, and error reporting before cutover can happen safely, but upstream loader should stay source of truth for loader mechanics.
  - [ ] Add dependency and backend adapter surface.
    - [ ] Add `tree-sitter-loader` to backend dependency graph at `xi-core-lib` or a small backend-only support crate if separation improves testability.
    - [ ] Define `RuntimeLoader` adapter that wraps upstream loader `Config`/`Loader`, runtime roots, and ee-specific policy.
    - [ ] Define `RuntimeLanguage` view that exposes canonical id, aliases, file-type matchers, shebang markers, grammar library path, and supported query kinds to backend consumers.
    - [ ] Define `GrammarHandle` wrapper or equivalent handle policy for loaded tree-sitter `Language` plus source path/mtime metadata needed for cache invalidation.
  - [ ] Define editor config integration.
    - [ ] Load built-in language config first, then merge user overrides, then merge workspace overrides, mirroring editor config precedence.
    - [ ] Keep grammar source selection keyed by merged language entries instead of asking upstream loader to discover arbitrary parser repos from parser-directories alone.
    - [ ] Preserve trusted-workspace boundary for workspace-local grammar config overrides.
  - [ ] Define metadata schema and merge rules.
    - [ ] Reuse upstream `tree-sitter.json` language-configuration shape where possible; add ee-only metadata layer only for capabilities not represented upstream such as shebangs, glob precedence, edit-feature gates, and extra query kinds.
    - [ ] Support bundled runtime plus user/project overrides with deterministic precedence.
    - [ ] Allow language aliasing without duplicate grammar load.
    - [ ] Reject ambiguous file-type ownership unless precedence rule is explicit.
  - [ ] Add cache behavior.
    - [ ] Cache loaded libraries by canonical path plus modified time.
    - [ ] Cache parsed loader language metadata separately from ee-compiled query objects.
    - [ ] Expose invalidation hook for runtime reload or startup refresh.

- [ ] Phase 2: load grammars from runtime shared libraries and remove compile-time grammar coupling.
  - [ ] Why: runtime grammar adoption is not complete until parser selection stops depending on `tree-sitter-*` crate list baked into `xi-core-lib`.
  - [ ] Replace static grammar registry path in backend.
    - [ ] Audit `crates/xi-core-lib/src/tree_sitter_support.rs` for `LANGUAGE_REGISTRY`, per-language constructor functions, and direct `tree-sitter-*` crate references.
    - [ ] Route grammar resolution through loader-backed metadata lookup and loaded `LanguageConfiguration` results.
    - [ ] Keep comment-style and indentation capability metadata runtime-derived instead of hardcoded per built-in language.
  - [ ] Add dynamic library loading safeguards.
    - [ ] Validate runtime root and canonical library path before handing library path to loader/open path.
    - [ ] Surface explicit error for missing export, incompatible ABI, unreadable library, or loader build failure.
    - [ ] Keep loaded handle alive as long as any `Language` from that library can be queried.
  - [ ] Remove compile-time grammar dependency list once runtime path covers current built-ins.
    - [ ] Drop direct `tree-sitter-*` grammar crate dependencies from `xi-core-lib`.
    - [ ] Keep only generic `tree-sitter` dependency plus loader crate API.
    - [ ] Preserve non-tree-sitter builds or unsupported-platform behavior with explicit feature gate or runtime-disabled diagnostic.

- [ ] Phase 3: add runtime `.scm` query support with inheritance and feature-scoped fallbacks.
  - [ ] Why: runtime grammar loading alone is insufficient; upstream loader only covers standard query groups, while ee needs extra query kinds plus helix-style composition rules that can evolve without recompiling editor.
  - [ ] Add query discovery API to loader.
    - [ ] Reuse upstream query-path metadata for standard `highlights`, `injections`, `locals`, and `tags` when available.
    - [ ] Resolve ee-owned query text for `textobjects`, `indents`, `folds`, and `rainbows` from runtime query directories keyed by canonical language id.
    - [ ] Return empty/absent only for optional missing kind; malformed files must produce actionable compile error with source file attribution.
  - [ ] Implement query inheritance semantics.
    - [ ] Support `; inherits:` header parsing for one or more parent query sets.
    - [ ] Merge inherited queries in deterministic order before local query text.
    - [ ] Detect cycles and fail with precise language/kind chain instead of infinite recursion.
  - [ ] Compile queries per feature boundary.
    - [ ] Syntax/highlighting path compiles `highlights`, `injections`, and `locals` together, reusing upstream loader query ordering unless ee override paths are explicitly supplied.
    - [ ] Semantic navigation path compiles `textobjects` and `tags` separately.
    - [ ] Indentation path compiles `indents` separately so missing indent support does not disable highlighting.
    - [ ] Future fold/rainbow consumers can compile their own query kinds without duplicating loader logic.

- [ ] Phase 4: cut backend syntax, feature gating, and language detection over to loader-backed runtime data.
  - [ ] Why: runtime assets only matter if all backend entry points consume same loader-backed resolution path; partial cutover would leave mismatched behavior between highlighting, reindent, and semantic motions.
  - [ ] Wire loader through tree-sitter entry points.
    - [ ] `visible_syntax_spans` and whole-buffer parse helpers must request grammar/query data from loader, not static tables.
    - [ ] `language_feature_availability` must derive syntax, textobject, indent, and comment capabilities from runtime-loaded metadata/query presence.
    - [ ] `lang_features.rs` must ask loader whether indentation/textobjects/comments are supported before dispatching feature paths.
  - [ ] Preserve existing mode-specific safety budgets.
    - [ ] Keep VLF visible-range parse limits independent from runtime loader.
    - [ ] Do not let query inheritance or runtime reload turn visible-range work into whole-file work.
    - [ ] Cache compiled query objects so repeated viewport renders do not reread `.scm` files on hot path.
  - [ ] Unify language detection.
    - [ ] Resolve language by explicit buffer setting first, then ee-owned shebang/glob rules, then loader-backed file-type/content-regex rules.
    - [ ] Keep alias resolution canonical so query directory, upstream grammar id, and user-facing language name cannot drift apart.
    - [ ] Decide behavior for unknown language with known extension conflict and document it.
  - [ ] Keep editor config and grammar config aligned.
    - [ ] Buffer language change commands must continue resolving against merged ee language config, not raw upstream grammar names.
    - [ ] Language-server, formatter, root-marker, and edit-feature settings must remain attached to same merged language entry that selects grammar/query assets.
    - [ ] Runtime grammar reload must not desynchronize active document language config from active parser/query set.

- [ ] Phase 5: add runtime operations, diagnostics, and distribution contract.
  - [ ] Why: runtime grammar system needs clear install/update/debug workflow or users cannot safely add grammars and queries outside source tree.
  - [ ] Align operations with editor config workflow.
    - [ ] Grammar fetch/build commands operate on grammars selected by merged ee language config.
    - [ ] Health/diagnostic commands report both effective language config source and resolved runtime grammar/query assets.
    - [ ] Runtime diagnostics must distinguish config-merge problem, grammar-source fetch/build problem, and runtime asset lookup problem.
  - [ ] Define runtime root search order and overrides.
    - [ ] Bundled runtime ships with default grammars and query files.
    - [ ] User runtime can add or override grammars/query assets without editing install tree.
    - [ ] Project-local runtime may override both when explicitly enabled.
  - [ ] Add diagnostics surface.
    - [ ] Expose command or status report listing resolved language id, grammar path, loaded query kinds, and missing query kinds.
    - [ ] Distinguish load failure, compile failure, and unsupported-feature cases.
    - [ ] Show effective runtime root used for current buffer.
  - [ ] Decide packaging/update story.
    - [ ] Document how bundled runtimes are produced for release artifacts.
    - [ ] Decide whether runtime assets are fetched, vendored, or user-supplied for development builds.
    - [ ] Ensure runtime additions do not require editing `Cargo.toml` for each new language.

- [ ] Phase 6: validate runtime loading, query semantics, and migration off built-in registry.
  - [ ] Why: runtime loading touches dynamic linking, parser correctness, feature gating, and startup behavior; regression coverage must prove both happy paths and failure containment.
  - [ ] Add loader unit tests.
    - [ ] Runtime root precedence.
    - [ ] Canonical-path cache invalidation on modified library/query file.
    - [ ] Alias, loader file-type/content-regex, ee glob, and ee shebang resolution.
    - [ ] Query inheritance merge order and cycle detection.
  - [ ] Add backend integration tests.
    - [ ] Runtime-loaded grammar produces syntax spans for normal buffers.
    - [ ] Runtime-loaded grammar works with visible-range VLF parsing without bypassing budgets.
    - [ ] Missing `indents.scm` disables reindent only.
    - [ ] Missing `textobjects.scm` disables semantic textobjects only.
    - [ ] Missing standard `highlights`/`locals`/`injections`/`tags` query paths behave like upstream loader contract and report file-attributed query errors.
    - [ ] Broken shared library reports explicit error and leaves editor usable.
  - [ ] Add migration closeout.
    - [ ] Remove now-obsolete compile-time grammar registry code and related checklist items only after runtime path covers current languages.
    - [ ] Add short architecture note describing final flow: runtime root -> `tree-sitter-loader` -> grammar/query cache -> `xi-core-lib` parse/query consumers -> frontend diagnostics.

### Tree-Sitter Tags for Symbols + Navigation Fallback

- Rules:
  - Use `tree-sitter-tags` only for code-navigation metadata such as document symbols, local symbol outline, and definition/reference-style tag extraction. Do not treat it as generic structured-data query engine.
  - Keep backend ownership in `xi-core-lib`; frontends consume normalized `SymbolItem` or navigation payloads only.
  - Reuse same language resolution and runtime query-loading path as canonical tree-sitter backend. Do not add second grammar or query discovery path just for tags.
  - Prefer runtime `tags.scm` and `locals.scm` assets once loader lands; avoid hardcoded per-language tagging logic beyond temporary bootstrap needed before runtime cutover.
  - LSP remains authoritative when active and healthy for richer cross-file/project semantics. Tree-sitter tags provide fallback and local fast-path, not competing source of truth for workspace intelligence.
  - Any fallback behavior must be explicit in UI and diagnostics so users can tell whether symbol/navigation data came from LSP or local tree-sitter tags.
  - Every phase must land with regression tests and at least one malformed-query or unsupported-language failure-path test.

- [ ] Phase 0: freeze `tree-sitter-tags` scope and backend contract.
  - [ ] Why: tags overlap with existing semantic motions and LSP symbol flows; lock exact role before implementation to avoid duplicated navigation stacks.
  - [ ] Define feature boundary.
    - [ ] Document that tags cover document-local symbol extraction and optional local definition/reference indexing.
    - [ ] Exclude generic JSON/YAML/TOML querying, formatting, and non-code document inspection from this work.
    - [ ] Decide initial command surface: `:symbols`/`:outline` fallback only, or also direct non-LSP definition/reference fallback.
  - [ ] Define data contract.
    - [ ] Map tag output into existing `SymbolItem` shape where possible.
    - [ ] Decide whether current navigation target type is sufficient for tag-based definition/reference results or needs backend-owned tagged range struct.
    - [ ] Define source marker for UI/status so picker and jump flows can distinguish `lsp` vs `tree-sitter-tags`.

- [ ] Phase 1: add backend tag extraction adapter.
  - [ ] Why: `tree-sitter-tags` must integrate through one backend façade, not leak crate-specific APIs across editor layers.
  - [ ] Add dependency and adapter surface in `crates/xi-core-lib`.
    - [ ] Add `tree-sitter-tags` to workspace/backend dependency graph.
    - [ ] Define backend helper that accepts resolved `Language`, source bytes, and tagging query inputs, then returns normalized tags.
    - [ ] Reuse existing parser/language selection path from `tree_sitter_support.rs` instead of creating separate registry.
  - [ ] Define temporary bootstrap strategy before runtime loader cutover.
    - [ ] If runtime query loading is not ready yet, decide whether to defer implementation or carry minimal stopgap query source with clear removal plan.
    - [ ] Do not duplicate long-term query ownership between stopgap and runtime assets.

- [ ] Phase 2: support document symbols fallback through tree-sitter tags.
  - [ ] Why: current `:symbols` / `:outline` path depends on LSP; local fallback is highest-value first use.
  - [ ] Add backend symbol extraction.
    - [ ] Convert tag definitions into `SymbolItem` values with stable kind mapping and byte/line-column conversion.
    - [ ] Filter out low-signal reference tags from symbol outline output.
    - [ ] Decide ordering rules: source order, kind grouping, or query-defined order.
  - [ ] Wire fallback behavior.
    - [ ] When LSP document symbols unavailable, unsupported, or disabled, serve tree-sitter tag symbols automatically for supported languages.
    - [ ] Keep current picker and RPC shape stable so frontend integration stays minimal.
    - [ ] Surface clear status message when fallback engaged or when no tagging query exists for current language.

- [ ] Phase 3: evaluate definition/reference fallback scope.
  - [ ] Why: tag extraction can produce lightweight def/ref data, but quality and UX tradeoffs differ from LSP and may not justify full command parity.
  - [ ] Define supported targets.
    - [ ] Decide whether local go-to-definition from tags is useful enough for first pass.
    - [ ] Decide whether local references should stay document-local only or wait for project indexing infrastructure.
    - [ ] Reject low-confidence jumps when ambiguity is too high; prefer explicit picker over silent wrong jump.
  - [ ] Keep semantics bounded.
    - [ ] Do not claim workspace-accurate references without index/build step.
    - [ ] Do not regress current LSP flows when language server is available.

- [ ] Phase 4: align tags with runtime query-loading architecture.
  - [ ] Why: long-term tags support should ride same runtime grammar/query system already planned for `tags.scm` and `locals.scm`.
  - [ ] Integrate with runtime assets.
    - [ ] Load `tags.scm` and optional `locals.scm` from runtime query directories through shared loader-backed path.
    - [ ] Cache compiled tag configurations alongside other query artifacts.
    - [ ] Keep missing `tags.scm` isolated to symbol/navigation fallback only.
  - [ ] Preserve mode constraints.
    - [ ] Define whether VLF/constrained buffers get disabled tags, visible-range-only tags, or explicit unsupported status.
    - [ ] Avoid whole-file tag extraction on giant buffers when parse/query budgets would violate large-file goals.

- [ ] Phase 5: validate symbol quality, fallback behavior, and failure containment.
  - [ ] Why: tag-based navigation only helps if outputs are stable, correctly typed, and clearly bounded when unsupported.
  - [ ] Add unit and integration coverage.
    - [ ] Document-symbol fallback returns expected `SymbolItem` values for at least Rust, Python, and JavaScript/TypeScript.
    - [ ] Missing `tags.scm` disables fallback cleanly without crashing picker or command flow.
    - [ ] Malformed tagging query reports actionable error with language/query attribution.
    - [ ] LSP success path still wins over tag fallback when both are available.
    - [ ] Large-buffer or unsupported-language cases fail closed with explicit status instead of expensive best-effort scan.


### Optional Future Boundary Work

- [ ] Move text-object range resolution from `crates/ee-tui/src/app/mod.rs` into `xi-core-lib` if we want backend-owned semantic text objects across future frontends.
- [ ] Move visual-block delete/change/yank execution from `crates/ee-tui/src/app/mod.rs` into `xi-core-lib` so rectangular selection mutations become backend-owned editor semantics.
- [ ] Re-evaluate visual-block insert setup and replay split between `ee-tui` and `xi-core-lib`; keep frontend workflow glue only, move any remaining selection-truth or mutation semantics backend-side if reused by another frontend.

### Other works

- [ ] Time it using hyperfine against the original head and tail commands, and implement ways to be on par or much faster than the original command.
- [ ] Implement a `jq` like command `do file query|q --type json`, to query document files in similar ways
  - [ ] Implement for `json`
  - [ ] Implement for `yaml`
  - [ ] Implement for `toml`
  - [ ] Implement for `kdl`
- [ ] When trying to save and user doesn't have permission, ask if they want to re-execute with higher privilage with `sudo`, `su`, `run0`
