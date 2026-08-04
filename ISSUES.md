# ISSUES

## Optional Agents Tooling Plan: ACP v1 + MCP 2026-07-28

Agents tooling stays optional, compile-time and runtime gated behind `agents`. ACP v1 remains the agent session protocol, and MCP `2026-07-28` remains the tool transport for the ee proxy. New tools should extend the existing `ee.*` MCP proxy surface in `crates/ee-mcp`, route execution through `ee-agent-host` handlers when editor state or approvals are needed, and avoid adding ee-owned ACP wire structs unless the official ACP SDK already defines the method.

Tooling goals:

- [ ] Give LLMs enough editor-native context to avoid terminal probing and path guessing.
- [ ] Keep read-only discovery cheap, bounded, and approval-free.
- [ ] Route every write, terminal execution, and code action through the existing approval path.
- [ ] Prefer buffer-aware operations over disk-only operations when unsaved editor state exists.
- [ ] Preserve fail-closed validation: absolute paths, workspace-root containment, bounded output, redacted secrets, and typed tool errors.

Tool simplicity contract:

- [ ] Keep each tool purpose single and obvious from name.
- [ ] Keep required arguments to the minimum, preferably one or two fields.
- [ ] Avoid mode flags that change tool behavior in surprising ways.
- [ ] Split complex parameters into separate tools instead of adding nested option objects.
- [ ] Prefer simple strings, numbers, booleans, and flat arrays over deeply nested JSON.
- [ ] Use safe defaults for limits and visibility; add a separate explicit tool when caller needs broader access.
- [ ] Return stable structured data, but keep input schemas easy for LLM agents to construct correctly.
- [ ] Reject ambiguous requests with clear errors rather than accepting many overloaded shapes.

### Phase 1: Workspace and file discovery tools

Add low-risk read-only discovery tools to the ee MCP proxy. These tools make path selection explicit before reads or edits.

#### Tools

- [x] Add `ee.workspace_roots`.
  - [x] Return configured worktree roots, active root, active file, and optional additional directories advertised to the session.
  - [x] Never return environment values or secret config.
  - [x] Validate roots as canonical absolute paths before exposing them.
- [x] Add `ee.list_directory`.
  - [x] Accept `path` only.
  - [x] Return one directory level with entries containing `path`, `kind`, and `size`.
  - [x] Hide hidden/ignored entries by default.
  - [x] Reject paths outside allowed roots.
  - [x] Cap result count with host default.
- [x] Add `ee.list_directory_all` only if hidden/ignored listing is needed.
  - [x] Accept `path` only.
  - [x] Return one directory level including hidden/ignored entries with flags.
- [x] Add `ee.search_files`.
  - [x] Accept `pattern` only.
  - [x] Search allowed roots using glob/path matching only, not content search.
  - [x] Respect project ignore rules.
  - [x] Cap results with host default.
- [x] Add `ee.search_files_all` only if hidden/ignored file search is needed.
  - [x] Accept `pattern` only.
  - [x] Include hidden/ignored files and mark them in results.
- [x] Add `ee.search_text`.
  - [x] Accept `query` only.
  - [x] Perform literal, case-sensitive search across allowed roots.
  - [x] Return bounded matches with file, 1-based line, and short context.
- [x] Add `ee.search_text_regex` only if regex search is needed.
  - [x] Accept `pattern` only.
  - [x] Enforce regex safety, time limits, and result caps.
- [x] Add `ee.search_text_in_files` only if scoped content search is needed.
  - [x] Accept `query` and `file_glob` only.
  - [x] Perform literal search within files matching the glob.

#### Implementation notes

- [x] Extend `EeProxyBackend` with read-only workspace/search methods.
- [x] Keep schemas in `EeMcpProxy::tools()` with simple flat arguments and limits in descriptions.
- [x] Implement argument validation in `crates/ee-mcp/src/proxy.rs`.
- [x] Implement root policy in the host/backend bridge.
- [x] Add unit tests for schemas and argument validation.
- [x] Add unit tests for absolute-path/root rejection.
- [x] Add unit tests for result caps and hidden file behavior.
- [x] Add unit tests for tool-level error mapping.

#### Exit criteria

- [x] LLM can discover roots, list files, find files, and search text without using terminal commands.
- [x] All returned paths are absolute, canonical where practical, and inside an allowed root.

### Phase 2: Safer edit tools

Add patch-oriented editing so agents do not need full-file overwrites for common changes.

#### Tools

