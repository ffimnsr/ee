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
- [x] On save permission denial, detect write failure and prompt user to retry with elevated privileges via `sudo`, `su`, or `run0`
  - [x] Trigger only on permission-related save errors, not generic I/O failures
  - [x] Preserve pending buffer changes before privileged re-execution
  - [x] Show target path and exact elevated command before confirmation
- [ ] Make sparse editing workable for VLF (very large files)
  - [ ] Support insert/replace/delete against unloaded regions without loading entire file into memory
  - [ ] Keep cursor/selection mapping correct when edits shift later offsets
  - [ ] Save only touched spans plus required surrounding context, then verify on-disk patch result
- [x] Fix split views for same file opening empty peer buffer
  - [x] Repro: open file, run `vsplit` or `hsplit`, and show same file in both panes; second pane must render existing buffer content instead of blank view
  - [x] Both panes must stay attached to same underlying document state so edits in one pane appear in other without reopening file
  - [x] Switching focus, resizing splits, or closing one pane must not clear or detach remaining pane buffer
  - [x] Add regression coverage for same-file split behavior in both vertical and horizontal splits

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

- [x] Phase 3: design and load tree-sitter indent query assets.
  - Why: long-term smart indent should use same runtime grammar/query architecture instead of hardcoded per-language indentation logic.
  - [x] Extend runtime query model.
    - [x] Define minimal indent-query contract and capture vocabulary for `indent.scm` or equivalent runtime asset.
    - [x] Extend runtime loader in `crates/xi-core-lib` to discover and cache indent query assets alongside existing query types.
    - [x] Keep missing indent-query assets isolated so languages without support still edit normally.
  - [x] Preserve architecture boundaries.
    - [x] Reuse existing language resolution and query directory precedence rules.
    - [x] Do not introduce a second grammar or query loading path just for indentation.

- [x] Phase 4: implement syntax-aware smart indent evaluation.
  - Why: tree-sitter gives structural context, but editor still needs backend logic that converts captures and syntax position into concrete indent edits.
  - [x] Add backend indent engine.
    - [x] Introduce backend-owned indent evaluation module in `crates/xi-core-lib` that computes newline indentation from syntax tree context plus buffer settings.
    - [x] Support at least inherit-indent, indent-one-level, and dedent-one-level outcomes for MVP.
    - [x] Fail closed to heuristic or baseline auto-indent when parse state incomplete, query missing, or language unsupported.
  - [x] Wire editor context into newline command.
    - [x] Pass enough syntax/runtime context from editor layer to newline path without pushing parser ownership into frontend.
    - [x] Keep text mutation logic separate from syntax-query evaluation so newline edits remain testable in isolation.

- [x] Phase 5: validate behavior, config, and mode-specific fallbacks.
  - Why: indentation features are high-frequency editing paths; must prove correctness, performance, and non-support behavior before broadening language coverage.
  - [x] Add regression and failure-path coverage.
    - [x] Baseline auto-indent tests for plain text and non-code buffers.
    - [x] Smart-indent tests for at least Rust, JSON, and one indentation-sensitive language only if query semantics are ready.
    - [x] Missing parser, missing indent query, and disabled config all fall back without panic or stale indentation artifacts.
    - [x] Multi-cursor and selection cases remain deterministic under smart-indent path too.
  - [x] Validate mode/performance constraints.
    - [x] Confirm large or constrained buffers avoid expensive whole-file syntax work on Enter.
    - [x] Define whether VLF or parser-disabled modes use baseline auto-indent only, heuristic smart-indent, or explicit unsupported status.
    - [x] Document config semantics and supported smart-indent behavior in user-facing docs once implementation lands.

### Optional Agents Mode Overview: ACP v1 + MCP 2026-07-28

Goal: optional agents mode provides ACP v1 agent chat plus MCP 2026-07-28 server integration without changing default editor behavior. Implementation must target latest supported protocols only. Prefer official protocol SDK crates over handrolled wire/protocol code whenever they support required behavior: ACP Rust SDK crates (`agent-client-protocol`, `agent-client-protocol-derive`, `agent-client-protocol-rmcp`, `agent-client-protocol-conductor`, `agent-client-protocol-http`, `agent-client-protocol-trace-viewer`) and official `rmcp` for MCP. Do not implement deprecated MCP features or compatibility shims: roots, sampling, logging, dynamic client registration, sampling `includeContext: "thisServer"` / `"allServers"`, or HTTP+SSE transport. Use Streamable HTTP and stdio only. Agents commands must be lowercase snake_case ex commands.

#### Phase 0: freeze agents-mode boundaries, feature gates, and config contract

- Rules:
  - Agents mode is disabled by default at compile time and runtime.
  - `xi-core-lib` remains frontend-agnostic and owns only editor truth: buffers, revisions, save semantics, and existing RPC operations.
  - `ee-cli` owns agents pane UI, key handling, prompts, confirmations, config loading, and terminal presentation.
  - Agent subprocesses and MCP servers start lazily only after user enables agents mode and opens the agents pane or runs an agents command.
  - All agent file writes must route through existing buffer/edit/save semantics; no raw writes behind active buffers.
  - All agent terminal executions and file writes require an approval path before execution.
  - ACP implementation targets latest ACP v1 only; unsupported ACP versions fail closed.
  - ACP wire types, method metadata, and transport helpers should come from official ACP Rust SDK crates when available; ee-owned code should be limited to editor integration, permission policy, adapters, and compatibility gaps documented in tests.
  - MCP implementation targets protocol version `2026-07-28` only; unsupported MCP server versions fail closed.
  - MCP wire types, client/server helpers, stdio transport, and Streamable HTTP support should use official `rmcp` crate when it supports required 2026-07-28 behavior; custom MCP code must be isolated behind thin adapters with tests proving why SDK coverage was insufficient.
  - Deprecated MCP features are not implemented as client features: `roots`, `sampling`, protocol `logging`, dynamic client registration, sampling `includeContext: "thisServer"` / `"allServers"`, and HTTP+SSE transport.
  - MCP migration paths are concrete: pass workspace files/directories through tool parameters, resource URIs, or server config; use direct LLM provider APIs instead of sampling; use stderr or OpenTelemetry instead of MCP logging; use Client ID Metadata Documents instead of dynamic registration; use Streamable HTTP instead of HTTP+SSE.
  - All agents ex commands are lowercase snake_case: `:agents`, `:agents_close`, `:agents_stop`, `:agents_new`, and `:agents_clear`. CamelCase command names are invalid.

