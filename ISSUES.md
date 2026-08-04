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

- [x] Always allow `text` and `resource_link` prompt content.
- [x] Send `image` prompt content only when `agentCapabilities.promptCapabilities.image` is true.
- [x] Send `audio` prompt content only when `agentCapabilities.promptCapabilities.audio` is true.
- [x] Send embedded `resource` content only when `agentCapabilities.promptCapabilities.embeddedContext` is true.
- [x] Add tests proving unsupported rich content fails locally before `session/prompt`.
- [x] Preserve `_meta` fields without using them to enable unsupported behavior.

#### Exit criteria

- [x] Client never sends prompt content not supported by the agent.
- [x] Content validation errors are local, clear, and fail closed.

### Phase 2: Tool-call rendering and state completeness

Render and retain all tool-call fields defined by ACP v1.

#### Work items

- [x] Preserve and render tool `kind` values: `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`, and `other`.
- [x] Render `tool_call_update` status transitions: `pending`, `in_progress`, `completed`, and `failed`.
- [x] Render tool-call `content` blocks, including regular content, diffs, and terminal references.
- [x] Preserve `locations` so UI can follow files and 1-based lines mentioned by the agent.
- [x] Preserve `rawInput` and `rawOutput` for diagnostics without leaking secrets into user-visible logs by default.
- [x] Keep current upsert behavior for constructible `tool_call_update` messages and test unknown-id rejection when required fields are missing.

#### Exit criteria

- [x] Tool calls display useful progress, output, diffs, terminals, and affected file locations.
- [x] Tool-call state remains bounded and secret-conscious.

### Phase 3: Elicitation security and completion semantics

Implement elicitation according to ACP privacy and URL-safety requirements.

#### Work items

- [x] Advertise elicitation modes explicitly; `{}` must not imply form support.
- [x] Reject `elicitation/create` modes not advertised with JSON-RPC `-32602`.
- [x] In form mode, display agent identity, request message, schema fields, decline, and cancel controls.
- [x] In form mode, validate submitted values against supported flat JSON schema before responding.
- [x] In form mode, prevent secret-like fields and credentials from being requested when detectable.
- [x] In URL mode, show full URL and highlighted host before navigation.
- [x] In URL mode, require explicit user consent before opening the URL.
- [x] In URL mode, do not prefetch URLs or expose resulting credentials/tokens to ACP, model context, or logs.
- [x] Track URL `elicitationId` per connection and ignore unknown or already-completed `elicitation/complete` notifications.

#### Exit criteria

- [x] Elicitation respects privacy, consent, and safe URL handling requirements.
- [x] URL completions are connection-scoped and idempotent.

### Phase 4: Filesystem request semantics

Make ACP filesystem methods match editor-aware requirements.

#### Work items

- [x] Serve `fs/read_text_file` from unsaved editor buffer state when present.
- [x] Enforce absolute paths and 1-based line values for reads.
- [x] Enforce workspace/effective-root containment for reads and writes.
- [x] Make `fs/write_text_file` create files when missing.
- [x] Route writes through existing buffer/edit/save semantics and approval flow.
- [x] Return `null`/empty ACP result shape expected by `WriteTextFileResponse`.
- [x] Add regression tests for create-if-missing, dirty-buffer reads, root escape, and line window reads.

#### Exit criteria

- [x] Filesystem methods reflect editor state and cannot escape allowed roots.
- [x] Writes are safe, approved, and protocol-shaped.

### Phase 5: Terminal lifecycle semantics

Implement all ACP terminal behavior, not only method dispatch.

#### Work items

- [x] Support `terminal/create` `command`, `args`, `env`, absolute `cwd`, and `outputByteLimit`.
- [x] Require approval before starting terminal commands.
- [x] Truncate retained output from the beginning when exceeding `outputByteLimit`.
- [x] Preserve valid UTF-8/character boundaries when truncating output.
- [x] Make `terminal/output` return output, `truncated`, and optional exit status.
- [x] Make `terminal/wait_for_exit` resolve only when command exits or request is cancelled.
- [x] Make `terminal/kill` terminate without releasing terminal state.
- [x] Make `terminal/release` kill if still running and invalidate the terminal id afterward.
- [x] Keep terminal output displayable after release when referenced by a tool call.
- [x] Track terminal ownership by agent connection and session.

#### Exit criteria

- [x] Terminals can be created, observed, waited, killed, and released safely.
- [x] Terminal ids cannot be guessed across sessions or agents.
- [x] Output remains bounded and valid text.

### Phase 6: Cancellation conformance

Align explicit prompt cancellation and JSON-RPC request cancellation behavior.

#### Work items

- [x] Continue sending `session/cancel` for active prompt turns.
- [x] Continue responding to pending `session/request_permission` requests with `cancelled` when the turn is cancelled.
- [x] Verify outgoing prompt cancellation also sends `$/cancel_request` for the in-flight `session/prompt` request where SDK support exists.
- [x] Handle incoming `$/cancel_request` for long-running client-side requests when SDK exposes it.
- [x] Use JSON-RPC `-32800` for internally cancelled request responses where applicable.
- [x] Accept late `session/update` notifications until the cancelled prompt response arrives.

#### Exit criteria

- [x] Prompt turn cancellation is graceful and protocol-compliant.
- [x] Long-running client-side fs, terminal, elicitation, and permission work can stop without hanging the agent.

### Phase 7: Extensibility rules

Keep custom behavior compatible with ACP extension rules.

#### Work items

- [x] Do not add custom root-level fields to ACP-defined types; use `_meta` only.
- [x] Preserve `_meta` fields for correlation when safe, including trace context keys.
- [x] Keep custom methods under underscore-prefixed names if ACP-level extensions are added.
- [x] Advertise custom capabilities under `_meta` instead of new standard-looking capability names.
- [x] Ignore unknown custom notifications and return method-not-found for unknown custom requests.

#### Exit criteria

- [x] Extensions cannot collide with future ACP standard fields or methods.
- [x] Unknown extension data remains diagnostic/correlation-only unless explicitly supported.

## General-Purpose Standalone ACP Agent Server Framework

Build a reusable ACP agent-side server framework so provider binaries stop handrolling JSON-RPC loops. The framework must live in a new `ee-acp-agent-server` crate, use `ee-agent-protocol` SDK-backed ACP v1 types, and keep `ee-agent-host` as the editor/client-side host only.

### Phase 1: Crate skeleton, public boundaries, and transport

Goal: create the standalone framework crate with a minimal stdio/memory transport and no provider-specific behavior.

Overview: this phase establishes crate shape, compile targets, transport frame parsing, frame writing, and shared error/config types. It must be usable by tests without spawning real subprocesses.

Rules:

- Use `ee-agent-protocol` re-exports for ACP types.
- Do not introduce ee-owned ACP wire structs.
- Keep `ee-agent-host` dependency out of this crate.
- Keep frame parsing bounded and newline-delimited.
- Prefer existing workspace dependencies; add no new dependency unless strictly needed.

#### Work items

- [ ] Add `crates/ee-acp-agent-server` to the workspace members in `ee/Cargo.toml`.
  - [ ] Create `crates/ee-acp-agent-server/Cargo.toml`.
    - [ ] Set package name to `ee-acp-agent-server`.
    - [ ] Set version to `0.1.0`.
    - [ ] Use workspace `edition`, `rust-version`, `license`, and author conventions.
    - [ ] Add dependencies on `ee-agent-protocol`, `futures`, `serde`, `serde_json`, `tokio`, and `tracing` from workspace dependencies.
    - [ ] Add `tempfile` as dev-dependency only if tests need filesystem fixtures.
  - [ ] Create `crates/ee-acp-agent-server/src/lib.rs`.
    - [ ] Export `config`, `error`, `transport`, `provider`, `server`, `session`, `updates`, `client`, `ids`, and `validate` modules.
    - [ ] Re-export primary public types from crate root.
    - [ ] Add crate-level docs stating this crate is ACP agent-side only.
- [ ] Implement framework config in `src/config.rs`.
  - [ ] Add `AcpAgentServerConfig`.
    - [ ] Include `request_timeout: Duration`.
    - [ ] Include `prompt_timeout: Option<Duration>`.
    - [ ] Include `max_frame_bytes: usize`.
    - [ ] Include `session_id_prefix: String`.
    - [ ] Include `implementation: ee_agent_protocol::Implementation`.
  - [ ] Implement `Default`.
    - [ ] Set request timeout to `30s`.
    - [ ] Set prompt timeout to `None`.
    - [ ] Set max frame bytes to `4 * 1024 * 1024`.
    - [ ] Set session id prefix to `session`.
    - [ ] Set implementation name/title to framework defaults.
  - [ ] Add unit tests for default values.
- [ ] Implement framework errors in `src/error.rs`.
  - [ ] Add `AcpServerError` variants for I/O, JSON parse, protocol, unsupported version, unknown session, request timeout, transport closed, and provider errors.
  - [ ] Add `ProviderError` variants for invalid request, backend failure, cancellation, client request failure, and permission denied.
  - [ ] Implement `Display` and `std::error::Error`.
  - [ ] Implement helper methods to map errors to JSON-RPC error code and message.
  - [ ] Add unit tests for JSON-RPC code mapping.
- [ ] Implement ID generation in `src/ids.rs`.
  - [ ] Add monotonic request-id generator returning ACP `RequestId` or SDK-compatible value.
  - [ ] Add monotonic session-id generator using configured prefix.
  - [ ] Ensure generated IDs are process-local unique without global mutable state.
  - [ ] Add unit tests for monotonic request IDs.
  - [ ] Add unit tests for configured session-id prefix.
- [ ] Implement transport abstraction in `src/transport.rs`.
  - [ ] Define `AcpTransport` trait.
    - [ ] Add `read_message` async method returning `Option<JsonRpcMessage>`.
    - [ ] Add `write_message` async method.
    - [ ] Require `Send + 'static`.
  - [ ] Implement `StdioTransport`.
    - [ ] Read newline-delimited JSON-RPC messages from stdin.
    - [ ] Write one JSON-RPC message per line to stdout.
    - [ ] Flush stdout after every message.
    - [ ] Enforce `max_frame_bytes` before parsing.
    - [ ] Treat EOF as clean shutdown.
  - [ ] Implement test-only `MemoryTransport`.
    - [ ] Accept inbound messages from an in-memory queue.
    - [ ] Capture outbound messages in deterministic order.
    - [ ] Support injecting EOF.
  - [ ] Add tests for valid frame parse.
  - [ ] Add tests for oversized frame rejection.
  - [ ] Add tests for EOF returning clean shutdown.
  - [ ] Add tests for write preserving one-line JSON.

#### Actionable criteria

- [ ] `cargo fmt --check` passes after crate creation.
- [ ] `cargo clippy --quiet -p ee-acp-agent-server --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server transport` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server config` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server error` passes.

### Phase 2: Provider trait, server runtime, and initialize/session lifecycle

Goal: add a provider-facing API and a server runtime that handles ACP initialization and session lifecycle requests.

Overview: providers implement business logic through a trait. The framework owns JSON-RPC dispatch, version negotiation, session store, and typed response/error shaping.

