# Issues

## xi-rpc improvements

### Transport and framing

- [x] Introduce a transport abstraction in `crates/xi-rpc` so the RPC layer is not hard-wired to stdio.
- [x] Replace newline-delimited message framing in `crates/xi-rpc/src/parse.rs` with explicit framing such as `Content-Length`, or implement that framing behind the new transport abstraction.
- [x] Flush the writer after each outbound RPC message in `crates/xi-rpc/src/lib.rs`.
- [x] Reduce writer lock scope in `RawPeer::send` by serializing the message before locking the writer.

### Runtime and concurrency

- [x] Fix `RawPeer::send_rpc_request` so `is_blocked` is always reset after a synchronous request completes.
- [x] Add a timeout-enabled synchronous request path so callers are not forced to block forever waiting for a response.
- [x] Add request cancellation support for pending RPCs and wire it through the pending response map.
- [x] Replace the `MAX_IDLE_WAIT` polling loop with a wake-up mechanism that does not require periodic 5 ms polling.
- [x] Review `needs_exit` atomic ordering and use stronger ordering where cross-thread shutdown visibility matters.
- [x] Replace panic-prone `lock().unwrap()` and similar synchronization unwraps with explicit error handling or clearer failure messages.
- [x] Surface scoped thread failures from `RpcLoop::mainloop` instead of unwrapping the scoped thread result.

### Protocol compliance and API cleanup

- [x] Support full JSON-RPC 2.0 request identifiers instead of limiting request ids to `u64`. Fully transition to JSON-RPC 2.0 and remove legacy 1.0 RPC.
- [x] Add the `jsonrpc: "2.0"` field to outbound requests and responses, or document and enforce the intentional protocol deviation in one place.
- [x] Evaluate replacing the hand-rolled RPC object parsing in `crates/xi-rpc` with `jsonrpc-lite` to reduce duplicate protocol code; track implementation under New World P1.
- [x] Add batch request handling support, or explicitly reject batch requests with a well-defined error response.
- [x] Replace mixed `u64` and `usize` request id handling with a single typed request id abstraction.
- [x] Tighten response validation in `crates/xi-rpc/src/parse.rs` so malformed objects with extra or missing fields are rejected consistently.
- [x] Stop disconnecting the whole RPC loop on unknown notifications in `crates/xi-rpc/src/lib.rs`; return structured errors for requests and ignore or log unknown notifications.
- [x] Make idle scheduling in `crates/xi-rpc` coalesce duplicate tokens under sustained producer load.
- [x] Add a cancellable timer API to `crates/xi-rpc` instead of token-only fire-and-forget scheduling.

### Error handling and observability

- [x] Review `RemoteError` mappings so invalid params, malformed requests, and unknown remote errors use consistent JSON-RPC error semantics.
- [x] Propagate outbound response write failures in a way callers can observe, instead of only logging them.
- [x] Replace legacy `extern crate` and `#[macro_use]` patterns in `crates/xi-rpc` with idiomatic Rust 2021 imports.
- [x] Evaluate migrating `crates/xi-rpc` instrumentation from `xi_trace` to `tracing` for more standard observability.

## New World

### P1. RPC and LSP protocol modernization