- [x] Add workspace feature gates for optional agents support.
  - [x] Add `agents` feature to root `Cargo.toml` workspace wiring without enabling it in `default` features.
  - [x] Add optional workspace members for `crates/ee-agent-protocol`, `crates/ee-agent-host`, and `crates/ee-mcp` behind the `agents` feature.
  - [x] Ensure `cargo build --quiet -p ee-cli` does not compile agents crates when `agents` feature is omitted.
  - [x] Ensure `cargo build --quiet -p ee-cli --features agents` compiles agents crates and links agents-mode entry points.
- [x] Add resolved agents configuration model in `crates/ee-cli/src/config.rs`.
  - [x] Add `AgentsSettings` with `enabled: bool`, `default_agent: Option<String>`, and `servers: BTreeMap<String, AgentServerSettings>`.
  - [x] Add `AgentServerSettings` with `command: String`, `args: Vec<String>`, `env: BTreeMap<String, String>`, and `cwd: Option<PathBuf>`.
  - [x] Add `McpSettings` with `servers: BTreeMap<String, McpServerSettings>` for shared MCP configuration.
  - [x] Add `McpServerSettings::Stdio` with `command`, `args`, `env`, and `cwd` fields.
  - [x] Add `McpServerSettings::StreamableHttp` with `url`, `headers`, and `timeout_ms` fields.
  - [x] Keep `EditorSettings::default().agents.enabled == false`.
  - [x] Parse `[agents]`, `[agents.servers.<id>]`, `[mcp]`, and `[mcp.servers.<id>]` from TOML layers.
  - [x] Reject empty agent server ids, empty MCP server ids, empty commands, and invalid HTTP URLs during config validation.
- [x] Add config schema coverage for agents settings.
  - [x] Extend schema generation tests to include `agents.enabled`, `agents.default_agent`, `agents.servers`, and `mcp.servers` fields.
  - [x] Add config merge tests proving project-local `.ee.toml` can enable agents while built-in defaults keep them disabled.
  - [x] Add config validation tests for invalid agent command, invalid MCP URL, and duplicate effective server ids.
- [x] Add inert agents command surface in `ee-cli`.
  - [x] Add `:agents` command that reports agents disabled when compiled without `agents` feature.
  - [x] Add `:agents` command that reports agents disabled when `agents.enabled = false`.
  - [x] Add `:agents_stop` command stub that is a no-op without an active agent session.
  - [x] Reject CamelCase aliases such as `:Agents`, `:AgentClose`, `:AgentStop`, `:AgentNew`, and `:AgentClear`.
  - [x] Add command parser tests for `:agents`, `:agents_stop`, and rejected CamelCase forms.

- Criteria:
  - `cargo build --quiet -p ee-cli` succeeds without compiling or starting agents code.
  - `cargo build --quiet -p ee-cli --features agents` succeeds with agents crates enabled.
  - `cargo test --quiet -p ee-cli config` covers agents and MCP config defaults, merge behavior, and validation failures.
  - `:agents` has deterministic disabled behavior when feature or runtime config is off.

#### Phase 1: implement ACP v1 protocol crate

- Rules:
  - ACP schemas live in `crates/ee-agent-protocol`; `ee-cli` and `xi-core-lib` must not duplicate wire structs.
  - `crates/ee-agent-protocol` should primarily wrap or re-export official `agent-client-protocol` / `agent-client-protocol-derive` types; only keep handrolled structs for missing SDK coverage, ee-specific validation, or stricter fail-closed behavior.
  - ACP wire structs use serde-compatible camelCase and snake_case discriminator values required by ACP v1.
  - ACP paths are absolute on every protocol boundary.
  - ACP line numbers are 1-based at protocol boundary and converted at editor boundary only.
  - Unknown future enum variants deserialize into explicit `Other` variants only where ACP allows extensibility; unsupported behavior fails with JSON-RPC invalid params.

- [x] Create `crates/ee-agent-protocol` crate.
  - [x] Add crate to workspace members behind agents feature wiring.
  - [x] Add dependencies on `serde`, `serde_json`, `schemars`, and `thiserror` or existing error crate if already available.
  - [x] Expose `ACP_PROTOCOL_VERSION` constant for v1.
  - [x] Expose JSON-RPC 2.0 envelope types for request, response, error, and notification messages.
- [x] Implement full ACP initialization and capability types.
  - [x] Add `InitializeRequest`, `InitializeResponse`, `ClientCapabilities`, `AgentCapabilities`, `ImplementationInfo`, and `ProtocolVersion` types.
  - [x] Add capability fields for `fs.readTextFile`, `fs.writeTextFile`, `terminal`, `elicitation`, `loadSession`, `session.set_mode`, and auth logout.
  - [x] Add worktree/root-directory session context fields required by ACP v1 without reusing MCP roots semantics.
  - [x] Add strict version negotiation helper that accepts only ACP v1 and returns an ACP-compatible JSON-RPC error for any other version.
  - [x] Add unknown-capability handling that preserves advertised data for diagnostics but never enables unsupported behavior.
- [x] Implement ACP authentication and session setup types.
  - [x] Add `AuthMethod`, `AuthenticateRequest`, `AuthenticateResponse`, and `LogoutRequest` types.
  - [x] Add `SessionNewRequest`, `SessionNewResponse`, `SessionLoadRequest`, `SessionLoadResponse`, `SessionId`, `SessionModeId`, and `SessionConfig` types.
  - [x] Add serde tests for creating new sessions and loading sessions with capabilities enabled and disabled.
- [x] Implement full ACP prompt-turn and streaming update types.
  - [x] Add `PromptRequest`, `PromptResponse`, `StopReason`, `ContentBlock`, `ContentChunk`, `MessageId`, `Usage`, location/range, annotation, and binary-safe resource reference types required by ACP v1.
  - [x] Add `SessionUpdate` variants for user message chunks, agent message chunks, agent thought chunks, tool calls, tool updates, plans, available commands, current mode, config options, usage updates, and errors.
  - [x] Add ordering helpers that reject chunks referencing unknown message/tool ids unless ACP v1 marks them valid.
  - [x] Add `SessionCancel` notification type.