- [x] Add `ee.replace_text`.
  - [x] Accept `path`, `old_text`, and `new_text` only.
  - [x] Require exactly one match for `old_text`.
  - [x] Apply through existing buffer/edit/save semantics, not raw disk writes.
  - [x] Require approval before mutating any buffer or file.
- [x] Add `ee.apply_patch` only if multi-edit patches are needed.
  - [x] Accept `path` and `edits` only.
  - [x] Each edit must use the same simple `old_text`/`new_text` shape.
  - [x] Reject range-based, hunk-based, or mixed edit shapes; add a separate tool later if needed.
- [x] Add `ee.create_text_file`.
  - [x] Accept `path` and `content` only.
  - [x] Fail if file exists.
- [x] Add `ee.overwrite_text_file` only if overwrite is needed.
  - [x] Accept `path` and `content` only.
  - [x] Require approval and clearly report existing file replacement.
- [x] Add `ee.read_buffer`.
  - [x] Accept `path` only.
  - [x] Read current editor buffer contents, including unsaved changes.
  - [x] Fall back to file read only when no buffer is open and policy allows.
- [x] Add `ee.read_buffer_lines` only if line-window reads are needed.
  - [x] Accept `path`, `line`, and `limit` only.
- [x] Add `ee.open_buffers`.
  - [x] Return open buffer paths, dirty flags, revision ids, active selection/cursor summary, and language id when known.
  - [x] Avoid exposing full content.

#### Implementation notes

- [x] Prefer new `ClientRequest` variants only if editor bridge needs typed host requests.
- [x] Keep operations inside the MCP backend bridge when ACP method additions are not needed.
- [x] Reuse existing write approval and buffer persistence path used by `fs/write_text_file`.
- [x] Reject stale buffer revisions internally when detected; do not require agents to pass revision ids unless a future dedicated tool needs them.
- [x] Reject ambiguous `old_text` matches.
- [x] Return structured success with changed file, byte count, edit count, new revision, and saved/dirty state.

#### Exit criteria

- [x] LLM can inspect dirty buffers and apply targeted edits without overwriting unrelated user changes.
- [x] Stale or ambiguous edits fail closed with actionable messages.

### Phase 3: Diagnostics and language intelligence tools

Expose editor and LSP context that agents currently must infer from terminal output.

#### Tools

- [x] Add `ee.get_diagnostics`.
  - [x] Accept no arguments.
  - [x] Return bounded LSP/editor diagnostics for the current workspace with path, range, severity, source, code, and message.
  - [x] Keep separate from current `ee.diagnostics`, which remains recent proxy/host diagnostic text.
- [x] Add `ee.get_file_diagnostics`.
  - [x] Accept `path` only.
  - [x] Return bounded LSP/editor diagnostics for one file.
- [x] Add `ee.document_symbols`.
  - [x] Accept `path`.
  - [x] Return LSP document symbols with name, kind, range, selection range, and container path.
- [x] Add `ee.references`.
  - [x] Accept `path`, `line`, and `character` only.
  - [x] Return bounded LSP references as absolute paths and 1-based ranges.
- [x] Add `ee.list_code_actions`.
  - [x] Accept `path`, `line`, and `character` only.
  - [x] Return available actions with simple `action_id`, title, and kind.
  - [x] Keep listing read-only.
- [x] Add `ee.apply_code_action`.
  - [x] Accept `path` and `action_id` only.
  - [x] Require approval and use buffer edit semantics.
- [x] Add `ee.format_file`.
  - [x] Accept `path`.
  - [x] Run configured formatter or LSP formatting.
  - [x] Require approval if it changes the buffer.
- [x] Add `ee.preview_rename_symbol`.
  - [x] Accept `path`, `line`, `character`, and `new_name` only.
  - [x] Return planned workspace edits without applying them.
- [x] Add `ee.rename_symbol`.
  - [x] Accept `path`, `line`, `character`, and `new_name` only.
  - [x] Require approval before applying edits.
  - [x] Validate all touched files against allowed roots.

#### Implementation notes

- [x] Route through `xi-lsp-lib` and existing editor buffer APIs.
- [x] Do not spawn language-server CLI commands.
- [x] Use 1-based line values at the tool boundary to match ACP validation expectations.
- [x] Convert internally where existing APIs use 0-based coordinates.
- [x] Keep output bounded and stable.
- [x] Include truncation markers and totals when result caps apply.
- [x] Add regression tests with fake LSP responses for diagnostics.
- [x] Add regression tests with fake LSP responses for symbols and references.
- [x] Add regression tests with fake LSP responses for code actions.
- [x] Add regression tests with fake LSP responses for formatting and rename previews.

