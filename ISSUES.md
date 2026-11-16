# ISSUES

# Cordis-Style Plugin System v2 Implementation Plan

Reference: [plugin-system-cordis.md](docs/upgrades/plugin-system-cordis.md)

## Execution Overview

**Goal:** implement the v2-only Cordis-style plugin system described in `docs/upgrades/plugin-system-cordis.md`, delete the old plugin system, and keep editor editing/VLF semantics unchanged.

**Overview:** each phase has one write scope and one runnable acceptance gate. Phases 2, 3, 5, and 6 can run in parallel after Phase 0/1 contracts land. Phase 4 needs Phase 2 and Phase 3. Phase 7 is the final cutover and runs last. No phase adds a compatibility shim, v1 manifest parser, or old RPC type.

**Fixed decisions already made (do not re-litigate):**

- v2 only: `manifest_version = 2`, `api = "ee-plugin/2"`.
- New crates: `ee-plugin-api`, `ee-plugin-runtime`, `ee-plugin-sdk`, `ee-plugin-derive`.
- Remove `xi-plugin-lib`, `xi-plugin-derive`, `xi-core-lib/src/plugins`, `plugin_rpc`, `plugin_manifest`, `PluginCatalog`, `CoreProxy`, and old plugin lifecycle code.
- Read services direct; mutations return `Effect` values; plugin callbacks never run inside a core borrow.
- Frontend is not a plugin host. It renders only declarative contributions and sends `command` / `ui_event` back.
- Colors and themes are frontend-owned. Contributions carry semantic style roles, never RGB/ANSI.
- Tree-sitter grammar/query runtime stays backend infrastructure.
- Native and Wasm plugins are capability-gated. Untrusted plugins use Wasm.
- Tests use `tempfile`, owned child processes, and no fixed user config paths.

**Parallel map:**

| Phase | Can start after | Write scope | Blocks |
|---|---|---|---|
| 0 Contracts | now | new plugin crates + root `Cargo.toml` | 1, 3 |
| 1 Runtime | 0 | `crates/ee-plugin-runtime/**` | 2, 4, 5, 6 |
| 2 Editor host | 1 | `crates/xi-core-lib/src/plugin_runtime/**` + listed core wiring files | 4, 7 |
| 3 Frontend | 0 | `crates/ee-cli/src/plugin_ui/**` + listed frontend files | 4, 7 |
| 4 LSP migration | 2, 3 | `crates/xi-lsp-lib/**` + `crates/ee-plugin-sdk/src/client/**` | 7 |
| 5 Agent host | 1 | `crates/ee-agent-orchestrator/**` + provider crates | 7 |
| 6 Isolation | 1 | `crates/ee-plugin-runtime/src/{native,wasm,health}.rs` + SDK Wasm + fuzz | 7 |
| 7 Cutover | 2, 3, 4, 5, 6 | deletions, packaging, schemas, release gates | — |

## Phase 0 — Contracts and Workspace Skeleton

**Goal:** create the shared crates and freeze the v2 contracts so all other phases can compile against stable types.

**Overview:** this phase adds `ee-plugin-api`, `ee-plugin-derive`, and the initial `ee-plugin-sdk`; it does not touch `xi-core-lib`, `ee-cli`, `ee-agent-orchestrator`, or `xi-lsp-lib`. Every public contract from `docs/upgrades/plugin-system-cordis.md` §4–§8 lands here, with unit tests.

**Rules:**

- `ee-plugin-api` must not depend on `xi-core-lib`, `ee-cli`, `ee-agent-orchestrator`, or `xi-lsp-lib`.
- `ee-plugin-sdk` may depend on `ee-plugin-api` and `xi-rpc` only.
- Use stable names from the reference document. Do not introduce `Legacy*`, `Compat*`, `V1*`, or `Old*` types.
- Every new module must stay below 1,000 LOC.
- Async support is feature-gated (`async`) using `futures`; sync hosts must not pull Tokio.
- Schema generation is deterministic and checked in.

**Work items:**

- [x] Add the four workspace crates and dependencies.
  - [x] Add `crates/ee-plugin-api/Cargo.toml` with `serde`, `serde_json`, `schemars`, `semver`, `thiserror`, `futures` (optional `async` feature).
  - [x] Add `crates/ee-plugin-runtime/Cargo.toml` with `ee-plugin-api`, `serde`, `serde_json`, `toml`, `jsonschema`, `semver`, `sha2`, `tempfile` (dev), and later optional `wasmtime`.
  - [x] Add `crates/ee-plugin-sdk/Cargo.toml` with `ee-plugin-api`, `xi-rpc`, `serde`, `serde_json`, `tokio-util` (wasm target gated later).
  - [x] Add `crates/ee-plugin-derive/Cargo.toml` with `proc-macro2`, `quote`, `syn` full features.
  - [x] Add all four under `[workspace].members` and `[workspace.dependencies]` in `Cargo.toml`.
  - [x] Add `ee-plugin-api` tests to the crate root with `#![forbid(unsafe_code)]`.
- [x] Implement `ee-plugin-api/src/ids.rs`.
  - [x] Add `ServiceId`, `CommandId`, `PluginId`, `PluginInstanceId`, `ScopeId`, and `ContributionId` newtypes.
  - [x] Implement `FromStr`, `Display`, `Serialize`, `Deserialize`, `JsonSchema`, and ordering for each newtype.
  - [x] Add `ServiceId::new("editor.document", 1)` that renders `editor.document@1`.
  - [x] Add tests for invalid empty ids, invalid version handling, display/parse round trips, and JSON round trips.
- [x] Implement `ee-plugin-api/src/model.rs` with neutral editor types.
  - [x] Add `Range`, `SelectionRange`, `Diagnostic`, `DiagnosticSeverity`, `Hover`, `TextEdit`, `FormattingOptions`, `CompletionSuggestion`, `CodeAction`, `SymbolItem`, `NavigationTarget`, `PluginBufferInfo`, `PluginUpdate`, and `EditRequest`.
  - [x] Copy the field shapes used by `xi_core_lib::plugin_rpc` so editor semantics are preserved, but rename no fields.
  - [x] Add `serde`/`schemars` derives and `#[serde(rename_all = "snake_case")]` where the old wire shape used it.
  - [x] Add tests for every model type with a JSON fixture and a schema snapshot.
- [x] Implement `ee-plugin-api/src/service.rs`.
  - [x] Add `Service` trait with `const ID: &'static str` and `ServiceKey<T>`.
  - [x] Add `ServiceRegistry::register`, `get`, `require`, `remove`, and `contains`.
  - [x] Reject duplicate `ServiceId` with a typed error.
  - [x] Reject `ServiceKey<T>` lookup when the registered `TypeId` does not match `T`.
  - [x] Add tests for duplicate registration, type mismatch, missing service, and removal.
- [x] Implement `ee-plugin-api/src/disposer.rs`.
  - [x] Add `Disposer` as a `Box<dyn FnOnce() + Send + 'static>` wrapper with `new`, `dispose`, and `is_disposed`.
  - [x] Add `DisposerStack` that runs disposers in reverse registration order.
  - [x] Add `#[must_use]` on `Disposer` and tests for LIFO order, double-dispose, and panic isolation.
- [x] Implement `ee-plugin-api/src/event.rs`.
  - [x] Add `Event` trait with `const NAME: &'static str` and `TypeId` lookup.
  - [x] Add `EventBus` methods `on`, `emit`, `waterfall`, `serial`, and `bail`.
  - [x] Add `AsyncEventBus` behind `async` with `emit`, `waterfall`, `parallel`, `serial`, and `bail`.
  - [x] Implement registration order, `prepend`, listener caps, dispatch depth cap, and panic isolation.
  - [x] Add tests for order, prepend, waterfall short-circuit, bail winner, serial last value, parallel completion, and cancellation.
- [x] Implement `ee-plugin-api/src/context.rs`.
  - [x] Add `PluginContext` with `service`, `require`, `on`, `effect`, `command`, `contribute`, and child scope creation.
  - [x] Add `scope()` and `with_config()` accessors.
  - [x] Add tests that child scopes inherit parent services and dispose children LIFO on scope drop.
- [x] Implement `ee-plugin-api/src/manifest.rs`.
  - [x] Add `ManifestV2`, `PluginKind`, `HostTarget`, `Activation`, `Requires`, `Provides`, `Permissions`, `Limits`, `LaunchConfig`, `ConfigSchema`, and contribution tables.
  - [x] Implement validation for `manifest_version == 2`, `api == "ee-plugin/2"`, non-empty name/version, known targets/kinds, unique contribution ids, and required service syntax.
  - [x] Implement `manifest_schema() -> serde_json::Value` using `schemars`.
  - [x] Add a test that writes `schemas/plugin-manifest-v2.json` and compares it byte-for-byte with the checked-in file.
  - [x] Add tests for valid native, valid Wasm, valid builtin, invalid version, invalid api, duplicate command id, and invalid permission name.
- [x] Implement `ee-plugin-api/src/contribution.rs`, `effect.rs`, `wire.rs`, `host.rs`, and `error.rs`.
  - [x] Add `Contribution` variants: command, ex command, keybinding, status item, decoration provider, panel, overlay, picker, menu item, language, formatter, LSP server, agent tool, model provider, policy rule, context provider, config schema.
  - [x] Add `Effect` variants: apply edit, selection, scroll, status, remove status, decorations, panel, overlay, picker, task, agent tool result, noop.
  - [x] Add wire enums for `plugin/initialize`, `plugin/event`, `plugin/command`, `plugin/contribution`, `plugin/request`, `plugin/effect`, `plugin/health`, `plugin/response`, and `plugin/shutdown`.
  - [x] Add `PluginError` with stable diagnostic classes and `HostKind` / `HostAdapter` trait.
  - [x] Add serde tests for every wire variant and contribution variant.
- [x] Implement `ee-plugin-derive`.
  - [x] Replace `#[xi_plugin]` with `#[plugin]` generating an `ee_plugin_api::Plugin` impl from an `apply` method.
  - [x] Add a compile test for a plugin with `apply(&mut self, ctx: &PluginContext)`.
  - [x] Add a compile-fail test for a missing `apply` method.
- [x] Implement the initial `ee-plugin-sdk` surface.
  - [x] Add `run(plugin: impl Plugin)` entry point for native plugins.
  - [x] Add `PluginConfig`, `CommandRequest`, `CommandResponse`, and `ServiceCall` helper types.
  - [x] Add `DocumentCache` and `ViewState` skeletons with tests for delta application and revision rejection.
  - [x] Add a fake transport used only by SDK tests; do not add a host dependency.
- [x] Add schema and contract tests.
  - [x] Add `schemas/plugin-manifest-v2.json` generated from `ManifestV2`.
  - [x] Add a test that `manifest_schema()` equals the checked-in schema.
  - [x] Add a test that service ids in the reference catalog parse and are unique.

**Acceptance criteria:**

- `cargo test --quiet -p ee-plugin-api` passes.
- `cargo test --quiet -p ee-plugin-derive` passes, including compile-fail coverage.
- `cargo test --quiet -p ee-plugin-sdk` passes.
- `cargo tree -p ee-plugin-api` contains no `xi-core-lib`, `ee-cli`, `ee-agent-orchestrator`, or `ee-xi-lsp-lib`.
- `schemas/plugin-manifest-v2.json` is generated and the snapshot test passes.
- `cargo fmt --all --check` passes for new crates.

## Phase 1 — Runtime Loader and Lifecycle

**Goal:** implement `ee-plugin-runtime` as a host-agnostic loader that can discover, validate, resolve, load, reload, and dispose fake/in-process plugins.

**Overview:** this phase owns only `crates/ee-plugin-runtime/**`. It exposes `PluginManager`, `HostAdapter`, activation, config layering, health, and diagnostics. Native process transport begins here with the new wire protocol; sandbox hardening lands in Phase 6.

**Rules:**

- Runtime must not depend on editor, frontend, agent, or LSP crates.
- Manifests other than v2 are rejected; do not parse v1.
- Activation order is deterministic: topological by service dependency, then plugin name.
- Duplicate service providers, cycles, missing required services, duplicate contribution ids, and unknown targets fail that plugin only.
- Reload must build a candidate generation and swap atomically; failed reload keeps the old generation.
- Tests use `tempfile::TempDir` and unique plugin ids; no fixed config dirs.

**Work items:**

- [x] Implement `discovery.rs`.
  - [x] Scan bundled, user, workspace, client-extra, and `EE_PLUGIN_PATH` roots for `manifest.toml`.
  - [x] Read each manifest and compute a SHA-256 origin hash over manifest bytes plus entrypoint bytes.
  - [x] Reject duplicate plugin names across roots with both paths in the diagnostic.
  - [x] Add tests for recursive discovery, duplicate names, missing manifest, unreadable directory, and origin hash stability.
- [x] Implement `config.rs`.
  - [x] Load host, workspace, plugin, and runtime override layers.
  - [x] Implement `${env:NAME}`, `${host:config_dir}`, `${host:workspace}`, and `${plugin:dir}` interpolation with an allowlist.
  - [x] Reject `${...}` expressions outside the allowlist and reject unresolved variables.
  - [x] Validate plugin config against the manifest `config.schema` using `jsonschema`.
  - [x] Add tests for precedence, interpolation, secret-free rejection, schema failure, and config reload.
- [x] Implement `resolver.rs`.
  - [x] Build a service provider graph from `[provides].services` and `[requires].services`.
  - [x] Detect cycles and missing required services; allow missing optional services.
  - [x] Produce a deterministic activation order and a stable diagnostic list.
  - [x] Add tests for satisfied dependencies, missing required service, optional service, cycle, duplicate provider, and deterministic ordering.
- [x] Implement `activation.rs`.
  - [x] Evaluate `on_startup`, `on_language`, `on_command`, `on_event`, and scope targets.
  - [x] Treat `on_command`/`command` scope as ephemeral with no update stream.
  - [x] Add tests for each activation condition and scope lifetime.
- [x] Implement `manager.rs`.
  - [x] Add `PluginManager::new(adapter, config)` and `start`, `stop`, `reload`, `status`, and `diagnostics`.
  - [x] Load builtins first, then discover external plugins, then activate in resolver order.
  - [x] Dispose listeners, commands, contributions, and services LIFO on stop/reload.
  - [x] Keep the old generation live when a candidate reload fails.
  - [x] Add tests for start/stop, reload success, reload rollback, crash restart backoff, and generation swap.
- [x] Implement `health.rs`.
  - [x] Add health states `loaded`, `blocked`, `degraded`, `crashed`, `disabled`, and `reloaded`.
  - [x] Add bounded crash counters, restart backoff, and typed health reasons.
  - [x] Add tests for backoff growth, stable uptime reset, resource breach transition, and health snapshot.
- [x] Implement `diagnostics.rs`.
  - [x] Add stable diagnostic classes: manifest, schema, dependency, permission, transport, contribution, config, resource, runtime.
  - [x] Include plugin name, origin, phase, and bounded message.
  - [x] Add snapshot tests for each diagnostic class.