Rules:

- Negotiate ACP v1 only.
- Treat omitted capabilities as unsupported.
- Keep provider trait independent from OpenRouter.
- Store session state in the framework, not in transport code.
- Reject malformed params before invoking provider.

#### Work items

- [ ] Implement provider API in `src/provider.rs`.
  - [ ] Add `ProviderFuture<T>` boxed-future type alias.
  - [ ] Add `AgentProvider` trait.
    - [ ] Add `info(&self) -> Implementation`.
    - [ ] Add `capabilities(&self) -> AgentCapabilities`.
    - [ ] Add `new_session(ctx) -> ProviderFuture<Result<SessionInit, ProviderError>>`.
    - [ ] Add `load_session(ctx) -> ProviderFuture<Result<SessionInit, ProviderError>>`.
    - [ ] Add `prompt(ctx, sink, client, cancel) -> ProviderFuture<Result<PromptResult, ProviderError>>`.
    - [ ] Add `cancel_session(session_id) -> ProviderFuture<Result<(), ProviderError>>`.
    - [ ] Add `close_session(session_id) -> ProviderFuture<Result<(), ProviderError>>`.
  - [ ] Add `NewSessionContext`.
    - [ ] Include `cwd`.
    - [ ] Include `additional_directories`.
    - [ ] Include `mcp_servers`.
    - [ ] Include initial ACP session metadata needed by providers.
  - [ ] Add `LoadSessionContext`.
    - [ ] Include `session_id`.
    - [ ] Include `cwd`.
    - [ ] Include `additional_directories`.
    - [ ] Include `mcp_servers`.
  - [ ] Add `PromptContext`.
    - [ ] Include `session_id`.
    - [ ] Include prompt content blocks.
    - [ ] Include raw request metadata needed by providers.
  - [ ] Add `SessionInit`.
    - [ ] Include resolved `session_id`.
    - [ ] Include optional title.
    - [ ] Include available commands.
    - [ ] Include modes/config options when supported by SDK types.
  - [ ] Add `PromptResult`.
    - [ ] Include ACP stop reason.
    - [ ] Include optional usage metadata when SDK supports it.
- [ ] Implement session store in `src/session.rs`.
  - [ ] Add `ServerSession`.
    - [ ] Store `SessionId`.
    - [ ] Store optional absolute `cwd`.
    - [ ] Store absolute `additional_directories`.
    - [ ] Store advertised `mcp_servers`.
    - [ ] Store provider-owned `serde_json::Value` metadata.
  - [ ] Add `SessionStore`.
    - [ ] Use `RwLock<BTreeMap<SessionId, ServerSession>>` or tokio equivalent.
    - [ ] Add `insert_new`.
    - [ ] Add `get`.
    - [ ] Add `list`.
    - [ ] Add `remove`.
    - [ ] Add `contains`.
  - [ ] Add unit tests for insert/get/list/remove.
  - [ ] Add unit tests for duplicate session id rejection.
- [ ] Implement server runtime shell in `src/server.rs`.
  - [ ] Add `AcpAgentServer<P>` generic over `AgentProvider`.
  - [ ] Add `new(provider, config)` constructor.
  - [ ] Add `run_stdio` method using `StdioTransport`.
  - [ ] Add `run_with_transport` method for tests.
  - [ ] Start reader/dispatcher loop.
  - [ ] Send every response through transport writer path.
  - [ ] Fail all pending state on transport close.
- [ ] Implement request dispatch in `src/dispatch.rs`.
  - [ ] Route `initialize`.
    - [ ] Parse `InitializeRequest` using SDK type.
    - [ ] Reject protocol versions other than ACP v1.
    - [ ] Return provider info and capabilities.
    - [ ] Include framework implementation metadata from config.
  - [ ] Route `session/new`.
    - [ ] Parse SDK `NewSessionRequest`.
    - [ ] Validate `cwd` is absolute.
    - [ ] Validate every additional directory is absolute.
    - [ ] Invoke provider `new_session`.
    - [ ] Store returned session in `SessionStore`.
    - [ ] Return SDK `NewSessionResponse` shape.
  - [ ] Route `session/load`.
    - [ ] Parse SDK `LoadSessionRequest`.
    - [ ] Validate `cwd` is absolute.
    - [ ] Invoke provider `load_session`.
    - [ ] Store loaded session in `SessionStore`.
    - [ ] Return SDK `LoadSessionResponse` shape.
  - [ ] Route `session/list`.
    - [ ] Return sessions from `SessionStore` in stable order.
    - [ ] Treat cursors as opaque when SDK field exists.
  - [ ] Route `session/close`.
    - [ ] Cancel active prompt for session if present.
    - [ ] Invoke provider `close_session`.
    - [ ] Remove session from `SessionStore`.
    - [ ] Return SDK close response shape.
  - [ ] Route unknown methods.
    - [ ] Return JSON-RPC method-not-found for requests.
    - [ ] Ignore unknown notifications with tracing debug only.
- [ ] Add integration tests in `crates/ee-acp-agent-server/tests/server_flows.rs`.
  - [ ] Create fake provider returning deterministic info and capabilities.
  - [ ] Test `initialize` with protocol version `1` succeeds.
  - [ ] Test `initialize` with protocol version `0` fails closed.
  - [ ] Test `initialize` with protocol version `2` fails closed.
  - [ ] Test `session/new` stores session and returns provider session id.
  - [ ] Test `session/new` rejects relative `cwd` before provider call.
  - [ ] Test `session/load` stores session and returns loaded session id.
  - [ ] Test `session/list` returns stable sessions.
  - [ ] Test `session/close` removes session and calls provider close.
  - [ ] Test unknown request returns method-not-found.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-acp-agent-server initialize` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server session` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server server_flows` passes.
- [ ] Provider fake records prove malformed session requests do not invoke provider.

### Phase 3: Prompt execution, typed updates, and cancellation

Goal: support `session/prompt`, `session/cancel`, update emission, active-turn tracking, and prompt cancellation.

Overview: the framework starts a cancellable task for each prompt, gives providers a typed `UpdateSink`, and guarantees one active prompt per session.

Rules:

- Allow parallel prompts across different sessions.
- Reject concurrent prompts in same session by default.
- Do not emit updates for unknown sessions.
- Preserve outbound update order per session.
- Always cleanup active prompt state on completion, error, cancellation, and close.

#### Work items

- [ ] Implement typed update sink in `src/updates.rs`.
  - [ ] Add `UpdateSink` bound to a session id and transport writer.
  - [ ] Add `agent_message_chunk(message_id, text)` helper.
    - [ ] Build SDK `SessionUpdate` value where available.
    - [ ] Send `session/update` notification with correct `sessionId`.
  - [ ] Add `agent_thought_chunk(message_id, text)` helper.
  - [ ] Add `tool_call_pending(tool_call_id, title, kind)` helper.
  - [ ] Add `tool_call_in_progress(tool_call_id, title, content)` helper.
  - [ ] Add `tool_call_completed(tool_call_id, title, content)` helper.
  - [ ] Add `tool_call_failed(tool_call_id, title, error)` helper.
  - [ ] Add `plan_replace(entries)` helper.
  - [ ] Add `available_commands_replace(commands)` helper.
  - [ ] Add `session_info_update(info)` helper.
  - [ ] Add `raw_update(update)` escape hatch using SDK type.
  - [ ] Validate message ids and tool-call ids are non-empty.
- [ ] Add active prompt tracking in `src/session.rs` or `src/server.rs`.
  - [ ] Store `CancellationToken` or equivalent cancellation flag per active session prompt.
  - [ ] Store prompt join handle per active session prompt.
  - [ ] Add helper to start active prompt only when no prompt exists for session.
  - [ ] Add helper to cancel active prompt.
  - [ ] Add helper to cleanup active prompt after completion.
- [ ] Route `session/prompt` in `src/dispatch.rs`.
  - [ ] Parse SDK `SessionPromptRequest`.
  - [ ] Reject unknown session before provider call.
  - [ ] Reject same-session concurrent prompt with clear JSON-RPC error.
  - [ ] Create `PromptContext` from request.
  - [ ] Create `UpdateSink` for session.
  - [ ] Create placeholder `ClientBridge` with no outbound methods until Phase 4.
  - [ ] Await provider prompt completion.
  - [ ] Return SDK prompt response with stop reason.
  - [ ] Map provider cancellation to a deterministic prompt result or JSON-RPC cancellation error according to SDK response support.
- [ ] Route `session/cancel` in `src/dispatch.rs`.
  - [ ] Parse SDK cancel request or notification shape.
  - [ ] Cancel active prompt for the session.
  - [ ] Invoke provider `cancel_session`.
  - [ ] Return success for request form.
  - [ ] Do not error when cancel targets no active prompt.
- [ ] Make `session/close` cancel active prompt.
  - [ ] Trigger active prompt cancellation before provider close.
  - [ ] Await prompt cleanup with bounded timeout.
  - [ ] Remove prompt state before removing session.
- [ ] Add update tests.
  - [ ] Test message chunk emits `session/update` before prompt response.
  - [ ] Test thought chunk emits expected ACP update name.
  - [ ] Test tool-call completed helper includes title/status/content.
  - [ ] Test plan replace helper emits complete replacement update.
  - [ ] Test update sink rejects unknown session.
- [ ] Add cancellation tests in `tests/cancellation.rs`.
  - [ ] Test `session/cancel` triggers provider cancellation token.
  - [ ] Test cancelled prompt eventually removes active prompt state.
  - [ ] Test `session/close` cancels active prompt.
  - [ ] Test second same-session prompt is rejected while first prompt is active.
  - [ ] Test prompts in two sessions run concurrently.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-acp-agent-server prompt` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server updates` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server cancellation` passes.
- [ ] Tests prove same-session prompt state is cleaned after success, provider error, and cancellation.

### Phase 4: Outbound ACP client request bridge

Goal: let provider implementations call ACP client methods through a typed `ClientBridge` instead of crafting JSON-RPC.

Overview: the framework sends client requests like `fs/read_text_file`, waits for matching responses, maps client errors, applies timeouts, and cleans pending requests on shutdown.

Rules:

- Generate framework-owned JSON-RPC ids for outbound client requests.
- Match responses strictly by id.
- Never leave pending requests after timeout, cancellation, transport close, or provider completion.
- Validate outbound file paths as absolute before sending.
- Use typed SDK request/response structs for every public bridge method.

#### Work items

- [ ] Implement pending request manager in `src/client.rs`.
  - [ ] Add `ClientBridge` cloneable handle.
  - [ ] Add pending map keyed by request id.
  - [ ] Add `send_request(method, params)` internal helper.
    - [ ] Allocate request id.
    - [ ] Insert pending oneshot sender before writing request.
    - [ ] Write JSON-RPC request to transport.
    - [ ] Await response with `request_timeout`.
    - [ ] Remove pending entry on success, timeout, cancellation, and write failure.
  - [ ] Add `handle_response(response)`.
    - [ ] Resolve matching pending request.
    - [ ] Ignore unknown response id with tracing debug.
    - [ ] Map JSON-RPC error response to `ProviderError::ClientRequestFailed` or permission denied where applicable.
  - [ ] Add `fail_all_pending(reason)`.
    - [ ] Resolve every pending request with transport-closed error.