#### Exit criteria

- [x] LLM can fix diagnostics, navigate symbols, and perform LSP-backed refactors without terminal-only workflows.
- [x] Applying language-server edits shares the same approval and conflict handling as manual patch edits.

### Phase 4: Terminal lifecycle tools

Expose the full ACP terminal lifecycle through MCP so agents can run and observe bounded commands without raw protocol access.

#### Tools

- [ ] Add `ee.terminal_output`.
  - [ ] Accept `terminal_id` only.
  - [ ] Return recent bounded stdout/stderr chunks with sequence ids and truncation flags.
- [ ] Add `ee.terminal_output_since` only if incremental polling is needed.
  - [ ] Accept `terminal_id` and `since_seq` only.
- [ ] Add `ee.terminal_wait`.
  - [ ] Accept `terminal_id` only.
  - [ ] Use host default timeout.
  - [ ] Return exit status when complete or timeout state when still running.
- [ ] Add `ee.terminal_wait_long` only if longer waits are needed.
  - [ ] Accept `terminal_id` and `timeout_ms` only.
- [ ] Add `ee.terminal_kill`.
  - [ ] Accept `terminal_id`.
  - [ ] Terminate only terminals owned by the current agent/session unless policy explicitly allows more.
- [ ] Add `ee.terminal_release`.
  - [ ] Accept `terminal_id`.
  - [ ] Release host resources and close retained output.
- [ ] Add `ee.run_task`.
  - [ ] Accept `task_id` only.
  - [ ] Run configured safe tasks such as format, lint, test, build, or project-specific entries from `tasks.yaml`.
  - [ ] Avoid shell strings, ad-hoc args, and secret-like environment overrides.
- [ ] Add dedicated task tools later instead of adding broad `args` to `ee.run_task` when common variants emerge.

#### Implementation notes

- [ ] Reuse existing ACP-side terminal request types already present in `ee-agent-host`.
- [ ] Preserve command approval for `terminal_create` and `run_task`.
- [ ] Add ownership tracking so agents cannot read or kill user terminals by guessing ids.
- [ ] Keep terminal output redaction and byte caps consistent with current diagnostics redaction.

#### Exit criteria

- [ ] LLM can start, poll, wait for, kill, and release command executions through structured tools.
- [ ] Long-running commands cannot hang the host or leak unbounded output.

### Phase 5: Git and review context tools

Add read-only source-control tools that support review and final self-checks without shelling out.

#### Tools

- [ ] Add `ee.git_status`.
  - [ ] Return branch, detached state, staged/unstaged/untracked files, and conflict state.
  - [ ] Keep read-only and bounded.
- [ ] Add `ee.git_diff`.
  - [ ] Accept no arguments.
  - [ ] Return bounded unstaged unified diff plus truncation metadata.
- [ ] Add `ee.git_diff_file`.
  - [ ] Accept `path` only.
  - [ ] Return bounded unstaged unified diff for one file.
- [ ] Add `ee.git_diff_staged` only if staged diff is needed.
  - [ ] Accept no arguments.
  - [ ] Return bounded staged unified diff.
- [ ] Add `ee.changed_files`.
  - [ ] Return editor/SCM changed files with dirty-buffer state and saved state.
- [ ] Add `ee.review_context`.
  - [ ] Return changed files, relevant diagnostics, nearby symbols, and configured test/task suggestions.
  - [ ] Never run tests or commands by itself.

#### Implementation notes

- [ ] Prefer library or existing editor SCM integration over shell commands where available.
- [ ] Treat repository paths as canonical identities.
- [ ] Do not invent local path-normalization helpers for cache keys or persisted ids.
- [ ] Redact credentials from remote URLs and command diagnostics.

#### Exit criteria

- [ ] LLM can summarize changes, inspect diffs, and identify obvious validation tasks from editor-provided context.

### Phase 6: Project memory and instructions tools

Expose project guidance and bounded session context so agents follow repo rules without repeatedly scanning files.

#### Tools

- [ ] Add `ee.project_instructions`.
  - [ ] Return applicable `AGENTS.md`, `RULE.md`, workspace config rules, and tool-use constraints for the current root.
  - [ ] Include source paths and precedence order.
- [ ] Add `ee.save_note`.
  - [ ] Accept `key` and `content` only.
  - [ ] Store non-secret, session-scoped notes for long-running tasks.
- [ ] Add `ee.read_notes`.
  - [ ] Accept no arguments.
  - [ ] Return bounded notes for the current agent/session only.
