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

### Configurable Bundled + User Tree-Sitter Languages

- Rules:
  - Adding a tree-sitter language must not require editing `crates/xi-core-lib/src/runtime_loader.rs`.
  - Keep bundled runtime languages as defaults, but let ee TOML config add, override, or disable runtime language definitions.
  - Use the same ee config precedence as editor settings and LSP config: system, XDG user, legacy user fallback, then ancestor `.ee.toml`.
  - Runtime language config must feed the existing `ee do runtime fetch` and `ee do runtime build` commands.
  - Grammar sources may come from a crates.io grammar crate or a git repository pinned by `branch`, `tag`, or `rev`.
  - Git grammar sources must be pinned deterministically for reproducible runtime builds; `rev` is preferred for release/runtime packaging.
  - Workspace runtime overrides remain trusted-only; untrusted workspace config must not fetch, build, or load arbitrary native grammar code.
  - Missing grammar source, invalid source pins, duplicate file-type ownership, unsupported queries, and build failures fail closed with clear diagnostics.
  - Every phase must land with regression tests and clippy success.

- Final config contract:
  - TOML key: `[languages.<id>]`, where `<id>` is stable runtime language id.
  - Required fields for enabled language: `name`, `file_types`, and `[languages.<id>.grammar]`.
  - Optional language fields: `enabled`, `aliases`, `globs`, `shebangs`, `scope`, `query_language`, `content_regex`, `first_line_regex`, `injection_regex`, `match_priority`, and `supported_query_kinds`.
  - Required grammar fields: `library`, `symbol`, and `[languages.<id>.grammar.source]`.
  - Grammar source must be exactly one of:
    - `[languages.<id>.grammar.source.crate]` with `name = "tree-sitter-demo"` plus `version = "1.2.3"`
    - `[languages.<id>.grammar.source.git]` with `url = "https://github.com/example/tree-sitter-demo"` plus exactly one of `branch`, `tag`, or `rev`
  - Defaults: `enabled = true`, `aliases = []`, `globs = []`, `shebangs = []`, `supported_query_kinds = ["highlights", "injections", "locals", "tags", "textobjects", "indents", "folds", "rainbows"]`.
  - Merge semantics: later config layers replace scalars, replace arrays, replace grammar source as one unit, and `enabled = false` disables the language id.
  - Example:
    ```toml
    [languages.gleam]
    name = "Gleam"
    file_types = ["gleam"]
    scope = "source.gleam"
    aliases = ["gleam"]

    [languages.gleam.grammar]
    library = "tree-sitter-gleam"
    symbol = "tree_sitter_gleam"
    [languages.gleam.grammar.source.git]
    url = "https://github.com/gleam-lang/tree-sitter-gleam"
    tag = "v1.0.0"

    [languages.demo.grammar]
    library = "tree-sitter-demo"
    symbol = "tree_sitter_demo"
    [languages.demo.grammar.source.crate]
    name = "tree-sitter-demo"
    version = "1.2.3"
    ```

- [ ] Phase 0: define runtime language TOML schema.
  - [ ] Add TOML structs for `[languages.<id>]` and nested `[languages.<id>.grammar]`.
  - [ ] Model grammar source as enum: crates.io `{ name, version }` or git `{ url, branch|tag|rev }`.
  - [ ] Reject configs with no source, multiple source kinds, multiple git refs, missing `library`, missing `symbol`, empty `file_types`, or empty language id.
  - [ ] Add parser tests for crates.io source, git branch source, git tag source, git rev source, and invalid mixed source fields.

- [ ] Phase 1: merge config-defined runtime languages.
  - [ ] Extend runtime config merge so user/project definitions can add new `LanguageDefinition`s, not only override built-ins.
  - [ ] Preserve current built-in language behavior when no `[languages]` config exists.
  - [ ] Apply `enabled = false` by removing language id from effective runtime language set.
  - [ ] Normalize file types by stripping leading `.` and rejecting empty values.
  - [ ] Add tests for adding new language, overriding built-in grammar metadata, disabling built-in language, duplicate file-type ownership, and config precedence.

- [ ] Phase 2: teach runtime fetch about git grammar sources.
  - [ ] Extend `fetch_grammar_sources` to fetch crates.io sources through current temporary cargo manifest path and git sources through explicit clone/fetch checkout path.
  - [ ] Use deterministic source directory names including language id and source pin.
  - [ ] For git `branch` and `tag`, record resolved commit SHA in fetch output and build metadata.
  - [ ] For git `rev`, checkout exact revision and fail if missing.
  - [ ] Add tests using a local git fixture for branch, tag, rev, missing ref, and source cache reuse.