- [ ] Add typed `ClientBridge` methods.
  - [ ] Implement `read_text_file(ReadTextFileRequest) -> ReadTextFileResponse`.
    - [ ] Validate path absolute.
    - [ ] Send `fs/read_text_file`.
    - [ ] Decode SDK response type.
  - [ ] Implement `write_text_file(WriteTextFileRequest) -> WriteTextFileResponse`.
    - [ ] Validate path absolute.
    - [ ] Send `fs/write_text_file`.
  - [ ] Implement `create_terminal(CreateTerminalRequest) -> CreateTerminalResponse`.
    - [ ] Validate `cwd` absolute when present.
    - [ ] Send `terminal/create`.
  - [ ] Implement `terminal_output(TerminalOutputRequest) -> TerminalOutputResponse`.
  - [ ] Implement `wait_for_terminal_exit(WaitForTerminalExitRequest) -> WaitForTerminalExitResponse`.
  - [ ] Implement `kill_terminal(KillTerminalRequest) -> KillTerminalResponse`.
  - [ ] Implement `release_terminal(ReleaseTerminalRequest) -> ReleaseTerminalResponse`.
  - [ ] Implement `create_elicitation(CreateElicitationRequest) -> CreateElicitationResponse`.
- [ ] Route inbound JSON-RPC responses to `ClientBridge`.
  - [ ] Distinguish inbound requests from responses in server reader loop.
  - [ ] Forward response envelopes to pending request manager.
  - [ ] Keep request dispatch path unchanged for inbound requests.
- [ ] Pass real `ClientBridge` to provider prompt.
  - [ ] Replace Phase 3 placeholder with live bridge handle.
  - [ ] Ensure prompt cancellation cancels bridge waits owned by that prompt.
- [ ] Add client bridge tests in `tests/client_requests.rs`.
  - [ ] Test provider calls `read_text_file` and framework emits `fs/read_text_file` request.
  - [ ] Test matching response returns content to provider.
  - [ ] Test JSON-RPC error response maps to provider client request failure.
  - [ ] Test unknown response id is ignored.
  - [ ] Test timeout removes pending entry.
  - [ ] Test transport close fails pending entries.
  - [ ] Test relative outbound read path fails before request is written.
  - [ ] Test terminal create rejects relative cwd.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-acp-agent-server client` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server client_requests` passes.
- [ ] Tests prove no pending requests remain after timeout and transport close.
- [ ] Tests prove relative outbound file paths do not write JSON-RPC requests.

### Phase 5: Protocol validation, hardening, and conformance coverage

Goal: make framework fail closed and cover protocol edge cases before migrating real agents.

Overview: validation lives at framework boundaries, errors are deterministic, and all unsupported or malformed protocol inputs produce shaped failures without panics.

Rules:

- Fail closed on invalid params, unknown sessions, unsupported versions, and oversized frames.
- Do not panic on malformed provider output.
- Keep logs secret-conscious.
- Keep extension data diagnostic-only unless explicitly supported.
- Every hardening rule needs regression coverage.

#### Work items

- [ ] Implement validation helpers in `src/validate.rs`.
  - [ ] Add `validate_protocol_version_v1`.
  - [ ] Add `validate_absolute_path`.
  - [ ] Add `validate_absolute_paths`.
  - [ ] Add `validate_session_id`.
  - [ ] Add `validate_message_id`.
  - [ ] Add `validate_tool_call_id`.
  - [ ] Add `validate_frame_len`.
  - [ ] Add tests for every validation helper.
- [ ] Harden dispatch error handling.
  - [ ] Convert JSON parse errors to JSON-RPC parse error response when possible.
  - [ ] Convert invalid params to `-32602`.
  - [ ] Convert unknown methods to `-32601`.
  - [ ] Convert provider backend errors to `-32603`.
  - [ ] Convert unknown session to deterministic protocol error.
  - [ ] Add tests for every error mapping.
- [ ] Harden provider result handling.
  - [ ] Reject provider-returned empty session ids.
  - [ ] Reject provider-returned duplicate session ids.
  - [ ] Reject provider attempts to emit update for removed session.
  - [ ] Map provider cancellation consistently.
  - [ ] Add tests for rejected provider result cases.
- [ ] Harden transport lifecycle.
  - [ ] Fail pending client requests on EOF.
  - [ ] Cancel active prompts on EOF.
  - [ ] Close writer path after reader shutdown.
  - [ ] Add tests for EOF during active prompt.
  - [ ] Add tests for EOF during outbound client request.
- [ ] Add conformance fixture tests.
  - [ ] Add initialize request fixture under `tests/fixtures`.
  - [ ] Add session new request fixture.
  - [ ] Add session prompt request fixture.
  - [ ] Add session cancel request fixture.
  - [ ] Add fs read response fixture.
  - [ ] Test fixture round-trips with SDK types.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-acp-agent-server validate` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server conformance` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server` passes.
- [ ] `cargo clippy --quiet -p ee-acp-agent-server --all-targets --all-features -- -D warnings` passes.

### Phase 6: Refactor `ee-openrouter-agent` onto framework

Goal: convert the existing OpenRouter ACP agent from a handrolled JSON-RPC binary into a provider implementation using `ee-acp-agent-server`.

Overview: OpenRouter keeps its behavior and tests, but protocol handling, sessions, updates, client file reads, cancellation, and errors move to the framework.

Rules:

- Preserve existing OpenRouter env vars and defaults.
- Never log `OPENROUTER_API_KEY`.
- Keep `.env` parsing local and non-mutating.
- Use `ClientBridge` for file reads.
- Use `UpdateSink` for messages, thoughts, and tool-call status.

#### Work items

- [ ] Update `crates/ee-openrouter-agent/Cargo.toml`.
  - [ ] Add dependency on `ee-acp-agent-server`.
  - [ ] Add dependency on `tokio` if binary runtime needs it.
  - [ ] Remove direct protocol-loop-only dependencies that become unused.
- [ ] Split OpenRouter modules.
  - [ ] Move CLI/env config code to `src/config.rs`.
    - [ ] Keep `OPENROUTER_MODEL`.
    - [ ] Keep `OPENROUTER_API_URL`.
    - [ ] Keep `OPENROUTER_SITE_URL`.
    - [ ] Keep `OPENROUTER_APP_TITLE`.
    - [ ] Keep `OPENROUTER_TIMEOUT_MS`.
    - [ ] Keep `OPENROUTER_REASONING_EFFORT`.
    - [ ] Keep `OPENROUTER_SYSTEM_PROMPT`.
    - [ ] Keep `OPENROUTER_API_KEY` lookup.
  - [ ] Move dotenv parser to `src/dotenv.rs`.
    - [ ] Preserve quote handling.
    - [ ] Preserve `export ` prefix support.
    - [ ] Preserve invalid-name skipping.
  - [ ] Move HTTP request/response mapping to `src/openrouter.rs`.
    - [ ] Keep request body model/messages/tools/tool_choice shape.
    - [ ] Keep reasoning effort insertion.
    - [ ] Keep OpenRouter HTTP error extraction.
  - [ ] Move provider/tool behavior to `src/provider.rs` and `src/tools.rs`.
- [ ] Implement `OpenRouterProvider`.
  - [ ] Store config and HTTP client.
  - [ ] Store per-session message history behind mutex/RwLock.
  - [ ] Implement `info` returning `ee-openrouter-agent` implementation metadata.
  - [ ] Implement `capabilities` for supported prompt/session behavior.
  - [ ] Implement `new_session`.
    - [ ] Store session cwd.
    - [ ] Initialize empty message history.
  - [ ] Implement `load_session` as unsupported provider error matching previous behavior.
  - [ ] Implement `prompt`.
    - [ ] Extract text prompt blocks.
    - [ ] Return invalid request when prompt contains no text.
    - [ ] Return backend error when API key missing.
    - [ ] Send OpenRouter request with existing message history.
    - [ ] Emit reasoning through `UpdateSink::agent_thought_chunk`.
    - [ ] Emit answer through `UpdateSink::agent_message_chunk`.
    - [ ] Return end-turn stop reason.
  - [ ] Implement bounded tool loop.
    - [ ] Keep max tool rounds at `6`.
    - [ ] Support `tool_read_file` and `read_file` aliases.
    - [ ] Resolve relative paths against session cwd.
    - [ ] Call `ClientBridge::read_text_file`.
    - [ ] Emit tool-call in-progress and completed/failed updates.
    - [ ] Append tool results to OpenRouter messages.
  - [ ] Implement `cancel_session` by marking active work cancelled if needed.
  - [ ] Implement `close_session` by removing stored history.
- [ ] Replace `src/main.rs` protocol loop.
  - [ ] Parse args.
  - [ ] Load `.env` from current directory.
  - [ ] Build `OpenRouterProvider`.
  - [ ] Run `AcpAgentServer::new(provider, config).run_stdio().await`.
  - [ ] Print only concise process-level errors to stderr.
- [ ] Preserve and update tests.
  - [ ] Keep prompt text extraction test.
  - [ ] Keep OpenRouter string answer extraction test.
  - [ ] Keep OpenRouter reasoning extraction test.
  - [ ] Keep reasoning effort request body test.
  - [ ] Keep tool-call argument extraction test.
  - [ ] Keep prompt-without-api-key JSON-RPC/framework error test.
  - [ ] Convert read-file tool test to assert framework emits `fs/read_text_file`.
  - [ ] Add test that provider emits thought update through framework.
  - [ ] Add test that provider emits answer update through framework.
  - [ ] Add test that tool loop max rounds maps to provider backend error.
  - [ ] Add test that close session removes message history.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-openrouter-agent` passes.
- [ ] `cargo clippy --quiet -p ee-openrouter-agent --all-targets --all-features -- -D warnings` passes.
- [ ] OpenRouter tests prove no handrolled stdin/stdout JSON-RPC loop remains in provider code.
- [ ] OpenRouter read-file test proves file access goes through `ClientBridge`.

### Phase 7: Echo example and compile-tested provider documentation

Goal: add a tiny local example provider that exercises the framework without network access and keep docs examples buildable.

Overview: the example gives automated smoke coverage for the public API. Documentation must include compile-tested examples rather than manual setup tasks.

Rules:

- Example must not call external APIs.
- Example must not depend on editor UI.
- Documentation examples must compile in tests when possible.
- Do not add manual verification checklist items.

#### Work items

- [ ] Add `crates/ee-acp-agent-server/examples/echo_agent.rs`.
  - [ ] Implement `EchoProvider` using `AgentProvider`.
  - [ ] Return deterministic implementation metadata.
  - [ ] Create sessions with framework-generated or provider-accepted ids.
  - [ ] On prompt, concatenate text blocks.
  - [ ] Emit echoed text through `UpdateSink::agent_message_chunk`.
  - [ ] Return end-turn prompt result.
  - [ ] Support cancellation by checking cancellation token before emitting final update.