- [ ] Add `ee.read_note`.
  - [ ] Accept `key` only.
  - [ ] Return one bounded note for the current agent/session.
- [ ] Add `ee.file_dependency_map`.
  - [ ] Accept `path` only.
  - [ ] Return known file dependency edges when an index exists.
  - [ ] Fail gracefully when no graph/index is available.
- [ ] Add `ee.symbol_dependency_map` only if symbol-scoped graph lookup is needed.
  - [ ] Accept `path`, `line`, and `character` only.

#### Implementation notes

- [ ] Never store secrets, environment values, tokens, or raw terminal output in notes.
- [ ] Keep notes session-scoped by default.
- [ ] Require explicit user opt-in before workspace persistence.
- [ ] Mark freshness in graph-backed responses when data is stale.

#### Exit criteria

- [ ] LLM can retrieve current project rules and task memory through structured, bounded tools.
- [ ] Knowledge tools degrade safely when no index or saved context exists.

### Phase 7: Tool governance, schemas, and compatibility

Harden the expanded tool surface before enabling it by default.

#### Work items

- [ ] Add versioned `ee.tools_manifest`.
  - [ ] Accept no arguments.
  - [ ] Return tool names.
  - [ ] Return schema versions.
  - [ ] Return side-effect class: `read`, `write`, or `execute`.
  - [ ] Return approval requirement.
  - [ ] Return output caps.
  - [ ] Return short examples using minimal arguments for each tool.
- [ ] Keep existing tool names stable.
- [ ] Add new names rather than changing schemas incompatibly.
- [ ] Document every tool argument, limit, error shape, approval behavior, and redaction rule in README and crate docs.
- [ ] Document the rule that complicated arguments mean the tool should be split into smaller tools.
- [ ] Add capability flags so hosts can advertise partial implementation without pretending unsupported tools exist.
- [ ] Add integration tests for MCP stdio proxy path for each tool class.
- [ ] Add integration tests for ACP-native MCP-over-ACP path for each tool class.
- [ ] Add security tests for path traversal.
- [ ] Add security tests for symlink escape.
- [ ] Add security tests for oversized inputs.
- [ ] Add security tests for secret-like env keys.
- [ ] Add security tests for stale revisions.
- [ ] Add security tests for terminal ownership.
- [ ] Add security tests for output truncation.

#### Exit criteria

- [ ] Expanded tools work through both stdio MCP proxy and ACP-native MCP-over-ACP.
- [ ] Unsupported or disabled tools fail closed with clear tool-level errors.
- [ ] Tool list is discoverable, versioned, and safe for LLM clients to cache within a session.

## ACP v1 Optional Method Gap Closure Plan

Close remaining ACP v1 host/client gaps found during protocol audit. Host already uses official `agent-client-protocol` v1 types and method constants; this plan finishes missing notification handling, confirms production bridges, and adds regression coverage for every optional method.

### Phase 1: Production client bridge verification

Confirm whether agents mode wires a real editor-backed `ClientRequestHandler` into `AgentManager`.

#### Work items

- [x] Find every `AgentManager::new` and `AgentManager::with_options` call site.
- [x] Confirm production agents mode passes a real `ClientRequestHandler`, not `DenyAllHandler`.
- [x] Confirm handler capabilities match actual editor-backed support for file reads, file writes, terminals, and elicitation.
- [x] Keep `DenyAllHandler` as fail-closed fallback for disabled or test-only configurations.
- [x] Production handler already exists; no extra handler needed before advertising optional ACP client capabilities.

#### Exit criteria

- [x] Agents mode advertises only capabilities backed by a real handler.
- [x] Unsupported client methods fail closed before handler invocation.
- [x] Production agents can execute advertised fs, terminal, and elicitation requests through editor-safe paths.

### Phase 2: `elicitation/complete` notification handling

Add missing handling for ACP `elicitation/complete`.

#### Work items

- [x] Register `CompleteElicitationNotification` in `build_client_builder`.
- [x] Route `elicitation/complete` to session/UI state or a handler callback.
- [x] Add an `AgentEvent` variant so the UI can clear pending URL elicitation state.
- [x] Treat unknown or stale elicitation completions as diagnostics, not panics.
- [x] Keep notification handling one-way; do not send JSON-RPC responses.

#### Exit criteria

- [x] Incoming `elicitation/complete` notifications are observed and handled.
- [x] URL/out-of-band elicitation flows can be marked complete.
- [x] Invalid completion state fails safe without disconnecting healthy sessions.

### Phase 3: Optional ACP client request coverage