- [x] Keep xi-specific `Peer`, idle queue, timer, synchronous request, asynchronous callback, cancellation, and transport APIs in `crates/xi-rpc`; do not replace `xi-rpc` wholesale with `lsp-server`, `tower-lsp`, or `jsonrpc-core`.
- [x] Replace the hand-rolled JSON-RPC envelope parsing in `crates/xi-rpc/src/parse.rs` with `jsonrpc-lite` while preserving existing `RequestId`, `RemoteError`, `Handler`, `RpcLoop`, `ReadTransport`, and `WriteTransport` behavior.
- [x] Migrate `crates/xi-rpc/src/lib.rs` `RpcLoop::mainloop` from the `std::thread::scope` blocking-I/O model to a `tokio` runtime: replace the dedicated reader thread with `tokio::io` or `tokio::task::spawn_blocking` over the existing `ReadTransport` API, drive timers, idle queue, and incoming messages through a single `tokio::select!`, and delete the `MAX_IDLE_WAIT` polling constant.
- [x] Replace the single `is_blocked` flag in `crates/xi-rpc/src/lib.rs` `RawPeer` with a `tokio::sync::Semaphore` whose permit count is configurable per peer, so more than one outstanding plugin request can be in flight at once.
- [x] Thread a `tokio_util::sync::CancellationToken` through the `Handler::handle_request` signature in `crates/xi-rpc/src/lib.rs` and surface it in `crates/xi-plugin-lib/src/dispatch.rs` so plugin implementations can abort in-flight hover, completion, and diagnostic work when the originating editor request is cancelled.
- [x] Keep `crates/xi-lsp-lib` as the xi-specific LSP adapter layer; replace only its hand-rolled LSP framing and dispatch in `crates/xi-lsp-lib/src/parse_helper.rs` and `crates/xi-lsp-lib/src/language_server_client.rs` with `lsp-server`; do not use `tower-lsp` until `xi-lsp-lib` has moved to async; delete `parse_helper.rs` once the `lsp-server` transport is wired in.
- [x] Enforce a maximum LSP message body size in the new `lsp-server` transport path before accepting messages; do not keep `crates/xi-lsp-lib/src/parse_helper.rs` solely for body-size checks.
- [x] Preserve existing `xi-rpc` coverage and add migration regression tests for `jsonrpc-lite` request/response parsing, callback dispatch, idle queue ordering, timer ordering, cancellation, timeout behavior, and malformed responses.
- [ ] Add `xi-lsp-lib` migration regression tests using a fake language server for initialize/open/change/save/close/hover/diagnostics flows.

### 0. Backend and protocol foundations