- [ ] Add example tests.
  - [ ] Add integration test that runs echo provider with memory transport.
  - [ ] Send `initialize` and assert ACP v1 response.
  - [ ] Send `session/new` and assert session id exists.
  - [ ] Send `session/prompt` and assert `session/update` contains echoed text.
  - [ ] Send `session/cancel` during blocked echo prompt and assert cancellation cleanup.
- [ ] Add crate docs with compile-tested example in `src/lib.rs`.
  - [ ] Show minimal provider struct.
  - [ ] Show `AgentProvider` implementation skeleton.
  - [ ] Show `AcpAgentServer::new(provider, config)` usage.
  - [ ] Mark non-runnable stdio example with `no_run` if needed.
- [ ] Add README validation tests where practical.
  - [ ] Keep code snippets mirrored in doc tests or examples.
  - [ ] Avoid untested command snippets as checklist work.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-acp-agent-server --examples` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server --doc` passes.
- [ ] Echo example integration test proves initialize/new/prompt/update flow works without network.

### Phase 8: Workspace validation and integration guardrails

Goal: prove framework, OpenRouter provider, and existing host/protocol crates remain compatible after refactor.

Overview: this phase adds focused tests and workspace checks that catch boundary regressions between agent server framework, ACP protocol facade, existing host, and provider binary.

Rules:

- Validate changed crates first, then broader workspace summary.
- Use quiet cargo test commands only.
- Do not change host/client behavior except where tests reveal necessary compatibility fixes.
- Keep framework public API minimal and stable before wider use.

#### Work items

- [ ] Add cross-crate compile checks.
  - [ ] Ensure `ee-acp-agent-server` depends on `ee-agent-protocol` but not `ee-agent-host`.
  - [ ] Add a unit test or crate-level compile assertion proving public API exposes SDK-backed `SessionId`, `SessionUpdate`, and request/response types.
  - [ ] Add a test ensuring framework-supported ACP version equals `ee_agent_protocol::SUPPORTED_ACP_VERSION`.
- [ ] Add compatibility tests with existing host fake transport where feasible.
  - [ ] Start framework fake provider over memory/pipe transport.
  - [ ] Connect `ee-agent-host` fake/client side if existing test utilities support injected transport.
  - [ ] Assert host can initialize, create session, prompt, receive update, and close session.
  - [ ] Keep this test behind existing `test-utils` feature if needed.
- [ ] Add public API hygiene checks.
  - [ ] Keep provider trait methods documented.
  - [ ] Keep exported structs non-exhaustive where future fields are likely.
  - [ ] Prefer `pub(crate)` for internal runtime structs.
  - [ ] Add compile-fail or privacy tests only if existing project pattern supports them.
- [ ] Run focused validation commands.
  - [ ] Validate format.
  - [ ] Validate clippy for `ee-acp-agent-server`.
  - [ ] Validate clippy for `ee-openrouter-agent`.
  - [ ] Validate framework tests.
  - [ ] Validate OpenRouter tests.
  - [ ] Validate protocol tests touched by public type usage.

#### Actionable criteria

- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --quiet -p ee-acp-agent-server --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo clippy --quiet -p ee-openrouter-agent --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server` passes.
- [ ] `cargo test --quiet -p ee-openrouter-agent` passes.
- [ ] `cargo test --quiet -p ee-agent-protocol` passes when protocol facade exports changed.
- [ ] `./scripts/test-workspace-summary.sh` passes after focused crate validation.

## Future General-Purpose Agent Orchestrator Framework

Build an optional orchestration layer above `ee-acp-agent-server` so ACP agent providers can run structured agent loops, delegate work to subagents, coordinate tools, enforce budgets, and maintain task state without reimplementing these behaviors per provider.

Recommended crate: `crates/ee-agent-orchestrator`. It must depend on `ee-acp-agent-server` and `ee-agent-protocol`, but not on `ee-agent-host` or `ee-cli`. It provides reusable agent-loop primitives for server-side agent binaries.

### Phase 1: Orchestrator crate skeleton and core runtime types

Goal: create orchestration crate with provider-agnostic runtime types, deterministic state containers, and no model/provider-specific behavior.

Overview: this phase defines the boundary between ACP protocol runtime and higher-level orchestration. The orchestrator consumes prompt contexts, produces updates through `UpdateSink`, calls tools through `ClientBridge`, and delegates model calls to provider-supplied traits.

Rules:

- Keep ACP transport/session protocol in `ee-acp-agent-server`.
- Keep orchestration model-provider-neutral.
- Do not depend on editor/client-side crates.
- Make orchestration state serializable for tests and future persistence.
- Use deterministic IDs from injected generators in tests.

#### Work items

- [ ] Add `crates/ee-agent-orchestrator` to workspace members in `ee/Cargo.toml`.
  - [ ] Create `crates/ee-agent-orchestrator/Cargo.toml`.
    - [ ] Set package name to `ee-agent-orchestrator`.
    - [ ] Use workspace `edition`, `rust-version`, `license`, and author conventions.
    - [ ] Add dependencies on `ee-acp-agent-server`, `ee-agent-protocol`, `serde`, `serde_json`, `tokio`, `futures`, and `tracing` from workspace dependencies.
    - [ ] Add dev-dependencies needed only for deterministic async tests.
  - [ ] Create `crates/ee-agent-orchestrator/src/lib.rs`.
    - [ ] Export `config`, `error`, `runtime`, `loop_engine`, `model`, `tools`, `tasks`, `subagents`, `memory`, `budget`, `policy`, `events`, and `test_support` modules.
    - [ ] Re-export primary public types.
    - [ ] Add crate docs stating this crate is optional server-side orchestration above ACP.
- [ ] Implement orchestrator config in `src/config.rs`.
  - [ ] Add `OrchestratorConfig`.
    - [ ] Include `max_loop_iterations`.
    - [ ] Include `max_tool_calls_per_turn`.
    - [ ] Include `max_subagent_depth`.
    - [ ] Include `max_parallel_subagents`.
    - [ ] Include `turn_timeout`.
    - [ ] Include `tool_timeout`.
    - [ ] Include `subagent_timeout`.
    - [ ] Include `memory_limit_bytes`.
  - [ ] Implement safe defaults.
    - [ ] Set max loop iterations to `16`.
    - [ ] Set max tool calls per turn to `32`.
    - [ ] Set max subagent depth to `2`.
    - [ ] Set max parallel subagents to `4`.
    - [ ] Set turn timeout to `300s`.
    - [ ] Set tool timeout to `120s`.
    - [ ] Set subagent timeout to `300s`.
    - [ ] Set memory limit bytes to `1 MiB`.
  - [ ] Add tests for default config values.
- [ ] Implement orchestrator errors in `src/error.rs`.
  - [ ] Add `OrchestratorError` variants for model failure, tool failure, policy denial, budget exceeded, timeout, cancellation, invalid state, subagent failure, and serialization failure.
  - [ ] Implement conversion to `ee_acp_agent_server::ProviderError`.
  - [ ] Add tests for error-to-provider-error mapping.
- [ ] Implement runtime state in `src/runtime.rs`.
  - [ ] Add `OrchestratorRuntime`.
    - [ ] Store config.
    - [ ] Store injected model router.
    - [ ] Store tool registry.
    - [ ] Store task store.
    - [ ] Store memory store.
    - [ ] Store budget tracker.
    - [ ] Store policy engine.
  - [ ] Add `run_turn(prompt_ctx, sink, client, cancel)` entry point.
    - [ ] Build initial root task from prompt.
    - [ ] Start loop engine with configured budgets.
    - [ ] Return `PromptResult` compatible with `ee-acp-agent-server`.
  - [ ] Add tests using fake model and fake tools to run one complete turn.

#### Actionable criteria

- [ ] `cargo fmt --check` passes after crate creation.
- [ ] `cargo clippy --quiet -p ee-agent-orchestrator --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator config` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator runtime` passes.

### Phase 2: Model abstraction and transcript normalization

Goal: define provider-independent model-call interfaces and normalized messages that the loop engine can use.

Overview: orchestrator must not know OpenRouter, Anthropic, local model APIs, or provider JSON. Providers implement a model adapter trait that consumes normalized requests and returns normalized responses with text, reasoning, tool intents, and delegation intents.

Rules:

- Keep model adapter async and cancellation-aware.
- Do not expose provider-specific JSON in core loop state.
- Preserve raw provider metadata only in bounded diagnostic fields.
- Make normalized transcript serializable and deterministic.
- Treat unsupported response parts as model errors, not silent drops.

#### Work items

- [ ] Implement normalized messages in `src/model.rs`.
  - [ ] Add `ModelMessage`.
    - [ ] Include role: system, user, assistant, tool, subagent.
    - [ ] Include content blocks.
    - [ ] Include optional reasoning summary.
    - [ ] Include bounded metadata.
  - [ ] Add `ModelContent`.
    - [ ] Include text.
    - [ ] Include tool result.
    - [ ] Include file reference.
    - [ ] Include terminal reference.
  - [ ] Add `ModelRequest`.
    - [ ] Include transcript.
    - [ ] Include available tool schemas.
    - [ ] Include budget snapshot.
    - [ ] Include current task state.
  - [ ] Add `ModelResponse`.
    - [ ] Include assistant text.
    - [ ] Include reasoning text.
    - [ ] Include tool intents.
    - [ ] Include subagent intents.
    - [ ] Include completion signal.
- [ ] Implement model adapter trait.
  - [ ] Add `ModelAdapter` trait with `complete(request, cancel)`.
  - [ ] Add `ModelFuture<T>` boxed-future alias.
  - [ ] Require adapter to be `Send + Sync + 'static`.
  - [ ] Add fake deterministic model adapter in `test_support`.
- [ ] Implement transcript builder.
  - [ ] Convert ACP prompt content into normalized `ModelMessage` values.
  - [ ] Append assistant text responses.
  - [ ] Append tool results with stable tool-call IDs.
  - [ ] Append subagent summaries.
  - [ ] Enforce memory byte limit while preserving newest context.
- [ ] Add model tests.
  - [ ] Test ACP text prompt converts to normalized user message.
  - [ ] Test reasoning is preserved separately from assistant text.
  - [ ] Test tool intent parsing from fake model response.
  - [ ] Test subagent intent parsing from fake model response.
  - [ ] Test transcript truncation preserves newest messages and records truncation metadata.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator model` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator transcript` passes.
- [ ] Tests prove normalized transcript never contains provider-specific required fields.

### Phase 3: Tool registry, execution pipeline, and policy gates

Goal: provide reusable tool orchestration around `ClientBridge` with validation, approval-aware execution, result shaping, and failure handling.

Overview: orchestrator exposes a tool registry to model adapters, maps tool intents to typed `ClientBridge` calls or custom server-side tools, emits tool-call updates, and enforces per-turn tool limits.

Rules:

- Use `ClientBridge` for editor/client operations.
- Keep tool input schemas flat and explicit.
- Validate tool arguments before execution.
- Emit `tool_call_update` for every tool execution lifecycle.
- Fail closed when policy denies tool execution.

#### Work items

