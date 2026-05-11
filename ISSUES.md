# Issues

## New World

### VLF Streaming Save Wiring

- Rules:
  - Reuse existing `VlfStore::stream_save`, `vlf::save::stream_save_pieces`, `VlfSavePolicy`, and `FileManager::save_vlf` as core save primitives. Do not add second VLF save pipeline unless existing API proves structurally insufficient.
  - Keep VLF write path chunk-native end-to-end. No phase may flatten VLF content into `String`, `Vec<String>`, or `Rope` just to save.
  - Keep durability policy aligned with current rope save path: temp-file rewrite or bounded in-place optimization, `sync_all`, atomic rename, and best-effort parent-directory sync.
  - Keep cancellation semantics explicit. Cancellation before commit deletes temp state and leaves original file intact; cancellation after commit is impossible and must never be reported as failure.
  - Save wiring must preserve VLF mode invariants: bounded memory, overlay as source of unsaved edits, and no hidden whole-buffer reload after save.
  - Prefer one shared async save orchestration model for rope and VLF where practical, but do not force premature abstraction if it obscures correctness.
  - Every phase must land with regression tests for success path, cancellation path, and failure path.

- [ ] Phase 0: freeze save architecture and decide ownership boundaries.
  - [ ] Document target save ownership across existing modules.
    - [ ] `crates/xi-core-lib/src/vlf/save.rs` owns piece streaming, durability policy, and optimization policy execution.
    - [ ] `crates/xi-core-lib/src/file.rs` owns path validation, external-modification checks, metadata refresh, and file-manager integration.
    - [ ] `crates/xi-core-lib/src/tabs.rs` owns command routing, async save kickoff, idle polling, alerts, and post-save UI/config updates.
    - [ ] `crates/xi-core-lib/src/editor.rs` owns dirty/pristine revision state and access to `VlfStore` when buffer is VLF-backed.
  - [ ] Define completion criteria for VLF save wiring.
    - [ ] `CoreNotification::Save` succeeds for editable VLF buffers instead of always alerting read-only.
    - [ ] VLF save and rope save both surface progress/cancellation through one coherent task/callback model.
    - [ ] Successful VLF save updates file metadata, pristine state, watcher state, and buffer path/config state correctly.
    - [ ] Save-as and policy-driven fallback paths are handled explicitly, not through placeholder `"<save-as-required>"` behavior leaking to UI.
    - [ ] Tests prove no save path materializes full VLF text.

- [ ] Phase 1: enable editable VLF save contract at editor and document-mode layer.
  - [ ] Remove unconditional VLF save disable path from document status and save command routing when editing is enabled.
    - [ ] Audit `VlfStore::edit_permission`, `doc_status`, and any disabled-feature lists that currently treat all VLF buffers as unsavable.
    - [ ] Distinguish read-only VLF from editable VLF in capability reporting.
      - [ ] Read-only VLF should still report save disabled with explicit reason.
      - [ ] Editable VLF should report save allowed while still advertising any other bounded-feature restrictions.
  - [ ] Wire `enable_editing()` and overlay save readiness to real editor lifecycle.
    - [ ] Decide where VLF editing becomes active for buffers allowed to edit.
    - [ ] Ensure enabling edit mode also marks overlay save gate ready only when streaming save path is fully wired and tested.
    - [ ] Preserve current behavior for read-only VLF opens until editable VLF policy intentionally opts in.
  - [ ] Align pristine/dirty state model for VLF edits.
    - [ ] Define what revision/token marks VLF buffer pristine before first edit.
    - [ ] Define what successful VLF save resets as pristine after commit.
    - [ ] Ensure dirty state comes from overlay/content changes, not from line-cache/view churn.