Add host-flow tests for every optional client request not currently covered.

#### Work items

- [x] Add coverage for `fs/write_text_file`.
  - [x] Assert request routes when `fs_write` is advertised.
  - [x] Assert request is rejected with `-32601` when `fs_write` is false.
- [x] Add coverage for `terminal/output`.
  - [x] Assert request routes when `terminal` is advertised.
  - [x] Assert request is rejected with `-32601` when `terminal` is false.
- [x] Add coverage for `terminal/wait_for_exit`.
  - [x] Assert request routes when `terminal` is advertised.
  - [x] Assert request is rejected with `-32601` when `terminal` is false.
- [x] Add coverage for `terminal/kill`.
  - [x] Assert request routes when `terminal` is advertised.
  - [x] Assert request is rejected with `-32601` when `terminal` is false.
- [x] Add coverage for `terminal/release`.
  - [x] Assert request routes when `terminal` is advertised.
  - [x] Assert request is rejected with `-32601` when `terminal` is false.
- [x] Add coverage for `elicitation/create`.
  - [x] Assert form mode routes when `elicitation_form` is advertised.
  - [x] Assert url mode routes when `elicitation_url` is advertised.
  - [x] Assert unadvertised elicitation is rejected with `-32601`.

#### Exit criteria

- [x] Every ACP optional client request has capability-gate coverage.
- [x] Every routed request serializes expected ACP result shape.
- [x] Tests prove handlers are not invoked for unadvertised capabilities.

### Phase 4: Fake ACP wire helper expansion

Expand fake agent helpers so host-flow tests stay concise and protocol-shaped.

#### Work items

- [x] Add fake wire helper for `fs/write_text_file`.
- [x] Add fake wire helper for `terminal/output`.
- [x] Add fake wire helper for `terminal/wait_for_exit`.
- [x] Add fake wire helper for `terminal/kill`.
- [x] Add fake wire helper for `terminal/release`.
- [x] Add fake wire helper for `elicitation/create` form mode.
- [x] Add fake wire helper for `elicitation/create` url mode.
- [x] Add fake wire helper for `elicitation/complete`.
- [x] Extend wire-helper tests to check exact ACP v1 method names.

#### Exit criteria

- [x] Tests no longer duplicate raw JSON for common ACP optional methods.
- [x] Fake helpers produce ACP v1-compatible method names and params.

### Phase 5: `logout` capability gating

Align `logout` with ACP advertised capability semantics.

#### Work items

- [x] Add `AgentConnection::supports_logout` if the SDK exposes `agentCapabilities.auth.logout`.
- [x] Make `AgentConnection::logout` return `AgentError::CapabilityUnsupported { method: "logout" }` when unsupported.
- [x] Add one test where logout is advertised and sent.
- [x] Add one test where logout is not advertised and fails locally.
- [x] Preserve existing authentication flow when auth exists but logout does not.

#### Exit criteria

- [x] `logout` is not sent to agents that did not advertise logout support.
- [x] Authenticated agents with logout support still round-trip successfully.

### Phase 6: Capability advertisement snapshots

Ensure initialize-time client capabilities always match implemented handler capability flags.

#### Work items

- [x] Add initialize request snapshot tests for fs read only.
- [x] Add initialize request snapshot tests for fs write only.
- [x] Add initialize request snapshot tests for fs read and write.
- [x] Add initialize request snapshot tests for terminal support.
- [x] Add initialize request snapshot tests for elicitation form support.
- [x] Add initialize request snapshot tests for elicitation url support.
- [x] Add initialize request snapshot tests for no capabilities.

#### Exit criteria

- [x] No client capability is advertised unless the handler can execute it.
- [x] Every implemented handler capability appears in `initialize`.
- [x] Capability mismatches are caught by tests before runtime.

### Phase 7: Validation

Run focused validation for ACP host and protocol crates.

#### Commands

- [x] Run `cargo fmt --check`.
- [x] Run `cargo clippy --quiet -p ee-agent-host -p ee-agent-protocol --all-targets --all-features -- -D warnings`.
- [x] Run `cargo test --quiet -p ee-agent-host host_flows`.
- [x] Run `cargo test --quiet -p ee-agent-protocol registry`.

#### Exit criteria

- [x] Formatting passes.
- [x] Clippy passes for changed crates.
- [x] Host-flow tests cover all ACP optional client methods.
- [x] Protocol registry tests confirm exact ACP v1 method names.

## ACP v1 Docs Audit Additions