- [x] Implement full ACP client method types.
  - [x] Add `RequestPermissionParams`, `PermissionOption`, `PermissionOptionKind`, and `RequestPermissionOutcome` types.
  - [x] Add `ReadTextFileParams` and `WriteTextFileParams` with absolute-path validation helpers, optional line ranges, expected revision/content guards, and size limits matching ACP v1.
  - [x] Add `TerminalCreateParams`, `TerminalOutputParams`, `TerminalWaitForExitParams`, `TerminalKillParams`, and `TerminalReleaseParams` with environment, cwd, timeout, output-window, and exit-status fields.
  - [x] Add `CreateElicitationRequest`, form-mode schema types, URL-mode types, and `CreateElicitationResponse`.
  - [x] Add typed JSON-RPC method registry so every ACP request/notification name maps to exactly one params/result type.
- [x] Add ACP fixture and round-trip tests.
  - [x] Add JSON fixture tests for `initialize`, `session/new`, `session/prompt`, `session/update`, `session/request_permission`, `fs/read_text_file`, `fs/write_text_file`, and `terminal/create`.
  - [x] Assert fixture serialization matches ACP v1 method names exactly.
  - [x] Assert invalid relative file paths fail validation before request dispatch.
  - [x] Assert unsupported elicitation modes produce invalid params.

- [x] Audit completed handrolled ACP protocol crate against official ACP Rust SDK.
  - [x] Replace duplicated ACP schema structs with `agent-client-protocol` types where names, serde shape, and validation semantics match ACP v1.
  - [x] Use `agent-client-protocol-derive` for method registration or protocol glue when it reduces local registry code without weakening fail-closed behavior.
  - [x] Keep ee wrapper types only for absolute-path validation, 1-based line boundary checks, size limits, unknown-capability diagnostics, and unsupported-version errors not covered by SDK APIs.
  - [x] Add fixture tests proving SDK-backed serialization still matches existing ACP v1 fixtures exactly.

- Criteria:
  - `cargo test --quiet -p ee-agent-protocol` passes.
  - ACP v1 method names, discriminator values, and path/line invariants are covered by fixture tests.
  - Unsupported ACP protocol versions and invalid paths fail closed through typed errors.
  - Local ACP protocol code is reduced to SDK wrappers/adapters or documented SDK gaps.

#### Phase 2: implement ACP subprocess transport and agent session host

- Rules:
  - Agent subprocess lifecycle belongs to `ee-agent-host`, not `xi-core-lib`.
  - Agent stdout is reserved for ACP JSON-RPC; stderr is captured as diagnostics and never parsed as protocol.
  - Every in-flight ACP request has a timeout or explicit cancellation path.
  - Dropping an agent connection kills or gracefully terminates the subprocess and resolves pending requests.
  - Client request handlers run through a permission broker before any file write or terminal execution.

- [x] Create `crates/ee-agent-host` crate.
  - [x] Add dependency on `ee-agent-protocol`, official ACP SDK transport/helper crates where applicable (`agent-client-protocol-conductor`, `agent-client-protocol-http`, `agent-client-protocol-rmcp`), `tokio`, `serde_json`, and existing workspace utilities. (SDK core `Builder`/`ConnectionTo`/`ByteStreams` cover subprocess stdio; conductor/http/rmcp crates are not needed for the stdio host and are deferred to MCP phases.)
  - [x] Expose `AgentManager`, `AgentConnection`, `AgentThread`, `AgentEvent`, and `AgentError` public APIs behind `pub(crate)` where possible.
  - [x] Keep crate UI-free by exposing events and command methods instead of ratatui widgets.
- [x] Implement ACP stdio transport.
  - [x] Reuse official ACP SDK connection/conductor abstractions when they support subprocess stdio JSON-RPC framing, request correlation, and cancellation semantics. (`Client.builder().connect_with(ByteStreams, main_fn)` over the subprocess pipes.)
  - [x] Spawn configured agent subprocess with explicit `command`, `args`, `env`, and `cwd`.
  - [x] Read JSON-RPC messages from stdout on a background task. (SDK transport; stderr reader is separate.)
  - [x] Write JSON-RPC messages to stdin through a serialized writer task. (SDK transport.)
  - [x] Capture stderr into bounded ring buffer for diagnostics.
  - [x] Correlate responses by JSON-RPC id and resolve pending request channels. (SDK `SentRequest`/`block_task`; driver resolves via typed oneshots.)
  - [x] Dispatch agent-to-client requests to registered handlers.
  - [x] Dispatch agent notifications to session event handlers.
  - [x] Convert malformed JSON, unknown methods, dropped stdout, and child exit into typed connection errors.
- [x] Implement ACP connection handshake.
  - [x] Send `initialize` after subprocess starts.
  - [x] Advertise client capabilities for read text file, write text file, terminal, and form/url elicitation only when corresponding handlers are registered.
  - [x] Reject agents that do not negotiate ACP v1.
  - [x] Store agent capabilities for optional `session/load`, `session/set_mode`, and logout behavior.
  - [x] Add authenticate call path for agents that return auth methods.
- [x] Implement session lifecycle.
  - [x] Add `AgentManager::new_session(agent_id, worktree_roots)` that starts connection lazily and calls `session/new`.
  - [x] Add `AgentManager::load_session(agent_id, session_id)` only when agent advertises load-session capability.
  - [x] Add `AgentThread::send_prompt` that sends `session/prompt`, streams `session/update` events, and resolves with `PromptResponse`.
  - [x] Add `AgentThread::cancel` that sends `session/cancel`, marks pending local authorizations canceled, and clears running turn state.
  - [x] Add `AgentThread::set_mode` only when agent advertises mode switching.
- [x] Implement session update reducer.
  - [x] Append optimistic user message before prompt dispatch.
  - [x] Merge user, assistant, and thought chunks by message id.
  - [x] Upsert tool calls by tool call id.
  - [x] Update tool call status, title, content, locations, raw input, and raw output.
  - [x] Store plan entries and replace them on plan updates.
  - [x] Store token usage and cost updates.
  - [x] Emit deterministic `AgentEvent` values for every state change.