- [x] Implement `native.rs` new wire transport.
  - [x] Spawn `entry` with `[launch]` args, cwd, env, and `stdio_newline` / `stdio_content_length`.
  - [x] Send `plugin/initialize`; handle `plugin/request`, `plugin/effect`, `plugin/contribution`, `plugin/health`, and `plugin/response`.
  - [x] Enforce RPC deadline, stderr cap, exit status, and terminate-on-shutdown.
  - [x] Add a fake native plugin test binary under `crates/ee-plugin-runtime/tests/fixtures/` that echoes events and commands.
  - [x] Add tests for initialize, command round trip, event delivery, malformed response, timeout, and shutdown.
- [x] Implement `builtin.rs`.
  - [x] Load builtin descriptors from a generated catalog and call `Plugin::apply` in-process.
  - [x] Register builtin services before external plugin resolution.
  - [x] Add a test builtin plugin that provides and consumes a service.
- [x] Add runtime integration tests.
  - [x] Test a two-plugin dependency chain activates in order and disposes in reverse.
  - [x] Test a failed dependent does not abort unrelated plugins.
  - [x] Test contribution conflict rejects only the conflicting plugin.
  - [x] Test workspace plugin blocked without trust and loaded with trust.

**Acceptance criteria:**

- `cargo test --quiet -p ee-plugin-runtime` passes.
- `cargo tree -p ee-plugin-runtime` contains no editor, frontend, agent, or LSP dependency.
- Fake native plugin tests cover initialize, event, command, effect, contribution, health, timeout, and shutdown.
- Reload rollback test proves the old generation remains active after a candidate failure.
- All runtime tests use `tempfile::TempDir` and leave no child process running.

## Phase 2 — Editor Host Services

**Goal:** integrate `ee-plugin-runtime` into `xi-core-lib` and expose the `editor.*` services without changing editing, undo, selection, revision, or VLF semantics.

**Overview:** this phase owns `crates/xi-core-lib/src/plugin_runtime/**` and the listed wiring files. It uses fake in-process plugins for tests. The old plugin system remains until Phase 7 so the old LSP plugin can still run during parallel development; no new feature may use it.

**Rules:**

- Plugin mutations are `Effect` values applied after the `EventContext` borrow ends.
- Rev/undo/selection/VLF/file consistency stay core-owned.
- No plugin callback may run while `CoreState` holds a mutable borrow.
- `editor.document.changed` is per committed delta; external delivery coalesces.
- VLF checks run before command dispatch and before effect application.
- New files stay below 1,000 LOC; do not grow `event_context/dispatch.rs` with plugin logic.

**Work items:**

- [x] Add `crates/xi-core-lib/src/plugin_runtime/mod.rs` and `host.rs`.
  - [x] Add `EditorPluginHost` owning `PluginManager`, builtin service registry, contribution registry, and diagnostics.
  - [x] Add `EditorPluginHost::start(config, client, weak_core)` and `stop`.
  - [x] Add `EditorPluginHost::reload_paths(paths)` and `status_snapshot()`.
  - [x] Add tests for start/stop/reload with a fake plugin manager.
- [x] Implement `services/document.rs`.
  - [x] Add `DocumentService` read methods: snapshot, revision, length, line count, line/range text cap, document text cap.
  - [x] Add `DocumentEditService::apply(EditRequest) -> Result<EditAck>` that routes through the existing plugin edit path.
  - [x] Reject stale revisions, oversized edits, and VLF mutations with typed errors.
  - [x] Add tests for snapshot, range caps, stale revision, undo group propagation, and VLF rejection.
- [x] Implement `services/views.rs` and `services/selection.rs`.
  - [x] Add view lookup, active view, viewport, scroll request, and reveal request.
  - [x] Add selection read and selection-set effects through existing `View` APIs.
  - [x] Add tests for multiple views on one buffer, sibling views, scroll request, and selection clamping.
- [x] Implement `services/commands.rs`.
  - [x] Add command registry with unique ids, builtin commands, plugin commands, and aliases.
  - [x] Add deterministic dispatch, argument schema validation, timeout, cancellation, and effect collection.
  - [x] Add tests for duplicate id, missing command, schema rejection, timeout, cancellation, and VLF gate.
- [x] Implement `services/syntax.rs`.
  - [x] Expose read-only scopes, folds, symbols, and grammar availability through existing core APIs.
  - [x] Add tests for scope extraction, fold ranges, symbols, and VLF read-only behavior.
- [x] Implement `services/lsp.rs`.
  - [x] Add typed methods for hover, completion, definition, references, formatting, code actions, and rename preview.
  - [x] Keep protocol handling inside `xi-lsp-lib`; core only routes service calls.
  - [x] Add tests with a fake LSP service for request/response and timeout.
- [x] Implement `services/decorations.rs`.
  - [x] Add decoration provider registration and collection through `editor.decorations.collect`.
  - [x] Validate revision, range order/bounds, declared kind, style role, count, and byte cap.
  - [x] Merge providers by priority and store merged view state.
  - [x] Add tests for valid patch, stale revision, invalid range, unknown style, quota breach, and LIFO removal.
- [x] Implement `services/ui.rs`.
  - [x] Add contribution registry for status items, panels, overlays, pickers, menu items, keybindings, and ex commands.
  - [x] Add `editor.ui.render` handling with provider deadline and last-good content.
  - [x] Add tests for unique ids, unsupported renderer, provider timeout, last-good retention, and dynamic update.
- [x] Implement `services/tasks.rs` and `services/fs.rs`.
  - [x] Add cancellable plugin tasks with timeout and bounded output.
  - [x] Add scoped file read/write/stat/list with workspace root and glob checks.
  - [x] Add tests for task cancellation, timeout, path escape rejection, and permission denial.
- [x] Implement `events.rs`, `effects.rs`, and `bridge.rs`.
  - [x] Define typed editor events from `docs/upgrades/plugin-system-cordis.md` §7.1.
  - [x] Implement effect application for edit, selection, scroll, status, decorations, panel, overlay, picker, task, and noop.
  - [x] Hook `after_edit`, save, view open/close, config change, language change, and shutdown.
  - [x] Add tests for event order, effect batch application, no core borrow across callback, and panic isolation.
- [x] Wire editor language contributions.
  - [x] Apply `contributes.languages` and `contributes.lsp_servers` before dependent views initialize.
  - [x] Rebuild language map on plugin reload and notify the frontend.
  - [x] Add tests for language contribution, override precedence, and reload notification.
- [x] Add editor host status/health notifications.
  - [x] Emit `plugin_status` and `plugin_health` through `Client`.
  - [x] Include blocked workspace plugins and permission diagnostics.
  - [x] Add tests for loaded, blocked, degraded, crashed, and reloaded states.
- [x] Add editor host integration tests.
  - [x] Fake in-process plugin registers a command, reads a snapshot, applies a safe edit, and publishes status/decoration contributions.
  - [x] Fake plugin attempts a VLF edit and receives a typed rejection.
  - [x] Fake plugin panics in an event listener; core continues and reports health.
  - [x] Two plugins provide/consume `editor.document@1`; activation order is deterministic.

**Acceptance criteria:**

- `cargo test --quiet -p xi-core-lib plugin_runtime` passes.
- Existing `xi-core-lib` editing, undo, selection, and VLF tests still pass.
- Fake plugin tests cover command, edit, VLF rejection, decoration, status, task, and panic isolation.
- `cargo test --quiet -p xi-core-lib` passes.
- No new file exceeds 1,000 LOC.

## Phase 3 — Frontend Contribution Registry

**Goal:** implement the `ee-cli` contribution registry, protocol parsing, rendering, keymap/palette/ex integration, and capability negotiation without loading plugin code.

**Overview:** this phase owns `crates/ee-cli/src/plugin_ui/**` and the listed frontend protocol/keymap/command files. It can be developed against fake `plugin_contributions` payloads before Phase 2 lands.

**Rules:**

- Frontend never reads a plugin manifest, executable, Wasm module, or plugin config.
- Frontend never executes plugin callbacks.
- Contribution application is atomic per revision; malformed payload keeps the previous registry.
- Sanitize text, cap sizes, reject control/ANSI sequences, and reject duplicate ids.
- Colors and theme remain frontend-owned; contributions carry semantic style roles only.
- Unsupported contribution kinds are reported, not silently applied.

**Work items:**

- [x] Add `crates/ee-cli/src/plugin_ui/` module and wire it into `lib.rs`.
- [x] Implement `contributions.rs`.
  - [x] Add typed parsing for `plugin_contributions`, `plugin_ui_update`, `plugin_status`, and `plugin_health`.
  - [x] Add `ContributionBatch` with `revision`, `upsert`, and `remove`.
  - [x] Reject malformed JSON, unknown kinds, duplicate ids, empty ids, oversized text, and invalid key specs.
  - [x] Add snapshot tests for valid and invalid payloads.
- [x] Implement `sanitize.rs`.
  - [x] Add ANSI/control-sequence rejection, line-count caps, byte caps, and markdown escaping policy.
  - [x] Add tests for ANSI, C0/C1 controls, oversized text, too many items, and valid Unicode.
- [x] Implement `registry.rs`.
  - [x] Add registries for commands, ex commands, keybindings, status items, panels, overlays, pickers, and menu items.
  - [x] Add atomic revision apply and remove behavior.
  - [x] Add owner tracking for diagnostics and removal.
  - [x] Add tests for upsert, remove, duplicate id, revision ordering, stale revision, and owner removal.
- [x] Implement `render.rs` for declarative UI.
  - [x] Render status items in the existing status bar layout.
  - [x] Render panels with list/text/markdown renderers and placement fallback.
  - [x] Render overlays with bounded anchors and clipping.
  - [x] Render pickers with local filter/selection and column metadata.
  - [x] Add snapshot tests for each renderer and unsupported renderer fallback.
- [x] Implement `keybindings.rs`.
  - [x] Add `Action::PluginCommand(String)` and `Action::PluginCommandWithArgs(String, serde_json::Value)`.
  - [x] Parse/format `plugin_command:<id>` and `plugin_command:<id>:<json>`.
  - [x] Merge plugin defaults below user keybindings; reject conflicting defaults deterministically.
  - [x] Add tests for parse/format round trip, user override, conflict, and unknown command.
- [x] Update command palette and ex-command resolution.
  - [x] Add contributed commands to `command_help_items()` with category and usage.
  - [x] Add contributed ex aliases before static `COMMAND_SPECS`; duplicate alias fails validation.
  - [x] Add tests for palette row, filtered search, ex alias, duplicate alias, and missing args usage.
- [x] Add protocol senders and interaction routing.
  - [x] Send `command { id, args }` for palette/keymap/ex invocation.
  - [x] Send `ui_event` for panel/picker/overlay filter/select/activate/close.
  - [x] Send frontend capabilities in `client_started`.
  - [x] Add tests with a recording backend channel for each method.
- [x] Replace old lifecycle parsing.
  - [x] Remove `BackendEvent::AvailablePlugins` and plugin label parsing paths used only for old notifications.
  - [x] Parse `plugin_status` and `plugin_health` into displayable diagnostics.
  - [x] Add tests for blocked workspace plugin, permission failure, crash, timeout, and reload.
- [x] Add end-to-end frontend tests with a fake backend.
  - [x] Fake backend sends command, status item, panel, overlay, picker, keybinding, and ex-command contributions.
  - [x] Assert rendered status, panel, overlay, picker, palette row, keybinding action, and ex alias.
  - [x] Fake backend sends malformed revision; assert previous registry remains.
  - [x] Fake backend sends unsupported renderer; assert diagnostic and no render.

**Acceptance criteria:**

- `cargo test --quiet -p ee-cli plugin_ui` passes.
- `cargo test --quiet -p ee-cli --features agents` passes for touched tests.
- Fake backend tests cover every contribution kind and every protocol method.
- Malformed payload never replaces the previous registry.
- No plugin manifest, binary, or config path is read by `ee-cli` in tests.

## Phase 4 — LSP Plugin Migration

**Goal:** migrate `xi-lsp-lib` to the v2 SDK and remove all hardcoded LSP command routing from core and frontend.

**Overview:** this phase owns `crates/xi-lsp-lib/**` and `crates/ee-plugin-sdk/src/client/**`. It depends on Phase 2 and Phase 3. After this phase the bundled LSP plugin is the reference v2 plugin.

**Rules:**

- No `xi_plugin_lib::Plugin`, `CoreProxy`, `View`, `ChunkCache`, or old RPC type may remain in `xi-lsp-lib`.
- Command ids from `docs/upgrades/plugin-system-cordis.md` §10.2 are stable and must match keymap/ex targets.
- Status, decorations, diagnostics, hover, completions, code actions, locations, and symbols use v2 services/effects.
- User `[lsp.servers]` overrides remain frontend-owned and win over contributed defaults.
- No `501` formatting/code-action path remains.

**Work items:**

- [x] Implement `ee-plugin-sdk/src/client/`.
  - [x] Add `DocumentClient`, `ViewsClient`, `CommandsClient`, `SyntaxClient`, `LspClient`, `UiClient`, and `TasksClient` over the v2 wire protocol.
  - [x] Add `DocumentCache` delta apply, revision checks, and line/range fetch.
  - [x] Add `ViewState` selection, viewport, and language tracking.
  - [x] Add tests with a fake host transport for each client call.
- [x] Rewrite `crates/xi-lsp-lib/manifest.toml`.
  - [x] Set `manifest_version = 2`, `api = "ee-plugin/2"`, `targets = ["editor"]`, `kind = "native"`.
  - [x] Declare services `editor.document@1`, `editor.views@1`, `editor.selection@1`, `editor.commands@1`, `editor.lsp@1`, `editor.decorations@1`, `editor.ui@1`.
  - [x] Declare permissions for document read, decoration write, UI write, LSP write, filesystem read, and network as needed by existing LSP behavior.
  - [x] Declare command contributions for `lsp.completion`, `lsp.hover`, `lsp.declaration`, `lsp.definition`, `lsp.type_definition`, `lsp.references`, `lsp.implementation`, `lsp.document_symbols`, `lsp.workspace_symbols`, `lsp.format_document`, `lsp.code_actions`, `lsp.rename`, and agent-tool commands.
  - [x] Declare ex aliases and keybinding defaults currently used by LSP actions.
  - [x] Add a manifest validation test that parses the file through `ManifestV2`.
- [x] Rewrite the LSP plugin entry point.
  - [x] Replace `impl Plugin for LspPlugin` in `lsp_plugin/plugin_impl.rs` with `impl ee_plugin_sdk::Plugin` and `apply`.
  - [x] Subscribe to `editor.document.changed`, `editor.document.saved`, `editor.view.opened`, `editor.view.closed`, `editor.language.changed`, and `editor.config.changed`.
  - [x] Register command handlers for every contributed command.
  - [x] Replace `CoreProxy` status calls with `UiClient` status contribution effects.
  - [x] Replace `CoreProxy` edit/data calls with `DocumentClient` / `CommandsClient`.
- [x] Migrate LSP support modules.
  - [x] Update `conversion_utils.rs` to use `ee_plugin_api::model` types.
  - [x] Update `document_symbols.rs` to use `DocumentCache` and `SymbolItem`.
  - [x] Update `language_server_client.rs` to use `UiClient` and v2 status contributions.
  - [x] Update agent-tool request handlers to return v2 effects and `agent_tool_result` payloads.
  - [x] Add tests for conversion, symbols, status failure, and agent-tool replies.