- [x] Add tests for synchronous request completion, disconnect during a pending request, and timeout behavior.
- [x] Add tests for idle queue ordering and timer firing order in `crates/xi-rpc`.
- [x] Add tests that exercise write failures and malformed response handling in `crates/xi-rpc`.
- [x] Priority: retire `crates/xi-trace` in favor of `tracing`: add shared subscriber or layer setup for core and plugin processes, including explicit enable or disable control that replaces `xi_trace::enable_tracing`, `disable_tracing`, and `is_enabled`.
- [x] Replace all remaining `xi_trace::{trace, trace_payload, trace_block, trace_block_payload}` call sites in `crates/xi-core-lib` and `crates/xi-plugin-lib` with `tracing` spans or events plus structured fields.
- [x] Rework trace collection protocol around `TracingConfig`, `SaveTrace`, and `CollectTrace` in `crates/xi-core-lib/src/rpc.rs`, `crates/xi-core-lib/src/plugins/rpc.rs`, `crates/xi-core-lib/src/tabs.rs`, and `crates/xi-plugin-lib/src/dispatch.rs`: either reimplement cross-process export on top of `tracing` or remove the protocol entirely.
- [x] Remove stale `xi-trace` dependency edges that are not needed for runtime behavior, starting with `crates/xi-lsp-lib/Cargo.toml`, then prune remaining Cargo references from dependents as each crate finishes migration.
- [x] Delete workspace member `crates/xi-trace` and remove the workspace dependency from the root `Cargo.toml` only after the remaining instrumentation, trace export, and protocol paths no longer reference it.
- [x] Add a `manifest_version` field to plugin manifests and reject unsupported manifest schemas during load.
- [x] Normalize all relative plugin manifest paths against the manifest directory in `crates/xi-core-lib/src/plugins/catalog.rs`, not only paths starting with `./`.
- [x] Detect duplicate plugin names during catalog load and surface a structured load error instead of silently overwriting entries.
- [x] Replace `PluginCatalog::get_from_path` string `contains` matching in `crates/xi-core-lib/src/plugins/catalog.rs` with canonical path matching.
- [x] Add declared plugin capabilities to `PluginDescription`, such as edit, hover, annotations, status items, filesystem access, and network access.
- [x] Validate `PlaceholderRpc` command templates against declared command arguments when manifests load, instead of accepting arbitrary params blindly.
- [x] Implement manifest-driven activation behavior for `OnSyntax`, `OnCommand`, and `SingleInvocation` instead of leaving those modes partially defined.
- [x] Track plugin launches in progress in `crates/xi-core-lib/src/tabs.rs` so repeated start requests cannot race and spawn duplicate processes.
- [x] Make plugin process launch configuration extensible with manifest-controlled working directory, environment, and transport settings.
- [x] Capture plugin stderr and surface startup or runtime failures to logs or client-visible diagnostics.
- [x] Add graceful shutdown with timeout and forced child termination for plugins that ignore the shutdown notification.
- [x] Add plugin restart policy and crash backoff handling when a plugin exits unexpectedly.
- [x] Add plugin protocol capability negotiation during `initialize` so core and plugins can evolve features without lockstep upgrades.
- [x] Replace hard-coded plugin-name dispatch such as `xi-syntect-plugin` command routing in `crates/xi-core-lib/src/event_context.rs` with capability or command registry routing.
- [x] Implement `GetSelections` in `crates/xi-core-lib/src/event_context.rs` or remove it from the protocol until supported.
- [x] Return structured acknowledgements for plugin updates and edits instead of placeholder success values like `1`.
- [x] Add request cancellation support for long-running plugin features such as hover or analysis requests.
- [x] Add backpressure or coalescing for plugin update delivery so slow plugins cannot accumulate unbounded pending work.
- [x] Handle `shutdown` properly in `crates/xi-plugin-lib/src/dispatch.rs` so Rust plugins can terminate their main loop cleanly.
- [x] Replace `unwrap`-based config deserialization paths in `crates/xi-plugin-lib/src/dispatch.rs` and `crates/xi-plugin-lib/src/view.rs` with structured errors.
- [x] Expand `CoreProxy` in `crates/xi-plugin-lib/src/core_proxy.rs` with typed wrappers for all supported core-facing plugin RPCs, including a `request_is_pending` helper for non-`View` plugin paths.
- [x] Extend plugin requests with typed APIs for selections, diagnostics, formatting, code actions, and similar editor services instead of relying on ad hoc custom commands.
- [x] Add result-bearing edit APIs in `crates/xi-plugin-lib` so plugins can observe edit rejection or revision conflicts.
- [x] Remove the single-view assumption in `crates/xi-plugin-lib/src/view.rs` so global or multi-view plugins can be modeled directly.
- [x] Reconcile stale Python plugin protocol code in `python/xi_plugin` with current Rust protocol shapes, or mark the Python SDK as legacy and unsupported. DECISION: REMOVE THE PYTHON PLUGIN PROTOCOL
- [x] Add tests for manifest validation, duplicate plugin detection, and path normalization in `crates/xi-core-lib/src/plugins/catalog.rs`.
- [x] Add tests for multi-view plugin behavior and lifecycle events in `crates/xi-plugin-lib`.
- [x] Add integration tests that spawn real plugin processes and verify startup, shutdown, config updates, and crash handling.
- [x] Replace panic-prone `unwrap`/`expect` paths in `crates/xi-lsp-lib` request, response, and server startup flows with structured errors and client-visible failures where appropriate.
- [x] Track language server process stderr and surface startup or runtime failures from `crates/xi-lsp-lib` to logs or status items.
- [x] Add graceful shutdown and restart handling for language server child processes started in `crates/xi-lsp-lib/src/utils.rs`.
- [x] Add request timeouts or cancellation plumbing for long-running LSP requests such as hover, completion, and code actions.
- [x] Add tests for incremental sync conversion in `crates/xi-lsp-lib/src/utils.rs`, including insertions, deletions, selections, and full-document fallback cases.
- [x] Handle `textDocument/publishDiagnostics` in `crates/xi-lsp-lib/src/language_server_client.rs` and plumb diagnostics through the core/plugin protocol once diagnostic transport exists.
- [x] Add completion support to `crates/xi-lsp-lib`, including request/response mapping and result delivery back to the client once completion transport exists in core.
- [x] Add definition and reference navigation support to `crates/xi-lsp-lib`, including UTF-8/UTF-16 position conversion and multi-location responses.
- [x] Add formatting and code action support to `crates/xi-lsp-lib`, including edit application paths and conflict handling for stale revisions.