- [ ] Phase 3: build runtime assets from config-defined sources.
  - [ ] Reuse existing grammar build path for fetched crates.io and git source directories.
  - [ ] Keep query copy/discovery behavior identical for built-in and user-defined languages.
  - [ ] Support standard query paths from grammar repository `tree-sitter.json` when present.
  - [ ] Add tests proving git-sourced grammar builds into `grammars/` and queries copy into `queries/<language>/`.
  - [ ] Add failure-path tests for missing `src/parser.c`, bad `tree-sitter.json`, and grammar symbol mismatch.

- [ ] Phase 4: protect runtime loading and workspace trust boundaries.
  - [ ] Ensure untrusted workspace `.ee.toml` cannot introduce native grammar source fetch/build/load.
  - [ ] Allow trusted workspace runtime roots only through existing trusted-workspace mechanism.
  - [ ] Keep bundled and user runtime roots read/write behavior unchanged: bundled read-only, user build output writable.
  - [ ] Add diagnostics that include language id, source type, and ref, but not credentials embedded in URLs.
  - [ ] Add tests for untrusted workspace language ignored, trusted workspace language applied, and credential-redacted git URL diagnostics.

- [ ] Phase 5: document user workflow and examples.
  - [ ] Document `[languages.<id>]` and `[languages.<id>.grammar]` config fields.
  - [ ] Include examples for crates.io source, git branch source, git tag source, and git rev source.
  - [ ] Document recommendation: use `rev` for reproducible release builds; use `branch` only for local development.
  - [ ] Document commands: `ee do runtime fetch`, `ee do runtime build`, `scripts/build-runtime.sh`, and `EE_RUNTIME_DIR`.
  - [ ] Document limitations: native grammar code requires trusted source, one effective owner per file type, and LSP config remains separate.

- [ ] Phase 6: optional unified language config follow-up.
  - Why: runtime grammar config and LSP config likely belong under one user-facing language surface even if backend ownership stays split.
  - [ ] Design unified schema.
    - [ ] Evaluate nested `[languages.<id>.lsp.<server_id>]` or equivalent shape that keeps grammar and language-server metadata together.
    - [ ] Define merge and disable semantics so unified language config still compiles into current runtime-loader and `xi-lsp-plugin` inputs.
    - [ ] Decide whether one language may own multiple server entries and how per-server enable/disable interacts with file-type matching.
  - [ ] Keep migration bounded.
    - [ ] Decide whether to support legacy `[lsp.servers.<id>]` as transitional input or replace it outright.
    - [ ] Avoid mixing unified schema work with native grammar trust changes or file-type multi-owner resolution.

- [ ] Phase 7: optional LSP matching and routing expansion.
  - Why: unified language config becomes more useful if LSP routing no longer stays locked to extension-only single-winner matching.
  - [ ] Expand server selection signals.
    - [ ] Replace extension-only matching with richer language/server selection signals such as globs, shebangs, first-line hints, workspace markers, or explicit language id.
    - [ ] Define whether routing happens from resolved language id first, from direct server rules, or from both with deterministic precedence.
  - [ ] Define multi-server behavior.
    - [ ] Decide whether one buffer may fan out to multiple language servers for distinct capabilities or whether routing stays single-primary-server with explicit secondary roles.
    - [ ] Define capability ownership boundaries if multiple servers attach to one buffer.
  - [ ] Preserve inspectability and failure semantics.
    - [ ] Define deterministic conflict resolution and status/UI so active server ownership stays inspectable.
    - [ ] Keep current fail-closed behavior for missing executables, unsupported workspace roots, and disabled servers.

- [ ] Backlog goals beyond current scope.
  - [ ] Revisit trusted-only native grammar loading if runtime grammar execution moves to a sandboxed or non-native format.
  - [ ] Revisit one-effective-owner-per-file-type if language detection grows beyond extension matching into ranked per-buffer resolution.

### Keymap Help + Binding Discovery Unification

- Rules:
  - Keep keymap help derived from active binding data, not hand-maintained prose that can drift from actual defaults or user overrides.
  - Respect user-configured keymaps. `:keymap` and any keybinding discovery UI must reflect effective bindings after config/custom sequence bindings load.
  - Separate binding metadata from presentation. Binding tables stay source of truth; help rendering may group or filter them, but must not invent stale shortcuts.
  - Preserve high-signal help output. Derived help should surface important bindings and descriptions without dumping unreadable raw tables by default.
  - Every binding shown in keymap help must resolve in current mode/context, and every curated high-value binding policy must be testable.
  - Every phase must land with regression coverage proving help output tracks both built-in bindings and user overrides.

