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

- [ ] Backlog goals beyond current scope.
  - [ ] Revisit trusted-only native grammar loading if runtime grammar execution moves to a sandboxed or wasmtime format / non-native format.
  - [ ] Revisit one-effective-owner-per-file-type if language detection grows beyond extension matching into ranked per-buffer resolution.

### Auto Indent + Smart Indent

- Rules:
  - Keep newline indentation semantics owned by backend in `xi-core-lib`; frontend should trigger newline commands, not reimplement indentation policy.
  - Make `auto_indent` existing config authoritative for baseline newline indentation. Do not add duplicate toggle for same behavior.
  - Treat smart indent as additive on top of baseline auto indent. When syntax-aware indentation unavailable, editor must fall back cleanly to plain auto-indent or plain newline depending on config.
  - Reuse existing tab/indent settings such as `translate_tabs_to_spaces`, `tab_size`, and current indent helpers before adding new formatting logic.
  - Preserve deterministic multi-cursor and selection behavior. Newline indentation must produce stable results regardless of cursor count or selection order.
  - Keep large-file and degraded-mode behavior fast. Do not require whole-buffer parse or expensive synchronous query work on every Enter keypress.
  - Tree-sitter may provide structural signals, but `ee` must own final indentation decisions, fallback rules, and failure handling.
  - Every phase must land with regression tests, including disabled-config and parser-unavailable cases.

- [x] Phase 0: freeze auto-indent and smart-indent behavior contract.
  - Why: current config already exposes `auto_indent`, but newline path ignores it. Need exact semantics before wiring behavior through editor core.
  - [x] Define baseline newline behavior.
    - [x] Confirm `auto_indent = false` keeps current plain newline insertion semantics.
    - [x] Confirm `auto_indent = true` copies leading whitespace from current logical line on Enter.
    - [x] Define behavior for caret inside indentation, mid-line Enter, end-of-line Enter, and newline with active non-caret selections.
  - [x] Define smart-indent boundaries.
    - [x] Decide whether smart indent ships under new `smart_indent` config or remains implicit behind syntax availability at first.
    - [x] Confirm syntax-aware indentation never blocks basic editing when parser, runtime, or query assets are missing.
    - [x] Decide initial supported behaviors: indent-after-opener only, dedent-before-closer, brace-pair expansion, alignment, or narrower MVP.

- [x] Phase 1: implement baseline auto indent using existing config.
  - Why: highest-value missing behavior is simple indent carry-forward on Enter, and existing `auto_indent` field should start working before any syntax-aware work.
  - [x] Wire `auto_indent` into newline edit path.
    - [x] Update newline handling in `crates/xi-core-lib/src/edit_ops.rs` to copy current line leading whitespace when `auto_indent` is enabled.
    - [x] Preserve current line-ending behavior and selection replacement semantics.
    - [x] Reuse existing tab/space policy helpers so copied plus added indentation stays consistent with buffer settings.
  - [x] Add baseline regression coverage.
    - [x] Plain newline remains unchanged when `auto_indent = false`.
    - [x] Leading whitespace copies correctly for spaces, tabs, and mixed-indentation source lines.
    - [x] Multi-cursor Enter and selection-replacement Enter produce deterministic results.

- [x] Phase 2: add heuristic smart-indent fallback without tree-sitter dependency.
  - Why: simple structural heuristics deliver immediate value and provide fallback when syntax runtime unavailable.
  - [x] Add bounded syntax-agnostic indentation heuristics.
    - [x] Increase one indent level after trailing opener tokens such as `{`, `[`, or `(` when appropriate.
    - [x] Optionally reduce indentation when newline created before closing tokens such as `}`, `]`, or `)`.
    - [x] Keep heuristic scope narrow and predictable; do not add language-specific guesswork yet.
  - [x] Keep fallback behavior explicit.
    - [x] Heuristics should layer on top of baseline copied indentation, not replace it.
    - [x] When heuristics do not match, editor should fall back to copied-indent behavior only.

- [ ] Phase 3: design and load tree-sitter indent query assets.
  - Why: long-term smart indent should use same runtime grammar/query architecture instead of hardcoded per-language indentation logic.
  - [ ] Extend runtime query model.
    - [ ] Define minimal indent-query contract and capture vocabulary for `indent.scm` or equivalent runtime asset.
    - [ ] Extend runtime loader in `crates/xi-core-lib` to discover and cache indent query assets alongside existing query types.
    - [ ] Keep missing indent-query assets isolated so languages without support still edit normally.
  - [ ] Preserve architecture boundaries.
    - [ ] Reuse existing language resolution and query directory precedence rules.
    - [ ] Do not introduce a second grammar or query loading path just for indentation.

- [ ] Phase 4: implement syntax-aware smart indent evaluation.
  - Why: tree-sitter gives structural context, but editor still needs backend logic that converts captures and syntax position into concrete indent edits.
  - [ ] Add backend indent engine.
    - [ ] Introduce backend-owned indent evaluation module in `crates/xi-core-lib` that computes newline indentation from syntax tree context plus buffer settings.
    - [ ] Support at least inherit-indent, indent-one-level, and dedent-one-level outcomes for MVP.
    - [ ] Fail closed to heuristic or baseline auto-indent when parse state incomplete, query missing, or language unsupported.
  - [ ] Wire editor context into newline command.
    - [ ] Pass enough syntax/runtime context from editor layer to newline path without pushing parser ownership into frontend.
    - [ ] Keep text mutation logic separate from syntax-query evaluation so newline edits remain testable in isolation.

- [ ] Phase 5: validate behavior, config, and mode-specific fallbacks.
  - Why: indentation features are high-frequency editing paths; must prove correctness, performance, and non-support behavior before broadening language coverage.
  - [ ] Add regression and failure-path coverage.
    - [ ] Baseline auto-indent tests for plain text and non-code buffers.
    - [ ] Smart-indent tests for at least Rust, JSON, and one indentation-sensitive language only if query semantics are ready.
    - [ ] Missing parser, missing indent query, and disabled config all fall back without panic or stale indentation artifacts.
    - [ ] Multi-cursor and selection cases remain deterministic under smart-indent path too.
  - [ ] Validate mode/performance constraints.
    - [ ] Confirm large or constrained buffers avoid expensive whole-file syntax work on Enter.
    - [ ] Define whether VLF or parser-disabled modes use baseline auto-indent only, heuristic smart-indent, or explicit unsupported status.
    - [ ] Document config semantics and supported smart-indent behavior in user-facing docs once implementation lands.