### 1. Core frontend architecture

- [x] Replace full-buffer `debug_get_contents` refresh in `crates/ee-tui/src/main.rs` with xi `update` operation application against a local line cache so redraw cost stays proportional to visible edits.
- [x] Stop deriving cursor position from local frontend guesses in `crates/ee-tui/src/main.rs` and instead drive caret and selection state from xi update payloads so motions, inserts, and selections never desynchronize.
- [x] Add an explicit viewport model to `crates/ee-tui` that tracks top line, left column, and cursor target column, and only renders visible content instead of the full buffer.
- [x] Send scroll and viewport updates between `ee-tui` and xi core so large files, wrapped lines, and off-screen edits do not require whole-buffer refreshes.
- [x] Replace ad hoc key handling match arms in `crates/ee-tui/src/main.rs` with a table-driven input dispatcher keyed by mode, prefix state, and count so advanced Vim-style commands can be added safely.
- [x] Move xi RPC processing in `crates/ee-tui` off the synchronous input path into a dedicated event loop so redraw, input, and backend notifications cannot stall each other. Current 100 ms poll + blocking RPC recv on edit blocks UI. Move xi RPC onto tokio task, drain on redraw tick.
- [x] Use display-width-aware cursor and layout measurement in `ee-tui` so tabs, wide Unicode, emoji, and combining characters render and navigate correctly.

### 2. Modal editing parity with VIM

- [x] Implement count parsing and command composition in normal mode so sequences such as `3j`, `2w`, `d2w`, and `3dw` execute with Vim-compatible order.
- [x] Add missing core motions in normal mode: word motions, line start and end motions, document motions, character find motions, matching-pair jump, and search result traversal.
- [x] Implement operator-pending mode with `delete`, `change`, `yank`, `indent`, `outdent`, `case transform`, and `format` operators that compose with motions and text objects.
- [x] Add text objects for words, sentences, paragraphs, quotes, brackets, braces, angle brackets, and tag-like pairs so edit commands can target structured text precisely.
- [x] Add full insert-entry variants `a`, `A`, `I`, `o`, `O`, `s`, and `S`, each mapped to the correct cursor movement and selection behavior before entering insert mode.
- [x] Expand insert mode editing controls to include word delete, line delete, indent and outdent, literal insertion, register paste, and completion triggers.
- [x] Add visual line mode and visual block mode, including anchor swap, last-selection restore, and block insert/append semantics.
- [x] Implement registers for unnamed, numbered, named, black-hole, search, expression, and system clipboard targets so yank, delete, change, and paste behave predictably.
- [x] Expose xi undo and redo history through Vim-style commands, including repeat-last-change `.` and persistent undo storage once file persistence layer is ready.
- [x] Implement marks, jump list, and change list navigation so users can move reliably across files and editing history.
- [x] Add macro recording and replay with named registers so repetitive edit workflows do not require custom scripting.

### 3. File, buffer, and window workflow

- [x] Add a real buffer manager in `ee-tui` with commands for open, alternate buffer, next or previous buffer, list buffers, and close buffer without tearing down the whole process.
- [x] Support multiple xi views at once in `ee-tui` so horizontal splits, vertical splits, and focused-window navigation can share one process cleanly.
- [x] Add tab-page style workspace grouping on top of split windows so users can keep separate editing contexts without losing buffer state.
- [x] Implement command-line ranges, command history, and completion in `ee-tui` so ex commands can address lines, selections, buffers, and files unambiguously.
- [x] Add file and buffer pickers for open file, recent file, live grep, buffer switch, and symbol jump so common navigation does not depend on raw ex commands alone.
- [ ] Add quickfix and location-list views in `ee-tui` and make them navigable from keyboard commands so search results, diagnostics, and build errors share one workflow.
- [ ] Implement safe file reload, external-change detection, unsaved-change prompts, and crash recovery artifacts so file workflows do not lose user work.