- [x] Implement permission broker API.
  - [x] Add `PermissionRequest` values with tool call id, requested action, options, and response channel.
  - [x] Add `AgentThread::respond_permission` that resolves exactly one pending permission.
  - [x] Add duplicate-response guard that ignores stale permission responses.
  - [x] Add cancel path that resolves outstanding permissions as cancelled.
- [x] Add fake ACP server test harness.
  - [x] Implement in-process fake agent transport for tests without spawning external binaries.
  - [x] Simulate initialize/session/new/session/prompt/session/update flows.
  - [x] Simulate agent-to-client permission, file, terminal, and elicitation requests.
  - [x] Simulate agent process exit and malformed JSON failures.

- Criteria:
  - `cargo test --quiet -p ee-agent-host` passes with fake ACP server coverage.
  - Agent connection cannot send prompts before successful ACP v1 initialization.
  - Session cancellation resolves pending prompt, permissions, elicitations, and transport requests deterministically.
  - Malformed protocol input and subprocess exit produce typed errors without panics or hanging tasks.
  - Any local ACP transport code exists only where official ACP SDK transport/conductor APIs cannot satisfy ee lifecycle, timeout, or permission-broker requirements.

#### Phase 3: implement optional irssi-style TUI agents pane in `ee-cli`

- Rules:
  - Agents pane is frontend-owned and must not alter normal editor modes when hidden.
  - Agents pane must feel like classic IRC/irssi: scrollback transcript, nick column, status bar, input prompt, thread/channel list.
  - Agents pane must not start an agent unless `agents.enabled = true` and user invokes agents mode.
  - Prompt editing, focus, scrolling, split layout, thread switching, and permission UI are `ee-cli` responsibilities.
  - Agent state comes from `ee-agent-host` events; UI must not parse ACP JSON directly.
  - UI must represent agents as chat participants and tasks as chat turns, not as raw protocol events.

- [x] Add agents UI state to `crates/ee-cli/src/app/state.rs` behind `agents` feature.
  - [x] Add `Mode::Agent` for focused agents pane input and preserve previous editor mode for focus return.
  - [x] Add `AgentPaneState` with active thread id, prompt draft, scroll offset, stick-to-bottom flag, visible messages, running turn status, pending permission/elicitation state, selected option, and last error.
  - [x] Add `AgentPaneLayout` values for closed, right split, bottom split, and full-screen chat layouts.
  - [x] Add IRC-style thread metadata: display name, unread count, activity marker, connected/disconnected/running state, and current agent nick.
  - [x] Add transcript item model for user messages, assistant chunks, thoughts, tool calls, plan entries, permissions, elicitations, system notices, stderr/debug entries, and local optimistic messages.
  - [x] Ensure app startup initializes agents state as closed, inactive, empty scrollback, no host process.
- [x] Wire agents commands.
  - [x] Implement `:agents` to open agents pane and start default configured agent lazily.
  - [x] Implement `:agents_close` to hide pane without killing running agent session.
  - [x] Implement `:agents_stop` to cancel current running turn and active terminal tasks in current thread.
  - [x] Implement `:agents_new` to create new agent session with current workspace roots and switch active thread to it.
  - [x] Implement `:agents_next` and `:agents_prev` to switch between agent threads like IRC channels.
  - [x] Implement `:agents_clear` to clear local pane scrollback only when no turn is running.
  - [x] Implement `:agents_layout right|bottom|full` for explicit split control.
  - [x] Ensure CamelCase command forms remain rejected in enabled builds too.
- [x] Implement agents key handling.
  - [x] Route typing keys to agent prompt draft when `Mode::Agent` is active.
  - [x] Submit prompt on Enter when draft is non-empty and no permission/elicitation prompt is selected.
  - [x] Insert newline on configured multiline key sequence.
  - [x] Cancel current turn on Esc when generation is active; otherwise return focus to previous editor pane.
  - [x] Page scrollback with PageUp/PageDown, half-page keys, mouse wheel, and Home/End if existing TUI input supports them.
  - [x] Keep scrollback pinned to bottom while new messages stream unless user has scrolled upward.
  - [x] Move selection across permission options with left/right or tab keys.
  - [x] Accept selected permission/elicitation option on Enter while prompt is active.
  - [x] Switch threads with existing tab/channel-like keybindings when agents pane is focused.
  - [x] Return focus to previous editor pane when agents pane closes.
- [x] Render irssi-style agents pane.
  - [x] Render bordered chat split with thread/channel list, transcript scrollback, status/footer bar, and composer input line.
  - [x] Render each transcript line with timestamp, nick column (`you`, agent nick, `system`, tool name), role marker, and deterministic wrapping aligned after nick column.
  - [x] Render user, assistant, and thought chunks in stream order; thoughts must be visually distinct but readable in monochrome terminals.
  - [x] Render tool call cards as compact IRC notices with status, title, duration if known, and expandable text content.
  - [x] Render plan entries with pending, in-progress, completed, and failed markers.
  - [x] Render token usage, stop reason, model name if available, running spinner/status, unread count, and connection state in pane footer.
  - [x] Render stderr diagnostics in collapsible debug block when connection fails.
  - [x] Render disabled-state message when runtime config is disabled, including `agents.enabled = false` reason.
  - [x] Ensure narrow terminal wrapping remains deterministic and does not corrupt editor layout.
- [x] Render approval and elicitation UI as chat interactions.
  - [x] Render permission request as highlighted transcript event with action label, absolute path or command, risk text when provided, and option list.
  - [x] Render selected approval option in composer/status area so Enter confirms explicit choice.
  - [x] Render form elicitation fields from schema using text/boolean/enum widgets supported by TUI state.
  - [x] Render URL elicitation with full URL and explicit open/decline choices.
  - [x] Reject unsupported elicitation schema shapes with user-visible error entry and safe decline path.
  - [x] Send approval and elicitation responses through `ee-agent-host` only; never craft ACP JSON in `ee-cli`.