- [x] Remove hardcoded LSP routing.
  - [x] Remove LSP `EditNotification` variants from `xi-core-lib/src/rpc.rs`.
  - [x] Remove `DispatchPluginCommand` mappings for LSP commands from `xi-core-lib/src/edit_types.rs`.
  - [x] Remove `Action::RequestCompletion`, `RequestHover`, `RequestDeclaration`, `RequestDefinition`, `RequestTypeDefinition`, `RequestReferences`, `RequestImplementation`, `RequestDocumentSymbols`, `RequestWorkspaceSymbols`, and `RequestCodeActions` from `ee-cli`.
  - [x] Map those actions to `Action::PluginCommand("lsp.*")` in defaults, vim, spec parse/format, and hints.
  - [x] Replace `ee-cli/src/backend.rs` request helpers with generic `command` requests.
  - [x] Add tests asserting the old names no longer serialize and the new ids dispatch.
- [x] Add LSP regression tests.
  - [x] Test hover, completion, declaration, definition, type definition, references, implementation, document symbols, workspace symbols, formatting, code actions, rename, and agent-tool requests.
  - [x] Test missing language server returns an empty result without hanging.
  - [x] Test status failure and recovery through UI contributions.
  - [x] Test formatting and code actions no longer return `501`.

**Acceptance criteria:**

- `cargo test --quiet -p xi-lsp-lib` passes.
- `cargo test --quiet -p ee-cli` passes for LSP integration tests.
- `rg -n 'impl Plugin for LspPlugin|CoreProxy|request_completion|format_document|request_code_actions' crates/xi-lsp-lib crates/ee-cli crates/xi-core-lib/src/edit_types.rs` returns no old plugin-routing matches.
- All LSP commands are registered through the manifest/command registry.
- The bundled LSP plugin loads through `PluginManager` using the v2 manifest.

## Phase 5 — Agent Host

**Goal:** integrate the v2 plugin host into `ee-agent-orchestrator` and expose `agent.*` services while preserving policy, approval, budget, cache, retry, and replay behavior.

**Overview:** this phase owns `crates/ee-agent-orchestrator/**` and the provider crates. It can run in parallel with Phases 2, 3, 4, and 6 after Phase 1. It does not touch editor UI or LSP.

**Rules:**

- Mandatory policy, approval, workspace trust, destructive-operation safeguards, budget, and write transaction checks run before plugin listeners.
- Plugin tools execute through the exiscan we implement phase 5ting `ToolExecutor` pipeline; plugins do not bypass cache, retries, or evidence checks.
- Session-scoped contributions dispose at session close; turn-scoped contributions dispose at turn end.
- Agent plugins cannot write editor UI directly; they return structured tool results.
- Replay fixtures remain deterministic with and without plugin registration.

**Work items:**

- [x] Add `crates/ee-agent-orchestrator/src/plugin_runtime/` and `host.rs`.
  - [x] Add `AgentPluginHost` owning `PluginManager`, service registry, session scopes, and diagnostics.
  - [x] Add `start`, `session_scope`, `turn_scope`, `stop`, and `status`.
  - [x] Add tests for host start/stop, session scope disposal, and turn scope disposal.
- [x] Implement `services/tools.rs`.
  - [x] Wrap `ToolRegistry` with service registration/removal and list definitions.
  - [x] Register builtin tools through the service path.
  - [x] Reject duplicate tool names and session-scope mismatches.
  - [x] Add tests for register, remove, duplicate, session scope, and builtin registration.
- [x] Implement `services/models.rs`.
  - [x] Wrap `ModelRegistry` and `ModelRouter` adapter registration.
  - [x] Add model metadata and routing hints.
  - [x] Add tests for adapter registration, route resolution, missing adapter, and duplicate id.
- [x] Implement `services/policy.rs`.
  - [x] Add `agent.tool.before` and `agent.approval.before` bail listeners.
  - [x] Ensure mandatory deny/safeguard rules run before plugin listeners.
  - [x] Add tests for plugin allow, plugin deny, mandatory deny override, and approval non-bypass.
- [x] Implement `services/context.rs`, `services/strategies.rs`, `services/validation.rs`.
  - [x] Add bounded context providers with trust labels and byte caps.
  - [x] Add strategy candidates with deterministic host selection.
  - [x] Add validation planners/metadata with existing executor routing.
  - [x] Add tests for context cap, trust labeling, strategy selection, validation routing, and provenance.
- [x] Implement `services/sessions.rs`.
  - [x] Add session lifecycle events and session-scoped contributions.
  - [x] Add cancellation, checkpoint, and close handling.
  - [x] Add tests for create, cancel, close, checkpoint, and disposal.
- [x] Implement `events.rs`, `effects.rs`, and `bridge.rs`.
  - [x] Emit `agent.turn.*`, `agent.model.*`, `agent.tool.*`, `agent.approval.*`, `agent.context.*`, `agent.strategy.*`, `agent.subagent.*`, `agent.validation.*`, and `agent.session.*` from existing boundaries.
  - [x] Apply effects for tool results, context updates, and session control.
  - [x] Add tests for event order, bail precedence, effect application, and cancellation.
- [x] Wire provider crates.
  - [x] Update `ee-agent-openrouter` to construct `AgentPluginHost` and register its model adapter.
  - [x] Update `ee-agent-opencode` to construct `AgentPluginHost` and register its model adapter.
  - [x] Update `ee-agent-host` to forward plugin health/status to the frontend.
  - [x] Add tests for provider construction and adapter registration.
- [x] Bridge MCP and remote ACP into the service path.
  - [x] Register MCP tools through `agent.tools` with prompt-scoped disposal.
  - [x] Keep MCP policy, approval, redaction, and manifest gating.
  - [x] Keep ACP provider tools as remote transports, not in-process plugins.
  - [x] Add tests for prompt-scoped MCP registration, removal, policy denial, and redaction.
- [x] Add replay and determinism tests.
  - [x] Run existing replay fixtures with no plugins and with a fake agent plugin.
  - [x] Assert identical event trace, stop reason, and tool outcomes when the plugin is observational.
  - [x] Assert deterministic event trace when a plugin adds an allow-listed context provider.

**Acceptance criteria:**

- `cargo test --quiet -p ee-agent-orchestrator` passes.
- `cargo test --quiet -p ee-agent-openrouter` passes.
- `cargo test --quiet -p ee-agent-opencode` passes.
- `cargo test --quiet -p ee-agent-host` passes.
- Mandatory approval and destructive policy tests prove plugin listeners cannot bypass host rules.
- Replay fixtures remain deterministic.

## Phase 6 — Native and Wasm Isolation

**Goal:** finish the native process supervisor and Wasm SDK with capability-gated imports, resource limits, and fuzz coverage.

**Overview:** this phase owns `crates/ee-plugin-runtime/src/{native,wasm,health}.rs`, `crates/ee-plugin-sdk/src/wasm.rs`, and `fuzz/fuzz_targets/xi_plugin_wasm.rs`. It can run in parallel with Phases 2–5 after Phase 1.

**Rules:**

- Native isolation uses Linux seccomp, Windows Job Objects, and best-effort process limits elsewhere.
- Wasm is the required tier for untrusted plugins; native is for trusted installs.
- Wasm imports exist only for declared permissions. No ambient filesystem, network, process, or clock access.
- Every call has a fuel/epoch deadline, memory cap, output cap, and cancellation path.
- Filesystem and network permissions use scoped host brokers; they do not blanket-disable the sandbox.
- Fuzz targets must not require network, user home, or real plugin binaries.

**Work items:**

- [x] Harden `native.rs`.
  - [x] Move seccomp and Windows Job Object code from `xi-core-lib/src/plugins/mod.rs` into `ee-plugin-runtime/src/native.rs`.
  - [x] Add RSS and CPU monitors with termination reasons.
  - [x] Add filesystem/network brokers or explicit seccomp allowlists for declared permissions.
  - [x] Add tests for RSS breach, CPU breach, RPC timeout, stderr cap, and shutdown timeout.
- [x] Implement `wasm.rs`.
  - [x] Add `wasmtime` engine, module loading, memory, and capability-gated linker.
  - [x] Export `plugin_alloc`, `plugin_dealloc`, `plugin_apply`, `plugin_event`, `plugin_command`, and `plugin_contribution`.
  - [x] Import `host_event`, `host_command`, `host_service_call`, `host_contribute`, and `host_health`.
  - [x] Add fuel/epoch timeout, memory cap, contribution cap, and trap handling.
  - [x] Add tests for valid module, missing export, invalid import, timeout, memory cap, trap, and denied ambient access.
- [x] Implement `ee-plugin-sdk/src/wasm.rs`.
  - [x] Add the Wasm guest entry point and host import bindings.
  - [x] Add a compile test for a `wasm32-unknown-unknown` example plugin.
  - [x] Add a runtime test loading the example through `ee-plugin-runtime`.
- [x] Replace the fuzz target.
  - [x] Rename `fuzz/fuzz_targets/xi_plugin_wasm.rs` to `ee_plugin_wasm.rs`.
  - [x] Fuzz manifest parse, initialize payload, event payload, and command payload.
  - [x] Update `.github/workflows/fuzz.yml` matrix and any fuzz registration file.
  - [x] Add a local smoke test that runs the fuzz target with a bounded input.
- [x] Add isolation regression tests.
  - [x] Untrusted Wasm plugin cannot read a file outside its declared broker scope.
  - [x] Untrusted Wasm plugin cannot open a socket.
  - [x] Wasm plugin exceeds fuel and is terminated.
  - [x] Wasm plugin exceeds memory and is terminated.
  - [x] Native plugin without permission cannot read workspace files.
  - [x] Plugin crash or trap does not crash the host.

**Acceptance criteria:**

- `cargo test --quiet -p ee-plugin-runtime` passes with Wasm tests enabled.
- `cargo test --quiet -p ee-plugin-sdk` passes for the Wasm example.
- `cargo fuzz build ee_plugin_wasm` succeeds.
- Isolation tests prove denied filesystem, network, memory cap, fuel timeout, and trap containment.
- No ambient host capability is available to an undeclared Wasm import.

## Phase 7 — Cutover, Deletion, Packaging, and Release Gates

**Goal:** delete the old plugin system, update packaging and test harnesses, and leave a v2-only release tree.

**Overview:** this phase runs last and owns deletions, workspace wiring, release/install/fuzz scripts, test helpers, schemas, and repository-wide gates. It must not add compatibility shims.

**Rules:**

- No v1 manifest parser, old plugin trait, old RPC type, `CoreProxy`, `PluginCatalog`, or legacy lifecycle code remains.
- No old plugin crate or workspace member remains.
- Bundled LSP plugin is the reference v2 plugin.
- Release and install scripts package v2 manifests only.
- Every deletion is covered by a repository grep gate.
- Full CI-equivalent validation must pass before this phase is complete.

**Work items:**

- [x] Delete old plugin crates and modules.
  - [x] Delete `crates/xi-plugin-lib/**`.
  - [x] Delete `crates/xi-plugin-derive/**`.
  - [x] Delete `crates/xi-core-lib/src/plugins/**`.
  - [x] Delete `crates/xi-core-lib/src/tabs/{plugins,plugin_lifecycle,plugin_events}.rs`.
  - [x] Delete `crates/xi-core-lib/src/event_context/plugins.rs`.
  - [x] Delete old plugin fields from `CoreState`, `EventContext`, and `make_context`.
  - [x] Delete `CoreNotification::Plugin`, `CorePluginNotification`, and old plugin control RPC.
  - [x] Delete `Client::{plugin_started, plugin_stopped, plugin_terminated, available_plugins}` and `ClientPluginInfo`.
  - [x] Delete `xi_core_lib::{plugin_manifest, plugin_rpc}` re-exports.
- [x] Clean workspace and dependencies.
  - [x] Remove `xi-plugin-lib` and `xi-plugin-derive` from root `Cargo.toml` members and workspace dependencies.
  - [x] Remove `wasmtime`, `seccomp`, and `windows-sys` from `xi-core-lib/Cargo.toml` after Phase 6 moves them.
  - [x] Add `ee-plugin-runtime` to `ee-cli` for plugin diagnostics only.
  - [x] Remove any old plugin imports from `ee-cli`, `xi-lsp-lib`, and provider crates.
- [x] Update packaging and installers.
  - [x] Update `scripts/install-plugins.sh` to install v2 manifest and native/Wasm entry layout.
  - [x] Update `install.sh` `install_plugin_assets` to validate `manifest_version = 2` before copying.
  - [x] Update `.github/workflows/release.yml` build and package steps for the v2 plugin layout.
  - [x] Update `.github/workflows/fuzz.yml` target name and matrix.
  - [x] Update any `tasks.yaml` entries that reference old plugin paths.
- [x] Update test harnesses.
  - [x] Update `ee-cli/src/tests/helpers.rs` `install_test_lsp_plugin` to copy the v2 manifest.
  - [x] Replace old `PluginDescription` construction in `ee-cli/src/tests/notifications.rs` with v2 status/contribution fixtures.
  - [x] Replace `probe_running_plugins` / `PluginCatalog` tests in `ee-cli/src/plugins_cmd.rs` with runtime diagnostics tests.
  - [x] Update `ee-cli/src/tests/commands.rs` for `ee do plugins list/ls` diagnostics output.
  - [x] Remove `test_assets` or fixture references that depend on old plugin manifests.
- [x] Update CLI plugin management.
  - [x] Implement `ee do plugins list` using `PluginManager::diagnostics()`.
  - [x] Implement `ee do plugins validate [path]` using manifest v2 validation.
  - [x] Implement `ee do plugins schema` writing `schemas/plugin-manifest-v2.json`.
  - [x] Add CLI tests for list, validate, schema, blocked workspace plugin, and invalid v1 manifest rejection.
- [x] Regenerate schemas and snapshots.
  - [x] Regenerate `schemas/ee-config.schema.json` with the new `[plugins]` section.
  - [x] Generate `schemas/plugin-manifest-v2.json` from `ManifestV2`.
  - [x] Update snapshots for contribution payloads, diagnostics, and LSP manifest.
- [x] Run deletion gates.
  - [x] Assert `test ! -d crates/xi-plugin-lib`.
  - [x] Assert `test ! -d crates/xi-plugin-derive`.
  - [x] Assert no `manifest_version = 1`, `xi_plugin`, `xi-plugin-lib`, `plugin_rpc`, `HostNotification`, `PluginNotification`, `CoreProxy`, or `PluginCatalog` in `crates`, `scripts`, `fuzz`, `.github`, or `Cargo.toml`.
  - [x] Assert no old LSP `Action::Request*` or `EditNotification` LSP variants remain.