The current ACP v1 docs also define newer session lifecycle and configuration surfaces beyond the initial optional-method audit. Use this addendum to keep `ee-agent-protocol`, `ee-agent-host`, and UI behavior aligned with current docs.

### Phase 1: Update typed method registry to full current ACP v1 surface

Add SDK-backed entries for current client-to-agent methods that are missing from `ee-agent-protocol` registry coverage.

#### Work items

- [x] Add registry constants, enum variants, params validation, result type names, and exact-name tests for `session/list`.
- [x] Add registry constants, enum variants, params validation, result type names, and exact-name tests for `session/delete`.
- [x] Add registry constants, enum variants, params validation, result type names, and exact-name tests for `session/resume`.
- [x] Add registry constants, enum variants, params validation, result type names, and exact-name tests for `session/close`.
- [x] Add registry constants, enum variants, params validation, result type names, and exact-name tests for `session/set_config_option`.
- [x] Keep using official SDK v1 types: `ListSessionsRequest`, `DeleteSessionRequest`, `ResumeSessionRequest`, `CloseSessionRequest`, and `SetSessionConfigOptionRequest`.

#### Exit criteria

- [x] Registry mirrors current stable ACP v1 client-to-agent method names used by the SDK.
- [x] Unknown or malformed params still fail closed with JSON-RPC `invalid params`.

### Phase 2: Implement capability-gated session lifecycle methods

Expose current optional session lifecycle operations on `AgentConnection` and `AgentManager` only when the agent advertises matching capabilities.

#### Work items

- [x] Add `supports_session_list` using `agentCapabilities.sessionCapabilities.list`.
- [x] Add `supports_session_delete` using `agentCapabilities.sessionCapabilities.delete`.
- [x] Add `supports_session_resume` using `agentCapabilities.sessionCapabilities.resume`.
- [x] Add `supports_session_close` using `agentCapabilities.sessionCapabilities.close`.
- [x] Add `supports_additional_directories` using `agentCapabilities.sessionCapabilities.additionalDirectories`.
- [x] Add `AgentConnection::list_sessions(cwd, cursor)` with absolute-path validation for optional `cwd`.
- [x] Add `AgentConnection::delete_session(session_id)` with delete capability gate.
- [x] Add `AgentConnection::resume_session(session_id, cwd, additional_directories, mcp_servers)` with resume and additional-directory gates.
- [x] Add `AgentConnection::close_session(session_id)` with close capability gate and local thread cleanup.
- [x] Add matching `AgentManager` wrappers where UI needs direct access.

#### Exit criteria

- [x] Client never calls lifecycle methods unless advertised by the agent.
- [x] `session/list` pagination cursors are treated as opaque.
- [x] `session/delete` succeeds idempotently according to agent response.
- [x] `session/resume` does not expect replayed `session/update` history.
- [x] `session/close` cancels local pending work and releases thread state.

### Phase 3: Correct `session/load` and additional-directory behavior

Align existing `session/load` implementation with current docs.

#### Work items

- [x] Change `AgentConnection::load_session` to require caller-provided absolute `cwd` instead of `PathBuf::new()`.
- [x] Pass `mcpServers` and full intended `additionalDirectories` on `session/load`.
- [x] Only send `additionalDirectories` when agent advertises `sessionCapabilities.additionalDirectories`.
- [x] Validate every root is absolute before sending lifecycle requests.
- [x] Preserve replay handling: apply streamed `session/update` notifications before load response completes.

#### Exit criteria

- [x] `session/load` requests match ACP docs: `sessionId`, `cwd`, `mcpServers`, and optional `additionalDirectories`.
- [x] Relative or missing `cwd` cannot be sent.

### Phase 4: Session config options support

Implement preferred ACP configuration surface. Session config options supersede legacy session modes.

#### Work items

- [x] Advertise `clientCapabilities.session.configOptions.boolean` only if UI can render and set boolean config options.
- [x] Store initial `configOptions` from `session/new`, `session/load`, and `session/resume` responses in `AgentThread` state.
- [x] Prefer config option with `category: "mode"` over legacy `modes` when both are present.
- [x] Add `AgentThread::set_config_option(config_id, value)` using `session/set_config_option`.
- [x] Validate select values against currently advertised options before sending when possible.
- [x] Validate boolean values only when boolean config option support was advertised.
- [x] Apply `config_option_update` as complete replacement state.
- [x] Keep legacy `session/set_mode` support while ACP v1 still includes it.

#### Exit criteria

- [x] UI can display and mutate session config options in agent-provided order.
- [x] Mode-like config options work even when legacy `modes` is absent.
- [x] Boolean options are not accepted unless client advertised support.