- [x] Add TUI regression tests.
  - [x] Test `:agents` disabled path opens disabled message and does not start agent host.
  - [x] Test `:agents` enabled path creates pane and sends lazy session-new request.
  - [x] Test pane startup is invisible/inert and normal editing, picker, quickfix, and command-line modes remain unchanged while closed.
  - [x] Test prompt submission appends optimistic `you` message and sends ACP prompt through host adapter.
  - [x] Test streamed assistant chunks render in order with stable nick-column wrapping.
  - [x] Test scrollback pin-to-bottom and user-scrolled-up behavior.
  - [x] Test permission prompt selection resolves host permission request.
  - [x] Test elicitation widgets resolve host elicitation request and reject unsupported schema visibly.
  - [x] Test `:agents_stop` cancels running turn and updates statusline/footer.
  - [x] Test closing pane preserves active thread state and running agent session.
  - [x] Test thread switching preserves per-thread drafts, scroll offsets, unread counts, and activity markers.

- Criteria:
  - `cargo test --quiet -p ee-cli agent` passes with `--features agents` in test configuration.
  - Agents pane is invisible and inert unless opened by command.
  - Opened pane provides recognizable old IRC/irssi experience: transcript scrollback, nick column, channel/thread list, status bar, and bottom input prompt.
  - Normal editing, picker, quickfix, and command-line modes keep existing behavior when agents pane is closed.
  - Permission and elicitation responses flow through `ee-agent-host`, not direct ACP JSON handling in UI.
  - Agent host lifecycle stays lazy: no process, session, or task starts until enabled config plus explicit user action.

#### Phase 4: implement safe editor file and terminal bridge for ACP client methods

- Rules:
  - ACP absolute paths convert to project-relative or external buffer paths at bridge boundary only.
  - Agent reads use current buffer snapshots when file is open; disk reads are fallback for unopened files only when allowed by workspace scope.
  - Agent writes always open or reuse a buffer, apply edits through backend semantics, and save through existing save path.
  - VLF buffers reject unbounded full-file reads and writes unless operation is explicitly range-bounded and supported.
  - Terminal commands require approval and must not inherit secrets except explicitly configured safe environment values.
  - Terminal output is bounded by ACP output limits and editor-side hard caps.

- [x] Implement `fs/read_text_file` bridge.
  - [x] Validate path is absolute before resolving workspace membership.
  - [x] Resolve path against open buffers before reading from disk.
  - [x] Convert ACP 1-based line and limit fields to internal 0-based ranges.
  - [x] Return invalid params when start line is beyond buffer end.
  - [x] Return resource not found when path is outside allowed workspace and not explicitly granted.
  - [x] Enforce byte and line caps for unbounded reads.
  - [x] Add VLF-specific rejection for unbounded reads against VLF buffers.
- [x] Implement `fs/write_text_file` bridge.
  - [x] Validate absolute path and workspace or explicit grant before showing approval.
  - [x] Create permission request summarizing target path and replacement size.
  - [x] On allow, open existing buffer or create new buffer through backend-facing API.
  - [x] Compute minimal diff between current buffer snapshot and requested content.
  - [x] Apply diff as agent edit source through backend edit semantics.
  - [x] Save buffer through existing save pipeline and preserve permission-denied save retry behavior.
  - [x] On deny, return ACP permission-denied error without modifying buffer.
  - [x] Add stale-buffer handling that reapplies diff against latest buffer snapshot or fails with conflict error.
- [x] Implement terminal bridge.
  - [x] Validate command is non-empty and arguments are explicit list values.
  - [x] Create permission request showing command, args, cwd, and requested environment additions.
  - [x] On allow, spawn terminal through existing terminal task infrastructure.
  - [x] Track terminal id, output ring buffer, exit status, and release state.
  - [x] Implement `terminal/output` with byte-range or since-offset behavior matching ACP schema.
  - [x] Implement `terminal/wait_for_exit` with timeout and cancellation support.
  - [x] Implement `terminal/kill` to terminate process tree where platform supports it.
  - [x] Implement `terminal/release` to drop host tracking and kill unreleased active process.
  - [x] Redact configured secret-like env keys from permission UI and logs.
- [x] Implement shared action log for agent edits.
  - [x] Record read operations that expose buffer text to the agent.
  - [x] Record write operations with path, old snapshot id, new revision id, and tool call id.
  - [x] Expose enough data for future checkpoint/restore without changing current behavior.
- [x] Add bridge tests.
  - [x] Test read open buffer returns unsaved in-memory text rather than stale disk text.
  - [x] Test line-limited read returns expected 1-based ACP range behavior.
  - [x] Test read outside workspace fails closed.
  - [x] Test write denial leaves buffer and disk unchanged.
  - [x] Test write approval updates buffer and saves file.
  - [x] Test concurrent user edit during agent write produces deterministic merged edit or conflict error.
  - [x] Test VLF unbounded read rejection.
  - [x] Test terminal approval denial does not spawn process.
  - [x] Test terminal output is capped and preserves final visible output.
  - [x] Test terminal kill resolves wait-for-exit.

- Criteria:
  - `cargo test --quiet -p ee-cli agent_bridge --features agents` passes.
  - Agent file writes never bypass existing buffer/save code paths.
  - Denied or invalid operations leave editor state unchanged.
  - Long-running terminal and file operations can be cancelled without hanging the prompt turn.

#### Phase 5: implement MCP 2026-07-28 client manager

- Rules:
  - MCP code lives in `crates/ee-mcp` and is independent from ACP schemas except where content types are intentionally mapped by host code.
  - `crates/ee-mcp` should primarily wrap official `rmcp` client/server/transport APIs; custom protocol structs, JSON-RPC plumbing, and transport code are allowed only for unsupported 2026-07-28 features or stricter ee policy enforcement.
  - Every MCP request carries `_meta` with protocol version `2026-07-28`, ee client info, and client capabilities.
  - MCP discovery happens before primitive use and is cached according to server `ttlMs` and `cacheScope`.
  - Server tools are namespaced by server id to avoid collisions.
  - Deprecated MCP roots, sampling, logging, dynamic client registration, sampling `includeContext`, and HTTP+SSE are not sent or implemented by ee.
  - MCP URL elicitation must show full URL and never fetch or open it automatically.

- [x] Create `crates/ee-mcp` crate.
  - [x] Add dependency on official `rmcp` crate and enable only features required for stdio, Streamable HTTP, client/server types, and tests.
  - [x] Wrap `rmcp` schema types for JSON-RPC envelope, `_meta`, implementation info, capabilities, and cache metadata instead of duplicating them when serde shape and protocol version handling match MCP 2026-07-28.
  - [x] Add `MCP_PROTOCOL_VERSION` constant set to `2026-07-28`.
  - [x] Add typed errors for unsupported protocol version, unsupported capability, transport failure, invalid primitive result, and any required behavior missing from `rmcp`.