- [ ] Implement tool types in `src/tools.rs`.
  - [ ] Add `ToolDefinition`.
    - [ ] Include name.
    - [ ] Include description.
    - [ ] Include JSON schema.
    - [ ] Include side-effect class: read, write, execute, delegate.
    - [ ] Include required capability flags.
  - [ ] Add `ToolIntent`.
    - [ ] Include tool call id.
    - [ ] Include tool name.
    - [ ] Include JSON arguments.
  - [ ] Add `ToolResult`.
    - [ ] Include success flag.
    - [ ] Include text output.
    - [ ] Include structured output.
    - [ ] Include error kind.
- [ ] Implement tool registry.
  - [ ] Register built-in `read_file` mapping to `ClientBridge::read_text_file`.
  - [ ] Register built-in `write_file` mapping to `ClientBridge::write_text_file`.
  - [ ] Register built-in terminal lifecycle tools mapping to `ClientBridge` terminal methods.
  - [ ] Register built-in `ask_user` mapping to `ClientBridge::create_elicitation`.
  - [ ] Support custom provider-supplied tools through a `ServerTool` trait.
  - [ ] Add tests for duplicate tool name rejection.
- [ ] Implement policy engine in `src/policy.rs`.
  - [ ] Add `ToolPolicy`.
    - [ ] Allow read tools by default.
    - [ ] Require explicit allowance for write tools.
    - [ ] Require explicit allowance for execute tools.
    - [ ] Limit delegate tools by subagent depth and count.
  - [ ] Add `PolicyDecision` with allow/deny reason.
  - [ ] Add tests for read/write/execute/delegate policy decisions.
- [ ] Implement tool executor.
  - [ ] Validate tool exists.
  - [ ] Validate argument shape against tool schema where practical.
  - [ ] Check policy before execution.
  - [ ] Increment budget counters before execution.
  - [ ] Emit pending tool-call update.
  - [ ] Emit in-progress tool-call update.
  - [ ] Run tool with timeout and cancellation.
  - [ ] Emit completed or failed tool-call update.
  - [ ] Return normalized `ToolResult` to loop engine.