### Phase 5: Modes UX compatibility

Support VS Code-style modes when an agent advertises them, without hardcoding protocol semantics into the client.

#### Work items

- [x] Allow mode ids and names such as `ask`/`Ask`, `edit`/`Edit`, `agent`/`Agent`, and `plan`/`Plan` when present in agent-provided `availableModes`.
- [x] Do not invent modes client-side; render only agent-advertised legacy `modes` or preferred `configOptions` category `mode` values.
- [x] Keep `session/set_mode` locally gated by advertised `availableModes`.
- [x] Keep mode changes during a turn valid and update UI from `current_mode_update`.
- [x] If both config options and modes exist, keep rendered current value in sync and prefer config options for user actions.

#### Exit criteria

- [x] Agents can present Ask/Edit/Agent/Plan style modes through ACP.
- [x] Client remains protocol-compatible with arbitrary agent-defined mode names and ids.

### Phase 6: Agent plan and slash-command UI completion

Host reducer already stores plan and slash-command update state; finish UI and tests so these updates are visible and usable.

#### Work items

- [x] Keep plan updates as complete replacements and render priority/status/content in the agents pane.
- [x] Add host-flow coverage for `session/update` `plan` notifications.
- [x] Render `available_commands_update` in the agents pane or command picker.
- [x] Add prompt insertion/execution UX for slash commands as regular `session/prompt` text, e.g. `/plan ...`.
- [x] Add host-flow coverage for `available_commands_update` replacement behavior.
- [x] Render or expose `session_info_update` so title and metadata can update session lists.

#### Exit criteria

- [x] Plans are visible to users and replace wholesale per ACP docs.
- [x] Slash commands advertised by the agent become discoverable to users.
- [x] Running a slash command still sends a normal `session/prompt`.

### Phase 7: Initialization and auth conformance polish

Finish capability negotiation behavior called out by current initialization and authentication docs.

#### Work items

- [x] Advertise implementation `title` when available, not just `name` and `version`.
- [x] Verify prompt capabilities before sending non-text prompt content.
- [x] Gate `logout` on `agentCapabilities.auth.logout`, not merely presence of auth methods.
- [x] Keep unknown capabilities diagnostic-only and never enable behavior from unknown fields.
- [x] Add tests for omitted capabilities meaning unsupported.

#### Exit criteria

- [x] Initialization strictly treats omitted capabilities as unsupported.
- [x] Auth logout follows advertised capability semantics.
- [x] Rich prompt content is never sent unless the agent advertised support.

## ACP v1 Content, Tooling, and Safety Docs Additions

The content, tool-calls, elicitation, file-system, cancellation, terminal, and extensibility docs add behavioral requirements beyond method presence. Track them here so the implementation remains protocol-correct and safe.

### Phase 1: Content capability enforcement

Ensure prompt content sent by the client obeys agent-advertised prompt capabilities.

#### Work items

- [ ] Always allow `text` and `resource_link` prompt content.
- [ ] Send `image` prompt content only when `agentCapabilities.promptCapabilities.image` is true.
- [ ] Send `audio` prompt content only when `agentCapabilities.promptCapabilities.audio` is true.
- [ ] Send embedded `resource` content only when `agentCapabilities.promptCapabilities.embeddedContext` is true.
- [ ] Add tests proving unsupported rich content fails locally before `session/prompt`.
- [ ] Preserve `_meta` fields without using them to enable unsupported behavior.

#### Exit criteria

- [ ] Client never sends prompt content not supported by the agent.
- [ ] Content validation errors are local, clear, and fail closed.

### Phase 2: Tool-call rendering and state completeness

Render and retain all tool-call fields defined by ACP v1.

#### Work items

- [ ] Preserve and render tool `kind` values: `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`, and `other`.
- [ ] Render `tool_call_update` status transitions: `pending`, `in_progress`, `completed`, and `failed`.
- [ ] Render tool-call `content` blocks, including regular content, diffs, and terminal references.
- [ ] Preserve `locations` so UI can follow files and 1-based lines mentioned by the agent.
- [ ] Preserve `rawInput` and `rawOutput` for diagnostics without leaking secrets into user-visible logs by default.
- [ ] Keep current upsert behavior for constructible `tool_call_update` messages and test unknown-id rejection when required fields are missing.

#### Exit criteria

- [ ] Tool calls display useful progress, output, diffs, terminals, and affected file locations.
- [ ] Tool-call state remains bounded and secret-conscious.

### Phase 3: Elicitation security and completion semantics