- [ ] Phase 2: route save commands through VLF-aware file-manager path.
  - [ ] Refactor `tabs.rs::do_save` to branch by backing storage, not by hardcoded read-only VLF alert.
    - [ ] Keep rope path using existing `prepare_rope_save` and `SaveTask` snapshot flow.
    - [ ] Add VLF path using `FileManager::save_vlf` and `VlfStore::stream_save`.
    - [ ] Surface clear alerts for unsupported cases: read-only VLF, save-as-required policy, external modification, invalid destination, or editing not enabled.
  - [ ] Decide save request representation for async execution.
    - [ ] Either extend existing save task enum/request types to carry rope and VLF save variants.
    - [ ] Or introduce a thin shared save-task wrapper that can execute either `PreparedRopeSave` or VLF save request without duplicating polling/callback logic.
    - [ ] Keep result payload rich enough to update metadata and pristine state after either save kind.
  - [ ] Make VLF policy selection explicit at command boundary.
    - [ ] Use `suggested_save_policy()` only as default policy selector.
    - [ ] Do not silently pass placeholder `SaveAs` path through command flow.
    - [ ] When policy requires explicit save-as target, route to save-as UX/protocol rather than attempting invalid write.

- [ ] Phase 3: unify async save execution, progress, and cancellation.
  - [ ] Extend current async save task model beyond rope snapshots.
    - [ ] Preserve current generation/stale-result protection used by rope save tasks.
    - [ ] Allow VLF save worker to report byte progress from `SaveProgress` without blocking render/input loop.
    - [ ] Ensure cancel checks map cleanly to VLF pre-commit cancellation semantics.
  - [ ] Decide worker-thread data ownership for VLF saves.
    - [ ] Do not move `VlfStore` itself to worker thread if that breaks main-thread ownership assumptions.
    - [ ] If needed, extract immutable save plan or cloneable save inputs from overlay/pager before spawning background work.
    - [ ] Keep plan bounded: piece list, inserted buffers, pager/file references, selected policy, and destination path only.
  - [ ] Surface progress to caller/UI.
    - [ ] Reuse existing alert/progress channel if one exists for rope save.
    - [ ] Otherwise add minimal generic save-progress callback path usable by both rope and VLF saves.
    - [ ] Report bytes written and total bytes; avoid fake line-based progress for VLF.
  - [ ] Add cancellation coverage.
    - [ ] Cancel before first chunk writes.
    - [ ] Cancel mid-stream during temp rewrite.
    - [ ] Verify committed save cannot later report cancellation.

- [ ] Phase 4: finish post-save state updates and file-system integration.
  - [ ] Update file-manager metadata after successful VLF save just like rope save.
    - [ ] Refresh modification time.
    - [ ] Clear external-change marker.
    - [ ] Reacquire or preserve advisory lock expectations for saved path.
  - [ ] Update editor/buffer state after successful VLF save.
    - [ ] Mark buffer pristine at saved overlay revision.
    - [ ] Keep VLF buffer open in VLF mode; do not reopen as rope or force whole-file reload.
    - [ ] Ensure overlay/source-of-truth state reflects that file on disk now matches current logical document.
      - [ ] Either reset overlay against new base file contents.
      - [ ] Or rebuild overlay/base metadata so subsequent edits and saves operate on new on-disk revision correctly.
  - [ ] Update tabs/config/view side effects after successful VLF save.
    - [ ] Apply save-as path changes to buffer config, language detection, and view notifications.
    - [ ] Emit same post-save hooks expected by plugins/LSP/config watchers when appropriate.
    - [ ] Ensure watcher-triggered reload path does not treat own successful VLF save as unexpected external change.
  - [ ] Handle failure and interruption cleanly.
    - [ ] Preserve dirty state after failed or cancelled save.
    - [ ] Keep partial temp files cleaned up on failure paths.
    - [ ] Avoid leaving save task stuck in progress after worker exits.