- [x] Run full validation.
  - [x] `cargo fmt --all --check`.
  - [x] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`.
  - [x] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`.
  - [x] `./scripts/test-workspace-summary.sh`.
  - [x] `cargo test --quiet -p ee-plugin-api`.
  - [x] `cargo test --quiet -p ee-plugin-runtime`.
  - [x] `cargo test --quiet -p xi-core-lib`.
  - [x] `cargo test --quiet -p ee-agent-orchestrator`.
  - [x] `cargo test --quiet -p ee-cli --features agents`.
  - [x] `cargo test --quiet -p xi-lsp-lib`.
  - [x] `cargo fuzz build ee_plugin_wasm`.
- [x] Add final regression gates.
  - [x] Assert core editing, undo, selection, and VLF tests pass unchanged.
  - [x] Assert LSP hover/completion/definition/references/formatting/code actions/symbols pass through v2 registration.
  - [x] Assert agent replay fixtures remain deterministic.
  - [x] Assert a v2 plugin can register a command, status item, panel, decoration, agent tool, and model adapter.
  - [x] Assert a v1 manifest is rejected with a typed diagnostic.
  - [x] Assert `tasks.yaml` `lf` reports no new file over 1,000 LOC.

**Acceptance criteria:**

- All deletion gates pass with zero matches.
- `cargo fmt --all --check`, clippy, docs, and workspace tests pass.
- `./scripts/test-workspace-summary.sh` passes.
- LSP and agent regression suites pass through v2 registration only.
- `schemas/plugin-manifest-v2.json` and `schemas/ee-config.schema.json` are current.
- Release and install scripts package v2 manifests only.
- No old plugin crate, module, manifest parser, RPC type, or lifecycle path remains in the repository.

# DeepSeek Harness Prompt Sections Implementation Plan

Reference: [PLAN-harness.md](docs/upgrades/PLAN-harness.md) · capture: [deepseek-harness-agent-mode-prompt-capture.md](docs/upgrades/deepseek-harness-agent-mode-prompt-capture.md)

## Execution Overview

**Goal:** give `ee agent mode` the same ordered prompt-section structure as DeepSeek Harness while keeping `ee` tool names, policy, plan payload, injection guard, and subagent evidence rules authoritative.

**Overview:** Phase 0 freezes the registry and renderer with no behavior change. Phase 1 replaces the two flat context strings with a stable prompt bundle plus a runtime snapshot. Phase 2 adds per-tool guidance and the prose/schema consistency gate. Phases 3, 4, 5, and 5b are independent after Phase 1/2. Phase 5b makes turn/tool-call limits unlimited by default (opt-in `Some(n)`), then matches DSH's concurrency, repeat, depth, and pruning mechanisms. Phase 6 closes deferred slots, freezes snapshots, and runs full validation.

**Fixed decisions already made (do not re-litigate):**

- Section slot names, registration names, and numeric orders are DSH names verbatim. No renames, no `EE_` prefixes on DSH slots.
- `ee` keeps canonical tool names (`read_file`, `write_file`, `create_terminal`, `delegate_task`, ...). DSH prose is rewritten; DSH tool names are not advertised.
- Stable system prompt contains no cwd, roots, sandbox, approval, agent identity, or skill catalog.
- Runtime facts travel as a host-owned user-role snapshot; `WORKING_DIRECTORY` is the only required context.
- Repository instruction files are untrusted data, never system policy.
- `prompt_injection.rs` stays the single trust guard; `prompt_injection` policy text is never replaced by prompt prose.
- Plan mode keeps the `<!-- ee-plan ... -->` payload contract.
- Turn/model/tool-call limits default to `None` (unlimited); users may opt into `Some(n)`. DSH has no turn or total-tool-call cap. Concurrency (10), repeats (advisory at 3/5/8), timeouts, depth, and active-subagent counts remain bounded.
- No compatibility shims, no dual context emission, no feature flag to restore the old flat strings.
- New logic lives in `crates/ee-agent-orchestrator/src/prompt/`. Files already over 1,000 LOC receive delegation call sites only.

**Parallel map:**

| Phase | Can start after | Write scope | Blocks |
|---|---|---|---|
| 0 Contracts | now | `src/prompt/{mod,sections,render,error,context}.rs`, `src/lib.rs` | 1 |
| 1 Stable + runtime | 0 | `src/prompt/{runtime_context,sections}.rs`, `provider_adapter/{mod,conversation,agent_impl}.rs`, `runtime/{turn,resume,compact,strategic}.rs` | 2, 3, 4 |
| 2 Tool guidance | 1 | `src/prompt/tool_guidance.rs`, `src/runtime/prompt_views.rs`, `provider_adapter/{conversation,agent_impl}.rs` | 5, 6 |
| 3 Workspace instructions | 1 | `src/prompt/workspace.rs`, `src/trust.rs`, `src/prompt_injection.rs`, `ee-cli/src/app/agent_knowledge.rs` | 6 |
| 4 Mode policy | 1 | `src/prompt/sections.rs`, `provider_adapter/mod.rs` | 6 |
| 5 Delegation, MCP, compaction | 2 | `src/prompt/{mcp,compaction}.rs`, `src/prompt/runtime_context.rs` | 6 |
| 5b Limits and repetition | 2 | `src/config.rs`, `src/stuck.rs` call site, `src/prompt/repeat_guard.rs`, `src/prompt/runtime_context.rs`, `src/model.rs` | 6 |
| 6 Deferred slots + gates | 2, 3, 4, 5, 5b | tests, `src/prompt/sections.rs` | — |

## Phase 0 — Shared contracts

**Goal:** create the section registry, renderer, and transcript-context type so later phases never invent ad-hoc strings.

**Overview:** this phase adds `src/prompt/` and moves `TurnSystemContext` out of `loop_engine.rs`. It changes no rendered prompt. All DSH slot names and orders exist as constants with tests.

**Rules:**

- `SECTION_ORDERS` and `CONTEXT_ORDERS` mirror DSH exact values, including unused slots.
- Both slot names and registration names exist; registration names own identity and duplicate detection.
- Renderer is pure: no env, cwd, clock, or global state.
- Every error is a typed `PromptError` variant; no stringly errors.
- Each new file stays below 1,000 LOC.

**Work items:**

- [x] Add `prompt/sections.rs`.
  - [x] Define all stable slot constants and orders from DSH `SECTION_ORDERS`.
  - [x] Define all runtime slot constants and orders from DSH `CONTEXT_ORDERS`.
  - [x] Define registration names on each `Slot` (`harness:identity`, `deployment:persona-prefix`, `tool:read`, `mcp:<server>` template, ...).
  - [x] Add the `EE_TURN_GUIDANCE` extension constant at order 130 and `MCP_SERVER_INSTRUCTIONS` at 140.
  - [x] Add a test asserting every name/order pair against a hardcoded expected table.
- [x] Add `prompt/render.rs`.
  - [x] Implement `Section`, sort by `(order, registration)`, drop empty, join with `\n\n`.
  - [x] Implement strict `{{variable}}` interpolation with unknown, undefined, and malformed errors.
  - [x] Implement `interpolate: false`, `complete`, duplicate-registration, and byte-budget errors.
  - [x] Add tests for ordering ties, empty sections, braces, complete collision, and unknown variables.
- [x] Add `prompt/error.rs` with `PromptError` variants and `Display`.
- [x] Add `prompt/context.rs`.
  - [x] Move `TurnSystemContext` from `loop_engine.rs`.
  - [x] Add `stable: Option<String>`, `runtime: Option<RuntimeSnapshot>`, `workspace: Vec<WorkspaceMessage>` fields.
  - [x] Add `apply_to_transcript` that enforces the documented message order.
  - [x] Update callers to import from `prompt`; no duplicate type remains.
- [x] Add `prompt/mod.rs` with `PromptInputs`, `PromptBundle`, `PromptPreset`, and `build` signature returning `Result<_, PromptError>`.
- [x] Register `mod prompt;` in `lib.rs` and use `pub(crate)` visibility.
- [x] Add a temporary `#![allow(dead_code)]` in `prompt/mod.rs` for contracts consumed in Phase 1; removed after wiring (`cargo clippy -D warnings` clean).

**Acceptance criteria:**

- `cargo test --quiet -p ee-agent-orchestrator prompt::` passes.
- Prompt rendering is unchanged in behavior; existing provider adapter tests pass.
- Slot and registration inventories match the capture doc exactly.
- `loop_engine.rs` has no new logic beyond the moved type and one delegation call.

## Phase 1 — Stable prompt and runtime snapshot

**Goal:** replace `workspace_system_context` and `mode_system_context` with the registry builder, and deliver runtime facts as a separate snapshot.

**Overview:** this phase wires identity, persona prefix/suffix, `WORKING_DIRECTORY`, `SANDBOX_POLICY`, `APPROVAL_POLICY`, and `EE_TURN_GUIDANCE`. It deletes the old flattened strings and updates adapter tests.

**Rules:**

- The stable prompt must not contain cwd, workspace roots, sandbox text, approval text, or agent identity.
- Runtime text is derived from typed policy inputs, never from a free-form permission string.
- Approval prose never grants escalation when writes or execution are denied.
- `EE_TURN_GUIDANCE` carries the existing `capability_aware_guidance` text; strategy decisions do not change.
- No prompt section embeds tool output, repository text, or model output.

**Work items:**

- [x] Add `prompt/runtime_context.rs`.
  - [x] Render `WORKING_DIRECTORY` with cwd, sorted deduplicated roots, absolute-path rule, and workspace-scope rule.
  - [x] Render `SANDBOX_POLICY` from `ToolPolicy::{allow_write, allow_execute}` and `WorkspaceScope`.
  - [x] Render `APPROVAL_POLICY` from typed `ApprovalMode` resolved by the adapter from effective `ToolPolicy` (mode policy already encodes `agents.read_only_modes` and `allow_host_approved_side_effects`).
  - [x] Render `SUBAGENT_DELEGATION` when depth > 0 or delegation is denied; full delegation-budget text lands in Phase 5.
  - [x] Prepend the exact supersession header once (in `prompt/context.rs`).
  - [x] Add exact-render tests for read-only, workspace-write (scoped and unscoped), approval ask, approval denied, path quoting, and subagent identity.
- [x] Add `prompt/sections.rs` standard builder.
  - [x] Emit `HARNESS_IDENTITY`, `DEPLOYMENT_PERSONA_PREFIX`, `DEPLOYMENT_PERSONA_SUFFIX`.
  - [x] Emit `PLAN_POLICY` only for plan mode (existing plan contract moved into the section).
  - [x] Decide `FILE_REFERENCE`: permanently omitted. `ee` has no `@file`-reference expansion and `read_file` owns file access; the slot stays registered and audited `omit`, with no placeholder prose.
  - [x] Apply the `minimal` preset through `complete: true` (test-only preset).
- [x] Rewire `provider_adapter`.
  - [x] Delete `workspace_system_context` and `mode_system_context`.
  - [x] Resolve `PromptBundle` per prompt from stored session facts (`cwd`, sorted roots, provider/model labels, current mode, effective policy).
  - [x] Store the session facts needed to rebuild the bundle; the runtime snapshot is rebuilt per turn so cwd/policy drift cannot go stale.
  - [x] Pass both planes through `runtime/{turn,resume,compact,strategic}.rs` into `TurnSystemContext`.
- [x] Apply the bundle in `prompt/context.rs`.
  - [x] One system message for the stable prompt.
  - [x] Memory facts remain a separate system message.
  - [x] Runtime snapshot is a user-role message with `TrustLevel::SystemPolicy` and `metadata["ee_prompt"] = "runtime_context"`.
  - [x] Current prompt is last.
- [x] Update or delete tests that asserted the old flat strings.
- [x] Add builder tests for `standard`, `plan`, `minimal`, read-only, scoped/unscoped write, and approval variants; byte-level snapshot freezing remains Phase 6.
- [x] Add cache-stability test: two turns with different cwd produce identical stable strings and different runtime strings.

**Acceptance criteria:**

- [x] No `Session context:`, `Tool path rules:`, or `Agent mode:` flat string remains in production code.
- [x] Standard prompt snapshot is stable and contains no cwd path.
- [x] `read_file`, `write_file`, `create_terminal`, and `delegate_task` are absent from plan/ask prompt text.
- [x] Runtime snapshot metadata and trust level are asserted exactly.
- [x] `cargo test --quiet -p ee-agent-orchestrator` passes.

## Phase 2 — Tool guidance and consistency gate

**Goal:** every advertised tool has owned guidance, and prompt prose can never name a tool the turn does not advertise.

**Overview:** this phase adds `prompt/tool_guidance.rs` with one entry per `ee` capability and the bidirectional consistency test. It does not add tools.

**Rules:**

- Guidance is keyed by advertised tool name, not by DSH tool name.
- A section emits only when every required tool is advertised.
- No placeholder text for tools that do not exist.
- Advertised tools without guidance must be explicitly exempt with a reason.
- `tools.rs`, `tool_schemas.rs`, and `strategy.rs` are not edited.

**Work items:**

- [x] Add `prompt/tool_guidance.rs`.
  - [x] `TOOL_READ` for `read_file`.
  - [x] `TOOL_WRITE` for `write_file`.
  - [x] `TOOL_BASH` for `create_terminal`, `wait_for_terminal_exit`, and `cargo_check` exit/status rules.
  - [x] `TOOL_PTY` for `terminal_output`, `kill_terminal`, `release_terminal`.
  - [x] `TOOL_WEB_SEARCH` for `ee_web_search`.
  - [x] `TOOL_WEB_FETCH` for `ee_fetch_url`.
  - [x] `TOOL_SUBAGENT` for `delegate_task`, including role names and evidence markers. DSH registration name `tool:subagent`; the earlier `tool:<subagent-tool>` template was corrected.
  - [x] `FILE_REFERENCE` for ACP resource links: decided `omit` in Phase 6. No tool name exists to gate on, and `read_file` owns file access.
  - [x] Declare `GUIDANCE_EXEMPT` with reasons for `ask_user` and any unguided built-in.
    - [x] Every pinned `ee` proxy tool in `ee_mcp::STABLE_TOOL_NAMES` now has a guidance row; the blanket `ee_` exemption was removed so a new proxy tool without guidance fails a test.
    - [x] Covered families: buffer/disk reads, file create/overwrite/write, filesystem mutation, directory listing, literal search, code actions, rename/format, dependency maps, proxy terminals, git status/diffs/changed files, browser retrieval, session notes, workspace memory, and turn evidence.
  - [x] Gate is policy-derived: `OrchestratorRuntime::advertised_tool_names` filters registered tools through `PolicyEngine::check`.
  - [x] Rebuild the stable prompt after prompt-scoped MCP registration so conditional web-tool guidance tracks the turn.
- [x] Add tool-name extraction helper and `requires` validation.
- [x] Add consistency tests.
  - [x] Every mentioned tool is advertised.
  - [x] Every advertised tool is guided or exempt (built-in universe read from `ToolRegistry::register_builtins`, proxy universe read from `ee_mcp::STABLE_TOOL_NAMES`).
  - [x] Typo in `requires` fails.
  - [x] Plan/ask prompts mention no write, execute, or delegate tool.
- [x] Add prompt coverage per tool set: read-only, write, execute, delegation, MCP web (presence assertions; byte snapshots stay Phase 6).
- [x] Assert prompt bytes do not depend on advertised-tool order (registry definitions already sort alphabetically).

**Acceptance criteria:**