- [ ] Add tool execution tests.
  - [ ] Test read file tool calls `ClientBridge::read_text_file`.
  - [ ] Test write file tool is denied by default policy.
  - [ ] Test execute tool is denied by default policy.
  - [ ] Test custom tool runs and returns structured output.
  - [ ] Test tool timeout emits failed update.
  - [ ] Test cancellation stops running tool and records cancellation result.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator tools` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator policy` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator tool_executor` passes.
- [ ] Tests prove write/execute tools fail closed under default policy.

### Phase 4: Agent loop engine and completion control

Goal: implement reusable model-tool loop that can plan, call tools, observe results, and stop deterministically.

Overview: loop engine repeatedly calls the model adapter, emits assistant/thought updates, executes requested tools, appends observations, and stops on model completion, budget exhaustion, cancellation, or unrecoverable error.

Rules:

- Enforce max loop iterations.
- Enforce max tool calls per turn.
- Stop deterministically on repeated empty model responses.
- Keep all loop decisions evented for tests.
- Do not hide tool failures; feed failures back to model or stop according to configured policy.

#### Work items

- [ ] Implement loop event model in `src/events.rs`.
  - [ ] Add `OrchestratorEvent` variants for turn started, model requested, model responded, tool started, tool finished, subagent started, subagent finished, budget updated, turn stopped, and error.
  - [ ] Add in-memory event recorder for tests.
  - [ ] Add tests for event serialization.
- [ ] Implement loop engine in `src/loop_engine.rs`.
  - [ ] Add `LoopEngine`.
  - [ ] Build initial transcript from prompt and memory.
  - [ ] Emit thought updates when model returns reasoning.
  - [ ] Emit assistant message updates when model returns text.
  - [ ] Execute model tool intents in deterministic order.
  - [ ] Append tool results to transcript.
  - [ ] Continue loop after tool results when model has not completed.
  - [ ] Stop when model returns completion signal.
  - [ ] Stop when no tool intents and no assistant text are returned twice in a row.
  - [ ] Stop with budget-exceeded error when iteration/tool budgets are exceeded.
  - [ ] Stop promptly on cancellation token.
- [ ] Add loop tests.
  - [ ] Test one-model-response turn emits assistant update and stops.
  - [ ] Test model tool intent executes tool, appends result, and calls model again.
  - [ ] Test tool failure is appended as observation and model can recover.
  - [ ] Test max loop iterations stops before infinite loop.
  - [ ] Test max tool calls stops before unbounded tool use.
  - [ ] Test cancellation stops before next model call.
  - [ ] Test repeated empty responses stop deterministically.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator loop_engine` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator events` passes.
- [ ] Tests prove loop cannot run forever under adversarial fake model responses.

### Phase 5: Task graph and memory store

Goal: add structured task state and bounded memory usable by loop engine and subagents.

Overview: task graph tracks plan items, delegation, dependencies, status, and outputs. Memory store holds bounded per-turn/per-session facts without secrets and feeds compact context back into the model.

Rules:

- Keep task graph deterministic and serializable.
- Keep memory bounded by byte limit.
- Never store secrets or raw terminal output by default.
- Prefer summarized observations over full logs.
- Update `UpdateSink` plan state from task graph changes.

#### Work items

- [ ] Implement task graph in `src/tasks.rs`.
  - [ ] Add `TaskId`.
  - [ ] Add `TaskNode`.
    - [ ] Include title.
    - [ ] Include description.
    - [ ] Include parent id.
    - [ ] Include dependencies.
    - [ ] Include status: pending, running, blocked, completed, failed, cancelled.
    - [ ] Include assigned worker: root or subagent id.
    - [ ] Include bounded result summary.
  - [ ] Add `TaskGraph`.
    - [ ] Add root task creation.
    - [ ] Add child task creation.
    - [ ] Add dependency edges.
    - [ ] Add status transitions with validation.
    - [ ] Add topological ready-task query.
    - [ ] Add completed summary query.
  - [ ] Emit plan updates from task graph state.
- [ ] Implement memory store in `src/memory.rs`.
  - [ ] Add `MemoryItem`.
    - [ ] Include key.
    - [ ] Include value.
    - [ ] Include source task id.
    - [ ] Include byte size.
    - [ ] Include sensitivity flag.
  - [ ] Add `MemoryStore`.
    - [ ] Insert non-sensitive item.
    - [ ] Reject sensitive item by default.
    - [ ] Evict oldest low-priority items when over byte limit.
    - [ ] Query items relevant to active task by key/prefix/source.
    - [ ] Export compact context for model request.
- [ ] Add task graph tests.
  - [ ] Test valid status transitions.
  - [ ] Test invalid transition rejection.
  - [ ] Test dependency ordering.
  - [ ] Test plan update generation from task graph.
- [ ] Add memory tests.
  - [ ] Test insert/query.
  - [ ] Test sensitive item rejection.
  - [ ] Test byte-limit eviction.
  - [ ] Test compact context excludes evicted and sensitive items.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator tasks` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator memory` passes.
- [ ] Tests prove memory stays within configured byte limit.

### Phase 6: Subagent spawning and lifecycle management

Goal: add safe in-process subagent orchestration for decomposition, parallel work, and summarization.

Overview: subagents are logical workers inside the orchestrator, not OS subprocesses by default. They share model/tool abstractions, receive scoped task context, can call allowed tools, and return bounded summaries to parent tasks.

Rules:

- Use logical in-process subagents first; no subprocess spawning in this phase.
- Enforce max subagent depth.
- Enforce max parallel subagents.
- Give each subagent scoped memory and task context.
- Require bounded summary output from every subagent.
- Propagate cancellation from parent to all children.

#### Work items

- [ ] Implement subagent types in `src/subagents.rs`.
  - [ ] Add `SubagentId`.
  - [ ] Add `SubagentRole`.
    - [ ] Include name.
    - [ ] Include instructions.
    - [ ] Include allowed tool classes.
    - [ ] Include max iterations.
  - [ ] Add `SubagentRequest`.
    - [ ] Include parent task id.
    - [ ] Include child task id.
    - [ ] Include role.
    - [ ] Include scoped prompt.
    - [ ] Include context snapshot.
  - [ ] Add `SubagentResult`.
    - [ ] Include status.
    - [ ] Include summary.
    - [ ] Include produced memory items.
    - [ ] Include tool-call count.
    - [ ] Include error summary.
- [ ] Implement subagent manager.
  - [ ] Spawn logical subagent task using same `LoopEngine` with reduced config.
  - [ ] Enforce depth limit before spawn.
  - [ ] Enforce parallelism limit with semaphore.
  - [ ] Apply child-specific tool policy.
  - [ ] Capture child events with parent correlation ids.
  - [ ] Merge child summary into parent transcript.
  - [ ] Merge allowed child memory items into parent memory store.
  - [ ] Cancel child tasks when parent cancellation fires.
- [ ] Add delegation tool integration.
  - [ ] Register built-in `delegate_task` tool with side-effect class `delegate`.
  - [ ] Validate delegation arguments.
  - [ ] Create child task node before spawn.
  - [ ] Mark child task running/completed/failed from subagent result.
  - [ ] Return bounded child summary as tool result.
- [ ] Add subagent tests.
  - [ ] Test delegate tool spawns logical subagent.
  - [ ] Test subagent depth limit denies nested spawn beyond config.
  - [ ] Test parallel subagent limit bounds concurrency.
  - [ ] Test parent cancellation cancels children.
  - [ ] Test child memory merges only non-sensitive items.
  - [ ] Test child failure returns bounded error summary to parent.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator subagents` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator delegate_task` passes.
- [ ] Tests prove subagent depth and parallelism limits cannot be exceeded.

### Phase 7: Budgeting, rate limits, and cancellation propagation

Goal: centralize resource controls for model calls, tool calls, subagents, tokens/bytes where available, and wall-clock time.

Overview: budget tracker rejects new work before it starts, records usage after completion, and produces observable events for tests and future UI display.

Rules:

- Check budget before model call, tool call, and subagent spawn.
- Record budget after every operation.
- Treat missing provider token usage as unknown, not zero, when enforcing token budgets.
- Propagate cancellation to model calls, tools, and subagents.
- Do not use retries to hide failures.

#### Work items

- [ ] Implement budget tracker in `src/budget.rs`.
  - [ ] Add `BudgetConfig`.
    - [ ] Include max model calls.
    - [ ] Include max tool calls.
    - [ ] Include max subagents.
    - [ ] Include max output bytes.
    - [ ] Include optional max input tokens.
    - [ ] Include optional max output tokens.
    - [ ] Include wall-clock deadline.
  - [ ] Add `BudgetSnapshot`.
  - [ ] Add `BudgetTracker`.
    - [ ] Check model call allowance.
    - [ ] Check tool call allowance.
    - [ ] Check subagent allowance.
    - [ ] Check output byte allowance.
    - [ ] Check wall-clock deadline.
    - [ ] Record model usage.
    - [ ] Record tool usage.
    - [ ] Record subagent usage.
  - [ ] Emit budget update events.
- [ ] Integrate budget tracker.
  - [ ] Check before each model adapter call.
  - [ ] Check before each tool executor call.
  - [ ] Check before each subagent spawn.
  - [ ] Stop loop with budget-exceeded error when denied.
  - [ ] Include budget snapshot in model request.
- [ ] Add cancellation propagation tests.
  - [ ] Test cancellation before model call prevents adapter invocation.
  - [ ] Test cancellation during model call resolves turn cancellation.
  - [ ] Test cancellation during tool call resolves tool cancellation.
  - [ ] Test cancellation during subagent run cancels child task.
- [ ] Add budget tests.
  - [ ] Test max model calls enforced.
  - [ ] Test max tool calls enforced.
  - [ ] Test max subagents enforced.
  - [ ] Test output byte budget enforced.
  - [ ] Test wall-clock deadline enforced with paused time where supported.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator budget` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator cancellation` passes.
- [ ] Tests prove budget-denied operations are not invoked.

### Phase 8: ACP server integration adapter

Goal: make `ee-agent-orchestrator` usable as an `ee-acp-agent-server::AgentProvider` with minimal glue.

Overview: this phase adds an adapter that wraps `OrchestratorRuntime` as a provider implementation. Agent binaries can then choose framework-only providers or orchestration-backed providers.

Rules:

- Adapter must implement `AgentProvider` from `ee-acp-agent-server`.
- Adapter must not require OpenRouter-specific code.
- Session lifecycle must initialize orchestrator memory/task state.
- Prompt must delegate to `OrchestratorRuntime::run_turn`.
- Close/cancel must clean orchestrator state.

#### Work items

- [ ] Add provider adapter module.
  - [ ] Implement `OrchestratorProvider<M>` generic over `ModelAdapter`.
  - [ ] Implement `AgentProvider` for `OrchestratorProvider<M>`.
  - [ ] Map provider `info` from adapter config.
  - [ ] Map provider `capabilities` from orchestrator-supported ACP features.
  - [ ] Implement `new_session` by creating session task/memory state.
  - [ ] Implement `load_session` by restoring serialized orchestrator state when provided.
  - [ ] Implement `prompt` by calling `OrchestratorRuntime::run_turn`.
  - [ ] Implement `cancel_session` by cancelling active turn state.
  - [ ] Implement `close_session` by removing task/memory state.
- [ ] Add adapter tests.
  - [ ] Test adapter initialize metadata through `AcpAgentServer` memory transport.
  - [ ] Test adapter session/new creates orchestrator session state.
  - [ ] Test adapter prompt runs loop and emits assistant update.
  - [ ] Test adapter prompt can execute fake tool through `ClientBridge`.
  - [ ] Test adapter cancel stops active orchestrator turn.
  - [ ] Test adapter close removes memory/task state.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator provider_adapter` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator --features test-utils` passes if adapter tests use feature-gated support.
- [ ] Tests prove adapter works through `ee-acp-agent-server` memory transport.

### Phase 9: Provider migration path and orchestrated OpenRouter mode

Goal: let `ee-openrouter-agent` optionally use the orchestrator without removing simple provider mode.

Overview: OpenRouter can first run through `ee-acp-agent-server` directly. This phase adds an orchestrated mode that uses OpenRouter as `ModelAdapter`, enabling general tool loops and future subagents.

Rules:

- Keep non-orchestrated OpenRouter mode available until orchestrated mode has parity.
- Keep external API behavior behind same OpenRouter config and secrets handling.
- Do not send secrets to model or memory store.
- Keep tests network-free with fake HTTP/model adapters.

#### Work items

- [ ] Add `OpenRouterModelAdapter` in `ee-openrouter-agent`.
  - [ ] Convert normalized `ModelRequest` to OpenRouter chat completion request.
  - [ ] Convert OpenRouter text to `ModelResponse` assistant text.
  - [ ] Convert OpenRouter reasoning to `ModelResponse` reasoning.
  - [ ] Convert OpenRouter tool calls to normalized `ToolIntent` values.
  - [ ] Convert model completion/stop reason to normalized completion signal.
- [ ] Add orchestrated mode config.
  - [ ] Add CLI/env flag `OPENROUTER_ORCHESTRATED` or command-line option.
  - [ ] Default to non-orchestrated provider mode until parity tests pass.
  - [ ] Build `OrchestratorProvider<OpenRouterModelAdapter>` when enabled.
- [ ] Add OpenRouter orchestrator tests.
  - [ ] Test normalized model request converts to OpenRouter JSON body.
  - [ ] Test OpenRouter tool call converts to `ToolIntent`.
  - [ ] Test OpenRouter reasoning converts to normalized reasoning.
  - [ ] Test orchestrated mode with fake model executes read-file tool via `ClientBridge`.
  - [ ] Test orchestrated mode respects max tool-call budget.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-openrouter-agent orchestrated` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator` passes.
- [ ] Tests prove orchestrated OpenRouter mode remains network-free under fake adapter.

### Phase 10: Workspace validation and safety guardrails

Goal: validate orchestrator, ACP server framework, and OpenRouter integration without regressing existing host/client behavior.

Overview: final phase adds broader validation and dependency-boundary checks. It ensures the orchestrator remains optional, server-side, and safe under default policies.

Rules:

- Run focused crate tests before workspace summary.
- Use quiet cargo tests only.
- Do not introduce `ee-cli` or `ee-agent-host` dependencies into orchestrator.
- Keep default policy conservative: reads allowed, writes/executes/delegation bounded or denied unless configured.
- Keep all orchestration tests deterministic and network-free.

#### Work items

- [ ] Add dependency-boundary tests.
  - [ ] Assert `ee-agent-orchestrator` does not depend on `ee-agent-host`.
  - [ ] Assert `ee-agent-orchestrator` does not depend on `ee-cli`.
  - [ ] Assert `ee-agent-orchestrator` does depend on `ee-acp-agent-server`.
  - [ ] Assert `ee-agent-orchestrator` public adapter uses `AgentProvider` from `ee-acp-agent-server`.
- [ ] Add default policy regression tests.
  - [ ] Test read tools are available by default.
  - [ ] Test write tools are denied by default.
  - [ ] Test execute tools are denied by default.
  - [ ] Test delegation obeys depth and parallelism defaults.
- [ ] Add deterministic test fixtures.
  - [ ] Add fake model script fixture for simple answer.
  - [ ] Add fake model script fixture for tool call then answer.
  - [ ] Add fake model script fixture for delegation then answer.
  - [ ] Add fake model script fixture for infinite loop attempt.
  - [ ] Add tests that each fixture produces stable event sequence.
- [ ] Run focused validation commands.
  - [ ] Validate format.
  - [ ] Validate clippy for `ee-agent-orchestrator`.
  - [ ] Validate clippy for `ee-acp-agent-server`.
  - [ ] Validate clippy for `ee-openrouter-agent` when orchestrated mode code changes.
  - [ ] Validate orchestrator tests.
  - [ ] Validate ACP server tests.
  - [ ] Validate OpenRouter tests.

#### Actionable criteria

- [ ] `cargo fmt --check` passes.
- [ ] `cargo clippy --quiet -p ee-agent-orchestrator --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo clippy --quiet -p ee-acp-agent-server --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator` passes.
- [ ] `cargo test --quiet -p ee-acp-agent-server` passes.
- [ ] `cargo test --quiet -p ee-openrouter-agent` passes when OpenRouter adapter changes.
- [ ] `./scripts/test-workspace-summary.sh` passes after focused validation.

## Future Agent Orchestrator Feature Backlog

Extend `ee-agent-orchestrator` after the base loop, tool, memory, budget, and subagent framework exists. These features should remain server-side, provider-neutral, deterministic under tests, and optional unless enabled by config or policy.

### Phase 1: Checkpoint, restore, and deterministic replay

Goal: make orchestrated turns inspectable, resumable, and regression-testable.

Overview: persist orchestrator state snapshots and replay event/model/tool fixtures without network, editor UI, or nondeterminism.

Rules:

- Serialize only bounded, secret-conscious state.
- Store provenance for every restored item.
- Use deterministic IDs in replay tests.
- Do not persist raw secrets or unbounded terminal output.
- Replay must not call real model providers or real tools.

#### Work items

- [ ] Implement checkpoint data model.
  - [ ] Add `OrchestratorCheckpoint`.
    - [ ] Store schema version.
    - [ ] Store orchestrator config snapshot.
    - [ ] Store active session id.
    - [ ] Store task graph.
    - [ ] Store memory store.
    - [ ] Store transcript summary.
    - [ ] Store budget snapshot.
    - [ ] Store subagent tree state.
    - [ ] Store deterministic ID generator state.
  - [ ] Add checkpoint schema-version tests.
  - [ ] Add serialization round-trip tests.
- [ ] Implement checkpoint restore.
  - [ ] Validate schema version before restore.
  - [ ] Validate task graph references during restore.
  - [ ] Validate memory byte limits during restore.
  - [ ] Rebuild active runtime state from checkpoint.
  - [ ] Reject checkpoint containing sensitive memory items by default.
  - [ ] Add restore validation tests for invalid references.
  - [ ] Add restore validation tests for over-limit memory.
- [ ] Implement deterministic replay harness.
  - [ ] Add `ReplayScript` fixture type.
    - [ ] Include model responses in order.
    - [ ] Include tool responses in order.
    - [ ] Include expected events.
    - [ ] Include expected final task graph state.
  - [ ] Add replay runner using fake model and fake tools.
  - [ ] Add replay fixture for simple answer.
  - [ ] Add replay fixture for tool call then answer.
  - [ ] Add replay fixture for delegation then answer.
  - [ ] Add replay fixture for infinite-loop attempt.
  - [ ] Assert stable event order for every fixture.
- [ ] Add trace export.
  - [ ] Serialize `OrchestratorEvent` as JSONL.
  - [ ] Include task id, subagent id, tool call id, and budget snapshot where applicable.
  - [ ] Redact sensitive fields before export.
  - [ ] Add tests for JSONL trace export ordering.
  - [ ] Add tests for redaction in exported traces.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator checkpoint` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator replay` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator trace` passes.
- [ ] Tests prove replay never invokes real tools or model providers.

### Phase 2: Strategy selection and structured final responses

Goal: let orchestrator choose deterministic turn strategies and produce consistent user-facing final summaries.

Overview: strategies define loop behavior for simple answers, tool loops, planning, editing, validation, review, and parallel delegation. Final responses use a typed builder instead of provider-specific prose assembly.

Rules:

- Strategy choice must be testable and explainable through events.
- Default strategy must be conservative.
- Strategy must not bypass tool policy, budgets, or cancellation.
- Final response must be built from observed state, not fabricated claims.
- Validation status must only say passed when recorded validation succeeded.

#### Work items

- [ ] Implement strategy types.
  - [ ] Add `TurnStrategy` enum.
    - [ ] Include `SimpleAnswer`.
    - [ ] Include `ToolLoop`.
    - [ ] Include `PlanThenExecute`.
    - [ ] Include `ResearchThenEdit`.
    - [ ] Include `ValidateThenReview`.
    - [ ] Include `ParallelDelegation`.
  - [ ] Add `StrategyDecision`.
    - [ ] Include selected strategy.
    - [ ] Include deterministic reason code.
    - [ ] Include required capabilities.
  - [ ] Add serialization tests for strategy types.