- [x] Implement full MCP discovery and initialization for `2026-07-28` only.
  - [x] Use `rmcp` client initialization/discovery APIs where they support 2026-07-28 `server/discover`, version negotiation, capability parsing, and cache metadata.
  - [x] Send `server/discover` with required `_meta` before listing primitives.
  - [x] Parse `supportedVersions`, server info, protocol capabilities, primitive capabilities, `ttlMs`, and `cacheScope`.
  - [x] Reject servers that do not include `2026-07-28` in `supportedVersions`.
  - [x] Reject or ignore deprecated server capabilities for roots, sampling, logging, dynamic client registration, and HTTP+SSE; never expose them to ACP agents as available features.
  - [x] Cache discovery result until TTL expires.
  - [x] Refresh discovery after transport reconnect.
  - [x] Add deterministic capability snapshot so UI and ACP forwarding see same discovered MCP state.
- [x] Implement full MCP primitive client surface.
  - [x] Use `rmcp` primitive request/response types for tools, resources, prompts, content blocks, annotations, and `_meta` fields when SDK serde output matches required protocol fixtures.
  - [x] Implement `tools/list` with pagination cursor support, input/output schema preservation, annotations, and title/description metadata.
  - [x] Implement `tools/call` with JSON-schema-shaped arguments, structured content, text/image/audio/resource content blocks, `isError`, and `_meta` result parsing.
  - [x] Implement `resources/list`, `resources/templates/list`, `resources/read`, and URI/content MIME handling.
  - [x] Implement `prompts/list` and `prompts/get` with required/optional argument support and returned message content parsing.
  - [x] Add namespaced registry keys as `<server_id>/<primitive_name>` for tools and prompts.
  - [x] Add cache handling for primitive list `ttlMs` and `cacheScope`.
  - [x] Validate every primitive response against expected shape before exposing it to ACP host/UI.
- [x] Implement MCP stdio transport.
  - [x] Use `rmcp` stdio transport when it supports ee lifecycle cleanup, diagnostics capture, and message-size limits; otherwise wrap it with ee policy adapters.
  - [x] Spawn configured MCP server process with explicit command, args, env, and cwd.
  - [x] Send JSON-RPC over stdin and read stdout responses/notifications.
  - [x] Capture stderr in bounded diagnostics buffer.
  - [x] Cleanly kill process on drop or manager stop.
- [x] Implement MCP Streamable HTTP transport only.
  - [x] Use `rmcp` Streamable HTTP transport when available and disable HTTP+SSE features/fallbacks at compile time or adapter boundary.
  - [x] Send JSON-RPC requests with HTTP POST.
  - [x] Apply configured headers without logging sensitive values.
  - [x] Enforce per-request timeout from config.
  - [x] Parse JSON-RPC success and error responses.
  - [x] Implement optional notification stream for `subscriptions/listen` when server advertises list-changed capability.
  - [x] Do not implement HTTP+SSE transport or fallback compatibility path.
- [x] Implement MCP notification refresh.
  - [x] Subscribe to tool-list, resource-list, and prompt-list change notifications when server advertises list-changed capabilities.
  - [x] Refresh only affected server registry after list-changed notifications.
  - [x] Fall back to TTL-based refresh when subscriptions are unavailable.
  - [x] Treat deprecated protocol logging notifications as diagnostics-only input when received; never register a logging feature path.
- [x] Implement MCP elicitation support.
  - [x] Handle `InputRequiredResult` with `elicitation/create` requests.
  - [x] Emit host event for form elicitation with schema and message.
  - [x] Emit host event for URL elicitation with full URL and message.
  - [x] Retry original MCP request with collected `inputResponses` and echoed `requestState`.
  - [x] Reject form elicitation fields that request suspicious secret-like names.
- [x] Add MCP tests.
  - [x] Test every request includes required `2026-07-28` `_meta` fields.
  - [x] Test discovery rejects unsupported version.
  - [x] Test tool list pagination and namespacing.
  - [x] Test tools/call response content parsing.
  - [x] Test primitive cache TTL expiration.
  - [x] Test stdio server lifecycle cleanup.
  - [x] Test HTTP request timeout and JSON-RPC error parsing.
  - [x] Test tool, resource, and prompt list-changed notification refresh.
  - [x] Test elicitation retry with `inputResponses`.
  - [x] Test no roots, sampling, logging, dynamic client registration, sampling `includeContext`, or HTTP+SSE requests are emitted by client manager.
  - [x] Test `rmcp`-backed serialization matches MCP 2026-07-28 fixtures and that any local fallback code is covered by documented SDK-gap tests.

- Criteria:
  - `cargo test --quiet -p ee-mcp` passes.
  - MCP client sends only protocol-version `2026-07-28` requests.
  - MCP discovery, primitive listing, tool calls, notifications, and elicitation are covered by deterministic fake-server tests.
  - Deprecated roots, sampling, logging, dynamic client registration, sampling `includeContext`, and HTTP+SSE paths are absent or tested as never emitted.
  - Local MCP protocol and transport code is reduced to `rmcp` wrappers/adapters or documented SDK gaps.

#### Phase 6: integrate ACP agents with MCP configuration and optional ee MCP proxy

- Rules:
  - First-class path is forwarding user-configured MCP server definitions to ACP agents that can connect directly.
  - ee starts its own MCP clients only for health, discovery, prompt/resource browsing, or proxy mode.
  - First-class ee MCP proxy path should be ACP-native MCP-over-ACP when the official ACP Rust SDK supports the required ACP v1 behavior and `rmcp` version compatibility; stdio proxy remains a fallback only while MCP-over-ACP is unavailable or behind an explicit migration gate.
  - ee MCP proxy should use `rmcp` server APIs and ACP/MCP bridging helpers from `agent-client-protocol-rmcp` where they reduce custom conversion code without hiding approval boundaries.
  - ACP-native MCP-over-ACP must use explicit `mcp/connect`, `mcp/message`, and `mcp/disconnect` handling, with per-session lifecycle, message-size limits, cancellation, and fail-closed unsupported-version behavior.
  - ACP and MCP tool names remain clearly attributed to avoid misleading approvals.