- [x] Adding a tool definition without guidance or exemption fails a test.
- [x] No section text mentions the DSH tool names as callable tools: `read`, `write`, `edit`, `bash`, `glob`, `grep`, `subagent`, `apply_patch`. Match exact advertised names, not substrings of `read_file` / `write_file`. Namespaced `ee_apply_patch` and `ee_replace_text` are `ee` proxy tools and remain allowed.
- [x] `cargo test --quiet -p ee-agent-orchestrator prompt::tool_guidance` passes.
- [x] Full `cargo test --quiet -p ee-agent-orchestrator`, clippy, and `cargo fmt --all --check` pass.

## Phase 3 — Workspace instructions

**Goal:** discover `AGENTS.md` / `CLAUDE.md` hierarchies, deliver them as labeled data, and refresh after file touches.

**Overview:** this phase adds `prompt/workspace.rs`, a new untrusted trust label, and discovery wiring in `ee-cli`. It does not move instructions into the system prompt.

**Rules:**

- Files are untrusted; the injection guard wraps and scans them.
- Discovery walks root to cwd, sorted, with content-hash dedupe.
- Paths are canonicalized and must stay inside their root.
- Budget and truncation markers are explicit.
- `.git` marker directories are skipped.
- No read of a developer's real config tree in tests.

**Work items:**

- [x] Add `prompt/workspace.rs`.
  - [x] Discover `AGENTS.md`, `CLAUDE.md`, `AGENTS.local.md`, `CLAUDE.local.md` from each root toward cwd.
  - [x] Include root-only `RULE.md` and redacted `.ee.toml`.
  - [x] Dedupe by trimmed content hash; render broad-to-specific so deeper files win conflicts.
  - [x] Apply 64 KiB total and 1 MiB source caps with truncation markers.
  - [x] Render `Instructions from: <path>` blocks inside an escaped `<system-reminder>` frame.
  - [x] Refresh by rebuilding the bundle every prompt; `TurnSystemContext` replaces earlier prompt-plane messages on resume.
- [x] Add `TrustLevel::WorkspaceInstructionUntrusted` with label `workspace_instruction`.
  - [x] Return true from `is_untrusted`.
  - [x] Extend trust and injection detection tests.
- [x] Align host instruction discovery in `ee-cli/src/app/agent_knowledge.rs`.
  - [x] Add `CLAUDE.md`, `AGENTS.local.md`, and `CLAUDE.local.md` to `project_instructions` with precedence 11–13.
  - [x] Keep `.ee.toml` sanitization; orchestrator owns the session prompt snapshot.
- [x] Add refresh behavior.
  - [x] Recompute on every prompt bundle build, including the post-MCP rebuild.
  - [x] Drop stale `ee_prompt=workspace_instructions` history before inserting the fresh snapshot; deleted files disappear.
- [x] Add tests for precedence, dedupe, caps, injection detection, frame escaping, sanitization, and path escape rejection.

**Acceptance criteria:**

- Workspace text never appears in the stable system prompt.
- Workspace messages carry the `workspace_instruction` label and are scanned by the injection guard.
- Discovery order and dedupe match the plan exactly.
- Tests use only `tempfile` roots and never the real home config.

## Phase 4 — Mode policy sections

**Goal:** express ask, plan, and write policy as ordered sections without losing the plan payload.

**Overview:** this phase rewrites `PLAN_POLICY` prose for `ee`, adds mode gating for tool guidance, and removes the old mode concatenation. It keeps `parse_plan_items` and the payload contract unchanged.

**Rules:**

- `PLAN_POLICY` orders before tool guidance and after persona.
- Plan payload marker and JSON shape stay byte-identical.
- Ask mode is read-only and advertises no mutation guidance.
- Write mode retains host approval and trusted-write semantics.
- Mode sets come from `mode_policy`; prose never grants a capability the policy denies.

**Work items:**

- [x] Rewrite plan text in the prompt module.
  - [x] Investigation rules, deliverable sections, `## Open questions` then payload.
  - [x] Explicit prohibition on writes, execution, and delegation.
  - [x] Keep the payload shape and hidden-Markdown note.
- [x] Add ask-mode text: answer directly, read-only tools only, no mutation or delegation (`EE_MODE_POLICY` runtime slot).
- [x] Add write-mode text: implement with advertised tools; host approval remains required (`EE_MODE_POLICY` runtime slot).
- [x] Gate tool guidance by mode-advertised tool set.
- [x] Remove `mode_system_context` and its call site.
- [x] Extend plan-mode tests.
  - [x] Payload still parses through `parse_plan_items`.
  - [x] Snapshot contains `## Plan`, `## Validation`, `## Open questions`.
  - [x] Snapshot contains no write/execute/delegate tool names.
- [x] Add ask-mode and write-mode snapshots.

**Acceptance criteria:**

- Plan responses from the model still compile to the existing ACP task plan.
- Mode prompts differ only by documented sections.
- `cargo test --quiet -p ee-agent-orchestrator provider_adapter` passes.

## Phase 5 — Delegation, MCP, and compaction

**Goal:** add delegation identity/depth contexts, MCP server facts and labeled server instructions, and a DSH-shaped compaction checkpoint prompt.

**Overview:** this phase adds `prompt/mcp.rs` and `prompt/compaction.rs`. MCP server-supplied text is delivered as untrusted runtime messages, never as system policy.

**Rules:**

- MCP display naming stays `mcp_<server>_<tool>`; `ee` proxy tools keep their unprefixed names.
- `MCP_SERVERS` stable section contains host facts only.
- Server instruction text carries label `mcp_server` and is untrusted.
- `SUBAGENT_DELEGATION` renders identity, depth, and remaining budget from policy.
- Compaction keeps `ee` evidence fields; only the section ordering follows DSH.
- `mcp/manager.rs` (over 1,000 LOC) receives no logic; add a thin summary accessor or a new module that reads existing public accessors.

**Work items:**

- [x] Add `prompt/mcp.rs`.
  - [x] Render configured server names and display-name rule into `MCP_SERVERS` (sorted, deduplicated, non-interpolating).
  - [x] Render per-server instruction messages for runtime delivery, bounded to 8 KiB and labeled `mcp_server`.
  - [x] Add tests for naming, ordering, empty text, truncation, frame escaping, and trust label.
- [x] Add `prompt/runtime_context.rs` subagent support.
  - [x] Denial line when delegation is policy-disabled.
  - [x] Child identity with role and depth.
  - [x] Depth line only at the cap (`max_delegate_depth` from the effective policy), including a zero cap at the root; no standing remaining-depth prose. DSH contributes no other delegation budget text.
- [x] Capture MCP server instructions from the initialize handshake in `mcp/manager.rs`.
  - [x] Store `instructions` per connection; expose `server_instructions()` as a thin accessor (no logic in the over-1,000-LOC file).
  - [x] Populate `PromptBundle::mcp` after discovery and deliver the messages after the runtime snapshot.
- [x] Rewrite `compaction.rs::build_compaction_prompt`.
  - [x] Adopt DSH section order: primary request, concepts, files, errors, current work, next step, critical context.
  - [x] Keep `ee` task graph, memory facts, validation facts, and budget sections under `## Session evidence`.
  - [x] Omit `Pending Jobs` because no job tools exist; document the reason in the prompt rules and audit.
  - [x] Add tests for section order and retained evidence.
- [x] Add subagent and MCP prompt snapshots.

**Acceptance criteria:**

- MCP instruction text cannot appear in the stable prompt.
- Subagent context appears only when delegation is possible or depth > 0.
- Compaction summary still satisfies existing continuation tests.
- `cargo test --quiet -p ee-agent-orchestrator compaction` passes.

## Phase 5b — Loop limits, repetition, and budget parity

**Goal:** match DSH/Codex behavior: no default turn or tool-call cap, read-only parallelism 10, advisory repeat reminder at 3, subagent depth line only at the cap, pressure notice only for opt-in caps, and tool-result middle prune.

**Overview:** DSH caps concurrency, repeats, time, depth, and active subagents, but not turns or total tool calls (capture §13). `ee` currently defaults to 16 iterations, 16 model calls, and 180 tool calls per turn; that stops tasks that would finish if allowed to continue. Make those limits `Option<usize>` with default `None`, keep `stuck.rs` and timeouts as the real safety net, and add the missing advisory/pruning behavior.

**Rules:**

- `max_loop_iterations`, `max_model_calls`, `max_tool_calls_per_turn`, and `max_output_bytes` default to `None` (unlimited); `turn_timeout` defaults to `None` too. `Some(...)` is user/deployment opt-in; zero values are rejected.
- Delete `DEFAULT_MAX_LOOP_ITERATIONS`, `DEFAULT_MAX_MODEL_CALLS`, `DEFAULT_MAX_TOOL_CALLS_PER_TURN`, `DEFAULT_MAX_OUTPUT_BYTES`, and `DEFAULT_TURN_TIMEOUT`; do not replace them with sentinel values.
- `stuck.rs`, host cancellation, `max_subagents`, and per-tool `tool_timeout` remain.
- `DEFAULT_MAX_PARALLEL_TOOLS` becomes `10`; write/execute waves stay serial.
- Repeat reminder thresholds are `[3, 5, 8]`; `ee` stops at `stuck.max_repeated_tool_calls = 4`, so only 3 fires by default.
- Pressure notice appears only when a finite `Some(max)` is configured; `None` emits nothing.
- Bump `CHECKPOINT_SCHEMA_VERSION`; old numeric-cap checkpoints fail closed, no migration shim.
- Reminder and pressure notice are orchestrator-authored, user-role, `TrustLevel::SystemPolicy`, metadata `ee_prompt=repeat_reminder` / `ee_prompt=runtime_context`.
- No standing budget table and no new DSH slot; reuse `EE_TURN_GUIDANCE`.
- `loop_engine.rs`, `stuck.rs`, and `model.rs` receive call sites or small helpers only; detector logic lives in `prompt/repeat_guard.rs`.

**Work items:**

- [x] Make the count, output, and time caps optional.
  - [x] In `src/config.rs`, change `max_loop_iterations`, `max_model_calls`, `max_tool_calls_per_turn`, and `max_output_bytes` to `Option<usize>`; change `turn_timeout` to `Option<Duration>`; default all to `None`.
  - [x] Delete the five `DEFAULT_MAX_*` / `DEFAULT_TURN_TIMEOUT` constants and every import of them.
  - [x] Add `OrchestratorConfig::validate` rejecting `Some(0)` and `Some(Duration::ZERO)`; call it in fallible runtime constructors.
  - [x] In `src/budget.rs`, make `BudgetConfig` count caps `Option<usize>` and its deadline derive from `turn_timeout.map(...)`.
  - [x] Change every `try_reserve_*`, `check_output_allowance`, `record_model_usage`, and `try_reserve_subagent_and_model_calls` check to deny only on `Some(max)`; `None` never denies.
  - [x] Make `reset_deadline` take `Option<Duration>`; `None` clears the deadline. Updated `runtime/{compact,resume,strategic}.rs` and subagent call sites.
  - [x] Make `BudgetSnapshot` count/output caps `Option<usize>`; render `unbounded` in `Display`.
  - [x] Update `restore_used` to compare `Option` caps and skip usage-above-cap checks when `None`.
  - [x] Add `src/checkpoint_budget.rs` (checkpoint.rs is over 1,000 LOC) and validate `Option` caps; bump `CHECKPOINT_SCHEMA_VERSION` to 6.
  - [x] In `src/subagents/manager.rs`, map role caps with `Some(role.max_iterations)`; keep rubber-duck constants and child timeout finite.
  - [x] Update every `BudgetSnapshot` / config literal in tests/fixtures to `Some(...)` or `None`; no bare numeric caps remain.
- [x] Make provider limits optional.
  - [x] `ee-agent-opencode`: `Config.max_iterations` and CLI/env parsing become `Option<usize>`; default `None`; explicit `0` fails validation.
  - [x] `ee-agent-openrouter`: same change, including the manifest input description and default.
  - [x] Map directly into `OrchestratorConfig` without `unwrap_or(0)`; omitted means `None`.
  - [x] Update provider tests: default is `None`; explicit values are `Some(n)`; `0` is rejected.
- [x] Add unlimited-default regression tests.
  - [x] Fake adapter drives 20 tool iterations plus completion in one turn; task completes.
  - [x] Budget tracker admits 1,000 iterations/model calls/tool calls and 10 MiB output with default caps.
  - [x] Explicit `Some(1)` caps still stop; `Some(0)` fails config validation.
  - [x] Checkpoint round-trips `None` caps; restore rejects a mismatch.
  - [x] Old schema-version checkpoint fails closed.
- [x] Match read-only parallelism.
  - [x] Change `DEFAULT_MAX_PARALLEL_TOOLS` to `10` in `src/config.rs`; loop uses `self.config.max_parallel_tools` instead of hardcoded `2`.
  - [x] Update the default-value assertions in `src/config.rs` tests.
  - [x] Existing write/execute serialization tests remain unchanged.
- [x] Add `src/prompt/repeat_guard.rs`.
  - [x] Canonicalize arguments with deep key sorting; key on `(tool name, canonical JSON)`.
  - [x] Track consecutive counts with thresholds `[3, 5, 8]`, `argumentsPreviewChars = 500`.
  - [x] Implement `*` wildcard `include` / `exclude`; excluded calls neither count nor reset.
  - [x] Reset the chain on a user message; keep per-runtime state only.
  - [x] Emit the gentle/detailed reminder texts, with `ee` tool names.
  - [x] Add unit tests for threshold, canonical ordering, preview truncation, filters, reset, and argument change.
- [x] Wire the reminder into the loop at one call site.
  - [x] Observe each executed tool call after its result; append the returned notice to the transcript.
  - [x] Skipped automatically when the tool-call budget denied the call (no result is pushed).
  - [x] Existing hard stop at 4 (`src/stuck.rs`) remains.
- [x] Add the budget pressure notice helper (`prompt/budget_notice.rs`).
  - [x] Compute remaining iterations/model calls/tool calls from `BudgetSnapshot` for finite caps only.
  - [x] Emit one line only at `iterations_remaining <= 2` or `tool_calls_used >= 90%`.
  - [x] Emit nothing when the corresponding cap is `None`.
  - [x] Never print all caps; no new section name.
  - [x] Wire `pressure_notice` into `EE_TURN_GUIDANCE` when Phase 1 builds the runtime snapshot.
- [x] Add the subagent depth line helper (`prompt/delegation.rs`).
  - [x] Render `Delegation depth for \`delegate_task\`: current <n>, maximum <m>, remaining levels 0.` only when `depth >= max_depth`.
  - [x] Unit tests cover below-cap silence and at/over-cap text.
  - [x] Wire it into `SUBAGENT_DELEGATION` for depth > 0. No root delegation-budget text is added: DSH contributes none, and D14 forbids advertising caps.
- [x] Add tool-result middle pruning.
  - [x] Prune only tool-result text longer than 8192 chars: keep 4096 head + 1024 tail.
  - [x] Insert the exact marker `[... tool result middle pruned ...]`.
  - [x] Project only the request copy; durable transcript and checkpoints keep the full result.
  - [x] Add tests for boundary sizes, exact output, and non-mutation.