- [ ] Phase 0: freeze keymap-help scope and effective-binding contract.
  - Why: current `keymap_help_items()` is curated static text; replacing it needs clear boundary between full binding inspection and concise discovery help.
  - [ ] Define output contract.
    - [ ] Decide whether `:keymap` should show curated high-value bindings, full effective binding table, or both views.
    - [ ] Decide how sequences, mode-specific bindings, and prefix maps appear in help output.
    - [ ] Decide whether hidden/internal bindings stay excluded from discovery output.
  - [ ] Define effective-binding semantics.
    - [ ] Confirm help reads post-config merged bindings, not compile-time defaults only.
    - [ ] Confirm user override/removal semantics propagate into help output.
    - [ ] Decide how conflicts or shadowed bindings should display when multiple mappings target same key path.

- [ ] Phase 1: introduce registry-backed binding discovery helpers.
  - Why: help cannot stay accurate until it reads from same binding state used for dispatch.
  - [ ] Add helper surface around effective key bindings.
    - [ ] Define data shape for discovered bindings: mode, key sequence, action, description, source, and visibility flags.
    - [ ] Add helper to enumerate active bindings after defaults and user config merge.
    - [ ] Reuse existing sequence/binding metadata instead of adding parallel static help tables.
  - [ ] Preserve readability.
    - [ ] Keep helper output stable enough for tests and help rendering.
    - [ ] Avoid coupling UI string formatting directly into binding storage structures.

- [ ] Phase 2: move `:keymap` help onto effective bindings.
  - Why: static `keymap_help_items()` misses changes whenever defaults or user preferences shift.
  - [ ] Render help from active binding metadata.
    - [ ] Replace hardcoded keymap-help rows with generated rows from effective bindings.
    - [ ] Preserve concise descriptions for high-value actions using binding descriptions already present in config/default tables.
    - [ ] Group results by mode, prefix, or category so derived help stays readable.
  - [ ] Respect user changes.
    - [ ] User-added bindings should appear automatically when they have descriptions.
    - [ ] User-overridden bindings should replace default help output rather than showing stale defaults.
    - [ ] Removed or shadowed defaults should not remain in derived keymap help.

- [ ] Phase 3: define curated-discovery layer on top of raw binding data.
  - Why: full binding dumps and concise onboarding help solve different problems; one view may not fit both.
  - [ ] Decide presentation strategy.
    - [ ] Keep `:keymap` as concise curated discovery and add separate full binding inspector if needed.
    - [ ] Or extend `:keymap` to support filtered/full modes without duplicating source data.
    - [ ] Ensure prefix-driven sequences like `g`, `z`, and `SPC` remain discoverable.
  - [ ] Keep descriptions trustworthy.
    - [ ] Reuse action or binding descriptions from real bindings where available.
    - [ ] Add explicit metadata only when binding tables lack enough human-readable text.

- [ ] Phase 4: validate drift resistance with user-config coverage.
  - Why: keymap help only solves real problem if custom config changes immediately reflect in help and picker output.
  - [ ] Add regression coverage.
    - [ ] Help output changes when user config overrides a default binding.
    - [ ] Help output includes user-added sequence bindings with descriptions.
    - [ ] Help output excludes removed or shadowed default bindings.
    - [ ] Built-in defaults still render expected high-value bindings when no config overrides exist.
  - [ ] Add edge-case coverage.
    - [ ] Conflicting bindings produce deterministic help output.
    - [ ] Mode-specific bindings stay scoped to correct help view.
    - [ ] Prefix/help discovery remains correct for nested sequences.

- [ ] Phase 5: optional follow-up UX cleanup.
  - Why: once keymap help derives from real bindings, richer discovery tooling becomes safer to build.
  - [ ] Evaluate next steps.
    - [ ] Decide whether command palette should also surface keybinding hints from same data model.
    - [ ] Decide whether key-hint footer, `:keymap`, and sequence-help popups should share one presentation layer.
    - [ ] Decide whether exporting effective keymaps for docs/tests is worth adding.
  - [ ] Keep scope bounded.
    - [ ] Do not mix this work with unrelated binding behavior changes.
    - [ ] Do not redesign keybinding UX until derived-data model lands first.


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
- [ ] Make sparse editing workable on VLF
- [ ] vsplit and hsplit problems on working with same file showing empty buffer
