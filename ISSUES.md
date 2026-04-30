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
- [x] Evaluate replacing the hand-rolled RPC object parsing in `crates/xi-rpc` with `jsonrpc-lite` to reduce duplicate protocol code. If its good implement it.
- [x] Add batch request handling support, or explicitly reject batch requests with a well-defined error response.
- [x] Replace mixed `u64` and `usize` request id handling with a single typed request id abstraction.
- [x] Tighten response validation in `crates/xi-rpc/src/parse.rs` so malformed objects with extra or missing fields are rejected consistently.
- [x] Stop disconnecting the whole RPC loop on unknown notifications in `crates/xi-rpc/src/lib.rs`; return structured errors for requests and ignore or log unknown notifications.
- [x] Make idle scheduling in `crates/xi-rpc` coalesce duplicate tokens or otherwise bound queue growth under sustained producer load.
- [x] Add a cancellable timer API to `crates/xi-rpc` instead of token-only fire-and-forget scheduling.

### Error handling and observability

- [x] Review `RemoteError` mappings so invalid params, malformed requests, and unknown remote errors use consistent JSON-RPC error semantics.
- [x] Propagate outbound response write failures in a way callers can observe, instead of only logging them.
- [x] Replace legacy `extern crate` and `#[macro_use]` patterns in `crates/xi-rpc` with idiomatic Rust 2021 imports.
- [x] Evaluate migrating `crates/xi-rpc` instrumentation from `xi_trace` to `tracing` for more standard observability.

### Test coverage

- [ ] Add tests for synchronous request completion, disconnect during a pending request, and timeout behavior.
- [ ] Add tests for idle queue ordering and timer firing order in `crates/xi-rpc`.
- [ ] Add tests that exercise write failures and malformed response handling in `crates/xi-rpc`.

## Plugin extensibility improvements

### Manifest and capability model

- [ ] Add a `manifest_version` field to plugin manifests and reject unsupported manifest schemas during load.
- [ ] Normalize all relative plugin manifest paths against the manifest directory in `crates/xi-core-lib/src/plugins/catalog.rs`, not only paths starting with `./`.
- [ ] Detect duplicate plugin names during catalog load and surface a structured load error instead of silently overwriting entries.
- [ ] Replace `PluginCatalog::get_from_path` string `contains` matching in `crates/xi-core-lib/src/plugins/catalog.rs` with canonical path matching.
- [ ] Add declared plugin capabilities to `PluginDescription`, such as edit, hover, annotations, status items, filesystem access, and network access.
- [ ] Validate `PlaceholderRpc` command templates against declared command arguments when manifests load, instead of accepting arbitrary params blindly.
- [ ] Implement manifest-driven activation behavior for `OnSyntax`, `OnCommand`, and `SingleInvocation` instead of leaving those modes partially defined.

### Lifecycle and process management

- [ ] Track plugin launches in progress in `crates/xi-core-lib/src/tabs.rs` so repeated start requests cannot race and spawn duplicate processes.
- [ ] Add plugin restart policy and crash backoff handling when a plugin exits unexpectedly.
- [ ] Capture plugin stderr and surface startup or runtime failures to logs or client-visible diagnostics.
- [ ] Add graceful shutdown with timeout and forced child termination for plugins that ignore the shutdown notification.
- [ ] Make plugin process launch configuration extensible with manifest-controlled working directory, environment, and transport settings.

### Host/plugin protocol and SDK

- [ ] Add plugin protocol capability negotiation during `initialize` so core and plugins can evolve features without lockstep upgrades.
- [ ] Replace hard-coded plugin-name dispatch such as `xi-syntect-plugin` command routing in `crates/xi-core-lib/src/event_context.rs` with capability or command registry routing.
- [ ] Extend plugin requests with typed APIs for selections, diagnostics, formatting, code actions, and similar editor services instead of relying on ad hoc custom commands.
- [ ] Implement `GetSelections` in `crates/xi-core-lib/src/event_context.rs` or remove it from the protocol until supported.
- [ ] Return structured acknowledgements for plugin updates and edits instead of placeholder success values like `1`.
- [ ] Add request cancellation support for long-running plugin features such as hover or analysis requests.
- [ ] Add backpressure or coalescing for plugin update delivery so slow plugins cannot accumulate unbounded pending work.

### Plugin SDK correctness

- [ ] Remove the single-view assumption in `crates/xi-plugin-lib/src/view.rs` so global or multi-view plugins can be modeled directly.
- [ ] Handle `shutdown` properly in `crates/xi-plugin-lib/src/dispatch.rs` so Rust plugins can terminate their main loop cleanly.
- [ ] Replace `unwrap`-based config deserialization paths in `crates/xi-plugin-lib/src/dispatch.rs` and `crates/xi-plugin-lib/src/view.rs` with structured errors.
- [ ] Expand `CoreProxy` in `crates/xi-plugin-lib/src/core_proxy.rs` with typed wrappers for all supported core-facing plugin RPCs.
- [ ] Add result-bearing edit APIs in `crates/xi-plugin-lib` so plugins can observe edit rejection or revision conflicts.
- [ ] Reconcile stale Python plugin protocol code in `python/xi_plugin` with current Rust protocol shapes, or mark the Python SDK as legacy and unsupported.

### Plugin test coverage

- [ ] Add integration tests that spawn real plugin processes and verify startup, shutdown, config updates, and crash handling.
- [ ] Add tests for manifest validation, duplicate plugin detection, and path normalization in `crates/xi-core-lib/src/plugins/catalog.rs`.
- [ ] Add tests for multi-view plugin behavior and lifecycle events in `crates/xi-plugin-lib`.

## xi-lsp-lib improvements

### Editor feature coverage

- [ ] Handle `textDocument/publishDiagnostics` in `crates/xi-lsp-lib/src/language_server_client.rs` and plumb diagnostics through the core/plugin protocol once diagnostic transport exists.
- [ ] Add completion support to `crates/xi-lsp-lib`, including request/response mapping and result delivery back to the client once completion transport exists in core.
- [ ] Add definition and reference navigation support to `crates/xi-lsp-lib`, including UTF-8/UTF-16 position conversion and multi-location responses.
- [ ] Add formatting and code action support to `crates/xi-lsp-lib`, including edit application paths and conflict handling for stale revisions.

### Process and protocol robustness

- [ ] Replace panic-prone `unwrap`/`expect` paths in `crates/xi-lsp-lib` request, response, and server startup flows with structured errors and client-visible failures where appropriate.
- [ ] Track language server process stderr and surface startup or runtime failures from `crates/xi-lsp-lib` to logs or status items.
- [ ] Add graceful shutdown and restart handling for language server child processes started in `crates/xi-lsp-lib/src/utils.rs`.
- [ ] Add request timeouts or cancellation plumbing for long-running LSP requests such as hover, completion, and code actions.

### Test coverage

- [ ] Add tests for `Content-Length` message parsing in `crates/xi-lsp-lib/src/parse_helper.rs`, including malformed headers and truncated bodies.
- [ ] Add tests for incremental sync conversion in `crates/xi-lsp-lib/src/utils.rs`, including insertions, deletions, selections, and full-document fallback cases.
- [ ] Add integration tests that run a fake language server and verify initialize, open/change/save/close, hover, and diagnostics flows in `crates/xi-lsp-lib`.