- [x] Audit per-tool timeouts.
  - [x] Record `ee`'s 120 s global tool timeout vs DSH's 30-300 s per-tool defaults in the plan.
  - [x] Do not change timeout defaults in this phase.
- [x] Cover output and wall-clock caps.
  - [x] `None` output bytes never deny; finite `Some` still denies on overage.
  - [x] `None` turn timeout leaves no deadline; `Some(d)` expires; `reset_deadline(None)` clears an existing deadline.
  - [x] Per-tool `tool_timeout` stays finite and independent.
- [x] Run validation for touched modules.
  - [x] `cargo test --quiet -p ee-agent-orchestrator prompt::`
  - [x] `cargo test --quiet -p ee-agent-orchestrator config`
  - [x] `cargo test --quiet -p ee-agent-orchestrator stuck`
  - [x] `scripts/run-tests-memguard.sh cargo test --quiet -p ee-agent-orchestrator`

**Acceptance criteria:**

- Default config has `None` for iterations, model calls, tool calls, output bytes, and turn timeout; no sentinel `usize::MAX`, `Duration::MAX`, or `0`.
- A 20-iteration turn completes under defaults; budget unit tests admit 1,000 counts and 10 MiB output.
- Explicit `Some(n)` still stops at `n`; zero values fail validation; checkpoints compare `Option` caps.
- Provider manifests default to unlimited and reject explicit `0`.
- `DEFAULT_MAX_PARALLEL_TOOLS == 10`; write/execute stays serial.
- Third identical call emits one advisory notice; fourth stops the loop.
- Reminder does not fire on changed arguments, a reset user message, or an excluded tool.
- Pressure-notice helper returns text only for finite caps under the documented conditions and is wired into `EE_TURN_GUIDANCE` by the runtime.
- [x] Depth-line helper returns text only at the cap; wiring into `SUBAGENT_DELEGATION` is complete.
- Oversized tool result is pruned to head + marker + tail; small results are byte-identical.
- No new slot name and no budget table in the prompt.

## Phase 6 — Deferred slots and final gates

**Goal:** document inert capabilities, freeze snapshots, and prove no drift remains.

**Overview:** this phase adds no new product behavior. It audits every registered slot, pins the deferred list, and runs the full validation set.

**Rules:**

- Inert slots emit nothing and are listed with the condition that would activate them.
- No placeholder prose for missing tools.
- No DSH repository `AGENTS.md` content in any snapshot.
- Snapshots are deterministic and checked in.
- Full validation runs with the memory guard.

**Work items:**

- [x] Audit every slot against the registry table.
  - [x] Include the phase 5b additions: repeat reminder metadata, pressure notice, depth line, middle-prune marker.
  - [x] Add a test that iterates all slots and asserts an explicit status: `emitted`, `conditional`, or `omit`.
  - [x] Assert no slot is left `deferred`; every inert slot has a reason.
- [x] Add guards against blind porting.
  - [x] No `DeepSeek`, `dsh`, `@path`, bare `apply_patch`, `run_code`, or `job_output` in `ee` prompt output. Namespaced `ee_apply_patch` is an `ee` proxy tool.
  - [x] No `harness-example/` path referenced by runtime code or tests.
- [x] Freeze snapshots: exact standard stable text, exact runtime snapshot text, exact MCP host-facts section, and exact plan contract; ask, write, minimal, read-only, delegation, and MCP behavior use exact assertions.
- [x] Run final validation.
  - [x] `cargo fmt --all --check`.
  - [x] `cargo clippy -p ee-agent-orchestrator --all-targets -- -D warnings`.
  - [x] `cargo clippy -p ee-cli --all-targets --features agents -- -D warnings`.
  - [x] `scripts/run-tests-memguard.sh cargo test --quiet -p ee-agent-orchestrator`.
  - [x] `cargo test --quiet -p ee-cli --features agents project_instructions`.

**Acceptance criteria:**

- All slot and registration names match DSH exactly.
- No prompt text names an unadvertised tool.
- Stable prompts contain no runtime facts.
- Snapshots contain no DSH product or repository content.
- Full validation passes under the memory guard.

---

# Unified Harness Runtime Plan — Phases 2, 3, 4, 6

Reference: [PLAN-harness.md](docs/upgrades/PLAN-harness.md) · capture: [deepseek-harness-agent-mode-prompt-capture.md](docs/upgrades/deepseek-harness-agent-mode-prompt-capture.md)

**Scope:** this plan adds only the selected harness work. Phase numbers are local to this plan. Phase 1 repo router/state and Phase 5 worktree isolation are excluded.

**Goal:** unify repo-level instructions with the `ee` agent runtime: trusted global instructions, cache-friendly runtime deltas, lossless-before-lossy compaction, append-only model-visible logging, evidence-gated stop, event parity, and deterministic integration gates.

**Execution order:** Phase 2 → Phase 4a → Phase 3 → Phase 4b → Phase 6.

Reason: Phase 4a creates the durable session log. Phase 3 projects the compaction funnel from it. Phase 4b uses it for stop evidence and checkpoint branching. Phase 2 is independent and can start immediately.

**Fixed decisions already made (do not re-litigate):**

- Session log stores full local content. Content-addressed message bodies live under the owner-only state directory; exports redact; telemetry never receives raw prompts.
- Global instructions use a new `TrustLevel::UserInstructionTrusted` scope. Workspace instructions remain untrusted and cannot override global or system policy.
- Runtime delta is emitted only when the previous full snapshot is still retained. First turn, resume, compaction, transcript truncation, or missing previous snapshot always emits a full snapshot.
- Stop gate applies to every changed turn. Answer-only turns bypass it.
- Branch from checkpoint replays the full session log when present. When absent, branch task/memory/budget only and mark the result `conversation_unreplayed`.
- No compatibility shim for old flat prompt strings, old transcript behavior, or old checkpoint branches.
- New logic lives in new modules where the target file is near or above 1,000 LOC. Existing large files receive call sites only.
- Tests use `tempfile`, temp XDG/state homes, and no real user config or session directories.

**Parallel map:**

| Phase | Can start after | Write scope | Blocks |
|---|---|---|---|
| 2 Prompt scope + runtime delta | now | `src/prompt/**`, `src/trust.rs`, `provider_adapter/**`, runtime prompt call sites | 6 |
| 4a Session log | 2 | `src/session_log.rs`, `src/model.rs`, `src/loop_engine.rs`, host/CLI session paths | 3, 4b, 6 |
| 3 Compaction funnel + strategy | 4a | `src/compaction/**`, `src/runtime/compact.rs`, `src/config.rs` | 6 |
| 4b Stop gate + events + branch | 4a | `src/loop_engine.rs`, `src/completion.rs`, `src/final_response.rs`, `src/plugin_runtime/**`, `src/checkpoint.rs` | 6 |
| 6 Integration gates | 2, 3, 4 | tests and validation commands | — |

## Phase 2 — Prompt Scope and Runtime Delta

**Goal:** load trusted user-level instructions and stop resending unchanged runtime facts every turn.

**Overview:** this phase adds a global instruction scope and section-level runtime snapshots. The stable system prompt remains byte-identical across turns and never contains cwd, workspace roots, sandbox, approval, agent identity, or instruction-file content.

**Rules:**

- Global instructions are host-user-controlled and trusted relative to workspace files, but remain below system policy.
- Global instruction discovery is `$XDG_CONFIG_HOME/ee/AGENTS.md`, falling back to `~/.config/ee/AGENTS.md`.
- Workspace instructions stay `WorkspaceInstructionUntrusted`; they never override global or system policy.
- Runtime facts remain a host-owned user-role snapshot with `TrustLevel::SystemPolicy` and `metadata["ee_prompt"] = "runtime_context"`.
- Delta mode never omits a section that changed. Unchanged sections are named, not repeated.
- Full snapshot is mandatory after resume, compaction, transcript truncation, or whenever the previous full snapshot is absent from retained history.
- No global instruction text enters the stable prompt.

**Work items:**

- [x] Add `src/prompt/global.rs`.
  - [x] Discover `$XDG_CONFIG_HOME/ee/AGENTS.md`, then `~/.config/ee/AGENTS.md`.
  - [x] Reuse workspace loader sanitization, canonicalization, and byte caps.
  - [x] Reject symlink escape outside the user config directory.
  - [x] Return at most one global instruction message.
- [x] Add `TrustLevel::UserInstructionTrusted`.
  - [x] Label `user_instruction`.
  - [x] Return false from `is_untrusted`.
  - [x] Keep injection scanning and redaction for diagnostics.
  - [x] Extend label round-trip and role-trust tests.
- [x] Extend prompt assembly.
  - [x] Add `global` instruction messages to `PromptBundle` and `TurnSystemContext`.
  - [x] Apply order: stable system → global instructions → memory → history → workspace → runtime → current prompt.
  - [x] Assert global text never appears in stable prompt.
- [x] Add runtime section snapshots.
  - [x] Add `RuntimeSection { name, order, text, hash }` and `RuntimeSnapshotMode::{Full, Delta}`.
  - [x] Keep `RuntimeSnapshot.text` as the complete rendered text for full mode.
  - [x] Track previous section hashes and `previous_full_retained` per provider session.
  - [x] Render delta text with changed sections and an `Unchanged sections:` line.
  - [x] Force full mode on first turn, resume, compaction, truncation, or missing previous snapshot.
- [x] Add tests.
  - [x] Temp XDG global instruction loads and is labeled `user_instruction`.
  - [x] Workspace `AGENTS.md` cannot override a global rule.
  - [x] Symlink escape and over-budget global file are skipped with markers.
  - [x] Two turns with identical cwd/policy emit no full resend.
  - [x] cwd change emits only `WORKING_DIRECTORY`.
  - [x] Truncation/resume/compaction emits full snapshot.

**Acceptance criteria:**

- [x] Global instructions load only from a temp XDG path in tests.
- [x] Stable prompt bytes are unchanged across turns with different cwd/policy.
- [x] Workspace instruction text is never promoted to system policy.
- [x] Runtime delta contains every changed section and no stale section value.
- [x] `cargo test --quiet -p ee-agent-orchestrator prompt::` passes.

## Phase 3 — Compaction Funnel and Pluggable Strategy

**Goal:** make compaction lossless before lossy and replaceable without changing default output.

**Overview:** this phase formalizes the existing prune, window, deterministic memory, and LLM summary steps into one ordered pipeline. It runs only on the `/compact` path and never mutates live memory until the staged result commits.

**Rules:**

- Request projection and lossless pruning happen before any LLM summary.
- Only read-only tool results may be deduplicated: byte-identical duplicates and reads superseded by a later read of the same path.
- Never deduplicate write, execute, delegate, approval, validation, or evidence results.
- Protected memory keys (`decision:`, `constraint:`, `validation:`) survive every stage.
- Default strategy must produce byte-identical output when no prune applies.
- A failed compaction leaves live memory byte-for-byte unchanged.
- No untrusted plugin strategy loads without host approval.

**Work items:**

- [x] Add `src/compaction/pipeline.rs`.
  - [x] Define ordered stages: request projection, lossless prune, window, deterministic memory, LLM summary.
  - [x] Define `CompactPlan` with bounded context, prompt, and per-stage counts.
  - [x] Define `CompactionStrategy` trait at the consumer boundary; planning is synchronous, the LLM call stays in the runtime.
  - [x] Add `DefaultCompactionStrategy` preserving current behavior.
- [x] Implement lossless prune.
  - [x] Deduplicate byte-identical read results, keeping the newest occurrence.
  - [x] Replace superseded reads with `[... duplicate tool result elided ...]`.
  - [x] Record `pruned_tool_results`, `deduped_messages`, and `dropped_messages` in `CompactTurnReport`.
  - [x] Keep durable transcript and checkpoint evidence untouched; prune only the compaction view.
- [x] Extend configuration.
  - [x] Add serde-default `compaction.strategy` selecting the default strategy by name.
  - [x] Reject unknown strategy names at validation time.
  - [x] Preserve existing `memory` and `max_input_bytes` behavior.
- [x] Wire the pipeline into `runtime/compact.rs`.
  - [x] Run pipeline stages in order before the model call.
  - [x] Keep staged-memory commit, summary validation, cancellation, timeout, and usage checks unchanged.
- [x] Add tests.
  - [x] Exact duplicate read result is pruned once and counted.
  - [x] Superseded read is pruned; later read survives.
  - [x] Write/execute/delegate results are never pruned.
  - [x] Protected decisions, constraints, and validation facts survive.
  - [x] Default strategy output is byte-identical when no prune applies.
  - [x] Unknown strategy fails config validation.

**Acceptance criteria:**

- [x] Funnel order is asserted by test.
- [x] Live memory is unchanged after failed compaction.
- [x] `cargo test --quiet -p ee-agent-orchestrator compaction` passes.
- [x] No new unbounded buffer or full-transcript copy is introduced.

## Phase 4 — Session Log, Stop Gate, Events, and Branch

### Phase 4a — Append-Only Session Log

**Goal:** make every model-visible request reconstructable from a durable append-only log.

**Overview:** the current `Transcript` drops oldest messages to stay under the memory limit, and durable checkpoints retain zero transcript messages. This phase separates the durable log from the bounded request view. Full local content is allowed; exports redact; telemetry never receives raw prompts.

**Rules:**

- Every message visible to a model request must have a log entry before the model call.
- Log write failure fails the turn closed before the model call.
- The durable log is append-only; windowing and truncation affect only the request view.
- Message bodies are content-addressed and stored under the owner-only state directory.
- Session log files are owner-readable/writable only.
- Export paths redact secret-like values; internal reconstruction keeps the original content.
- Checkpoints keep their bounded summary and do not embed the full transcript.

**Work items:**

- [x] Add `src/session_log.rs`.
  - [x] Define append-only record kinds: turn start/stop, message appended, model request, compaction applied, checkpoint created, branch created.
  - [x] Define `SessionMessageRecord` with message id, role, trust, content hash, turn, iteration, and tool-call id.
  - [x] Define `ModelRequestRecord` with ordered message ids, tool-schema hash, budget snapshot, model id, and projection hash.
  - [x] Store message bodies as content-addressed files owned by the session.
  - [x] Add bounded retention and per-session size validation.
  - [x] Add a `SessionLogSink` trait; file-backed implementation lives in host/CLI wiring, in-memory implementation serves tests.
- [x] Separate durable log from request view.
  - [x] Make `Transcript::push_*` append to the log through the sink.
  - [x] Keep `Transcript.messages` as the bounded view used by the loop.
  - [x] `enforce_memory_limit` records truncation and never deletes log records.
  - [x] Resume replays the log into a bounded view under the configured memory limit.
- [x] Wire request recording in `loop_engine` and `runtime/compact.rs`.
  - [x] Record the effective post-prune request, not the unprojected transcript.
  - [x] Fail before `model.complete` if the request record cannot be appended.
  - [x] Record compaction model calls with the same invariant.
- [x] Add reconstruction invariant tests.
  - [x] Rebuild every recorded request from log + content store; assert message order, roles, trust, tool ids, schema hash, and projection hash.
  - [x] Truncation does not remove reconstructable messages.
  - [x] Resume reconstructs the same current request view.
  - [x] Log write failure prevents model invocation.
  - [x] Secret-like values are absent from exported JSONL but present in internal reconstruction.