- [x] Forward MCP configuration to ACP session setup.
  - [x] Convert `McpServerSettings` into ACP session metadata supported by ACP v1 extensibility fields.
  - [x] Include stdio MCP command, args, env, cwd, and HTTP URL/header config without exposing secret values in logs.
  - [x] Include current workspace directory list as plain config values, not MCP roots.
  - [x] Add tests proving ACP `session/new` receives MCP config when configured.
  - [x] Add tests proving ACP `session/new` omits MCP config when no MCP servers are configured.
- [x] Add MCP health registry to agents pane.
  - [x] Start `ee-mcp` manager lazily when agents pane opens and MCP servers exist.
  - [x] Show per-server states: disabled, starting, ready, failed, and refreshing.
  - [x] Surface server identity and capability summary from `server/discover`.
  - [x] Keep MCP health failures non-fatal for ACP chat startup unless selected agent requires MCP config validation.
  - [x] Add TUI tests for healthy and failed MCP server status rendering.
- [x] Add MCP prompt/resource browsing for prompt composer.
  - [x] Fetch `prompts/list` for ready MCP servers.
  - [x] Insert selected prompt content into agent prompt draft through host event.
  - [x] Fetch `resources/list` and display resource labels in a picker-compatible list.
  - [x] Insert selected resource URI as a mention-like text block into prompt draft.
  - [x] Add tests for prompt insertion and resource mention insertion.
- [x] Implement optional ee MCP proxy mode.
  - [x] Build proxy surface with `rmcp` server APIs and use `agent-client-protocol-rmcp` adapters for ACP/MCP content conversion where applicable.
  - [x] Add `mcp.proxy.enabled` runtime config defaulting to false.
  - [x] Start local proxy only when agents mode is enabled and proxy mode is configured.
  - [x] Expose proxy as stdio MCP server config forwarded to ACP agent.
  - [x] Proxy tool calls back into `ee-agent-host` permission and bridge APIs.
  - [x] Expose initial proxy tools: `ee.read_text_file`, `ee.write_text_file`, `ee.terminal_create`, and `ee.diagnostics`.
  - [x] Namespace proxy tools under server id `ee`.
  - [x] Add tests proving proxy tool calls use same permission broker as ACP direct file/terminal methods.
- [x] Add ACP/MCP integration tests.
  - [x] Fake ACP agent receives MCP config and emits tool call referencing MCP server name.
  - [x] Fake MCP server provides prompt that can be inserted into ACP prompt draft.
  - [x] Fake MCP server tool-list change refreshes agents pane tool metadata.
  - [x] ee MCP proxy write tool denial leaves buffer unchanged.
  - [x] ee MCP proxy terminal denial does not spawn terminal.

- Criteria:
  - `cargo test --quiet -p ee-cli agent_mcp --features agents` passes.
  - ACP direct MCP config forwarding works without starting ee MCP clients when health UI is disabled.
  - MCP health and prompt/resource browsing start only when agents pane is active.
  - Proxy mode uses same approval and bridge paths as direct ACP client methods.

#### Phase 6b: migrate ee MCP proxy to ACP-native MCP-over-ACP

- Rules:
  - MCP-over-ACP is enabled only when agents mode is compiled in, `agents.enabled = true`, `mcp.proxy.enabled = true`, and the selected ACP agent advertises support for ACP v1 MCP server entries and `mcp/connect` / `mcp/message` / `mcp/disconnect`.
  - Keep first-class direct MCP config forwarding for user-configured MCP servers; MCP-over-ACP applies only to the ee-owned proxy server unless explicitly expanded later.
  - Prefer official `agent-client-protocol-rmcp` adapters over local ACP/MCP conversion code, but only if crate versions are compatible with `agent-client-protocol = 2.x` and the workspace `rmcp` version without pulling duplicate incompatible `rmcp` APIs into public ee types.
  - If `agent-client-protocol-rmcp` is not compatible with workspace `rmcp`, document the SDK gap in tests and keep the minimal stdio proxy fallback until upstream compatibility exists.
  - MCP-over-ACP traffic must not bypass existing `ee-agent-host` permission broker, bridge file semantics, terminal limits, redaction, or shutdown orchestration.
  - Unsupported MCP-over-ACP agents or unsupported SDK behavior fail closed and fall back to no ee proxy unless the user explicitly configures stdio proxy fallback.

- [x] Audit official ACP MCP-over-ACP SDK support.
  - [x] Add dependency analysis for `agent-client-protocol-rmcp` release compatible with `agent-client-protocol = 2.x` and workspace `rmcp`.
  - [x] Verify `McpServer::Acp`, `mcp/connect`, `mcp/message`, and `mcp/disconnect` are ACP v1-compatible and not ACP v2-only behavior.
  - [x] Verify required feature flags such as `unstable_mcp_over_acp` are acceptable for this project before enabling them.
  - [x] Add compile-time test or documentation test proving no duplicate incompatible `rmcp` major version is exposed through ee public APIs.
- [x] Add ACP-native MCP server advertisement.
  - [x] Extend `ee-agent-protocol` wrappers to expose ACP SDK MCP-over-ACP server entries without handrolling wire structs.
  - [x] Convert enabled ee proxy config into an ACP `McpServer::Acp` session metadata entry named `ee` instead of a stdio `ee --mcp-proxy` entry when MCP-over-ACP is supported.
  - [x] Keep workspace directories in plain session metadata, not MCP roots.
  - [x] Add tests proving `session/new` advertises `ee` as ACP-native MCP server when supported.
  - [x] Add tests proving `session/new` omits ACP-native proxy when runtime config or capability negotiation disables it.
- [x] Implement ACP MCP-over-ACP connection lifecycle in `ee-agent-host`.
  - [x] Register handlers for `mcp/connect`, `mcp/message`, and `mcp/disconnect` using official ACP SDK method metadata when available.
  - [x] Maintain per-agent-session logical MCP connections keyed by MCP connection id and server id.
  - [x] Reject unknown server ids, duplicate connection ids, messages before connect, and disconnects for unknown connections with JSON-RPC invalid params.
  - [x] Close all logical MCP connections on turn cancel, session close, agent disconnect, and app shutdown.
  - [x] Add size caps for MCP-over-ACP frames matching or stricter than existing stdio proxy caps.