### 4. Display, discoverability, and editing ergonomics

- [ ] Add syntax highlighting integration for `ee-tui`, starting with existing xi syntax support, so code is readable before more advanced IDE features land.
- [ ] Replace removed `crates/experimental/xi-lang` with direct frontend syntax coloring. Phase 1: use in-tree `syntect` parsing and theming in `crates/ee-tui` for visible highlighting only, no plugin process.
- [ ] Phase 2: move language-sensitive edit features such as reindent and toggle-comment off hard-coded plugin dispatch in `crates/xi-core-lib/src/event_context.rs` onto typed capabilities backed by `syntect` or tree-sitter queries.
- [ ] Phase 3: evaluate tree-sitter for incremental parsing, folds, and indentation once viewport rendering and diagnostics transport are stable, instead of reviving xi-lang-style custom parser plugins.
- [ ] Support relative line numbers, sign column, cursor line, color column, visible whitespace, and configurable statusline content so screen layout matches established terminal-editor workflows.
- [ ] Implement line wrapping, horizontal scrolling, break indentation, and configurable scroll offsets so navigation remains stable in long and wrapped lines.
- [ ] Add fold state management and fold commands in `ee-tui`, starting with manual folds and then syntax- or indent-driven folds once parser support is available.
- [ ] Add search UI with incremental preview, highlighted matches, smart-case behavior, and repeat navigation so text discovery feels immediate and accurate.
- [ ] Implement substitution UX for `:s` with range support, flags, and optional confirmation so batch edits are safe and inspectable.
- [ ] Add mouse support, bracketed paste handling, and OSC 52 clipboard integration so terminal interaction works correctly both locally and over SSH.
- [ ] Provide built-in keymap and command discovery for supported motions, operators, and ex commands so new functionality remains learnable as parity grows.

### 5. IDE and ecosystem workflow integration

- [ ] Integrate `ee-tui` UI surfaces for diagnostics, completion menus, hover popups, references, rename prompts, formatting actions, and code actions once corresponding `xi-lsp-lib` backend support is available.
- [ ] Add symbol outline, workspace symbol jump, and definition or reference navigation UI in `ee-tui` so language navigation is usable without leaving terminal workflow.
- [ ] Add git-aware gutter signs, hunk navigation, blame display, and diff views so common source-control tasks are available without shelling out.
- [ ] Add embedded terminal buffers and shell-command execution flows so users can run builds, tests, and one-off commands without leaving the editor session.
- [ ] Add session persistence for open buffers, window layout, cursor positions, marks, jump history, and command history so longer-lived workflows restore cleanly.

### 6. Plugin runtime modernization