**Acceptance criteria:**

- [x] No model call succeeds without a matching `ModelRequestRecord`.
- [x] Every model-visible message is reconstructable from the session log.
- [x] Transcript truncation is a view operation only.
- [x] Session log tests use temp state directories only.
- [x] `cargo test --quiet -p ee-agent-orchestrator session_log` passes.

### Phase 4b — Stop Gate, Event Parity, and Checkpoint Branch

**Goal:** prevent unverified changed turns from claiming completion, expose missing lifecycle events, and branch safely from checkpoints.

**Overview:** `FinalResponse::can_finish` already derives from observed evidence, but the loop can return `EndTurn` when the model says it is complete. This phase installs the gate at that call site for every changed turn. It also adds step/stop events and a log-backed branch path.

**Rules:**

- Gate every changed turn; bypass answer-only turns with no changed files.
- Gate uses `derive_completion` and `FinalResponse::can_finish`; model prose never counts as evidence.
- A blocked gate may run bounded repair, otherwise it returns a blocker; it never changes `EndTurn` into a fake success.
- Plugin listeners may veto a stop but cannot override mandatory evidence or policy.
- Event names are external contract; add names, never rename existing ones.
- Branch replays the exact log prefix when available; otherwise mark `conversation_unreplayed` and branch only task/memory/budget.

**Work items:**

- [x] Add `StopGate` and call site.
  - [x] Add `StopGateOutcome::{Allow, Repair, Blocked}`.
  - [x] Invoke before `LoopEngine` returns `EndTurn` on `response.completed`.
  - [x] Skip when no changed files were observed.
  - [x] Allow verified completion.
  - [x] Run bounded repair when configured and budget allows; otherwise return a blocker.
  - [x] Keep cancellation, timeout, and stuck detection unchanged.
- [x] Add lifecycle events.
  - [x] Add `AgentStepBefore` (`agent.step.before`) with session, task, iteration, and advertised tool count.
  - [x] Add `AgentStopBefore` (`agent.stop.before`) with bounded completion state, changed-file count, and evidence ids.
  - [x] Map both in `AgentBridge` and add stable-name tests.
  - [x] Document `agent.tool.before` as the existing `tools/pre-execute` equivalent; mandatory policy runs first.
- [x] Add checkpoint branch.
  - [x] Add `SessionLog::branch_from(session_id, checkpoint_seq)` returning a new session id.
  - [x] Replay log messages up to the checkpoint sequence into the new bounded view.
  - [x] When no log exists, branch task/memory/budget only and set `conversation_unreplayed`.
  - [x] Reject branch on missing checkpoint or mismatched session.
- [x] Add tests.
  - [x] Model says complete with no evidence → gate blocks or repairs.
  - [x] Verified evidence → gate allows `EndTurn`.
  - [x] Answer-only turn bypasses the gate.
  - [x] Plugin stop veto is honored; mandatory gate denial wins.
  - [x] Branch replays the exact prefix or reports `conversation_unreplayed`.
  - [x] Event payloads never carry prompt text or raw tool output.

**Acceptance criteria:**

- [x] Changed turns cannot claim success without verified evidence.
- [x] Stop gate behavior is deterministic and replay-safe.
- [x] New event names are stable-contract tested.
- [x] Branch output identifies whether conversation was replayed.
- [x] `cargo test --quiet -p ee-agent-orchestrator completion` and `plugin_runtime` pass.

## Phase 6 — Integration Gates

**Goal:** prove Phases 2, 3, and 4 are coherent, deterministic, and regression-free.

**Overview:** this phase adds no product behavior. It runs the full local gate set and the existing replay/evaluation gates that guard prompt, policy, and model-default changes.

**Rules:**

- Do not run plain `cargo test`; use `--quiet` and the memory guard for full suites.
- Tests must not read real user config, plugin roots, or session directories.
- Prompt changes require the replay/evaluation gate before default model, prompt, route, or policy changes land.
- Any failure names the phase, fixture, or invariant that failed.

**Work items:**

- [x] Run format/lint/docs.
  - [x] `cargo fmt --all --check`.
  - [x] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`.
  - [x] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`.
- [x] Run targeted phase tests.
  - [x] `cargo test --quiet -p ee-agent-orchestrator prompt::`.
  - [x] `cargo test --quiet -p ee-agent-orchestrator compaction`.
  - [x] `cargo test --quiet -p ee-agent-orchestrator session_log`.
  - [x] `cargo test --quiet -p ee-agent-orchestrator completion`.
  - [x] `cargo test --quiet -p ee-agent-orchestrator plugin_runtime`.
  - [x] `cargo test --quiet -p ee-agent-orchestrator checkpoint`.
- [x] Run replay/evaluation gates.
  - [x] `cargo test --quiet -p ee-agent-orchestrator --features test-utils evaluation`.
  - [x] `cargo test --quiet -p ee-agent-host --test orchestrated_replay --locked`.
  - [x] Existing phase fixture matrix gate.
- [x] Run full workspace.
  - [ ] `scripts/run-tests-memguard.sh cargo test --quiet --workspace --all-features --locked`.
  - [ ] Or `mk ci` when the full local gate is desired.
- [x] Verify invariants.
  - [x] Every model request reconstructable from session log.
  - [x] Transcript truncation never deletes durable log records.
  - [x] Runtime delta falls back to full snapshot on resume/truncation/compaction.
  - [x] Protected memory survives compaction funnel.
  - [x] Stop gate blocks unverified changed turns.
  - [x] `agent.step.before` and `agent.stop.before` names are stable.
  - [x] No real home/config/session paths in tests.
- [x] Update this plan's checkboxes only after each gate passes.

**Acceptance criteria:**

- [x] All targeted and full gates pass under the memory guard.
- [x] Replay traces remain deterministic with and without plugins.
- [x] Stable prompt is byte-identical across turns.
- [x] No raw prompt or tool output enters telemetry/export paths.
- [x] Phase 2, 3, and 4 checklists are complete before Phase 6 is marked done.
---

# Agent Harness Audit — Remaining Wiring Gaps & Next-Wave Improvements

**Goal:** close remaining surface wiring gaps between `ee-agent-orchestrator` and the provider/ACP/CLI layers, and implement next-wave harness improvements for prompt cacheability, context efficiency, worktree isolation, async background jobs, skills loading, subagent continuity, and risk-tiered auto-approval.

**Overview:** Phases 0–6 of the DeepSeek Harness Prompt Sections Plan and Phases 2, 3, 4a, 4b, and 6 of the Unified Harness Runtime Plan are implemented in `crates/ee-agent-orchestrator`. However, three capabilities in `ee-agent-orchestrator` are not yet wired through provider binaries or ACP/CLI surfaces, and nine high-impact harness capabilities remain open for next-wave parity and performance.

---

## Part 1 — Production Surface Wiring Gaps

### Gap 1 — Durable `session_log_dir` Not Wired in Provider Binaries (`ee-agent-openrouter` & `ee-agent-opencode`)

- [x] Wire opt-in durable `session_log_dir` in `ee-agent-openrouter` and `ee-agent-opencode`.
  - [x] **Problem:**
    - `OrchestratorProviderConfig::session_log_dir` (`crates/ee-agent-orchestrator/src/provider_adapter/mod.rs:102`) defaults to `None`, which keeps `SessionLogStore` in memory (`InMemorySessionLogSink`) for the process lifetime only.
    - Both `openrouter_orchestrator_config` (`crates/ee-agent-openrouter/src/orchestrated.rs:44-75`) and `opencode_orchestrator_config` (`crates/ee-agent-opencode/src/adapter.rs:274-303`) wire `session_state_dir: Some(session_state_dir)` for normal session snapshots, but leave `session_log_dir: None` and provide no CLI flag or environment variable to enable `FileSessionLogSink`.
    - As a result, append-only `ModelRequestRecord` entries and content-addressed `StoredMessageBody` blobs are lost on agent process exit, preventing post-crash request reconstruction and cross-restart log-backed checkpoint branching.
  - [x] **Fix:**
    - Add explicit opt-in configuration (`EE_SESSION_LOG_DIR` / `--session-log-dir`, or boolean opt-in `EE_SESSION_LOG_DURABLE=1` defaulting under `<session_state_dir>/session-log`) in `crates/ee-agent-openrouter/src/config.rs` and `crates/ee-agent-opencode/src/config.rs`.
    - Populate `OrchestratorProviderConfig.session_log_dir` in `openrouter_orchestrator_config` and `opencode_orchestrator_config` when enabled so `apply_session_log_dir` (`crates/ee-agent-orchestrator/src/provider_adapter/builder.rs:161-166`) configures `SessionLogConfig::durable(dir)`.
    - Keep `SensitiveDataGuard` redaction enforced on export and ensure file permissions are `0600` / `0700` on Unix.
  - [x] **Acceptance criteria:**
    - Setting `EE_SESSION_LOG_DIR` (or `EE_SESSION_LOG_DURABLE=1`) writes `session-log/` records and content-addressed message bodies to disk in both `ee-agent-openrouter` and `ee-agent-opencode`.
    - Leaving the setting unset preserves default in-memory behavior with zero disk writes of raw message bodies.
    - Unit tests in `ee-agent-openrouter` and `ee-agent-opencode` verify `provider_config.session_log_dir` and `provider_config.orchestrator.session_log.dir` propagation.

### Gap 2 — Checkpoint Branching (`branch_from_checkpoint`) Has No ACP or CLI Surface

- [x] Expose `OrchestratorRuntime::branch_from_checkpoint` through ACP slash command and host UI.
  - [x] **Problem:**
    - `OrchestratorRuntime::branch_from_checkpoint(session_id, checkpoint_sequence)` (`crates/ee-agent-orchestrator/src/runtime/branch.rs:12-54`) and `SessionLogStore::branch_from` (`crates/ee-agent-orchestrator/src/session_log.rs:413-453`) are implemented and tested in `crates/ee-agent-orchestrator/src/runtime/tests/branch_tests.rs`, but have zero callers in `provider_adapter`, `ee-agent-protocol`, `ee-agent-host`, or `ee-cli`.
    - Unlike `/compact`, `/resume`, `/discard`, and `/rubber-duck` (`crates/ee-agent-orchestrator/src/provider_adapter/agent_impl.rs:130-237`), users cannot trigger a checkpoint branch from the agent pane or editor command palette.
  - [x] **Fix:**
    - Define `BRANCH_COMMAND_NAME` (`branch`) and `branch_available_command()` in `crates/ee-agent-protocol`.
    - Advertise `/branch [checkpoint_seq]` in `OrchestratorProvider` available slash commands (`crates/ee-agent-orchestrator/src/provider_adapter/mod.rs`).
    - Handle `/branch` in `OrchestratorProvider::prompt` (`crates/ee-agent-orchestrator/src/provider_adapter/agent_impl.rs`): resolve latest or specified `checkpoint_seq` for the current session, invoke `runtime.branch_from_checkpoint(&session_id, seq)`, register the newly branched `SessionRuntime` (with replayed conversation from `BranchOutcome` or `conversation_unreplayed` notice), and emit a structured summary chunk with the new session id and replay status.
    - Wire `:agents_branch` / agent pane keybinding in `crates/ee-cli` to dispatch `/branch` or switch active pane session to the branched session id.
  - [x] **Acceptance criteria:**
    - `/branch` forks the current session from the target checkpoint sequence, clones task/memory/budget state, replays log prefix up to `checkpoint_seq` when `SessionLog` is available, and surfaces `conversation_unreplayed: true` when log is absent.
    - Missing checkpoint or cross-session sequence mismatch fails closed with a clear diagnostic without mutating active session state.

### Gap 3 — Live Session History Truncates to 16 Text Messages Before Compaction & Turn Loop