- [ ] Implement strategy selector.
  - [ ] Select `SimpleAnswer` when prompt requires no workspace/tool context.
  - [ ] Select `ToolLoop` when prompt asks for file inspection or tool use.
  - [ ] Select `PlanThenExecute` when prompt asks for implementation over multiple files.
  - [ ] Select `ResearchThenEdit` when prompt asks for unknown codebase change.
  - [ ] Select `ValidateThenReview` when task has code changes and validation tools are available.
  - [ ] Select `ParallelDelegation` only when task graph has independent read-only or disjoint write scopes.
  - [ ] Emit strategy decision event.
  - [ ] Add selector tests for each strategy.
- [ ] Implement strategy execution wrappers.
  - [ ] Make `SimpleAnswer` run one model call with no tool execution.
  - [ ] Make `ToolLoop` run standard loop engine.
  - [ ] Make `PlanThenExecute` require task graph creation before tools.
  - [ ] Make `ResearchThenEdit` run read-only tools before write tools.
  - [ ] Make `ValidateThenReview` run validation and review after edits.
  - [ ] Make `ParallelDelegation` use subagent manager with write-scope checks.
  - [ ] Add tests that each wrapper respects cancellation and budget limits.
- [ ] Implement final response builder.
  - [ ] Add `FinalResponse` data model.
    - [ ] Include changed files.
    - [ ] Include validation commands and outcomes.
    - [ ] Include unresolved risks.
    - [ ] Include follow-up suggestions.
  - [ ] Build final response from task graph, tool results, validation records, and memory provenance.
  - [ ] Prevent claiming validation success without recorded passed command.
  - [ ] Add tests for final response with no code changes.
  - [ ] Add tests for final response with changed files and passing validation.
  - [ ] Add tests for final response with failed validation.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator strategy` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator final_response` passes.
- [ ] Tests prove final responses cannot claim unrecorded validation success.

### Phase 3: Reflection, review, validation, and stuck detection

Goal: add bounded self-review and validation loops that improve quality without creating infinite retries.

Overview: after edits or tool loops, orchestrator can run validation tools, summarize diagnostics, perform one bounded review pass, and stop if no progress is made.

Rules:

- Reflection/review must be bounded by config.
- Validation must route through tool executor and policy.
- Do not retry failed tests blindly.
- Stuck detection must stop loops deterministically.
- Review output must cite observed evidence from tools/state.

#### Work items

- [ ] Implement validation task planner.
  - [ ] Infer validation tools from changed file types and project metadata.
  - [ ] Create validation task nodes in task graph.
  - [ ] Route validation commands through existing tool executor.
  - [ ] Store validation results with command, status, output summary, and timestamp.
  - [ ] Add tests for Rust file validation plan.
  - [ ] Add tests for no validation tools available.
- [ ] Implement reflection pass.
  - [ ] Add `ReflectionConfig`.
    - [ ] Include `enabled`.
    - [ ] Include `max_review_iterations`.
    - [ ] Include `max_fix_iterations`.
  - [ ] Add one model review call after tool/edit loop when enabled.
  - [ ] Feed changed files, diagnostics, validation results, and task state to review model request.
  - [ ] Convert review findings into task graph items.
  - [ ] Allow at most configured fix iterations.
  - [ ] Add tests for one review pass finding issue.
  - [ ] Add tests for review disabled.
- [ ] Implement stuck detection.
  - [ ] Track repeated identical model responses.
  - [ ] Track repeated identical tool calls.
  - [ ] Track repeated failed edit attempts.
  - [ ] Track loop iterations with no task graph state change.
  - [ ] Stop with `Stuck` reason when threshold exceeded.
  - [ ] Add tests for repeated model response stop.
  - [ ] Add tests for repeated tool call stop.
  - [ ] Add tests for repeated failed edit stop.
- [ ] Implement progress scoring.
  - [ ] Add task completion confidence field.
  - [ ] Update confidence from completed tools, validation pass, and review findings.
  - [ ] Prevent final success when required tasks remain failed or blocked.
  - [ ] Add tests for confidence updates.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator validation_planner` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator reflection` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator stuck_detection` passes.
- [ ] Tests prove reflection cannot exceed configured iteration limits.

### Phase 4: Advanced safety policy and prompt-injection resistance

Goal: harden orchestrated tool use, memory, and subagents against unsafe actions and untrusted workspace content.

Overview: orchestrator should distinguish trusted system policy from untrusted file/tool output, redact sensitive data, and gate destructive operations separately.

Rules:

- Treat file contents, terminal output, tool results, and subagent summaries as untrusted.
- Never let untrusted content override developer/system/orchestrator policy.
- Deny destructive operations by default.
- Narrow subagent scopes by default.
- Redact secret-like values before memory or traces.

#### Work items

- [ ] Implement trust labels.
  - [ ] Add `TrustLevel` enum.
    - [ ] Include `SystemPolicy`.
    - [ ] Include `UserPrompt`.
    - [ ] Include `ModelOutput`.
    - [ ] Include `ToolOutputUntrusted`.
    - [ ] Include `SubagentSummaryUntrusted`.
  - [ ] Label every transcript and memory item.
  - [ ] Add tests for trust labels on file/tool outputs.
- [ ] Implement prompt-injection guard.
  - [ ] Wrap untrusted content in model requests with explicit labels.
  - [ ] Add policy reminder that untrusted content cannot modify instructions.
  - [ ] Detect common injection phrases in untrusted tool output.
  - [ ] Emit diagnostic event when suspicious text is detected.
  - [ ] Add tests with file content containing “ignore previous instructions”.
  - [ ] Add tests proving suspicious text does not alter policy decisions.
- [ ] Implement sensitive-data guard.
  - [ ] Detect secret-like keys and token-like values.
  - [ ] Redact sensitive values before memory insertion.
  - [ ] Redact sensitive values before trace export.
  - [ ] Redact sensitive values before final response builder.
  - [ ] Add tests for API key redaction.
  - [ ] Add tests for env-var-like secret redaction.
- [ ] Implement destructive action gate.
  - [ ] Add side-effect subclasses for delete, move, overwrite, chmod-like operations, terminal kill, and external network request.
  - [ ] Deny destructive subclasses by default.
  - [ ] Require explicit policy allowance for destructive subclasses.
  - [ ] Add tests for delete denied by default.
  - [ ] Add tests for overwrite denied without configured allowance.
  - [ ] Add tests for terminal kill denied outside owned terminal scope.
- [ ] Implement workspace scope policy.
  - [ ] Add allowed roots and file glob scopes to task policy.
  - [ ] Narrow subagent scopes from parent scopes.
  - [ ] Reject tool intents outside active scope before client bridge call.
  - [ ] Add tests for root escape rejection.
  - [ ] Add tests for subagent narrowed-scope enforcement.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator trust` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator prompt_injection` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator sensitive_data` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator destructive_policy` passes.
- [ ] Tests prove untrusted tool output cannot change tool policy decisions.

### Phase 5: Tool dependency graph, caching, parallelism, and retries

Goal: make tool execution faster and safer through dependency-aware scheduling, read-only caching, deterministic parallelism, and classified retries.

Overview: orchestrator should avoid duplicate tool calls, parallelize independent read-only work, serialize writes, and retry only transient failures under a small budget.

Rules:

- Cache only read-only tool results by default.
- Invalidate cache entries affected by writes.
- Parallelize only independent read-only tools unless policy allows more.
- Preserve deterministic result ordering after parallel execution.
- Retry only transient errors; never retry policy denials or invalid params.

#### Work items

- [ ] Implement tool dependency graph.
  - [ ] Add `ToolDependency` metadata to `ToolDefinition`.
    - [ ] Include required prior data classes.
    - [ ] Include produced data classes.
    - [ ] Include affected path scope.
  - [ ] Build dependency graph from planned tool intents.
  - [ ] Reject cyclic tool dependencies.
  - [ ] Add tests for dependency ordering.
  - [ ] Add tests for cycle rejection.
- [ ] Implement tool result cache.
  - [ ] Add cache key from tool name, normalized args, session id, and scope.
  - [ ] Store read-only tool results only.
  - [ ] Add TTL or turn-scoped lifetime.
  - [ ] Invalidate path-scoped cache entries on write/edit tool success.
  - [ ] Add tests for read cache hit.
  - [ ] Add tests for write invalidation.
  - [ ] Add tests that write/execute results are not cached.
- [ ] Implement parallel read-only tool execution.
  - [ ] Group independent read-only tool intents.
  - [ ] Run group concurrently under configured parallelism limit.
  - [ ] Collect results in original intent order.
  - [ ] Emit events for each started/completed tool.
  - [ ] Add tests for concurrent execution with deterministic final ordering.
  - [ ] Add tests proving write tools are serialized.
- [ ] Implement retry classifier.
  - [ ] Add `RetryPolicy`.
    - [ ] Include max retries.
    - [ ] Include transient error classes.
    - [ ] Include backoff strategy using testable clock.
  - [ ] Classify timeout, rate-limit, and temporary I/O as transient.
  - [ ] Classify invalid params, policy denial, and permission denial as permanent.
  - [ ] Add tests for transient retry.
  - [ ] Add tests for permanent no-retry.
  - [ ] Add tests for retry budget exhaustion.
- [ ] Implement tool schema compiler.
  - [ ] Generate provider-facing tool schemas from `ToolDefinition`.
  - [ ] Validate generated schemas include names, descriptions, required fields, and side-effect metadata.
  - [ ] Add snapshot tests for built-in tool schemas.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator tool_dependencies` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator tool_cache` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator parallel_tools` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator retries` passes.
- [ ] Tests prove policy denials are never retried.

### Phase 6: Subagent roles, verification, fan-out/fan-in, and conflict detection

Goal: make delegation useful and safe through built-in roles, bounded fan-out/fan-in workflows, output verification, and conflict handling.

Overview: subagents should get scoped role instructions and budgets, produce evidence-backed summaries, and merge results only after parent verification.

Rules:

- Built-in roles must be tool-scope limited.
- Subagent summaries must be bounded and cite evidence.
- Parent must verify before merging child memory into durable memory.
- Concurrent subagents must not edit overlapping files.
- Failed subagents must be quarantined from parent memory by default.

#### Work items

- [ ] Implement subagent role library.
  - [ ] Add built-in `researcher` role.
    - [ ] Allow read-only tools.
    - [ ] Deny writes and executes.
  - [ ] Add built-in `code_reader` role.
    - [ ] Allow file/search/symbol tools.
    - [ ] Deny writes and executes.
  - [ ] Add built-in `implementer` role.
    - [ ] Allow writes only within assigned file scopes.
    - [ ] Deny terminal execution by default.
  - [ ] Add built-in `test_runner` role.
    - [ ] Allow configured validation tools.
    - [ ] Deny file writes.
  - [ ] Add built-in `reviewer` role.
    - [ ] Allow read-only and diagnostics tools.
    - [ ] Deny writes.
  - [ ] Add built-in `summarizer` role.
    - [ ] Deny all tools by default.
  - [ ] Add tests for default tool scopes of every role.
- [ ] Implement fan-out/fan-in coordinator.
  - [ ] Split ready independent tasks into subagent requests.
  - [ ] Enforce max parallel subagents.
  - [ ] Collect child summaries in deterministic task order.
  - [ ] Merge completed summaries into parent transcript.
  - [ ] Mark parent task blocked if required child fails.
  - [ ] Add tests for parallel fan-out deterministic merge.
  - [ ] Add tests for child failure blocking parent task.
- [ ] Implement subagent result verifier.
  - [ ] Require child summary to include cited files/tools when role requires evidence.
  - [ ] Check cited files/tools exist in child event log.
  - [ ] Reject child memory merge when citations are missing.
  - [ ] Add tests for valid cited summary.
  - [ ] Add tests for missing citation rejection.
- [ ] Implement subagent quarantine.
  - [ ] Store failed child output in quarantine state.
  - [ ] Exclude quarantined output from normal memory context.
  - [ ] Allow parent model to inspect bounded quarantine summary.
  - [ ] Add tests that failed child memory is not merged.
- [ ] Implement write-scope conflict detector.
  - [ ] Track intended file scopes per subagent.
  - [ ] Reject overlapping write scopes for concurrent subagents.
  - [ ] Lock file scopes during active write task.
  - [ ] Release locks after task completion/cancellation.
  - [ ] Add tests for overlapping file conflict.
  - [ ] Add tests for disjoint file scopes running concurrently.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator subagent_roles` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator fanout_fanin` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator subagent_verifier` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator write_conflicts` passes.
- [ ] Tests prove failed subagent memory is quarantined by default.

### Phase 7: Advanced planning, task compilation, and issue integration

Goal: convert vague model plans into executable task graphs and keep project issue checklists synchronized when automated criteria pass.

Overview: model plans are parsed into typed tasks with dependencies, scopes, and validation criteria. Completed automated criteria can update checklists like `ISSUES.md` through approved edit tools.

Rules:

- Reject vague task graph nodes without actionable criteria.
- Validate dependencies before execution.
- Do not update issue checklists unless criteria have recorded passing evidence.
- Use approved file-edit tools for issue updates.
- Keep issue integration optional and scoped to configured files.

#### Work items

- [ ] Implement plan compiler.
  - [ ] Parse model plan items into `TaskNode` values.
  - [ ] Require task title, action, scope, and expected result.
  - [ ] Infer dependencies from explicit model output.
  - [ ] Reject cyclic dependencies.
  - [ ] Reject tasks without executable action or verification criteria.
  - [ ] Add tests for valid plan compilation.
  - [ ] Add tests for vague task rejection.
  - [ ] Add tests for dependency cycle rejection.
- [ ] Implement task readiness scoring.
  - [ ] Mark tasks ready only when dependencies complete.
  - [ ] Mark tasks blocked when dependency fails.
  - [ ] Compute progress percentage from task graph status.
  - [ ] Add tests for progress scoring.
- [ ] Implement milestone summaries.
  - [ ] Generate bounded summary after configured number of events or completed tasks.
  - [ ] Store summary in memory with provenance.
  - [ ] Drop low-value raw observations after summary when memory pressure exists.
  - [ ] Add tests for milestone summary creation.
  - [ ] Add tests for compaction under memory pressure.
- [ ] Implement issue checklist integration.
  - [ ] Parse markdown checklist items from configured issue files.
  - [ ] Match completed task criteria to checklist items by stable text or configured key.
  - [ ] Require recorded passing validation before marking item complete.
  - [ ] Use write/edit tool path to update checklist text.
  - [ ] Add tests for checklist parse.
  - [ ] Add tests for marking item only after criteria pass.
  - [ ] Add tests that failed criteria do not mark item complete.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator plan_compiler` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator progress` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator milestones` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator issue_integration` passes.
- [ ] Tests prove vague tasks are rejected before execution.

### Phase 8: Context pack builder and memory provenance

Goal: feed models compact, relevant, provenance-rich context without repeated codebase probing or unsafe memory growth.

Overview: context packs combine task state, relevant memory, recent tool outputs, file references, policy reminders, and budget summaries into deterministic model input.

Rules:

- Context packs must fit configured byte budget.
- Every included fact must carry provenance.
- Prefer summaries over raw large outputs.
- Keep untrusted content labeled.
- Never include sensitive memory items.

#### Work items

- [ ] Implement context pack model.
  - [ ] Add `ContextPack`.
    - [ ] Include active task summary.
    - [ ] Include relevant memory items.
    - [ ] Include recent tool summaries.
    - [ ] Include file references.
    - [ ] Include policy reminders.
    - [ ] Include budget snapshot.
    - [ ] Include truncation metadata.
  - [ ] Add `ContextItemProvenance`.
    - [ ] Include source kind.
    - [ ] Include source id.
    - [ ] Include optional file path/range.
    - [ ] Include trust label.
- [ ] Implement context pack builder.
  - [ ] Score memory relevance by task id, key match, source recency, and explicit dependency.
  - [ ] Include policy reminders before untrusted content.
  - [ ] Include newest high-value tool summaries.
  - [ ] Exclude sensitive items.
  - [ ] Enforce byte budget with deterministic truncation.
  - [ ] Add tests for relevance ordering.
  - [ ] Add tests for byte-budget truncation.
  - [ ] Add tests for sensitive exclusion.
- [ ] Implement memory compaction and decay.
  - [ ] Merge repeated facts with same key and compatible provenance.
  - [ ] Decay low-value stale observations.
  - [ ] Preserve decisions, constraints, and validation results.
  - [ ] Add tests for repeated fact merge.
  - [ ] Add tests for preserving decisions during compaction.
- [ ] Add optional semantic memory adapter trait.
  - [ ] Define trait for external vector/index lookup without adding required embedding dependency.
  - [ ] Add fake semantic adapter for tests.
  - [ ] Merge semantic results into context pack with provenance.
  - [ ] Add tests for adapter disabled behavior.
  - [ ] Add tests for fake adapter result inclusion.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator context_pack` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator memory_compaction` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator semantic_memory` passes.