- [ ] Add a `runtime` field to `PluginDescription` in `crates/xi-core-lib/src/plugins/manifest.rs` accepting `"native"` (current subprocess) or `"wasm"`, and add a `wasmtime`-backed plugin host alongside `crates/xi-core-lib/src/plugins/mod.rs::start_plugin_process` that exposes the same `HostNotification`, `HostRequest`, `PluginRequest`, and `PluginNotification` surface over linear-memory buffers instead of stdin/stdout pipes.
- [ ] Add per-plugin syscall sandboxing in `crates/xi-core-lib/src/plugins/mod.rs::start_plugin_process`: on Linux apply a `seccompiler` filter (allow `read`, `write`, `futex`, `mmap`, `brk`; deny `fork`, `ptrace`, `socket`, raw `open*`); on Windows attach a Job Object with RSS and CPU caps; on macOS document the gap until a stable equivalent exists.
- [ ] Add manifest-driven per-plugin resource limits (`max_rss_bytes`, `max_cpu_seconds`, `rpc_timeout_ms`) parsed in `crates/xi-core-lib/src/plugins/manifest.rs` and enforced by the supervisor in `crates/xi-core-lib/src/plugins/mod.rs`; on breach, kill the child and emit a structured `plugin_terminated` notification with the breach reason.
- [ ] Split `Error::PeerDisconnect` in `crates/xi-rpc/src/error.rs` into `PeerExited { exit_status }`, `PeerTimedOut { after_ms }`, and `PeerProtocolError { reason }`, and update `RpcLoop::mainloop` and the plugin supervisor to populate the originating cause instead of collapsing all disconnects to one variant.
- [ ] Install a `std::panic::set_hook` in `crates/xi-plugin-lib/src/lib.rs::mainloop` that captures the panic payload and backtrace, serializes them into a `RemoteError` with custom code `-32099` (`PluginPanicked`), replies to the in-flight request before the process exits, and flushes the writer.
- [ ] Define a JSON Schema for `PluginDescription` in `crates/xi-core-lib/src/plugins/manifest.rs` and validate every manifest against it at catalog load time using the `jsonschema` crate; on failure return a structured catalog error containing the JSON pointer of the offending field.
- [ ] Add a `requires` field to `PluginDescription` accepting semver expressions for `xi-core` and other plugin names (for example `["xi-core>=0.4.0", "syntect>=0.2.0"]`), and resolve them during `PluginCatalog` load in `crates/xi-core-lib/src/plugins/catalog.rs`; reject the catalog with a structured error when a requirement is unsatisfied or when the dependency graph is cyclic.
- [ ] Add a new proc-macro crate `crates/xi-plugin-derive` providing `#[xi_plugin(syntax(lang = "..."))]` to auto-implement `Plugin` with the appropriate `Cache` selection, `#[derive(SpanType)]` for scope span builders, and a `xi_plugin::log!` facade that writes structured records to stderr prefixed with the plugin name; re-export it from `crates/xi-plugin-lib/src/lib.rs`.

## Code quality audit (xi-* crates)

### crates/xi-core-lib

- [x] Replace panic-prone unwraps in `crates/xi-core-lib/src/event_context.rs` (`sel_regions().last().unwrap()` at L169, `delta_rev_head().unwrap()` at L208, plugin lookup `unwrap()` at L243, `serde_json::to_value().unwrap()` chains at L324-325, config serialization unwraps at L486/L496) with explicit error handling.
- [x] Validate `Find` state at API boundary in `crates/xi-core-lib/src/find.rs` (L171, L195, L259, L261) instead of `search_string.as_ref().unwrap()`.
- [x] Replace `lock().unwrap()` mutex calls in `crates/xi-core-lib/src/watcher.rs` (L108, L160, L178, L221) with explicit poisoning recovery or descriptive `expect` messages.
- [x] Handle `create_if_missing` failure in `crates/xi-core-lib/src/layers.rs` (L73, L96) instead of `layers.get_mut(&layer).unwrap()`.
- [x] Replace `unreachable!()` at `crates/xi-core-lib/src/event_context.rs` L542 with structured error or non-exhaustive enum guard.
- [x] Audit path handling in `crates/xi-core-lib/src/file.rs` (L179 TODO) for non-UTF-8 paths via `OsStr`/`Path` APIs.
- [x] Resolve `\r` line ending TODO in `crates/xi-core-lib/src/word_boundaries.rs` L198.
- [x] Resolve combining-class TODO in `crates/xi-core-lib/src/backspace.rs` L152 for full Unicode combining char support.
- [x] Resolve outstanding config TODOs in `crates/xi-core-lib/src/config.rs` (L85, L313, L418, L498): legacy config name handling, missing plugin configs, incomplete update flow.
- [x] Migrate inconsistent error types per TODO in `crates/xi-core-lib/src/file.rs` L279.

### crates/xi-rope