- [x] Preserve richer multi-turn history and tool-call provenance into `DefaultCompactionStrategy` and `run_turn_strategic_recoverable_with_history`.
  - [x] **Problem:**
    - `OrchestratorProvider::prompt` (`crates/ee-agent-orchestrator/src/provider_adapter/agent_impl.rs:238`) constructs `history` via `recent_live_history(&session.conversation)` (`crates/ee-agent-orchestrator/src/provider_adapter/conversation.rs:23-62`), which caps at `MAX_RECENT_TURNS_MESSAGES = 16` and only stores `ConversationRole::{User, Agent, FinalResponse}` text messages (omitting prior turns' `ModelRole::Tool` outputs and `tool_calls`).
    - Because `history` is already pre-sliced to 16 text-only messages before `auto_compact_before_prompt` (`crates/ee-agent-orchestrator/src/provider_adapter/auto_compact.rs:33-106`) runs:
      1. `prune_redundant_readonly_tool_outputs` (`crates/ee-agent-orchestrator/src/compaction/pipeline.rs:334-398`) has no `ModelRole::Tool` entries to deduplicate across turns at the provider boundary.
      2. `auto_compact_before_prompt` evaluates token pressure only on the 16-message tail (capped at 64 KiB) rather than the full accumulated session transcript, delaying automatic compaction until memory/prompt overhead alone crosses `trigger_tokens` or messages silently drop off the 16-message sliding window without being summarized into `SESSION_SUMMARY_KEY`.
  - [x] **Fix:**
    - Retain a bounded session-level normalized `Vec<ModelMessage>` transcript (up to `CONVERSATION_MAX_MESSAGES = 256`, including compact tool-call/tool-result records or log-backed message references) alongside the ACP replay `conversation` log in `SessionRuntime`.
    - When the session transcript exceeds the sliding window or crosses `CompactionConfig::trigger_tokens`, run `DefaultCompactionStrategy` over the full uncompacted message slice so dropped older turns are folded into `SESSION_SUMMARY_KEY` (`session.compacted_summary`) before slicing the post-compaction tail for `run_turn_strategic_recoverable_with_history`.
    - Also invoke `prune_redundant_readonly_tool_outputs` inside `LoopEngine` intra-turn transcript assembly before each model step so repeated `read_file`/`search_text` calls within a long multi-step turn are deduplicated before hitting the model context window.
  - [x] **Acceptance criteria:**
    - Messages aging out beyond the active turn window are summarized into `session.compacted_summary` via `DefaultCompactionStrategy` rather than silently dropping off the 16-message tail.
    - Intra-turn and cross-turn duplicate read-only tool outputs (`read_file`, `ee_search_text`, `ee_file_outline`, etc.) are pruned losslessly before model requests.

---

## Part 2 — Next-Wave Harness Improvements

### Improvement 1 — Explicit Provider Prompt-Cache Breakpoints (`cache_control` & `prompt_cache_key`)

- [x] Emit provider-specific prompt cache markers on stable and pinned prefix planes.
  - [x] **Problem:**
    - `PLAN-harness.md` §17 and Phase 2 split `PromptBundle.stable`, `PromptBundle.global`, `PromptBundle.workspace`, `PromptBundle.mcp`, and `RuntimeSnapshotMode::Delta` specifically to keep the leading prefix byte-identical across turns for LLM KV-cache reuse.
    - However, `crates/ee-chat-completions`, `crates/ee-agent-opencode/src/messages.rs` (Anthropic Messages dialect), and `crates/ee-agent-opencode/src/responses.rs` (OpenAI Responses dialect) do not emit explicit `cache_control: {"type": "ephemeral"}` breakpoints or `prompt_cache_key` / session cache headers.
    - Without explicit cache markers on Anthropic and OpenRouter Anthropic routes, providers re-tokenize and bill full input price on every tool step for the stable system prompt, tool definitions, and workspace instructions.
  - [x] **Fix:**
    - Add a `CacheHint` / prefix-breakpoint marker on `ModelRequest` or `ModelMessage` metadata (`ee_cache_breakpoint=true`) for the stable system prompt, last tool definition, and last static instruction message (`global`/`workspace`).
    - In `crates/ee-agent-opencode/src/messages.rs`, attach `cache_control: {"type": "ephemeral"}` to the system block, final tool schema entry, and last pinned instruction block (respecting Anthropic's 4-breakpoint limit).
    - In `crates/ee-chat-completions`, support opt-in OpenRouter/Anthropic multipart content `cache_control` breakpoints when targeting Anthropic family models.
    - In `crates/ee-agent-opencode/src/responses.rs`, pass deterministic session-scoped `prompt_cache_key` when supported by the route profile.
    - Surface `cached_input_tokens` / `cache_creation_input_tokens` in `TokenUsage` and `OrchestratorMetrics`.
  - [x] **Acceptance criteria:**
    - Anthropic Messages and OpenRouter Anthropic requests include deterministic `cache_control` breakpoints on stable prefix boundaries without mutating message text.
    - `UsageUpdate` and `TelemetrySummary` report cache hit/write token counts when returned by the provider.

### Improvement 2 — Dynamic Mid-Turn Subdirectory `AGENTS.md` Discovery

- [x] Discover nested `<subdir>/AGENTS.md` and `CLAUDE.md` dynamically when tools access files below `cwd`.
  - [x] **Problem:**
    - `prompt::workspace::discover` (`crates/ee-agent-orchestrator/src/prompt/workspace.rs:85-162`) walks ancestor directories from the workspace root to `cwd` once at turn start (`OrchestratorProvider::prompt_bundle`).
    - In monorepos where nested crates or packages (`crates/foo/AGENTS.md`, `packages/bar/CLAUDE.md`) define directory-specific rules below `cwd`, the agent never sees those instructions when reading or editing files inside `crates/foo/` during a turn.
  - [x] **Fix:**
    - Track a per-session `HashSet<String>` of already-injected workspace instruction content hashes (`WorkspaceInstructionSource.content_hash`) initialized from turn-start `WorkspaceInstructions`.
    - In `LoopEngine` after `read_file`, `write_file`, `ee_edit_apply`, or `ee_file_outline` succeeds on a workspace path `P`, walk directories between `cwd` and `P.parent()` for `AGENTS.md`, `CLAUDE.md`, `AGENTS.local.md`, and `CLAUDE.local.md`.
    - If unseen instruction files are found (within the remaining 64 KiB `MAX_WORKSPACE_INSTRUCTION_BYTES` budget), sanitize `</system-reminder>` tags and inject a `TrustLevel::WorkspaceInstructionUntrusted` message before the next model call.
  - [x] **Acceptance criteria:**
    - Touching `crates/sub/src/lib.rs` mid-turn injects `crates/sub/AGENTS.md` exactly once per session under `TrustLevel::WorkspaceInstructionUntrusted`.
    - Already-seen instruction files (by SHA-256 content hash) and files outside workspace roots or above the 64 KiB cap are never re-injected.

### Improvement 3 — Multimodal Compaction Offload Pass (`compaction-image-offload`)

- [x] Offload binary `Image`, `Audio`, and large `EmbeddedResource` blocks from older turns before LLM compaction.
  - [x] **Problem:**
    - Multimodal ACP inputs (`ModelContent::{Image, Audio, EmbeddedResource}`) are now preserved across `ee-agent-orchestrator`, `ee-chat-completions`, and `ee-agent-opencode`.
    - However, `DefaultCompactionStrategy` (`crates/ee-agent-orchestrator/src/compaction/pipeline.rs`) and `estimate_message_bytes` do not strip or offload base64 binary payloads in older turns before running LLM summarization or retaining the sliding window.
    - Large base64 images or audio clips in historical turns consume massive context budget and can cause compaction requests themselves to exceed context limits.
  - [x] **Fix:**
    - Add a deterministic stage in `DefaultCompactionStrategy` (between Stage 2 read-only pruning and Stage 3 windowing) that replaces `ModelContent::Image` and `ModelContent::Audio` blocks older than the most recent `N` user turns (e.g. `keep_recent_multimodal_turns = 2`) with a compact `ModelContent::Text` placeholder: `[image offloaded: mime=<mime_type>, size=<bytes>B, sha256=<short_hash>]`.
    - Keep full binary payloads in the content-addressed `SessionLog` store so replay/audit remains lossless.
  - [x] **Acceptance criteria:**
    - Historical multimodal blocks beyond the retention window are replaced with deterministic text placeholders before windowing and LLM summary calls.
    - Active-turn and immediate prior-turn multimodal content remains intact for the model.

### Improvement 4 — Git Worktree Isolation for Write Subagents & Parallel Sessions

- [x] Add optional `git worktree` isolation for `implementer` subagents and concurrent write sessions.
  - [x] **Problem:**
    - Phase 5 (Worktree isolation) was excluded from the initial Unified Harness Runtime Plan (`ISSUES.md:1067`).
    - Currently, parallel write sessions fail closed on overlapping buffer/path claims (`WriteConflict` in `docs/upgrades/parallel-agent-sessions.md`), and `implementer` subagents spawned via `delegate_task` mutate the shared working tree directly, making speculative or parallel subagent edits unsafe.
  - [x] **Fix:**
    - Add an opt-in `WorkspaceIsolation::GitWorktree` mode on `SubagentSpawnSpec` and session configuration.
    - When enabled in a git repository, create a temporary detached or branch-scoped worktree under `<session_state_dir>/worktrees/<session_or_child_id>` before executing `implementer` writes, scoping the child's `cwd`, `workspace_roots`, and `PolicyEngine` write roots to that worktree.
    - On child completion, capture `git diff` / changed file inventory as verified handoff evidence, allow the root agent or host to apply/cherry-pick the patch cleanly into the primary workspace, and auto-prune the worktree on session close or `/discard`.
  - [x] **Acceptance criteria:**
    - Worktree-isolated `implementer` subagents cannot mutate files in the primary checkout directly.
    - Parallel isolated subagents can edit overlapping files in separate worktrees without `WriteConflict` until merge/apply time.
    - Stale worktrees are cleaned up deterministically on cancellation, `/discard`, and `session/close`.

### Improvement 5 — Async Background Terminals & Reactive Session Wakeup (`TOOL_JOBS` Parity)

- [x] Support background terminal jobs with runtime status projection and reactive completion notices.
  - [x] **Problem:**
    - `ee_terminal_spawn` and `ee_terminal_wait` (`crates/ee-mcp`) require the model to block or poll `ee_terminal_output` / `ee_terminal_wait` synchronously within a turn.
    - Long-running builds, dev servers, or test suites either block the turn loop until `tool_timeout` expires or require manual user re-prompting to check status.
  - [x] **Fix:**
    - Extend terminal management in `ee-agent-host` / `ee-mcp` with a background job registry per session (`background: true` on `ee_terminal_spawn` or auto-background after `wait_ms_before_async`).
    - Project active/completed background jobs into `RuntimeSnapshot` via a new `BACKGROUND_JOBS` runtime context section (ordered in `RUNTIME_CONTEXT_ORDER` and hashed for `RuntimeSnapshotMode::Delta`).
    - Emit a bounded system-reminder notice at the next step boundary (or wake an idle session via ACP update) when a background terminal job exits.
  - [x] **Acceptance criteria:**
    - Model can launch a build/test command with `wait_ms_before_async`, continue read/analysis steps while it runs, and receive exit status + tail output automatically on the step after completion.
    - All background jobs are terminated cleanly on `session/cancel` and `session/close`.

### Improvement 6 — Per-Tool Category Timeout Tiers

- [x] Replace flat global `tool_timeout` with per-category timeout tiers and bounded per-call override.
  - [x] **Problem:**
    - `OrchestratorConfig::tool_timeout` (`crates/ee-agent-orchestrator/src/config.rs`) applies one flat `120s` timeout to every tool call.
    - Fast local read/LSP tools (`read_file`, `ee_file_outline`, `ee_diagnostics`) should fail fast (15s–30s) when hung, whereas workspace test/build runs via `ee_terminal_wait` legitimately need up to 300s when explicitly requested.
  - [x] **Fix:**
    - Add `ToolTimeoutPolicy` to `OrchestratorConfig` with category defaults:
      - `fast_local_read`: `15s` (`read_file`, `list_files`, `ee_file_outline`, `ee_diagnostics`, `ee_workspace_roots`)
      - `search_and_mcp`: `30s` (`search_text`, `ee_search_text`, external MCP reads)
      - `write_and_edit`: `60s` (`write_file`, `ee_edit_apply`, `ee_format_buffer`, `ee_lsp_rename`)
      - `terminal_default`: `120s` (`ee_terminal_wait`, validation commands)
      - `terminal_max`: `300s` (opt-in via bounded `timeout_ms` argument on `ee_terminal_wait`)
    - Enforce `timeout_ms.clamp(1_000, terminal_max)` inside `ToolExecutor`.
  - [x] **Acceptance criteria:**
    - Hung read/search tools time out at their category ceiling (15s/30s) without waiting 120s.
    - `ee_terminal_wait` respects an explicit `timeout_ms` parameter up to `terminal_max` (`300s`), clamped against `OrchestratorConfig`.

### Improvement 7 — First-Class Skills Discovery & On-Demand `load_skill` Tool (`SKILLS` Parity)

- [ ] Discover workspace/user `SKILL.md` frontmatter and provide a read-only `load_skill` tool.
  - [ ] **Problem:**
    - `SKILLS` is marked `IntentionallyOmitted` in `crates/ee-agent-orchestrator/src/prompt/sections.rs:656`, even though the `ee` repository itself stores agent skills under `.agents/skills/*/SKILL.md` and `skills/system/*/SKILL.md`.
    - Without skill catalog discovery and an on-demand loader tool, the agent in `ee` agent mode either misses repo-specific skills entirely or must manually grep for `SKILL.md` files.
  - [ ] **Fix:**
    - Add `prompt/skills.rs` to scan `<workspace>/.agents/skills/*/SKILL.md`, `<workspace>/skills/**/SKILL.md`, and `$XDG_CONFIG_HOME/ee/skills/*/SKILL.md` for YAML frontmatter (`name`, `description`), bounded to 64 skills and 8 KiB total catalog text.
    - Render a compact skill catalog block (name + 1-line description + path) into `WorkspaceInstructions` or a dedicated runtime/workspace section when skills are present, under `TrustLevel::WorkspaceInstructionUntrusted` for repo skills and `TrustLevel::UserInstructionTrusted` for user-global skills.
    - Register a built-in read-only `load_skill` tool (and `ToolGuidance` entry in `prompt/tool_guidance.rs`) that reads and returns a requested skill's `SKILL.md` content within workspace/config boundaries.
  - [ ] **Acceptance criteria:**
    - Repositories with `.agents/skills/*/SKILL.md` surface available skill names and descriptions in the prompt catalog only when skills exist.
    - Calling `load_skill` returns the validated `SKILL.md` body and rejects path traversal or symlinks escaping the skill root.

### Improvement 8 — Conversational / Resumable Subagents (`agent_id` Resume & Context Fork)

- [ ] Allow `delegate_task` subagents to be resumed by `agent_id` or forked from parent context.
  - [ ] **Problem:**
    - `DelegateTool` and `SubagentManager` (`crates/ee-agent-orchestrator/src/subagents/mod.rs`) only execute one-shot stateless subagent runs (`explorer`, `planner`, `implementer`, `verifier`, `reviewer`).
    - If a root agent needs a follow-up question answered by an `explorer` that already read 15 files, spawning a new `delegate_task` starts from a blank transcript and re-reads the same files, wasting tokens and latency.
  - [ ] **Fix:**
    - Retain completed subagent transcripts in `ChildRegistry` / `SessionLogStore` keyed by `child_id` (bounded per session, e.g. up to 8 retained child transcripts).
    - Add an optional `resume_child_id: Option<String>` parameter on `delegate_task` so the root agent can send a follow-up task to an existing subagent instance while preserving its prior message history and role policy.
    - Add an optional `fork_context: bool` flag for `explorer` / `planner` subagents to inherit the parent's compacted summary and current runtime snapshot without re-discovering workspace basics.
  - [ ] **Acceptance criteria:**
    - Passing `resume_child_id` to `delegate_task` continues the target child's transcript and increments its bounded step/token accounting under the parent session budget.
    - Cross-session or mismatched-role `resume_child_id` requests fail closed.

### Improvement 9 — Opt-In Risk-Tiered Auto-Approval Classifier (`REVIEW_POLICY` Parity)

- [ ] Add an opt-in structured risk classifier for medium-risk tool calls outside static `trusted-write` rules.
  - [ ] **Problem:**
    - `ee` currently combines static `PolicyEngine` rules (`crates/ee-agent-orchestrator/src/policy.rs`) with host-owned `TrustRule` persistence (`crates/ee-cli/src/policy/store.rs`).
    - Any command or write outside pre-existing static `TrustRule` patterns immediately blocks on manual user approval in `trusted-write` mode, whereas `unrestricted` mode bypasses approval entirely. There is no middle tier that uses a fast/contrasting model to classify command risk (`low` / `medium` / `high`) before prompting the user.
  - [ ] **Fix:**
    - Add an opt-in `ApprovalClassifier` hook in `crates/ee-agent-orchestrator/src/policy.rs` / `crates/ee-agent-host` (disabled by default; enabled via `agents.approval_classifier = "auto"`).
    - When a tool call requires `ApprovalRequirement::NeedsApproval` and is not covered by a persisted `TrustRule` allow/deny entry:
      1. Hard-blocked patterns (secret files, `.git/hooks`, `.ee.toml`, credential env vars, destructive root commands, external network writes) immediately escalate or deny without calling the classifier.
      2. Otherwise, run a single bounded, non-streaming classification call on the registered critic/fast model with a strict JSON schema `{ "risk": "low" | "medium" | "high", "reason": "<bounded string>" }` and `TrustLevel::UntrustedToolOutput` isolation.
      3. Only `risk == "low"` auto-approves (logged in `DecisionLog` and `OrchestratorEvent`); `medium`, `high`, timeout, or schema failure falls back to the interactive host approval prompt.
  - [ ] **Acceptance criteria:**
    - Classifier is strictly opt-in and never overrides mandatory `PolicyEngine` denials or persisted `TrustRule` deny rules.
    - Classifier timeout, parse error, or `medium`/`high` classification always fails safe to interactive host approval.
    - Every auto-approved decision records an auditable `DecisionLog` entry with model id, tool name, and redacted reason.