- [ ] Phase 5: wire save-as, policy overrides, and optimization fallback behavior.
  - [ ] Add explicit VLF save-as routing.
    - [ ] When overlay policy returns save-as-required, surface path-prompt flow instead of opaque error.
    - [ ] Ensure alternate destination keeps source file unchanged until successful commit.
    - [ ] After successful save-as, decide whether buffer continues tracking original path or new path and make behavior explicit.
  - [ ] Expose and validate optimization policies.
    - [ ] Keep `SameSizeInPlaceOverwrite` and `TailShift` behind policy validation, not implicit assumptions.
    - [ ] Fall back to temp-file rewrite automatically when optimization preconditions are not met.
    - [ ] Record tests for canonical-path mismatch, oversized changed-window fallback, and non-zero/zero delta mismatches.
  - [ ] Align user-facing messaging.
    - [ ] Alerts should distinguish save complete, save cancelled, save failed, and save-as required.
    - [ ] Do not reuse stale “VLF mode is read-only” alert once editable VLF save is wired.

- [ ] Phase 6: validate with focused regression, budget, and failure tests.
  - [ ] Add unit/integration coverage at save primitive level.
    - [ ] Temp rewrite success.
    - [ ] Same-size overwrite success.
    - [ ] Tail-shift success and fallback.
    - [ ] Cancellation before commit.
    - [ ] I/O failure cleanup.
  - [ ] Add core integration tests for save command routing.
    - [ ] `CoreNotification::Save` on read-only VLF still alerts correctly.
    - [ ] `CoreNotification::Save` on editable VLF schedules async save and finishes successfully.
    - [ ] Save-as-required path yields explicit prompt/error contract instead of invalid placeholder path write.
    - [ ] Successful VLF save updates pristine state and file metadata.
  - [ ] Add watcher/reload regression tests.
    - [ ] Saving VLF does not trigger self-reload loop.
    - [ ] Real external modification after VLF save is still detected.
  - [ ] Add budget and invariant tests.
    - [ ] Saving large edited VLF fixture stays within bounded memory cap.
    - [ ] Save path never calls full-text extraction or builds `Rope` from VLF contents.
    - [ ] Progress and cancellation tests do not require whole-buffer staging.
  - [ ] Close migration.
    - [ ] Mark older high-level VLF save checklist items complete only after command routing, async execution, save-as handling, and post-save state are all wired.
    - [ ] Add short note describing final architecture: `tabs` routes save, `file` validates and updates metadata, `vlf::save` streams pieces, and VLF buffers remain sparse before and after save.

### Large File Support: VLF Mode (Very Large Files)

- [ ] Improve large-buffer quit latency.
  - [ ] Exit event loop immediately after `handle_event` sets `should_quit`, before another expensive frame, scroll notification, source-control refresh, or external-change scan.
  - [ ] Add explicit fast app-shutdown path for `BufferManager` that stops reader/core threads without best-effort `close_view` cleanup work.
  - [ ] Keep interactive buffer-close commands (`:bc`, window close, tab close) on normal close semantics; use fast shutdown only for whole-app exit.
  - [ ] Avoid full-buffer close/render/plugin cleanup for large `ConstrainedNormal` buffers during app exit.
  - [ ] Re-evaluate exact-threshold behavior for 8 MiB fixtures: keep constrained-normal and ensure teardown stays non-blocking.
  - [ ] Add regression coverage proving `:q` on pristine large buffers does not save, does not close-buffer synchronously, and exits within quit budget.

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


### Optional Future Boundary Work

- [ ] Move text-object range resolution from `crates/ee-tui/src/app/mod.rs` into `xi-core-lib` if we want backend-owned semantic text objects across future frontends.
- [ ] Move visual-block delete/change/yank execution from `crates/ee-tui/src/app/mod.rs` into `xi-core-lib` so rectangular selection mutations become backend-owned editor semantics.
- [ ] Re-evaluate visual-block insert setup and replay split between `ee-tui` and `xi-core-lib`; keep frontend workflow glue only, move any remaining selection-truth or mutation semantics backend-side if reused by another frontend.