- [x] Replace invariant `panic!()` calls in `crates/xi-rope/src/tree.rs` (L105, L259, L267, L283, L326) with `Result`/`Option` returns or `debug_assert!` plus typed errors.
- [x] Add bounds/None handling for boundary unwraps in `crates/xi-rope/src/tree.rs` L731 (`prev_leaf().unwrap()` and metric conversion unwraps).
- [x] Add invariant assertions or bounds checks in `TreeBuilder` loop at `crates/xi-rope/src/tree.rs` L503-L517 (`last_mut().unwrap()`, `pop().unwrap()`).
- [x] Document formal safety requirements (buffer length ≥ 16/32 bytes) on `pub unsafe fn` SIMD helpers in `crates/xi-rope/src/compare.rs` (L46, L70, L108, L132, L154).
- [x] Address tree-walking efficiency TODOs in `crates/xi-rope/src/tree.rs` (L101, L704, L729).
- [x] Evaluate `rayon` for `crates/xi-rope/src/diff.rs` `LineHashDiff::compute_delta` only: benchmark parallel base-line hashing and target-line match collection on large inputs, keep LIS and match expansion serial, adopt `rayon` only if wall-clock diff time improves materially without regressions on small files or interactive edit latency; otherwise reject and document why.

### crates/xi-unicode

- [x] Document the `extern crate alloc;` in `crates/xi-unicode/src/lib.rs` L18 (no_std rationale) or remove if unused.

### crates/xi-plugin-lib

- [x] Resolve single-view TODO at `crates/xi-plugin-lib/src/dispatch.rs` L59 with typed multi-view dispatch.
- [x] Replace `panic!("entry already exists")` in `crates/xi-plugin-lib/src/state_cache.rs` L189 with `Result` return.
- [x] Bounds-check `cached_offset_of_line()` result in `crates/xi-plugin-lib/src/base_cache.rs` L92 instead of `.unwrap() - self.offset`.
- [x] Validate inputs at API boundary in `crates/xi-plugin-lib/src/base_cache.rs` (L270, L390) instead of panicking on "offset greater than content length".

### Cross-cutting

- [x] Add doc comments documenting preconditions on public APIs in `crates/xi-core-lib` (`event_context.rs`, `editor.rs`, `layers.rs`).
- [ ] Unify error handling across xi-* crates: reduce mix of `FileError`, `RemoteError`, `Option`, and panics via shared error types or conversion traits.
- [ ] Identify xi-* source files exceeding the 1000-line module guideline from AGENTS.md and split into cohesive submodules.

## Code quality audit (second pass)

### Resource limits and DoS hardening

- [x] Bound total idle queue size in `crates/xi-rpc/src/lib.rs` (around L547-L550) in addition to token coalescing so distinct tokens cannot accumulate unboundedly.
- [x] Cap `recording_buffer` history in `crates/xi-core-lib/src/recorder.rs` (L29) with a max length or circular buffer.
- [x] Limit per-plugin annotation storage in `crates/xi-core-lib/src/annotations.rs` (L169-L195) so a misbehaving plugin cannot exhaust memory.
- [x] Bound or compact `IndexSet` ranges vector in `crates/xi-core-lib/src/index_set.rs` (L25) to prevent unbounded growth.
- [x] Validate `u32::try_from(end - start)` ranges in `crates/xi-lsp-lib/src/utils.rs` (L63, L91) instead of `.expect()` on potentially huge offsets from a malicious server.

### Persistence and durability

- [x] Add `sync_all()` (fsync) before/after rename in the atomic save path of `crates/xi-core-lib/src/file.rs` (L210-L225) to guarantee durability after crash.
- [x] Add an advisory file lock (`fs2::FileExt::try_lock_exclusive` or similar) in `crates/xi-core-lib/src/file.rs` (L100-L160) so concurrent editor instances cannot silently corrupt the same file.

### Terminal frontend (ee-tui) robustness

