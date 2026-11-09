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
  - [x] Update `ee-openrouter-agent` to construct `AgentPluginHost` and register its model adapter.
  - [x] Update `ee-opencode-agent` to construct `AgentPluginHost` and register its model adapter.
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
- `cargo test --quiet -p ee-openrouter-agent` passes.
- `cargo test --quiet -p ee-opencode-agent` passes.
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

- [ ] Delete old plugin crates and modules.
  - [ ] Delete `crates/xi-plugin-lib/**`.
  - [ ] Delete `crates/xi-plugin-derive/**`.
  - [ ] Delete `crates/xi-core-lib/src/plugins/**`.
  - [ ] Delete `crates/xi-core-lib/src/tabs/{plugins,plugin_lifecycle,plugin_events}.rs`.
  - [ ] Delete `crates/xi-core-lib/src/event_context/plugins.rs`.
  - [ ] Delete old plugin fields from `CoreState`, `EventContext`, and `make_context`.
  - [ ] Delete `CoreNotification::Plugin`, `CorePluginNotification`, and old plugin control RPC.
  - [ ] Delete `Client::{plugin_started, plugin_stopped, plugin_terminated, available_plugins}` and `ClientPluginInfo`.
  - [ ] Delete `xi_core_lib::{plugin_manifest, plugin_rpc}` re-exports.
- [ ] Clean workspace and dependencies.
  - [ ] Remove `xi-plugin-lib` and `xi-plugin-derive` from root `Cargo.toml` members and workspace dependencies.
  - [ ] Remove `wasmtime`, `seccomp`, and `windows-sys` from `xi-core-lib/Cargo.toml` after Phase 6 moves them.
  - [ ] Add `ee-plugin-runtime` to `ee-cli` for plugin diagnostics only.
  - [ ] Remove any old plugin imports from `ee-cli`, `xi-lsp-lib`, and provider crates.
- [ ] Update packaging and installers.
  - [ ] Update `scripts/install-plugins.sh` to install v2 manifest and native/Wasm entry layout.
  - [ ] Update `install.sh` `install_plugin_assets` to validate `manifest_version = 2` before copying.
  - [ ] Update `.github/workflows/release.yml` build and package steps for the v2 plugin layout.
  - [ ] Update `.github/workflows/fuzz.yml` target name and matrix.
  - [ ] Update any `tasks.yaml` entries that reference old plugin paths.
- [ ] Update test harnesses.
  - [ ] Update `ee-cli/src/tests/helpers.rs` `install_test_lsp_plugin` to copy the v2 manifest.
  - [ ] Replace old `PluginDescription` construction in `ee-cli/src/tests/notifications.rs` with v2 status/contribution fixtures.
  - [ ] Replace `probe_running_plugins` / `PluginCatalog` tests in `ee-cli/src/plugins_cmd.rs` with runtime diagnostics tests.
  - [ ] Update `ee-cli/src/tests/commands.rs` for `ee do plugins list/ls` diagnostics output.
  - [ ] Remove `test_assets` or fixture references that depend on old plugin manifests.
- [ ] Update CLI plugin management.
  - [ ] Implement `ee do plugins list` using `PluginManager::diagnostics()`.
  - [ ] Implement `ee do plugins validate [path]` using manifest v2 validation.
  - [ ] Implement `ee do plugins schema` writing `schemas/plugin-manifest-v2.json`.
  - [ ] Add CLI tests for list, validate, schema, blocked workspace plugin, and invalid v1 manifest rejection.
- [ ] Regenerate schemas and snapshots.
  - [ ] Regenerate `schemas/ee-config.schema.json` with the new `[plugins]` section.
  - [ ] Generate `schemas/plugin-manifest-v2.json` from `ManifestV2`.
  - [ ] Update snapshots for contribution payloads, diagnostics, and LSP manifest.
- [ ] Run deletion gates.
  - [ ] Assert `test ! -d crates/xi-plugin-lib`.
  - [ ] Assert `test ! -d crates/xi-plugin-derive`.
  - [ ] Assert no `manifest_version = 1`, `xi_plugin`, `xi-plugin-lib`, `plugin_rpc`, `HostNotification`, `PluginNotification`, `CoreProxy`, or `PluginCatalog` in `crates`, `scripts`, `fuzz`, `.github`, or `Cargo.toml`.
  - [ ] Assert no old LSP `Action::Request*` or `EditNotification` LSP variants remain.
- [ ] Run full validation.
  - [ ] `cargo fmt --all --check`.
  - [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`.
  - [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`.
  - [ ] `./scripts/test-workspace-summary.sh`.
  - [ ] `cargo test --quiet -p ee-plugin-api`.
  - [ ] `cargo test --quiet -p ee-plugin-runtime`.
  - [ ] `cargo test --quiet -p xi-core-lib`.
  - [ ] `cargo test --quiet -p ee-agent-orchestrator`.
  - [ ] `cargo test --quiet -p ee-cli --features agents`.
  - [ ] `cargo test --quiet -p xi-lsp-lib`.
  - [ ] `cargo fuzz build ee_plugin_wasm`.
- [ ] Add final regression gates.
  - [ ] Assert core editing, undo, selection, and VLF tests pass unchanged.
  - [ ] Assert LSP hover/completion/definition/references/formatting/code actions/symbols pass through v2 registration.
  - [ ] Assert agent replay fixtures remain deterministic.
  - [ ] Assert a v2 plugin can register a command, status item, panel, decoration, agent tool, and model adapter.
  - [ ] Assert a v1 manifest is rejected with a typed diagnostic.
  - [ ] Assert `tasks.yaml` `lf` reports no new file over 1,000 LOC.

**Acceptance criteria:**

- All deletion gates pass with zero matches.
- `cargo fmt --all --check`, clippy, docs, and workspace tests pass.
- `./scripts/test-workspace-summary.sh` passes.
- LSP and agent regression suites pass through v2 registration only.
- `schemas/plugin-manifest-v2.json` and `schemas/ee-config.schema.json` are current.
- Release and install scripts package v2 manifests only.
- No old plugin crate, module, manifest parser, RPC type, or lifecycle path remains in the repository.