- [x] Bridge ACP-native MCP-over-ACP to the existing ee MCP proxy backend.
  - [x] Reuse `EeMcpProxy` / `rmcp::ServerHandler` tool definitions for `ee.read_text_file`, `ee.write_text_file`, `ee.terminal_create`, and `ee.diagnostics`.
  - [x] Route proxy tool calls through existing bridge approval prompts and `ApprovalPolicy`; do not craft approval results directly in ACP handlers.
  - [x] Preserve absolute-path validation, current-buffer reads, buffer/edit/save writes, terminal env redaction, output caps, and diagnostics redaction.
  - [x] Ensure MCP-over-ACP `tools/list` returns namespaced `ee.*` tools and never exposes direct user-configured MCP server tools as ee-owned tools.
- [x] Update optional stdio proxy fallback.
  - [x] Make current `ee --mcp-proxy` forwarding a fallback path behind a documented config/feature gate when ACP-native MCP-over-ACP is unavailable.
  - [x] Ensure fallback and MCP-over-ACP modes are mutually exclusive for server id `ee` to avoid duplicate tools.
  - [x] Add user-visible diagnostics explaining whether ee proxy is ACP-native, stdio fallback, or disabled.
  - [x] Keep socket token, frame cap, and shutdown tests for fallback while fallback remains supported.
- [x] Add MCP-over-ACP integration tests.
  - [x] Fake ACP agent receives ACP-native `ee` MCP server entry and performs `mcp/connect`.
  - [x] Fake ACP agent sends `tools/list` through `mcp/message` and receives `ee.*` tools.
  - [x] Fake ACP agent calls `ee.write_text_file` through `mcp/message`; denial leaves buffer/disk unchanged.
  - [x] Fake ACP agent calls `ee.terminal_create` through `mcp/message`; denial does not spawn terminal.
  - [x] Oversized `mcp/message` frame fails closed without panic or hung turn.
  - [x] Disconnect, cancel, agent exit, and app shutdown close logical MCP connections deterministically.
  - [x] Unsupported MCP-over-ACP method ordering (`mcp/message` before `mcp/connect`, duplicate ids, unknown server) returns invalid params.
  - [x] Tests prove direct MCP config forwarding still works independently of ee proxy MCP-over-ACP.

- Criteria:
  - `cargo test --quiet -p ee-agent-host mcp_over_acp --features test-utils` passes.
  - `cargo test --quiet -p ee-cli agent_mcp --features agents` passes with ACP-native ee proxy coverage.
  - No local ACP/MCP conversion code exists where `agent-client-protocol-rmcp` supports the required ACP v1 behavior and workspace `rmcp` version.
  - ee proxy MCP-over-ACP never bypasses approval, buffer/save semantics, terminal limits, redaction, or shutdown cleanup.
  - Stdio proxy fallback remains tested or is removed completely if ACP-native MCP-over-ACP fully replaces it.

#### Phase 7: harden security, shutdown, persistence, and regression coverage

- Rules:
  - Agents mode fails closed on invalid config, unsupported protocol versions, denied permissions, and lost subprocess connections.
  - Secrets are never written to debug logs, stderr panes, test snapshots, or approval text.
  - Exiting ee cancels agent turns, kills tracked terminals, stops MCP servers, and resolves pending UI prompts.
  - Existing editor behavior must not regress when agents feature is absent or runtime disabled.

- [x] Add shutdown orchestration.
  - [x] Cancel running ACP turns before app shutdown completes.
  - [x] Resolve pending permissions as cancelled during shutdown.
  - [x] Resolve pending elicitations as cancelled during shutdown.
  - [x] Kill active agent-owned terminals during shutdown.
  - [x] Stop ACP agent subprocesses and MCP server subprocesses during shutdown.
  - [x] Add tests proving shutdown completes within bounded time with hung agent and hung MCP server fakes.
- [x] Add permission policy persistence.
  - [x] Add in-memory allow-once and deny-once behavior for all permissions.
  - [x] Add session-scoped allow/deny policy keyed by action kind, server id, command fingerprint, and path prefix.
  - [x] Persist explicit allow-always choices in config only when existing config-writing infrastructure supports safe updates; otherwise keep allow-always disabled at schema level.
  - [x] Add tests for allow-once, deny-once, session allow, session deny, and policy invalidation after session close.
- [x] Add secret redaction utilities.
  - [x] Redact environment keys matching `TOKEN`, `KEY`, `SECRET`, `PASSWORD`, `AUTH`, and `CREDENTIAL` in approval UI.
  - [x] Redact matching header names in MCP HTTP diagnostics.
  - [x] Redact matching values from captured subprocess stderr before storing in debug pane.
  - [x] Add unit tests for case-insensitive redaction and partial structured values.
- [x] Add resource limit enforcement.
  - [x] Cap ACP message size accepted from agent stdout.
  - [x] Cap MCP message size accepted from stdio and HTTP transports.
  - [x] Cap per-turn number of tool call updates retained in memory.
  - [x] Cap terminal output per terminal and per session.
  - [x] Cap form elicitation schema depth and field count.
  - [x] Add tests for limit-exceeded errors and non-panicking cleanup.
- [x] Add disabled-mode regression coverage.
  - [x] Test default build has no agents command side effects.
  - [x] Test runtime-disabled `:agents` does not spawn subprocesses.
  - [x] Test config with agents section does not enable agents unless `agents.enabled = true`.
  - [x] Test regular open/edit/save flow does not call agent or MCP code when pane is closed.
- [x] Add workspace validation coverage.
  - [x] Run targeted protocol, host, MCP, and CLI tests in quiet mode.
  - [x] Run workspace summary script once agents phases are complete.
  - [x] Run clippy with agents feature enabled and fix warnings introduced by agents crates.
  - [x] Run rustfmt check after each implementation phase.

- Criteria:
  - `cargo fmt --all -- --check` passes.
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes.
  - `cargo test --quiet -p ee-agent-protocol` passes.
  - `cargo test --quiet -p ee-agent-host` passes.
  - `cargo test --quiet -p ee-mcp` passes.
  - `cargo test --quiet -p ee-cli --features agents agent` passes.
  - `./scripts/test-workspace-summary.sh` passes after full feature integration.