- [x] Install a panic hook in `crates/ee-tui` that disables raw mode and leaves the alternate screen so a panic does not leave the terminal unusable.
- [x] Handle `SIGWINCH`, `SIGINT`, and `SIGTERM` in `crates/ee-tui` for clean resize and shutdown via `signal-hook` or crossterm events.

### Concurrency profile

- [x] Evaluate replacing `Arc<Mutex<WatcherState>>` in `crates/xi-core-lib/src/watcher.rs` (L57) with `RwLock` for read-heavy paths.
- [x] Evaluate replacing `Arc<Mutex<CoreState>>` in `crates/xi-core-lib/src/core.rs` (L39) with `RwLock` for read-heavy paths.

### View / selection panics in xi-core-lib

- [x] Replace `.last().unwrap()` on `sel_regions()` at `crates/xi-core-lib/src/view.rs` L339 with empty-selection handling.
- [x] Replace `.first().unwrap()`/`.last().unwrap()` on selection regions at `crates/xi-core-lib/src/view.rs` L404-L405.
- [x] Replace `.split_last().unwrap()` on selection regions in selection drag at `crates/xi-core-lib/src/view.rs` L513.
- [x] Replace `.last_mut().unwrap()` on find state at `crates/xi-core-lib/src/view.rs` L1036.

### Plugin host edge cases (beyond manifest items)

- [x] Replace `child.stdin.take().unwrap()` / `child.stdout.take().unwrap()` in `crates/xi-core-lib/src/plugins/mod.rs` (L182-L183) with `ok_or` plus structured startup errors.
- [x] Replace `to_str().unwrap()` plugin-path conversions in `crates/xi-core-lib/src/plugins/rpc.rs` (L301-L302) with `OsStr`/`Path` APIs or explicit non-UTF-8 rejection.
- [x] Replace `path.parent().unwrap()` in `crates/xi-core-lib/src/plugins/catalog.rs` (L145, L150) with `ok_or` to handle root paths and resolve language config errors cleanly.
- [x] Replace `serde_json` and `Into` `.unwrap()` calls in `crates/xi-core-lib/src/plugins/manifest.rs` (L153, L179, L191, L243, L288, L292) with `?` propagation.

### LSP host edge cases (beyond items already tracked)

- [x] Replace `panic!("unexpected value for id: None")` and `.parse().expect()` id handling in `crates/xi-lsp-lib/src/language_server_client.rs` (L59-L60) with structured errors.
- [x] Replace `language_config.get_mut(...).unwrap()` and path/URI unwraps in `crates/xi-lsp-lib/src/lsp_plugin.rs` (L116, L119, L122, L135) with explicit failure paths.
- [x] Replace `serde_json::from_value(...).unwrap()` for server responses in `crates/xi-lsp-lib/src/lsp_plugin.rs` (L141, L168) with structured errors that surface to the client.
- [x] Redesign `process.stdin.take().unwrap()` (and matching stdout) in `crates/xi-lsp-lib/src/utils.rs` (L192) to return `Result` rather than relying on the "unwrap so the thread panics" pattern.

### Encoding

- [x] Add support (or explicit rejection with a clear error) for legacy `\r`-only line endings in `crates/xi-core-lib/src/line_ending.rs` (L46-L60).

### Configuration

- [x] Replace widespread `.unwrap()` chains across `crates/xi-core-lib/src/config.rs` (L199, L207, L221, L233, L236, L355, L367, L398-L410, L444, L495, L507-L509, L642, L744, L882, L897) with structured config errors that surface to clients.

### Tooling and CI

- [x] Add GitHub Actions workflows under `.github/workflows/` for build, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.
- [ ] Add `cargo-fuzz` targets for the rope delta/CRDT operations in `crates/xi-rope`, the JSON-RPC parser in `crates/xi-rpc`, and the LSP transport wrapper in `crates/xi-lsp-lib`.
- [ ] Add property-based tests (`proptest` or `quickcheck`) in `crates/xi-rope` for delta application, merging, and CRDT invariants.