Implement elicitation according to ACP privacy and URL-safety requirements.

#### Work items

- [ ] Advertise elicitation modes explicitly; `{}` must not imply form support.
- [ ] Reject `elicitation/create` modes not advertised with JSON-RPC `-32602`.
- [ ] In form mode, display agent identity, request message, schema fields, decline, and cancel controls.
- [ ] In form mode, validate submitted values against supported flat JSON schema before responding.
- [ ] In form mode, prevent secret-like fields and credentials from being requested when detectable.
- [ ] In URL mode, show full URL and highlighted host before navigation.
- [ ] In URL mode, require explicit user consent before opening the URL.
- [ ] In URL mode, do not prefetch URLs or expose resulting credentials/tokens to ACP, model context, or logs.
- [ ] Track URL `elicitationId` per connection and ignore unknown or already-completed `elicitation/complete` notifications.

#### Exit criteria

- [ ] Elicitation respects privacy, consent, and safe URL handling requirements.
- [ ] URL completions are connection-scoped and idempotent.

### Phase 4: Filesystem request semantics

Make ACP filesystem methods match editor-aware requirements.

#### Work items

- [ ] Serve `fs/read_text_file` from unsaved editor buffer state when present.
- [ ] Enforce absolute paths and 1-based line values for reads.
- [ ] Enforce workspace/effective-root containment for reads and writes.
- [ ] Make `fs/write_text_file` create files when missing.
- [ ] Route writes through existing buffer/edit/save semantics and approval flow.
- [ ] Return `null`/empty ACP result shape expected by `WriteTextFileResponse`.
- [ ] Add regression tests for create-if-missing, dirty-buffer reads, root escape, and line window reads.

#### Exit criteria

- [ ] Filesystem methods reflect editor state and cannot escape allowed roots.
- [ ] Writes are safe, approved, and protocol-shaped.

### Phase 5: Terminal lifecycle semantics

Implement all ACP terminal behavior, not only method dispatch.

#### Work items

- [ ] Support `terminal/create` `command`, `args`, `env`, absolute `cwd`, and `outputByteLimit`.
- [ ] Require approval before starting terminal commands.
- [ ] Truncate retained output from the beginning when exceeding `outputByteLimit`.
- [ ] Preserve valid UTF-8/character boundaries when truncating output.
- [ ] Make `terminal/output` return output, `truncated`, and optional exit status.
- [ ] Make `terminal/wait_for_exit` resolve only when command exits or request is cancelled.
- [ ] Make `terminal/kill` terminate without releasing terminal state.
- [ ] Make `terminal/release` kill if still running and invalidate the terminal id afterward.
- [ ] Keep terminal output displayable after release when referenced by a tool call.
- [ ] Track terminal ownership by agent connection and session.

#### Exit criteria

- [ ] Terminals can be created, observed, waited, killed, and released safely.
- [ ] Terminal ids cannot be guessed across sessions or agents.
- [ ] Output remains bounded and valid text.

### Phase 6: Cancellation conformance

Align explicit prompt cancellation and JSON-RPC request cancellation behavior.

#### Work items

- [ ] Continue sending `session/cancel` for active prompt turns.
- [ ] Continue responding to pending `session/request_permission` requests with `cancelled` when the turn is cancelled.
- [ ] Verify outgoing prompt cancellation also sends `$/cancel_request` for the in-flight `session/prompt` request where SDK support exists.
- [ ] Handle incoming `$/cancel_request` for long-running client-side requests when SDK exposes it.
- [ ] Use JSON-RPC `-32800` for internally cancelled request responses where applicable.
- [ ] Accept late `session/update` notifications until the cancelled prompt response arrives.

#### Exit criteria

- [ ] Prompt turn cancellation is graceful and protocol-compliant.
- [ ] Long-running client-side fs, terminal, elicitation, and permission work can stop without hanging the agent.

### Phase 7: Extensibility rules

Keep custom behavior compatible with ACP extension rules.

#### Work items

- [ ] Do not add custom root-level fields to ACP-defined types; use `_meta` only.
- [ ] Preserve `_meta` fields for correlation when safe, including trace context keys.
- [ ] Keep custom methods under underscore-prefixed names if ACP-level extensions are added.
- [ ] Advertise custom capabilities under `_meta` instead of new standard-looking capability names.
- [ ] Ignore unknown custom notifications and return method-not-found for unknown custom requests.

#### Exit criteria

- [ ] Extensions cannot collide with future ACP standard fields or methods.
- [ ] Unknown extension data remains diagnostic/correlation-only unless explicitly supported.
