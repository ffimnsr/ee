# Issues

## New World

### 5. IDE and ecosystem workflow integration

- [x] Priority P0: integrate `ee-tui` UI surfaces for diagnostics, completion menus, hover popups, references, rename prompts, formatting actions, and code actions through backend-owned, frontend-agnostic editor RPCs and notifications. Completion criteria: feature entrypoints in `ee-tui` use backend protocol only, and direct `xi-lsp-plugin` / `plugin_rpc` routing is removed from interactive IDE flows.
- [x] Add symbol outline, workspace symbol jump, and definition or reference navigation UI in `ee-tui` so language navigation is usable without leaving terminal workflow.
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

### Cross-cutting

- [x] Add doc comments documenting preconditions on public APIs in `crates/xi-core-lib` (`event_context.rs`, `editor.rs`, `layers.rs`).
- [ ] Unify error handling across xi-* crates: reduce mix of `FileError`, `RemoteError`, `Option`, and panics via shared error types or conversion traits.
- [ ] Identify xi-* source files exceeding the 1000-line module guideline from AGENTS.md and split into cohesive submodules.

### Tooling and CI

- [x] Add GitHub Actions workflows under `.github/workflows/` for build, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.
- [x] Add `cargo-fuzz` targets for the rope delta/CRDT operations in `crates/xi-rope`, the JSON-RPC parser in `crates/xi-rpc`, and the LSP transport wrapper in `crates/xi-lsp-lib`.
- [ ] Add property-based tests (`proptest` or `quickcheck`) in `crates/xi-rope` for delta application, merging, and CRDT invariants.