- [ ] Tests prove context packs stay within byte budget and preserve provenance.

### Phase 9: Provider routing, streaming, and rate limits

Goal: support multiple model adapters, streaming updates, provider rate limits, and tool-call dialect normalization.

Overview: orchestrator can route tasks to different model adapters, normalize provider-specific tool call formats, stream partial text/reasoning, and enforce shared provider rate limits.

Rules:

- Routing decisions must be deterministic and evented.
- Rate limits must apply across subagents sharing the same provider.
- Streaming must preserve final transcript consistency.
- Provider dialects must normalize into `ModelResponse` and `ToolIntent`.
- Tests must not use network.

#### Work items

- [ ] Implement model router.
  - [ ] Add `ModelRoute`.
    - [ ] Include route id.
    - [ ] Include model adapter id.
    - [ ] Include task kind constraints.
    - [ ] Include cost/strength tier.
  - [ ] Route simple summaries to cheap adapter.
  - [ ] Route implementation/review tasks to strong adapter when configured.
  - [ ] Route subagent roles to role-specific adapters when configured.
  - [ ] Add tests for deterministic route selection.
- [ ] Implement rate-limit adapter.
  - [ ] Add provider-level semaphore/concurrency limit.
  - [ ] Add request-per-window limiter using testable clock.
  - [ ] Queue model calls when allowed by timeout budget.
  - [ ] Fail fast when queue wait would exceed turn deadline.
  - [ ] Add tests for concurrency limit.
  - [ ] Add tests for per-window limit with paused time.
  - [ ] Add tests for deadline-aware fail-fast behavior.
- [ ] Implement streaming model support.
  - [ ] Add streaming callback/event type for partial text.
  - [ ] Add streaming callback/event type for partial reasoning.
  - [ ] Merge streamed chunks into final transcript message.
  - [ ] Emit ACP updates through `UpdateSink` as chunks arrive.
  - [ ] Add tests for streamed text chunk ordering.
  - [ ] Add tests for streamed reasoning chunk ordering.
  - [ ] Add tests for stream cancellation.
- [ ] Implement tool-call dialect adapters.
  - [ ] Add OpenAI/OpenRouter-style function call normalization.
  - [ ] Add Anthropic-style tool use normalization.
  - [ ] Add local-model JSON tool-call normalization.
  - [ ] Reject malformed tool-call dialect payloads with model error.
  - [ ] Add fixtures and tests for each dialect.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator model_router` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator rate_limit` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator streaming` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator dialects` passes.
- [ ] Tests prove shared provider rate limits apply across subagents.

### Phase 10: Observability, metrics, and workspace validation

Goal: expose structured observability for debugging and enforce dependency/policy guardrails across the workspace.

Overview: metrics summarize model/tool/subagent usage and default policy behavior. Workspace validation ensures orchestrator remains optional, deterministic, and safely bounded.

Rules:

- Metrics must not include sensitive content.
- Decision logs must record reason codes, not hidden chain-of-thought.
- Tests must be deterministic and quiet.
- Orchestrator must remain independent from `ee-agent-host` and `ee-cli`.
- Run focused validation before workspace summary.

#### Work items

- [ ] Implement metrics model.
  - [ ] Count model calls.
  - [ ] Count tool calls by side-effect class.
  - [ ] Count subagent spawns by role.
  - [ ] Count cancellations.
  - [ ] Count denied policy actions.
  - [ ] Count budget-exceeded stops.
  - [ ] Count bytes/tokens where known.
  - [ ] Add tests for metrics increments.
- [ ] Implement decision log.
  - [ ] Record strategy decisions with reason codes.
  - [ ] Record tool policy decisions with reason codes.
  - [ ] Record routing decisions with reason codes.
  - [ ] Record subagent delegation decisions with reason codes.
  - [ ] Exclude hidden chain-of-thought and sensitive content.
  - [ ] Add tests for decision log redaction.
- [ ] Add dependency-boundary checks.
  - [ ] Assert `ee-agent-orchestrator` does not depend on `ee-agent-host`.
  - [ ] Assert `ee-agent-orchestrator` does not depend on `ee-cli`.
  - [ ] Assert orchestrator examples/tests remain network-free by default.
- [ ] Add default-safety regression suite.
  - [ ] Test writes denied by default.
  - [ ] Test executes denied by default.
  - [ ] Test destructive operations denied by default.
  - [ ] Test subagent depth limit default.
  - [ ] Test memory byte limit default.
  - [ ] Test prompt-injection guard enabled by default.
- [ ] Run focused validation commands.
  - [ ] Validate format.
  - [ ] Validate orchestrator clippy.
  - [ ] Validate orchestrator tests.
  - [ ] Validate ACP server framework tests when adapter APIs change.
  - [ ] Validate OpenRouter tests when model adapter code changes.

#### Actionable criteria

- [ ] `cargo test --quiet -p ee-agent-orchestrator metrics` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator decision_log` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator default_safety` passes.
- [ ] `cargo clippy --quiet -p ee-agent-orchestrator --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test --quiet -p ee-agent-orchestrator` passes.
- [ ] `./scripts/test-workspace-summary.sh` passes after focused validation.
