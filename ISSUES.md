# ISSUES

## Optional Agents Tooling Plan: ACP v1 + MCP 2026-07-28

Agents tooling stays optional, compile-time and runtime gated behind `agents`. ACP v1 remains the agent session protocol, and MCP `2026-07-28` remains the tool transport for the ee proxy. New tools should extend the existing `ee_*` MCP proxy surface in `crates/ee-mcp`, route execution through `ee-agent-host` handlers when editor state or approvals are needed, and avoid adding ee-owned ACP wire structs unless the official ACP SDK already defines the method.

Tooling goals:

- [ ] Give LLMs enough editor-native context to avoid terminal probing and path guessing.
- [x] Keep read-only discovery cheap, bounded, and approval-free.
- [x] Route every write, terminal execution, and code action through the existing approval path.
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

- [x] Add `ee_workspace_roots`.
  - [x] Return configured worktree roots, active root, active file, and optional additional directories advertised to the session.
  - [x] Never return environment values or secret config.
  - [x] Validate roots as canonical absolute paths before exposing them.
- [x] Add `ee_list_directory`.
  - [x] Accept `path` only.
  - [x] Return one directory level with entries containing `path`, `kind`, and `size`.
  - [x] Hide hidden/ignored entries by default.
  - [x] Reject paths outside allowed roots.
  - [x] Cap result count with host default.
- [x] Add `ee_list_directory_all` only if hidden/ignored listing is needed.
  - [x] Accept `path` only.
  - [x] Return one directory level including hidden/ignored entries with flags.
- [x] Add `ee_search_files`.
  - [x] Accept `pattern` only.
  - [x] Search allowed roots using glob/path matching only, not content search.
  - [x] Respect project ignore rules.
  - [x] Cap results with host default.
- [x] Add `ee_search_files_all` only if hidden/ignored file search is needed.
  - [x] Accept `pattern` only.
  - [x] Include hidden/ignored files and mark them in results.
- [x] Add `ee_search_text`.
  - [x] Accept `query` only.
  - [x] Perform literal, case-sensitive search across allowed roots.
  - [x] Return bounded matches with file, 1-based line, and short context.
- [x] Add `ee_search_text_regex` only if regex search is needed.
  - [x] Accept `pattern` only.
  - [x] Enforce regex safety, time limits, and result caps.
- [x] Add `ee_search_text_in_files` only if scoped content search is needed.
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

- [x] Add `ee_replace_text`.
  - [x] Accept `path`, `old_text`, and `new_text` only.
  - [x] Require exactly one match for `old_text`.
  - [x] Apply through existing buffer/edit/save semantics, not raw disk writes.
  - [x] Require approval before mutating any buffer or file.
- [x] Add `ee_apply_patch` only if multi-edit patches are needed.
  - [x] Accept `path` and `edits` only.
  - [x] Each edit must use the same simple `old_text`/`new_text` shape.
  - [x] Reject range-based, hunk-based, or mixed edit shapes; add a separate tool later if needed.
- [x] Add `ee_create_text_file`.
  - [x] Accept `path` and `content` only.
  - [x] Fail if file exists.
- [x] Add `ee_overwrite_text_file` only if overwrite is needed.
  - [x] Accept `path` and `content` only.
  - [x] Require approval and clearly report existing file replacement.
- [x] Add `ee_read_buffer`.
  - [x] Accept `path` only.
  - [x] Read current editor buffer contents, including unsaved changes.
  - [x] Fall back to file read only when no buffer is open and policy allows.
- [x] Add `ee_read_buffer_lines` only if line-window reads are needed.
  - [x] Accept `path`, `line`, and `limit` only.
- [x] Add `ee_open_buffers`.
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

- [x] Add `ee_get_diagnostics`.
  - [x] Accept no arguments.
  - [x] Return bounded LSP/editor diagnostics for the current workspace with path, range, severity, source, code, and message.
  - [x] Keep separate from current `ee_diagnostics`, which remains recent proxy/host diagnostic text.
- [x] Add `ee_get_file_diagnostics`.
  - [x] Accept `path` only.
  - [x] Return bounded LSP/editor diagnostics for one file.
- [x] Add `ee_document_symbols`.
  - [x] Accept `path`.
  - [x] Return LSP document symbols with name, kind, range, selection range, and container path.
- [x] Add `ee_references`.
  - [x] Accept `path`, `line`, and `character` only.
  - [x] Return bounded LSP references as absolute paths and 1-based ranges.
- [x] Add `ee_list_code_actions`.
  - [x] Accept `path`, `line`, and `character` only.
  - [x] Return available actions with simple `action_id`, title, and kind.
  - [x] Keep listing read-only.
- [x] Add `ee_apply_code_action`.
  - [x] Accept `path` and `action_id` only.
  - [x] Require approval and use buffer edit semantics.
- [x] Add `ee_format_file`.
  - [x] Accept `path`.
  - [x] Run configured formatter or LSP formatting.
  - [x] Require approval if it changes the buffer.
- [x] Add `ee_preview_rename_symbol`.
  - [x] Accept `path`, `line`, `character`, and `new_name` only.
  - [x] Return planned workspace edits without applying them.
- [x] Add `ee_rename_symbol`.
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

- [x] Add `ee_terminal_output`.
  - [x] Accept `terminal_id` only.
  - [x] Return recent bounded stdout/stderr chunks with sequence ids and truncation flags.
- [x] Add `ee_terminal_output_since` only if incremental polling is needed.
  - [x] Accept `terminal_id` and `since_seq` only.
- [x] Add `ee_terminal_wait`.
  - [x] Accept `terminal_id` only.
  - [x] Use host default timeout.
  - [x] Return exit status when complete or timeout state when still running.
- [x] Add `ee_terminal_wait_long` only if longer waits are needed.
  - [x] Accept `terminal_id` and `timeout_ms` only.
- [x] Add `ee_terminal_kill`.
  - [x] Accept `terminal_id`.
  - [x] Terminate only terminals owned by the current agent/session unless policy explicitly allows more.
- [x] Add `ee_terminal_release`.
  - [x] Accept `terminal_id`.
  - [x] Release host resources and close retained output.


#### Implementation notes

- [x] Reuse existing ACP-side terminal request types already present in `ee-agent-host`.
- [x] Preserve command approval for `terminal_create`.
- [x] Add ownership tracking so agents cannot read or kill user terminals by guessing ids.
- [x] Keep terminal output redaction and byte caps consistent with current diagnostics redaction.

#### Exit criteria

- [x] LLM can start, poll, wait for, kill, and release command executions through structured tools.
- [x] Long-running commands cannot hang the host or leak unbounded output.

### Phase 5: Git and review context tools

Add read-only source-control tools that support review and final self-checks without shelling out.

#### Tools

- [x] Add `ee_git_status`.
  - [x] Return branch, detached state, staged/unstaged/untracked files, and conflict state.
  - [x] Keep read-only and bounded.
- [x] Add `ee_git_diff`.
  - [x] Accept no arguments.
  - [x] Return bounded unstaged unified diff plus truncation metadata.
- [x] Add `ee_git_diff_file`.
  - [x] Accept `path` only.
  - [x] Return bounded unstaged unified diff for one file.
- [ ] Add `ee_git_diff_staged` only if staged diff is needed.
  - [ ] Accept no arguments.
  - [ ] Return bounded staged unified diff.
- [x] Add `ee_changed_files`.
  - [x] Return editor/SCM changed files with dirty-buffer state and saved state.
- [x] Add `ee_review_context`.
  - [x] Return changed files, relevant diagnostics, nearby symbols, and configured test/task suggestions.
  - [x] Never run tests or commands by itself.

#### Implementation notes

- [x] Prefer library or existing editor SCM integration over shell commands where available.
- [x] Treat repository paths as canonical identities.
- [x] Do not invent local path-normalization helpers for cache keys or persisted ids.
- [x] Redact credentials from remote URLs and command diagnostics.

#### Exit criteria

- [x] LLM can summarize changes, inspect diffs, and identify obvious validation tasks from editor-provided context.

### Phase 6: Project memory and instructions tools

Expose project guidance and bounded session context so agents follow repo rules without repeatedly scanning files.

#### Tools

- [x] Add `ee_project_instructions`.
  - [x] Return applicable `AGENTS.md`, `RULE.md`, workspace config rules, and tool-use constraints for the current root.
  - [x] Include source paths and precedence order.
- [x] Add `ee_save_note`.
  - [x] Accept `key` and `content` only.
  - [x] Store non-secret, session-scoped notes for long-running tasks.
- [x] Add `ee_read_notes`.
  - [x] Accept no arguments.
  - [x] Return bounded notes for the current agent/session only.
- [x] Add `ee_read_note`.
  - [x] Accept `key` only.
  - [x] Return one bounded note for the current agent/session.
- [x] Add `ee_file_dependency_map`.
  - [x] Accept `path` only.
  - [x] Return known file dependency edges when an index exists.
  - [x] Fail gracefully when no graph/index is available.
- [ ] Add `ee_symbol_dependency_map` for bounded symbol-scoped graph lookup.
  - [ ] Accept `path`, `line`, and `character` only.
  - [ ] Return resolved symbol, definition, callers, callees, implementations, tests, ownership/module hints, and capped related files.
  - [ ] Return graph freshness, graph version, result totals, and truncation metadata.
  - [ ] Fail closed with a typed unavailable/stale-index error when graph lookup cannot produce trustworthy results.

#### Implementation notes

- [x] Never store secrets, environment values, tokens, or raw terminal output in notes.
- [x] Keep notes session-scoped by default.
- [x] Require explicit user opt-in before workspace persistence.
- [x] Mark freshness in graph-backed responses when data is stale.

#### Exit criteria

- [x] LLM can retrieve current project rules and task memory through structured, bounded tools.
- [x] Knowledge tools degrade safely when no index or saved context exists.

### Phase 7: Tool governance, schemas, and compatibility

Harden the expanded tool surface before enabling it by default.

#### Work items

- [x] Add versioned `ee_tools_manifest` from one source of truth.
  - [x] Accept no arguments.
  - [x] Return tool names, schema versions, transport availability, and capability requirements.
  - [x] Return side-effect class: `read`, `write`, or `execute`.
  - [x] Return approval requirement, output caps, redaction rules, and typed error classes.
  - [x] Return short examples using minimal arguments for each tool.
  - [x] Return deprecation and replacement metadata before retiring a tool or schema version.
  - [x] Derive README/crate documentation and compatibility snapshots from manifest data where practical.
- [x] Keep existing tool names stable after the `ee_` compatibility rename.
- [x] Add new names rather than changing schemas incompatibly.
- [x] Document every tool argument, limit, error shape, approval behavior, and redaction rule in README and crate docs.
- [x] Document the rule that complicated arguments mean the tool should be split into smaller tools.
- [x] Add capability flags so hosts can advertise partial implementation without pretending unsupported tools exist.
- [x] Add integration tests for MCP stdio proxy path for each tool class.
- [x] Add integration tests for ACP-native MCP-over-ACP path for each tool class.
- [x] Snapshot tool-list and schema output to detect unintended compatibility changes.
- [x] Add property/fuzz tests for malformed arguments, argument caps, and output-cap boundaries.
- [x] Add security tests for path traversal.
- [x] Add security tests for symlink escape.
- [x] Add security tests for oversized inputs.
- [x] Add security tests for secret-like env keys.
- [x] Add security tests for stale revisions.
- [x] Add security tests for terminal ownership.
- [x] Add security tests for output truncation.
- [x] Add security tests that manifest claims match actual tool availability, approval routing, and policy enforcement.

#### Exit criteria

- [x] Expanded tools work through both stdio MCP proxy and ACP-native MCP-over-ACP.
- [x] Unsupported or disabled tools fail closed with clear tool-level errors.
- [x] Tool list is discoverable, versioned, and safe for LLM clients to cache within a session.
- [x] Every advertised tool has compatible schemas, explicit policy metadata, and conformance coverage on every supported transport.

### Phase 8: Replayable LLM harness evaluation

Build deterministic task fixtures and replay infrastructure before expanding agent behavior further. Use results to compare models, prompts, routing, tool schemas, and transports with evidence instead of subjective impressions.

#### Work items

- [x] Add versioned task fixtures covering bug fixes, features, refactors, code review, investigation, and multi-file changes.
- [x] Add fixtures for dirty buffers, stale revisions, write conflicts, interrupted sessions, recovery, denied approvals, and unavailable capabilities.
- [x] Add adversarial fixtures for prompt injection in repository content, secrets/redaction, path escape, and unsafe terminal requests.
- [x] Record deterministic model/tool traces with redacted workspace snapshots and stable fixture inputs.
- [x] Score task completion, validation success, policy violations, recovery success, diff size, approval count, tool calls, model calls, latency, and estimated token/cost usage.
- [x] Compare fixture results across model/provider versions, prompt versions, routing configurations, and MCP transports.
- [x] Define pass/fail regression thresholds before changing default model, prompt, tool, or policy behavior.
- [x] Keep fixtures hermetic: no network, secret, user-home, or mutable global-state dependency.

#### Exit criteria

- [x] Replaying same fixture produces stable evidence suitable for regression comparison.
- [x] CI reports harness quality regressions with failing task, score delta, and redacted trace reference.
- [x] Model or prompt changes cannot become defaults without baseline comparison against required fixtures.

### Phase 9: Evidence-gated completion and validation

Make agent completion state derive from recorded tool evidence, never model assertion alone.

#### Work items

- [x] Define explicit terminal states: `verified`, `partially_verified`, `blocked`, and `unverified`.
- [x] Require changed-file inventory, post-write diagnostics, final diff review, and selected validation result before marking work `verified`.
- [x] Build dynamic validation plans from changed files, symbols, workspace configuration, and declared project tasks.
- [x] Return structured validation evidence: command or tool, exit status, elapsed time, affected tests, diagnostics delta, output truncation, and skip reason.
- [x] Require final responses to cite evidence ids or structured results for claimed builds, tests, formatting, and diagnostics.
- [x] Keep `partially_verified` when validation cannot run; include exact blocker and safe user follow-up.
- [x] Prevent reflection/review text from overriding missing, failed, stale, or denied tool evidence.
- [x] Add regression tests for false-success, stale-diagnostics, skipped-validation, and failed-test completion paths.

#### Exit criteria

- [x] Agent cannot report successful validation without matching recorded evidence.
- [x] Final status clearly distinguishes verified work from blocked or unverified work.
- [x] Failed or unavailable validation produces actionable next steps without hiding uncertainty.

### Phase 10: Task-aware context planning

Select smallest fresh context set needed for current task. Prefer editor and graph evidence over broad repository dumps.

#### Work items

- [x] Add task-aware context planner that prioritizes project instructions, active selection, dirty buffers, diagnostics, git diff, symbol neighborhood, relevant tests, and related config/docs.
- [x] Label each context item with source, revision/freshness, trust level, token cost, selection reason, and truncation reason.
- [x] Treat repository text, terminal output, external tool output, and user-provided content as separate trust classes.
- [x] Prefer bounded excerpts and explicit drill-down tools over whole-file or whole-repository injection.
- [x] Invalidate or refresh context after writes, buffer revisions, diagnostics updates, graph changes, and checkout changes.
- [x] Cache only context with compatible revision, policy, and session identity.
- [x] Add tests proving task planner selects buffer and diagnostics context before terminal probing when editor state exists.

#### Exit criteria

- [x] Planning receives sufficient fresh context without unbounded repository reads.
- [x] Agent can explain source and freshness of context that informed an edit or conclusion.
- [x] Untrusted repository content cannot silently become trusted instruction context.

### Phase 11: Auditable write transactions

Strengthen existing buffer-aware edits with explicit transaction evidence and safe recovery.

#### Work items

- [x] Assign transaction id, expected source revision, changed paths, approval result, and post-write revision to each mutation sequence.
- [x] Enforce sequence: read revision, preview, approval, apply, diagnostics, final diff, selected validation, terminal state.
- [x] Detect stale, ambiguous, or conflicting revisions before apply; never overwrite dirty user buffers silently.
- [x] Record diagnostics delta and validation evidence against transaction id.
- [x] Reopen work as `blocked` or `unverified` after stale state or diagnostic regression instead of broad automatic repair.
- [x] Allow rollback only for agent-owned transaction changes with verified revisions and explicit safety checks.
- [x] Add integration tests for concurrent user edits, approval denial, partial multi-file failures, diagnostics regressions, and recovery after interruption.

#### Exit criteria

- [x] Every write can be traced from read revision through final verification state.
- [x] Conflict or stale-state failures preserve user work and return actionable structured errors.
- [x] Agent cannot claim final verification against pre-write diagnostics or diff state.

### Phase 12: Targeted test and command intelligence

Choose smallest appropriate validation action from declared project knowledge. Preserve approval and command policy boundaries.

#### Work items

- [ ] Add workspace-declared test/task metadata with stable command ids, scopes, prerequisites, and approval class.
- [ ] Map changed files and resolved symbols to likely targeted tests before escalating to broader validation.
- [ ] Return structured command results with command id, exit status, elapsed time, test ids, diagnostics, redaction, and truncation metadata.
- [ ] Retry only explicitly classified transient failures with bounded attempt count and recorded reason.
- [ ] Distinguish command failure, timeout, cancellation, policy denial, missing dependency, and unavailable environment.
- [ ] Keep shell allow-lists, user approval, cancellation, and output caps mandatory for every generated validation command.
- [ ] Add tests for target selection, escalation, transient retry limits, and unsafe command rejection.

#### Exit criteria

- [ ] Agent chooses focused validation when reliable target metadata exists.
- [ ] Validation escalation is explicit, bounded, and justified by changed scope or prior result.
- [ ] Command results provide enough evidence for Phase 9 completion states.

### Phase 13: Subagent delegation quality controls

Use subagents only when independent work produces more value than coordination cost. Keep root agent responsible for final synthesis and writes.

#### Work items

- [ ] Estimate expected information gain, token/cost budget, and write-conflict risk before delegation.
- [ ] Restrict parallel work to independent scopes; assign one write owner per file or module.
- [ ] Define role-specific response schema: findings, cited evidence, confidence, rejected alternatives, and recommended next action.
- [ ] Require verifier roles to reject uncited claims and separate observed facts from inference.
- [ ] Require root agent to reconcile contradictory findings before selecting plan or applying edits.
- [ ] Enforce bounded recursion, per-subagent budget, cancellation propagation, and deterministic quarantine for failed delegates.
- [ ] Record delegation effectiveness in replay metrics: useful findings, duplicate work, conflicts, latency, and cost.

#### Exit criteria

- [ ] Parallel delegation cannot produce overlapping unattended writes.
- [ ] Final agent conclusions identify supporting evidence and resolved disagreements.
- [ ] Evaluation data can show when a subagent role improves or degrades task quality.

### Phase 14: Privacy-safe harness observability

Measure reliability, safety, and efficiency without storing workspace secrets or raw sensitive content.

#### Work items

- [ ] Emit redacted per-turn waterfall events for model routing, tool execution, approval, retry, recovery, and validation.
- [ ] Attribute runs to model/provider, prompt, manifest/schema, policy, and routing versions.
- [ ] Aggregate tool failures by typed reason: invalid input, policy denial, stale state, timeout, transport failure, unavailable capability, or internal error.
- [ ] Export local redacted JSONL or equivalent stable format for evaluation and debugging.
- [ ] Link failed runs to replay fixture candidates and redacted evidence artifacts.
- [ ] Track quality, latency, approvals, tool calls, model calls, and estimated cost without logging secrets, raw prompts, or raw terminal output by default.
- [ ] Add retention limits, user controls, and tests for redaction before persistence or export.

#### Exit criteria

- [ ] Maintainers can identify quality, latency, cost, and policy regressions by versioned configuration.
- [ ] Observability artifacts remain useful for replay while preserving workspace privacy boundaries.
- [ ] No raw secret, token, environment value, or unapproved workspace content enters telemetry by default.

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

- [x] Add `crates/ee-acp-agent-server` to the workspace members in `ee/Cargo.toml`.
  - [x] Create `crates/ee-acp-agent-server/Cargo.toml`.
    - [x] Set package name to `ee-acp-agent-server`.
    - [x] Set version to `0.1.0`.
    - [x] Use workspace `edition`, `rust-version`, `license`, and author conventions.
    - [x] Add dependencies on `ee-agent-protocol`, `futures`, `serde`, `serde_json`, `tokio`, and `tracing` from workspace dependencies.
    - [x] Add `tempfile` as dev-dependency only if tests need filesystem fixtures.  (not needed — tests use in-memory buffers)
  - [x] Create `crates/ee-acp-agent-server/src/lib.rs`.
    - [x] Export `config`, `error`, `transport`, `provider`, `server`, `session`, `updates`, `client`, `ids`, and `validate` modules.
    - [x] Re-export primary public types from crate root.
    - [x] Add crate-level docs stating this crate is ACP agent-side only.
- [x] Implement framework config in `src/config.rs`.
  - [x] Add `AcpAgentServerConfig`.
    - [x] Include `request_timeout: Duration`.
    - [x] Include `prompt_timeout: Option<Duration>`.
    - [x] Include `max_frame_bytes: usize`.
    - [x] Include `session_id_prefix: String`.
    - [x] Include `implementation: ee_agent_protocol::Implementation`.
  - [x] Implement `Default`.
    - [x] Set request timeout to `30s`.
    - [x] Set prompt timeout to `None`.
    - [x] Set max frame bytes to `4 * 1024 * 1024`.
    - [x] Set session id prefix to `session`.
    - [x] Set implementation name/title to framework defaults.
  - [x] Add unit tests for default values.
- [x] Implement framework errors in `src/error.rs`.
  - [x] Add `AcpServerError` variants for I/O, JSON parse, protocol, unsupported version, unknown session, request timeout, transport closed, and provider errors.
  - [x] Add `ProviderError` variants for invalid request, backend failure, cancellation, client request failure, and permission denied.
  - [x] Implement `Display` and `std::error::Error`.
  - [x] Implement helper methods to map errors to JSON-RPC error code and message.
  - [x] Add unit tests for JSON-RPC code mapping.
- [x] Implement ID generation in `src/ids.rs`.
  - [x] Add monotonic request-id generator returning ACP `RequestId` or SDK-compatible value.
  - [x] Add monotonic session-id generator using configured prefix.
  - [x] Ensure generated IDs are process-local unique without global mutable state.
  - [x] Add unit tests for monotonic request IDs.
  - [x] Add unit tests for configured session-id prefix.
- [x] Implement transport abstraction in `src/transport.rs`.
  - [x] Define `AcpTransport` trait.
    - [x] Add `read_message` async method returning `Option<JsonRpcMessage>`.
    - [x] Add `write_message` async method.
    - [x] Require `Send + 'static`.
  - [x] Implement `StdioTransport`.
    - [x] Read newline-delimited JSON-RPC messages from stdin.
    - [x] Write one JSON-RPC message per line to stdout.
    - [x] Flush stdout after every message.
    - [x] Enforce `max_frame_bytes` before parsing.
    - [x] Treat EOF as clean shutdown.
  - [x] Implement test-only `MemoryTransport`.
    - [x] Accept inbound messages from an in-memory queue.
    - [x] Capture outbound messages in deterministic order.
    - [x] Support injecting EOF.
  - [x] Add tests for valid frame parse.
  - [x] Add tests for oversized frame rejection.
  - [x] Add tests for EOF returning clean shutdown.
  - [x] Add tests for write preserving one-line JSON.

#### Actionable criteria

- [x] `cargo fmt --check` passes after crate creation.
- [x] `cargo clippy --quiet -p ee-acp-agent-server --all-targets --all-features -- -D warnings` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server transport` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server config` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server error` passes.

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

- [x] Implement provider API in `src/provider.rs`.
  - [x] Add `ProviderFuture<T>` boxed-future type alias.
  - [x] Add `AgentProvider` trait.
    - [x] Add `info(&self) -> Implementation`.
    - [x] Add `capabilities(&self) -> AgentCapabilities`.
    - [x] Add `new_session(ctx) -> ProviderFuture<Result<SessionInit, ProviderError>>`.
    - [x] Add `load_session(ctx) -> ProviderFuture<Result<SessionInit, ProviderError>>`.
    - [x] Add `prompt(ctx, sink, client, cancel) -> ProviderFuture<Result<PromptResult, ProviderError>>`.
    - [x] Add `cancel_session(session_id) -> ProviderFuture<Result<(), ProviderError>>`.
    - [x] Add `close_session(session_id) -> ProviderFuture<Result<(), ProviderError>>`.
  - [x] Add `NewSessionContext`.
    - [x] Include `cwd`.
    - [x] Include `additional_directories`.
    - [x] Include `mcp_servers`.
    - [x] Include initial ACP session metadata needed by providers.
  - [x] Add `LoadSessionContext`.
    - [x] Include `session_id`.
    - [x] Include `cwd`.
    - [x] Include `additional_directories`.
    - [x] Include `mcp_servers`.
  - [x] Add `PromptContext`.
    - [x] Include `session_id`.
    - [x] Include prompt content blocks.
    - [x] Include raw request metadata needed by providers.
  - [x] Add `SessionInit`.
    - [x] Include resolved `session_id`.
    - [x] Include optional title.
    - [x] Include available commands.
    - [x] Include modes/config options when supported by SDK types.
  - [x] Add `PromptResult`.
    - [x] Include ACP stop reason.
    - [x] Include optional usage metadata when SDK supports it.
- [x] Implement session store in `src/session.rs`.
  - [x] Add `ServerSession`.
    - [x] Store `SessionId`.
    - [x] Store optional absolute `cwd`.
    - [x] Store absolute `additional_directories`.
    - [x] Store advertised `mcp_servers`.
    - [x] Store provider-owned `serde_json::Value` metadata.
  - [x] Add `SessionStore`.
    - [x] Use `RwLock<BTreeMap<SessionId, ServerSession>>` or tokio equivalent.  (keyed by session id string — SDK `SessionId` has no `Ord`)
    - [x] Add `insert_new`.
    - [x] Add `get`.
    - [x] Add `list`.
    - [x] Add `remove`.
    - [x] Add `contains`.
  - [x] Add unit tests for insert/get/list/remove.
  - [x] Add unit tests for duplicate session id rejection.
- [x] Implement server runtime shell in `src/server.rs`.
  - [x] Add `AcpAgentServer<P>` generic over `AgentProvider`.
  - [x] Add `new(provider, config)` constructor.
  - [x] Add `run_stdio` method using `StdioTransport`.
  - [x] Add `run_with_transport` method for tests.
  - [x] Start reader/dispatcher loop.
  - [x] Send every response through transport writer path.
  - [x] Fail all pending state on transport close.
- [x] Implement request dispatch in `src/dispatch.rs`.
  - [x] Route `initialize`.
    - [x] Parse `InitializeRequest` using SDK type.
    - [x] Reject protocol versions other than ACP v1.
    - [x] Return provider info and capabilities.
    - [x] Include framework implementation metadata from config.
  - [x] Route `session/new`.
    - [x] Parse SDK `NewSessionRequest`.
    - [x] Validate `cwd` is absolute.
    - [x] Validate every additional directory is absolute.
    - [x] Invoke provider `new_session`.
    - [x] Store returned session in `SessionStore`.
    - [x] Return SDK `NewSessionResponse` shape.
  - [x] Route `session/load`.
    - [x] Parse SDK `LoadSessionRequest`.
    - [x] Validate `cwd` is absolute.
    - [x] Invoke provider `load_session`.
    - [x] Store loaded session in `SessionStore`.
    - [x] Return SDK `LoadSessionResponse` shape.
  - [x] Route `session/list`.
    - [x] Return sessions from `SessionStore` in stable order.
    - [x] Treat cursors as opaque when SDK field exists.
  - [x] Route `session/close`.
    - [x] Cancel active prompt for session if present.
    - [x] Invoke provider `close_session`.
    - [x] Remove session from `SessionStore`.
    - [x] Return SDK close response shape.
  - [x] Route unknown methods.
    - [x] Return JSON-RPC method-not-found for requests.
    - [x] Ignore unknown notifications with tracing debug only.
- [x] Add integration tests in `crates/ee-acp-agent-server/tests/server_flows.rs`.
  - [x] Create fake provider returning deterministic info and capabilities.
  - [x] Test `initialize` with protocol version `1` succeeds.
  - [x] Test `initialize` with protocol version `0` fails closed.
  - [x] Test `initialize` with protocol version `2` fails closed.
  - [x] Test `session/new` stores session and returns provider session id.
  - [x] Test `session/new` rejects relative `cwd` before provider call.
  - [x] Test `session/load` stores session and returns loaded session id.
  - [x] Test `session/list` returns stable sessions.
  - [x] Test `session/close` removes session and calls provider close.
  - [x] Test unknown request returns method-not-found.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-acp-agent-server initialize` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server session` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server server_flows` passes.
- [x] Provider fake records prove malformed session requests do not invoke provider.

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

- [x] Implement typed update sink in `src/updates.rs`.
  - [x] Add `UpdateSink` bound to a session id and transport writer.
  - [x] Add `agent_message_chunk(message_id, text)` helper.
    - [x] Build SDK `SessionUpdate` value where available.
    - [x] Send `session/update` notification with correct `sessionId`.
  - [x] Add `agent_thought_chunk(message_id, text)` helper.
  - [x] Add `tool_call_pending(tool_call_id, title, kind)` helper.
  - [x] Add `tool_call_in_progress(tool_call_id, title, content)` helper.
  - [x] Add `tool_call_completed(tool_call_id, title, content)` helper.
  - [x] Add `tool_call_failed(tool_call_id, title, error)` helper.
  - [x] Add `plan_replace(entries)` helper.
  - [x] Add `available_commands_replace(commands)` helper.
  - [x] Add `session_info_update(info)` helper.
  - [x] Add `raw_update(update)` escape hatch using SDK type.
  - [x] Validate message ids and tool-call ids are non-empty.
- [x] Add active prompt tracking in `src/session.rs` or `src/server.rs`.
  - [x] Store `CancellationToken` or equivalent cancellation flag per active session prompt.  (watch channel sender)
  - [x] Store prompt join handle per active session prompt.
  - [x] Add helper to start active prompt only when no prompt exists for session.
  - [x] Add helper to cancel active prompt.
  - [x] Add helper to cleanup active prompt after completion.
- [x] Route `session/prompt` in `src/dispatch.rs`.
  - [x] Parse SDK `SessionPromptRequest`.
  - [x] Reject unknown session before provider call.
  - [x] Reject same-session concurrent prompt with clear JSON-RPC error.
  - [x] Create `PromptContext` from request.
  - [x] Create `UpdateSink` for session.
  - [x] Create placeholder `ClientBridge` with no outbound methods until Phase 4.
  - [x] Await provider prompt completion.
  - [x] Return SDK prompt response with stop reason.
  - [x] Map provider cancellation to a deterministic prompt result or JSON-RPC cancellation error according to SDK response support.
- [x] Route `session/cancel` in `src/dispatch.rs`.
  - [x] Parse SDK cancel request or notification shape.
  - [x] Cancel active prompt for the session.
  - [x] Invoke provider `cancel_session`.
  - [x] Return success for request form.
  - [x] Do not error when cancel targets no active prompt.
- [x] Make `session/close` cancel active prompt.
  - [x] Trigger active prompt cancellation before provider close.
  - [x] Await prompt cleanup with bounded timeout.
  - [x] Remove prompt state before removing session.
- [x] Add update tests.
  - [x] Test message chunk emits `session/update` before prompt response.
  - [x] Test thought chunk emits expected ACP update name.
  - [x] Test tool-call completed helper includes title/status/content.
  - [x] Test plan replace helper emits complete replacement update.
  - [x] Test update sink rejects unknown session.
- [x] Add cancellation tests in `tests/cancellation.rs`.
  - [x] Test `session/cancel` triggers provider cancellation token.
  - [x] Test cancelled prompt eventually removes active prompt state.
  - [x] Test `session/close` cancels active prompt.
  - [x] Test second same-session prompt is rejected while first prompt is active.
  - [x] Test prompts in two sessions run concurrently.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-acp-agent-server prompt` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server updates` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server cancellation` passes.
- [x] Tests prove same-session prompt state is cleaned after success, provider error, and cancellation.

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

- [x] Implement pending request manager in `src/client.rs`.
  - [x] Add `ClientBridge` cloneable handle.
  - [x] Add pending map keyed by request id.
  - [x] Add `send_request(method, params)` internal helper.
    - [x] Allocate request id.
    - [x] Insert pending oneshot sender before writing request.
    - [x] Write JSON-RPC request to transport.
    - [x] Await response with `request_timeout`.
    - [x] Remove pending entry on success, timeout, cancellation, and write failure.
  - [x] Add `handle_response(response)`.
    - [x] Resolve matching pending request.
    - [x] Ignore unknown response id with tracing debug.
    - [x] Map JSON-RPC error response to `ProviderError::ClientRequestFailed` or permission denied where applicable.
  - [x] Add `fail_all_pending(reason)`.
    - [x] Resolve every pending request with transport-closed error.
- [x] Add typed `ClientBridge` methods.
  - [x] Implement `read_text_file(ReadTextFileRequest) -> ReadTextFileResponse`.
    - [x] Validate path absolute.
    - [x] Send `fs/read_text_file`.
    - [x] Decode SDK response type.
  - [x] Implement `write_text_file(WriteTextFileRequest) -> WriteTextFileResponse`.
    - [x] Validate path absolute.
    - [x] Send `fs/write_text_file`.
  - [x] Implement `create_terminal(CreateTerminalRequest) -> CreateTerminalResponse`.
    - [x] Validate `cwd` absolute when present.
    - [x] Send `terminal/create`.
  - [x] Implement `terminal_output(TerminalOutputRequest) -> TerminalOutputResponse`.
  - [x] Implement `wait_for_terminal_exit(WaitForTerminalExitRequest) -> WaitForTerminalExitResponse`.
  - [x] Implement `kill_terminal(KillTerminalRequest) -> KillTerminalResponse`.
  - [x] Implement `release_terminal(ReleaseTerminalRequest) -> ReleaseTerminalResponse`.
  - [x] Implement `create_elicitation(CreateElicitationRequest) -> CreateElicitationResponse`.
- [x] Route inbound JSON-RPC responses to `ClientBridge`.
  - [x] Distinguish inbound requests from responses in server reader loop.
  - [x] Forward response envelopes to pending request manager.
  - [x] Keep request dispatch path unchanged for inbound requests.
- [x] Pass real `ClientBridge` to provider prompt.
  - [x] Replace Phase 3 placeholder with live bridge handle.
  - [x] Ensure prompt cancellation cancels bridge waits owned by that prompt.
- [x] Add client bridge tests in `tests/client_requests.rs`.
  - [x] Test provider calls `read_text_file` and framework emits `fs/read_text_file` request.
  - [x] Test matching response returns content to provider.
  - [x] Test JSON-RPC error response maps to provider client request failure.
  - [x] Test unknown response id is ignored.
  - [x] Test timeout removes pending entry.
  - [x] Test transport close fails pending entries.
  - [x] Test relative outbound read path fails before request is written.
  - [x] Test terminal create rejects relative cwd.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-acp-agent-server client` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server client_requests` passes.
- [x] Tests prove no pending requests remain after timeout and transport close.
- [x] Tests prove relative outbound file paths do not write JSON-RPC requests.

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

- [x] Implement validation helpers in `src/validate.rs`.
  - [x] Add `validate_protocol_version_v1`.
  - [x] Add `validate_absolute_path`.
  - [x] Add `validate_absolute_paths`.
  - [x] Add `validate_session_id`.
  - [x] Add `validate_message_id`.
  - [x] Add `validate_tool_call_id`.
  - [x] Add `validate_frame_len`.
  - [x] Add tests for every validation helper.
- [x] Harden dispatch error handling.
  - [x] Convert JSON parse errors to JSON-RPC parse error response when possible.
  - [x] Convert invalid params to `-32602`.
  - [x] Convert unknown methods to `-32601`.
  - [x] Convert provider backend errors to `-32603`.
  - [x] Convert unknown session to deterministic protocol error.
  - [x] Add tests for every error mapping.
- [x] Harden provider result handling.
  - [x] Reject provider-returned empty session ids.
  - [x] Reject provider-returned duplicate session ids.
  - [x] Reject provider attempts to emit update for removed session.
  - [x] Map provider cancellation consistently.
  - [x] Add tests for rejected provider result cases.
- [x] Harden transport lifecycle.
  - [x] Fail pending client requests on EOF.
  - [x] Cancel active prompts on EOF.
  - [x] Close writer path after reader shutdown.
  - [x] Add tests for EOF during active prompt.
  - [x] Add tests for EOF during outbound client request.
- [x] Add conformance fixture tests.
  - [x] Add initialize request fixture under `tests/fixtures`.
  - [x] Add session new request fixture.
  - [x] Add session prompt request fixture.
  - [x] Add session cancel request fixture.
  - [x] Add fs read response fixture.
  - [x] Test fixture round-trips with SDK types.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-acp-agent-server validate` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server conformance` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server` passes.
- [x] `cargo clippy --quiet -p ee-acp-agent-server --all-targets --all-features -- -D warnings` passes.

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

- [x] Update `crates/ee-openrouter-agent/Cargo.toml`.
  - [x] Add dependency on `ee-acp-agent-server`.
  - [x] Add dependency on `tokio` if binary runtime needs it.
  - [x] Remove direct protocol-loop-only dependencies that become unused.
- [x] Split OpenRouter modules.
  - [x] Move CLI/env config code to `src/config.rs`.
    - [x] Keep `OPENROUTER_MODEL`.
    - [x] Keep `OPENROUTER_API_URL`.
    - [x] Keep `OPENROUTER_SITE_URL`.
    - [x] Keep `OPENROUTER_APP_TITLE`.
    - [x] Keep `OPENROUTER_TIMEOUT_MS`.
    - [x] Keep `OPENROUTER_REASONING_EFFORT`.
    - [x] Keep `OPENROUTER_SYSTEM_PROMPT`.
    - [x] Keep `OPENROUTER_API_KEY` lookup.
  - [x] Move dotenv parser to `src/dotenv.rs`.
    - [x] Preserve quote handling.
    - [x] Preserve `export ` prefix support.
    - [x] Preserve invalid-name skipping.
  - [x] Move HTTP request/response mapping to `src/openrouter.rs`.
    - [x] Keep request body model/messages/tools/tool_choice shape.
    - [x] Keep reasoning effort insertion.
    - [x] Keep OpenRouter HTTP error extraction.
  - [x] Move provider/tool behavior to `src/provider.rs` and `src/tools.rs`.
- [x] Implement `OpenRouterProvider`.
  - [x] Store config and HTTP client.
  - [x] Store per-session message history behind mutex/RwLock.
  - [x] Implement `info` returning `ee-openrouter-agent` implementation metadata.
  - [x] Implement `capabilities` for supported prompt/session behavior.
  - [x] Implement `new_session`.
    - [x] Store session cwd.
    - [x] Initialize empty message history.
  - [x] Implement `load_session` as unsupported provider error matching previous behavior.
  - [x] Implement `prompt`.
    - [x] Extract text prompt blocks.
    - [x] Return invalid request when prompt contains no text.
    - [x] Return backend error when API key missing.
    - [x] Send OpenRouter request with existing message history.
    - [x] Emit reasoning through `UpdateSink::agent_thought_chunk`.
    - [x] Emit answer through `UpdateSink::agent_message_chunk`.
    - [x] Return end-turn stop reason.
  - [x] Implement bounded tool loop.
    - [x] Keep max tool rounds at `6`.
    - [x] Support `tool_read_file` and `read_file` aliases.
    - [x] Resolve relative paths against session cwd.
    - [x] Call `ClientBridge::read_text_file`.
    - [x] Emit tool-call in-progress and completed/failed updates.
    - [x] Append tool results to OpenRouter messages.
  - [x] Implement `cancel_session` by marking active work cancelled if needed.
  - [x] Implement `close_session` by removing stored history.
- [x] Replace `src/main.rs` protocol loop.
  - [x] Parse args.
  - [x] Load `.env` from current directory.
  - [x] Build `OpenRouterProvider`.
  - [x] Run `AcpAgentServer::new(provider, config).run_stdio().await`.
  - [x] Print only concise process-level errors to stderr.
- [x] Preserve and update tests.
  - [x] Keep prompt text extraction test.
  - [x] Keep OpenRouter string answer extraction test.
  - [x] Keep OpenRouter reasoning extraction test.
  - [x] Keep reasoning effort request body test.
  - [x] Keep tool-call argument extraction test.
  - [x] Keep prompt-without-api-key JSON-RPC/framework error test.
  - [x] Convert read-file tool test to assert framework emits `fs/read_text_file`.
  - [x] Add test that provider emits thought update through framework.
  - [x] Add test that provider emits answer update through framework.
  - [x] Add test that tool loop max rounds maps to provider backend error.
  - [x] Add test that close session removes message history.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-openrouter-agent` passes.
- [x] `cargo clippy --quiet -p ee-openrouter-agent --all-targets --all-features -- -D warnings` passes.
- [x] OpenRouter tests prove no handrolled stdin/stdout JSON-RPC loop remains in provider code.
- [x] OpenRouter read-file test proves file access goes through `ClientBridge`.

### Phase 7: Echo example and compile-tested provider documentation

Goal: add a tiny local example provider that exercises the framework without network access and keep docs examples buildable.

Overview: the example gives automated smoke coverage for the public API. Documentation must include compile-tested examples rather than manual setup tasks.

Rules:

- Example must not call external APIs.
- Example must not depend on editor UI.
- Documentation examples must compile in tests when possible.
- Do not add manual verification checklist items.

#### Work items

- [x] Add `crates/ee-acp-agent-server/examples/echo_agent.rs`.
  - [x] Implement `EchoProvider` using `AgentProvider`.
  - [x] Return deterministic implementation metadata.
  - [x] Create sessions with framework-generated or provider-accepted ids.
  - [x] On prompt, concatenate text blocks.
  - [x] Emit echoed text through `UpdateSink::agent_message_chunk`.
  - [x] Return end-turn prompt result.
  - [x] Support cancellation by checking cancellation token before emitting final update.
- [x] Add example tests.
  - [x] Add integration test that runs echo provider with memory transport.
  - [x] Send `initialize` and assert ACP v1 response.
  - [x] Send `session/new` and assert session id exists.
  - [x] Send `session/prompt` and assert `session/update` contains echoed text.
  - [x] Send `session/cancel` during blocked echo prompt and assert cancellation cleanup.
- [x] Add crate docs with compile-tested example in `src/lib.rs`.
  - [x] Show minimal provider struct.
  - [x] Show `AgentProvider` implementation skeleton.
  - [x] Show `AcpAgentServer::new(provider, config)` usage.
  - [x] Mark non-runnable stdio example with `no_run` if needed.
- [x] Add README validation tests where practical.
  - [x] Keep code snippets mirrored in doc tests or examples.
  - [x] Avoid untested command snippets as checklist work.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-acp-agent-server --examples` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server --doc` passes.
- [x] Echo example integration test proves initialize/new/prompt/update flow works without network.

### Phase 8: Workspace validation and integration guardrails

Goal: prove framework, OpenRouter provider, and existing host/protocol crates remain compatible after refactor.

Overview: this phase adds focused tests and workspace checks that catch boundary regressions between agent server framework, ACP protocol facade, existing host, and provider binary.

Rules:

- Validate changed crates first, then broader workspace summary.
- Use quiet cargo test commands only.
- Do not change host/client behavior except where tests reveal necessary compatibility fixes.
- Keep framework public API minimal and stable before wider use.

#### Work items

- [x] Add cross-crate compile checks.
  - [x] Ensure `ee-acp-agent-server` depends on `ee-agent-protocol` but not `ee-agent-host`.
  - [x] Add a unit test or crate-level compile assertion proving public API exposes SDK-backed `SessionId`, `SessionUpdate`, and request/response types.
  - [x] Add a test ensuring framework-supported ACP version equals `ee_agent_protocol::SUPPORTED_ACP_VERSION`.
- [x] Add compatibility tests with existing host fake transport where feasible.
  - [x] Start framework fake provider over memory/pipe transport.
  - [x] Connect `ee-agent-host` fake/client side if existing test utilities support injected transport.
  - [x] Assert host can initialize, create session, prompt, receive update, and close session.
  - [x] Keep this test behind existing `test-utils` feature if needed.
- [x] Add public API hygiene checks.
  - [x] Keep provider trait methods documented.
  - [x] Keep exported structs non-exhaustive where future fields are likely.
  - [x] Prefer `pub(crate)` for internal runtime structs.
  - [x] Add compile-fail or privacy tests only if existing project pattern supports them.
- [x] Run focused validation commands.
  - [x] Validate format.
  - [x] Validate clippy for `ee-acp-agent-server`.
  - [x] Validate clippy for `ee-openrouter-agent`.
  - [x] Validate framework tests.
  - [x] Validate OpenRouter tests.
  - [x] Validate protocol tests touched by public type usage.

#### Actionable criteria

- [x] `cargo fmt --check` passes.
- [x] `cargo clippy --quiet -p ee-acp-agent-server --all-targets --all-features -- -D warnings` passes.
- [x] `cargo clippy --quiet -p ee-openrouter-agent --all-targets --all-features -- -D warnings` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server` passes.
- [x] `cargo test --quiet -p ee-openrouter-agent` passes.
- [x] `cargo test --quiet -p ee-agent-protocol` passes when protocol facade exports changed.
- [x] `./scripts/test-workspace-summary.sh` passes after focused crate validation.

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

- [x] Add `crates/ee-agent-orchestrator` to workspace members in `ee/Cargo.toml`.
  - [x] Create `crates/ee-agent-orchestrator/Cargo.toml`.
    - [x] Set package name to `ee-agent-orchestrator`.
    - [x] Use workspace `edition`, `rust-version`, `license`, and author conventions.
    - [x] Add dependencies on `ee-acp-agent-server`, `ee-agent-protocol`, `serde`, `serde_json`, `tokio`, `futures`, and `tracing` from workspace dependencies.
    - [x] Add dev-dependencies needed only for deterministic async tests.
  - [x] Create `crates/ee-agent-orchestrator/src/lib.rs`.
    - [x] Export `config`, `error`, `runtime`, `loop_engine`, `model`, `tools`, `tasks`, `subagents`, `memory`, `budget`, `policy`, `events`, and `test_support` modules.
    - [x] Re-export primary public types.
    - [x] Add crate docs stating this crate is optional server-side orchestration above ACP.
- [x] Implement orchestrator config in `src/config.rs`.
  - [x] Add `OrchestratorConfig`.
    - [x] Include `max_loop_iterations`.
    - [x] Include `max_tool_calls_per_turn`.
    - [x] Include `max_subagent_depth`.
    - [x] Include `max_parallel_subagents`.
    - [x] Include `turn_timeout`.
    - [x] Include `tool_timeout`.
    - [x] Include `subagent_timeout`.
    - [x] Include `memory_limit_bytes`.
  - [x] Implement safe defaults.
    - [x] Set max loop iterations to `16`.
    - [x] Set max tool calls per turn to `32`.
    - [x] Set max subagent depth to `2`.
    - [x] Set max parallel subagents to `4`.
    - [x] Set turn timeout to `300s`.
    - [x] Set tool timeout to `120s`.
    - [x] Set subagent timeout to `300s`.
    - [x] Set memory limit bytes to `1 MiB`.
  - [x] Add tests for default config values.
- [x] Implement orchestrator errors in `src/error.rs`.
  - [x] Add `OrchestratorError` variants for model failure, tool failure, policy denial, budget exceeded, timeout, cancellation, invalid state, subagent failure, and serialization failure.
  - [x] Implement conversion to `ee_acp_agent_server::ProviderError`.
  - [x] Add tests for error-to-provider-error mapping.
- [x] Implement runtime state in `src/runtime.rs`.
  - [x] Add `OrchestratorRuntime`.
    - [x] Store config.
    - [x] Store injected model router.
    - [x] Store tool registry.
    - [x] Store task store.
    - [x] Store memory store.
    - [x] Store budget tracker.
    - [x] Store policy engine.
  - [x] Add `run_turn(prompt_ctx, sink, client, cancel)` entry point.
    - [x] Build initial root task from prompt.
    - [x] Start loop engine with configured budgets.
    - [x] Return `PromptResult` compatible with `ee-acp-agent-server`.
  - [x] Add tests using fake model and fake tools to run one complete turn.

#### Actionable criteria

- [x] `cargo fmt --check` passes after crate creation.
- [x] `cargo clippy --quiet -p ee-agent-orchestrator --all-targets --all-features -- -D warnings` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator config` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator runtime` passes.

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

- [x] Implement normalized messages in `src/model.rs`.
  - [x] Add `ModelMessage`.
    - [x] Include role: system, user, assistant, tool, subagent.
    - [x] Include content blocks.
    - [x] Include optional reasoning summary.
    - [x] Include bounded metadata.
  - [x] Add `ModelContent`.
    - [x] Include text.
    - [x] Include tool result.
    - [x] Include file reference.
    - [x] Include terminal reference.
  - [x] Add `ModelRequest`.
    - [x] Include transcript.
    - [x] Include available tool schemas.
    - [x] Include budget snapshot.
    - [x] Include current task state.
  - [x] Add `ModelResponse`.
    - [x] Include assistant text.
    - [x] Include reasoning text.
    - [x] Include tool intents.
    - [x] Include subagent intents.
    - [x] Include completion signal.
- [x] Implement model adapter trait.
  - [x] Add `ModelAdapter` trait with `complete(request, cancel)`.
  - [x] Add `ModelFuture<T>` boxed-future alias.
  - [x] Require adapter to be `Send + Sync + 'static`.
  - [x] Add fake deterministic model adapter in `test_support`.
- [x] Implement transcript builder.
  - [x] Convert ACP prompt content into normalized `ModelMessage` values.
  - [x] Append assistant text responses.
  - [x] Append tool results with stable tool-call IDs.
  - [x] Append subagent summaries.
  - [x] Enforce memory byte limit while preserving newest context.
- [x] Add model tests.
  - [x] Test ACP text prompt converts to normalized user message.
  - [x] Test reasoning is preserved separately from assistant text.
  - [x] Test tool intent parsing from fake model response.
  - [x] Test subagent intent parsing from fake model response.
  - [x] Test transcript truncation preserves newest messages and records truncation metadata.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator model` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator transcript` passes.
- [x] Tests prove normalized transcript never contains provider-specific required fields.

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

- [x] Implement tool types in `src/tools.rs`.
  - [x] Add `ToolDefinition`.
    - [x] Include name.
    - [x] Include description.
    - [x] Include JSON schema.
    - [x] Include side-effect class: read, write, execute, delegate.
    - [x] Include required capability flags.
  - [x] Add `ToolIntent`.
    - [x] Include tool call id.
    - [x] Include tool name.
    - [x] Include JSON arguments.
  - [x] Add `ToolResult`.
    - [x] Include success flag.
    - [x] Include text output.
    - [x] Include structured output.
    - [x] Include error kind.
- [x] Implement tool registry.
  - [x] Register built-in `read_file` mapping to `ClientBridge::read_text_file`.
  - [x] Register built-in `write_file` mapping to `ClientBridge::write_text_file`.
  - [x] Register built-in terminal lifecycle tools mapping to `ClientBridge` terminal methods.
  - [x] Register built-in `ask_user` mapping to `ClientBridge::create_elicitation`.
  - [x] Support custom provider-supplied tools through a `ServerTool` trait.
  - [x] Add tests for duplicate tool name rejection.
- [x] Implement policy engine in `src/policy.rs`.
  - [x] Add `ToolPolicy`.
    - [x] Allow read tools by default.
    - [x] Require explicit allowance for write tools.
    - [x] Require explicit allowance for execute tools.
    - [x] Limit delegate tools by subagent depth and count.
  - [x] Add `PolicyDecision` with allow/deny reason.
  - [x] Add tests for read/write/execute/delegate policy decisions.
- [x] Implement tool executor.
  - [x] Validate tool exists.
  - [x] Validate argument shape against tool schema where practical.
  - [x] Check policy before execution.
  - [x] Increment budget counters before execution.
  - [x] Emit pending tool-call update.
  - [x] Emit in-progress tool-call update.
  - [x] Run tool with timeout and cancellation.
  - [x] Emit completed or failed tool-call update.
  - [x] Return normalized `ToolResult` to loop engine.
- [x] Add tool execution tests.
  - [x] Test read file tool calls `ClientBridge::read_text_file`.
  - [x] Test write file tool is denied by default policy.
  - [x] Test execute tool is denied by default policy.
  - [x] Test custom tool runs and returns structured output.
  - [x] Test tool timeout emits failed update.
  - [x] Test cancellation stops running tool and records cancellation result.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator tools` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator policy` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator tool_executor` passes.
- [x] Tests prove write/execute tools fail closed under default policy.

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

- [x] Implement loop event model in `src/events.rs`.
  - [x] Add `OrchestratorEvent` variants for turn started, model requested, model responded, tool started, tool finished, subagent started, subagent finished, budget updated, turn stopped, and error.
  - [x] Add in-memory event recorder for tests.
  - [x] Add tests for event serialization.
- [x] Implement loop engine in `src/loop_engine.rs`.
  - [x] Add `LoopEngine`.
  - [x] Build initial transcript from prompt and memory.
  - [x] Emit thought updates when model returns reasoning.
  - [x] Emit assistant message updates when model returns text.
  - [x] Execute model tool intents in deterministic order.
  - [x] Append tool results to transcript.
  - [x] Continue loop after tool results when model has not completed.
  - [x] Stop when model returns completion signal.
  - [x] Stop when no tool intents and no assistant text are returned twice in a row.
  - [x] Stop with budget-exceeded error when iteration/tool budgets are exceeded.
  - [x] Stop promptly on cancellation token.
- [x] Add loop tests.
  - [x] Test one-model-response turn emits assistant update and stops.
  - [x] Test model tool intent executes tool, appends result, and calls model again.
  - [x] Test tool failure is appended as observation and model can recover.
  - [x] Test max loop iterations stops before infinite loop.
  - [x] Test max tool calls stops before unbounded tool use.
  - [x] Test cancellation stops before next model call.
  - [x] Test repeated empty responses stop deterministically.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator loop_engine` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator events` passes.
- [x] Tests prove loop cannot run forever under adversarial fake model responses.

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

- [x] Implement task graph in `src/tasks.rs`.
  - [x] Add `TaskId`.
  - [x] Add `TaskNode`.
    - [x] Include title.
    - [x] Include description.
    - [x] Include parent id.
    - [x] Include dependencies.
    - [x] Include status: pending, running, blocked, completed, failed, cancelled.
    - [x] Include assigned worker: root or subagent id.
    - [x] Include bounded result summary.
  - [x] Add `TaskGraph`.
    - [x] Add root task creation.
    - [x] Add child task creation.
    - [x] Add dependency edges.
    - [x] Add status transitions with validation.
    - [x] Add topological ready-task query.
    - [x] Add completed summary query.
  - [x] Emit plan updates from task graph state.
- [x] Implement memory store in `src/memory.rs`.
  - [x] Add `MemoryItem`.
    - [x] Include key.
    - [x] Include value.
    - [x] Include source task id.
    - [x] Include byte size.
    - [x] Include sensitivity flag.
  - [x] Add `MemoryStore`.
    - [x] Insert non-sensitive item.
    - [x] Reject sensitive item by default.
    - [x] Evict oldest low-priority items when over byte limit.
    - [x] Query items relevant to active task by key/prefix/source.
    - [x] Export compact context for model request.
- [x] Add task graph tests.
  - [x] Test valid status transitions.
  - [x] Test invalid transition rejection.
  - [x] Test dependency ordering.
  - [x] Test plan update generation from task graph.
- [x] Add memory tests.
  - [x] Test insert/query.
  - [x] Test sensitive item rejection.
  - [x] Test byte-limit eviction.
  - [x] Test compact context excludes evicted and sensitive items.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator tasks` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator memory` passes.
- [x] Tests prove memory stays within configured byte limit.

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

- [x] Implement subagent types in `src/subagents.rs`.
  - [x] Add `SubagentId`.
  - [x] Add `SubagentRole`.
    - [x] Include name.
    - [x] Include instructions.
    - [x] Include allowed tool classes.
    - [x] Include max iterations.
  - [x] Add `SubagentRequest`.
    - [x] Include parent task id.
    - [x] Include child task id.
    - [x] Include role.
    - [x] Include scoped prompt.
    - [x] Include context snapshot.
  - [x] Add `SubagentResult`.
    - [x] Include status.
    - [x] Include summary.
    - [x] Include produced memory items.
    - [x] Include tool-call count.
    - [x] Include error summary.
- [x] Implement subagent manager.
  - [x] Spawn logical subagent task using same `LoopEngine` with reduced config.
  - [x] Enforce depth limit before spawn.
  - [x] Enforce parallelism limit with semaphore.
  - [x] Apply child-specific tool policy.
  - [x] Capture child events with parent correlation ids.
  - [x] Merge child summary into parent transcript.
  - [x] Merge allowed child memory items into parent memory store.
  - [x] Cancel child tasks when parent cancellation fires.
- [x] Add delegation tool integration.
  - [x] Register built-in `delegate_task` tool with side-effect class `delegate`.
  - [x] Validate delegation arguments.
  - [x] Create child task node before spawn.
  - [x] Mark child task running/completed/failed from subagent result.
  - [x] Return bounded child summary as tool result.
- [x] Add subagent tests.
  - [x] Test delegate tool spawns logical subagent.
  - [x] Test subagent depth limit denies nested spawn beyond config.
  - [x] Test parallel subagent limit bounds concurrency.
  - [x] Test parent cancellation cancels children.
  - [x] Test child memory merges only non-sensitive items.
  - [x] Test child failure returns bounded error summary to parent.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator subagents` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator delegate_task` passes.
- [x] Tests prove subagent depth and parallelism limits cannot be exceeded.

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

- [x] Implement budget tracker in `src/budget.rs`.
  - [x] Add `BudgetConfig`.
    - [x] Include max model calls.
    - [x] Include max tool calls.
    - [x] Include max subagents.
    - [x] Include max output bytes.
    - [x] Include optional max input tokens.
    - [x] Include optional max output tokens.
    - [x] Include wall-clock deadline.
  - [x] Add `BudgetSnapshot`.
  - [x] Add `BudgetTracker`.
    - [x] Check model call allowance.
    - [x] Check tool call allowance.
    - [x] Check subagent allowance.
    - [x] Check output byte allowance.
    - [x] Check wall-clock deadline.
    - [x] Record model usage.
    - [x] Record tool usage.
    - [x] Record subagent usage.
  - [x] Emit budget update events.
- [x] Integrate budget tracker.
  - [x] Check before each model adapter call.
  - [x] Check before each tool executor call.
  - [x] Check before each subagent spawn.
  - [x] Stop loop with budget-exceeded error when denied.
  - [x] Include budget snapshot in model request.
- [x] Add cancellation propagation tests.
  - [x] Test cancellation before model call prevents adapter invocation.
  - [x] Test cancellation during model call resolves turn cancellation.
  - [x] Test cancellation during tool call resolves tool cancellation.
  - [x] Test cancellation during subagent run cancels child task.
- [x] Add budget tests.
  - [x] Test max model calls enforced.
  - [x] Test max tool calls enforced.
  - [x] Test max subagents enforced.
  - [x] Test output byte budget enforced.
  - [x] Test wall-clock deadline enforced with paused time where supported.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator budget` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator cancellation` passes.
- [x] Tests prove budget-denied operations are not invoked.

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

- [x] Add provider adapter module.
  - [x] Implement `OrchestratorProvider<M>` generic over `ModelAdapter`.
  - [x] Implement `AgentProvider` for `OrchestratorProvider<M>`.
  - [x] Map provider `info` from adapter config.
  - [x] Map provider `capabilities` from orchestrator-supported ACP features.
  - [x] Implement `new_session` by creating session task/memory state.
  - [x] Implement `load_session` by restoring serialized orchestrator state when provided.
  - [x] Implement `prompt` by calling `OrchestratorRuntime::run_turn`.
  - [x] Implement `cancel_session` by cancelling active turn state.
  - [x] Implement `close_session` by removing task/memory state.
- [x] Add adapter tests.
  - [x] Test adapter initialize metadata through `AcpAgentServer` memory transport.
  - [x] Test adapter session/new creates orchestrator session state.
  - [x] Test adapter prompt runs loop and emits assistant update.
  - [x] Test adapter prompt can execute fake tool through `ClientBridge`.
  - [x] Test adapter cancel stops active orchestrator turn.
  - [x] Test adapter close removes memory/task state.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator provider_adapter` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator --features test-utils` passes if adapter tests use feature-gated support.
- [x] Tests prove adapter works through `ee-acp-agent-server` memory transport.

### Phase 9: Provider migration path and orchestrated OpenRouter mode

Goal: let `ee-openrouter-agent` optionally use the orchestrator without removing simple provider mode.

Overview: OpenRouter can first run through `ee-acp-agent-server` directly. This phase adds an orchestrated mode that uses OpenRouter as `ModelAdapter`, enabling general tool loops and future subagents.

Rules:

- Keep non-orchestrated OpenRouter mode available until orchestrated mode has parity.
- Keep external API behavior behind same OpenRouter config and secrets handling.
- Do not send secrets to model or memory store.
- Keep tests network-free with fake HTTP/model adapters.

#### Work items

- [x] Add `OpenRouterModelAdapter` in `ee-openrouter-agent`.
  - [x] Convert normalized `ModelRequest` to OpenRouter chat completion request.
  - [x] Convert OpenRouter text to `ModelResponse` assistant text.
  - [x] Convert OpenRouter reasoning to `ModelResponse` reasoning.
  - [x] Convert OpenRouter tool calls to normalized `ToolIntent` values.
  - [x] Convert model completion/stop reason to normalized completion signal.
- [x] Add orchestrated mode config.
  - [x] Add CLI/env flag `OPENROUTER_ORCHESTRATED` or command-line option.
  - [x] Default to non-orchestrated provider mode until parity tests pass.
  - [x] Build `OrchestratorProvider<OpenRouterModelAdapter>` when enabled.
- [x] Add OpenRouter orchestrator tests.
  - [x] Test normalized model request converts to OpenRouter JSON body.
  - [x] Test OpenRouter tool call converts to `ToolIntent`.
  - [x] Test OpenRouter reasoning converts to normalized reasoning.
  - [x] Test orchestrated mode with fake model executes read-file tool via `ClientBridge`.
  - [x] Test orchestrated mode respects max tool-call budget.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-openrouter-agent orchestrated` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator` passes.
- [x] Tests prove orchestrated OpenRouter mode remains network-free under fake adapter.

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

- [x] Add dependency-boundary tests.
  - [x] Assert `ee-agent-orchestrator` does not depend on `ee-agent-host`.
  - [x] Assert `ee-agent-orchestrator` does not depend on `ee-cli`.
  - [x] Assert `ee-agent-orchestrator` does depend on `ee-acp-agent-server`.
  - [x] Assert `ee-agent-orchestrator` public adapter uses `AgentProvider` from `ee-acp-agent-server`.
- [x] Add default policy regression tests.
  - [x] Test read tools are available by default.
  - [x] Test write tools are denied by default.
  - [x] Test execute tools are denied by default.
  - [x] Test delegation obeys depth and parallelism defaults.
- [x] Add deterministic test fixtures.
  - [x] Add fake model script fixture for simple answer.
  - [x] Add fake model script fixture for tool call then answer.
  - [x] Add fake model script fixture for delegation then answer.
  - [x] Add fake model script fixture for infinite loop attempt.
  - [x] Add tests that each fixture produces stable event sequence.
- [x] Run focused validation commands.
  - [x] Validate format.
  - [x] Validate clippy for `ee-agent-orchestrator`.
  - [x] Validate clippy for `ee-acp-agent-server`.
  - [x] Validate clippy for `ee-openrouter-agent` when orchestrated mode code changes.
  - [x] Validate orchestrator tests.
  - [x] Validate ACP server tests.
  - [x] Validate OpenRouter tests.

#### Actionable criteria

- [x] `cargo fmt --check` passes.
- [x] `cargo clippy --quiet -p ee-agent-orchestrator --all-targets --all-features -- -D warnings` passes.
- [x] `cargo clippy --quiet -p ee-acp-agent-server --all-targets --all-features -- -D warnings` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator` passes.
- [x] `cargo test --quiet -p ee-acp-agent-server` passes.
- [x] `cargo test --quiet -p ee-openrouter-agent` passes when OpenRouter adapter changes.

### Phase 11: Default OpenRouter to orchestrated mode

Goal: make `ee-openrouter-agent` use the orchestrator by default now that Phase 9/10 parity checks passed, while keeping a short-lived explicit opt-out for fallback diagnostics.

Overview: current simple provider mode still hardcodes only `tool_read_file`, so it hides orchestrator built-ins and gives agents a weaker default experience. Flip the default to orchestrated mode, keep `OPENROUTER_ORCHESTRATED=0` / `--orchestrated=false` as the escape hatch, and make tests prove startup, tool loops, cancellation, and secret handling remain unchanged.

Rules:

- Keep OpenRouter API key handling identical: key only in Authorization header, never transcript, tool schema, events, or logs.
- Do not remove simple provider mode in this phase; only make it opt-out.
- Do not claim ee MCP proxy tools are available from this phase alone; MCP bridging is Phase 12.
- Preserve existing ACP identity (`ee-openrouter-agent`) so saved agent config keeps working.
- Keep all new tests network-free with fake HTTP/model adapters.

#### Work items

- [x] Flip the OpenRouter orchestrated default.
  - [x] Change `crates/ee-openrouter-agent/src/config.rs` so `orchestrated` defaults to `true`.
  - [x] Update config comments from "off until parity" to "default after parity; opt out for fallback diagnostics".
  - [x] Keep env/CLI override support for `OPENROUTER_ORCHESTRATED=0` and explicit `--orchestrated=false` if clap supports the current flag shape.
  - [x] Add/adjust config tests for default true and explicit false override.
- [x] Confirm simple provider remains available.
  - [x] Add a test proving `OPENROUTER_ORCHESTRATED=0` selects `OpenRouterProvider`.
  - [x] Add a test proving default config selects `OrchestratorProvider<OpenRouterModelAdapter>`.
  - [x] Keep `OpenRouterProvider` tests for direct `ClientBridge` read-file behavior.
- [x] Strengthen orchestrated OpenRouter regression coverage.
  - [x] Test default startup through `AcpAgentServer` with fake model and no network.
  - [x] Test read-file tool call still routes through `ClientBridge`.
  - [x] Test cancellation during an orchestrated model/tool loop returns promptly.
  - [x] Test OpenRouter reasoning and final answer still stream through ACP updates.
  - [x] Test secrets do not appear in model messages, tool definitions, events, or tool results.
- [x] Update user-facing docs/config examples.
  - [x] Document orchestrated mode as default.
  - [x] Document `OPENROUTER_ORCHESTRATED=0` as temporary fallback.
  - [x] Note that ee MCP proxy tool availability depends on Phase 12 MCP bridge work.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-openrouter-agent orchestrated` passes.
- [x] `cargo test --quiet -p ee-openrouter-agent config` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator` passes.
- [x] `cargo fmt --check -p ee-openrouter-agent -p ee-agent-orchestrator` passes.
- [x] `cargo clippy --quiet -p ee-openrouter-agent -p ee-agent-orchestrator --all-targets --all-features -- -D warnings` passes.
- [x] New tests prove default OpenRouter path is orchestrated and explicit opt-out still uses simple provider mode.

### Phase 12: Bridge ACP MCP servers into orchestrated tool registry

Goal: make `ee-openrouter-agent` discover and use session-advertised MCP servers, including the ee MCP proxy (`ee_workspace_roots`, `ee_list_directory`, `ee_search_text`, edit tools, diagnostics, formatting, rename, and terminal tools), through the orchestrator tool loop.

Overview: host code already appends the ee MCP proxy to `session/new`, either as ACP-native `McpServer::Acp` when the agent advertises `mcp_capabilities.acp`, or as stdio fallback otherwise. The current OpenRouter simple provider and orchestrator provider ignore `NewSessionContext.mcp_servers`, so those tools never reach the model. Add provider-neutral MCP session management inside `ee-agent-orchestrator`, translate MCP tool schemas into `ToolDefinition`s, execute calls back through MCP, and gate side effects with existing policy.

Rules:

- Prefer official `rmcp` SDK types/transports; do not add handrolled MCP wire code unless SDK lacks coverage and tests prove why.
- Keep backend/frontend boundary intact: MCP client/registry lives in provider/server-side crates, not UI code.
- Do not bypass approval/policy: write, execute, terminal, code action, format, and rename tools must keep existing `SideEffectClass` / destructive-subclass gates.
- Fail closed when a server cannot initialize, list tools, validate schemas, or execute within timeout.
- Use provider-compatible model tool names: no dots. Expose ee proxy tools to models as `ee_` names (`ee_workspace_roots`, etc.) because some upstream providers reject dots in function/tool names.
- Namespace external MCP tools to avoid collisions with provider-compatible separators only; keep a reversible mapping to original MCP tool names for dispatch.
- Never send secrets from MCP server config to model transcripts, tool schemas, events, logs, or memory.
- Keep tests deterministic and network-free with fake ACP MCP and fake stdio/HTTP MCP servers.

#### Work items

- [x] Advertise ACP MCP support from orchestrated providers.
  - [x] Extend `OrchestratorProvider::capabilities()` with `mcp_capabilities.acp` when the provider can host MCP-over-ACP.
  - [x] Add ACP server/provider tests proving host chooses `McpServer::Acp` for the ee proxy when capability is advertised.
  - [x] Keep fallback behavior for agents/providers that do not advertise ACP MCP.
- [x] Capture MCP session configuration.
  - [x] Change `OrchestratorProvider::new_session` to read `NewSessionContext.mcp_servers` and `cwd`.
  - [x] Store per-session MCP server descriptors in orchestrator runtime state.
  - [x] Validate descriptors and redact env/header secrets before any event/log/model exposure.
  - [x] Add tests proving `session/new` MCP servers are retained per session and never leak secrets.
- [x] Implement MCP client session manager in `ee-agent-orchestrator`.
  - [x] Reuse `ee-mcp` / `rmcp` client pieces where they fit without adding `ee-agent-host` or `ee-cli` dependencies.
  - [x] Support ACP-native MCP-over-ACP transport by routing through the framework/client bridge path exposed to providers.
  - [x] Support stdio fallback only if required for providers without ACP-native support.
  - [x] Enforce connect/list/call timeouts and cancellation.
  - [x] Close MCP connections on session close/cancel/drop.
  - [x] Add fake server tests for connect, initialize, tools/list, tools/call, timeout, and shutdown.
- [x] Translate MCP tools into provider-compatible orchestrator tool definitions.
  - [x] Convert MCP tool names, descriptions, and input schemas to `ToolDefinition`.
  - [x] Rename ee proxy MCP tools from `ee.*` to `ee_*` (for example, `ee.workspace_roots` → `ee_workspace_roots`).
  - [x] Keep a reversible dispatch mapping for sanitized external MCP tool names when provider-facing names differ from source names.
  - [x] Namespace non-ee server tools with provider-compatible names such as `mcp_<server_id>_<tool_name>` after sanitizing unsupported characters.
  - [x] Reject or disambiguate sanitized name collisions fail-closed before advertising tools to the model.
  - [x] Infer side-effect class/subclass from original MCP tool names and configured metadata, not only sanitized display names.
  - [x] Default unknown external MCP tools to conservative policy requiring approval or deny-by-default until classified.
  - [x] Add schema conversion tests, name-sanitization tests, reversible-mapping tests, collision tests, invalid schema rejection tests, and policy classification tests.
- [x] Execute MCP tool calls through orchestrator policy pipeline.
  - [x] Register MCP-backed tools in `ToolRegistry` after successful `tools/list`.
  - [x] Execute model tool intents by calling MCP `tools/call` and normalizing `CallToolResult` into `ToolResult`.
  - [x] Map MCP `isError` tool results to failed `ToolResult` without crashing the turn.
  - [x] Stream tool-call lifecycle updates through existing `UpdateSink`.
  - [x] Respect cancellation before connect/list/call and during long calls.
  - [x] Add tests for success, tool error, protocol error, timeout, cancellation, and policy-denied write/execute calls.
- [x] Expose ee MCP proxy tools to OpenRouter in default orchestrated mode.
  - [x] Add an end-to-end fake OpenRouter test where `session/new` includes ee proxy MCP server and OpenRouter receives `ee_workspace_roots` in tool schemas.
  - [x] Add a test where model calls `ee_workspace_roots`, dispatch maps it to MCP `ee_workspace_roots`, and result comes from the fake ee proxy backend.
  - [x] Add a test proving no advertised model-facing tool name contains `.`.
  - [x] Add a test where model calls an ee write/edit tool and existing approval/policy blocks or routes it correctly.
  - [x] Add a regression test for user prompt "what MCP tools do I have" proving response can list more than `tool_read_file` when ee proxy is present.
- [x] Update docs and diagnostics.
  - [x] Document orchestrated MCP support and fallback modes.
  - [x] Add diagnostics when no MCP tools are registered: no servers configured, connect failed, list failed, or policy filtered all tools.
  - [x] Surface bounded MCP discovery errors to user without secrets.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator mcp` passes.
- [x] `cargo test --quiet -p ee-openrouter-agent orchestrated` passes.
- [x] `cargo test --quiet -p ee-agent-host mcp_over_acp` passes.
- [x] `cargo test --quiet -p ee-mcp` passes if shared MCP client/proxy code changes.
- [x] `cargo fmt --check -p ee-agent-orchestrator -p ee-openrouter-agent -p ee-agent-host -p ee-mcp` passes.
- [x] `cargo clippy --quiet -p ee-agent-orchestrator -p ee-openrouter-agent -p ee-agent-host -p ee-mcp --all-targets --all-features -- -D warnings` passes.
- [x] End-to-end test proves default OpenRouter orchestrated mode receives `ee_workspace_roots` and executes MCP `ee_workspace_roots` through the dispatch path.
- [x] Tests prove no model-facing tool schema contains provider-rejected characters such as `.`.
- [x] Tests prove MCP write/execute tools cannot bypass orchestrator policy or host approvals.
- [x] Tests prove MCP config secrets are redacted from transcripts, schemas, events, logs, and errors.

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

- [x] Implement checkpoint data model.
  - [x] Add `OrchestratorCheckpoint`.
    - [x] Store schema version.
    - [x] Store orchestrator config snapshot.
    - [x] Store active session id.
    - [x] Store task graph.
    - [x] Store memory store.
    - [x] Store transcript summary.
    - [x] Store budget snapshot.
    - [x] Store subagent tree state.
    - [x] Store deterministic ID generator state.
  - [x] Add checkpoint schema-version tests.
  - [x] Add serialization round-trip tests.
- [x] Implement checkpoint restore.
  - [x] Validate schema version before restore.
  - [x] Validate task graph references during restore.
  - [x] Validate memory byte limits during restore.
  - [x] Rebuild active runtime state from checkpoint.
  - [x] Reject checkpoint containing sensitive memory items by default.
  - [x] Add restore validation tests for invalid references.
  - [x] Add restore validation tests for over-limit memory.
- [x] Implement deterministic replay harness.
  - [x] Add `ReplayScript` fixture type.
    - [x] Include model responses in order.
    - [x] Include tool responses in order.
    - [x] Include expected events.
    - [x] Include expected final task graph state.
  - [x] Add replay runner using fake model and fake tools.
  - [x] Add replay fixture for simple answer.
  - [x] Add replay fixture for tool call then answer.
  - [x] Add replay fixture for delegation then answer.
  - [x] Add replay fixture for infinite-loop attempt.
  - [x] Assert stable event order for every fixture.
- [x] Add trace export.
  - [x] Serialize `OrchestratorEvent` as JSONL.
  - [x] Include task id, subagent id, tool call id, and budget snapshot where applicable.
  - [x] Redact sensitive fields before export.
  - [x] Add tests for JSONL trace export ordering.
  - [x] Add tests for redaction in exported traces.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator checkpoint` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator replay` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator trace` passes.
- [x] Tests prove replay never invokes real tools or model providers.

### Phase 1a: Resilient turn recovery (resumable interruptions)

Goal: make orchestrated turns resilient to fatal-looking stops (deadline, timeout, transient provider failure) by persisting durable checkpoints and resuming on the same session without losing completed work. Feature-gated by `RecoveryConfig::enabled` (disabled = legacy fail-fast behavior).

Rules:

- Classify faults structurally at the failure point; never parse error strings.
- Persist bounded, secret-conscious checkpoints (no raw secrets, no unbounded content).
- Never auto-resume ambiguous side-effecting operations; block identical write/execute replays on resume.
- Cancellation and explicit close never leave stale pending checkpoints.

#### Work items

- [x] Shared wire types in `ee-agent-protocol`: `RecoverableFault`, `RecoverableError` carried in JSON-RPC error `data.recoverable`.
- [x] ACP server `ProviderError::Recoverable` / `RateLimited` / `Transient` variants with structured JSON-RPC data.
- [x] Orchestrator fault taxonomy + `TurnOutcome::{Completed, Interrupted}` and `RecoverableInterruption` (safe-resume flag, retry hint, checkpoint id, counters).
- [x] Distinct `OrchestratorError::DeadlineExceeded` (separate from `BudgetExceeded`).
- [x] Budget: per-turn deadline re-anchoring (`reset_deadline`; idle sessions no longer consume the next slice) + `deadline_remaining`.
- [x] Checkpoint schema v2: `ResumeState` (exact transcript tail, active task, completed tool calls, in-flight marker, resume count, first-started timestamp), provider identity, capture time.
- [x] Durable atomic bounded `CheckpointStore`: temp-file+rename, SHA-256 checksums, per-session caps, TTL pruning, memory fallback, delete-on-close.
- [x] Loop engine: transcript survives errors, milestone checkpoint captures (debounced) + forced interruption capture, in-flight marker, completed-tool idempotency guard (`ToolResultReused`), write/execute/delegate replay blocking on resumed runs.
- [x] Runtime: `run_turn_recoverable` (deadline/timeout → `Interrupted`), `resume_turn` (fresh slice deadline, cumulative caps, provider identity + session-timeout validation), completed/cancelled turns clear pending checkpoints.
- [x] Provider adapter: recoverable JSON-RPC error surface, safe single auto-resume (`auto_resume_max`), manual resume on prompt re-send, `/discard` command, crash restore via `session/load` from the checkpoint store, checkpoint cleanup on close.
- [x] OpenRouter: structural HTTP classification (429/5xx transient, Retry-After parse, 401/403 permanent), capped exponential jitter backoff, streaming retry only before first delta, retry config knobs, recovery enabled in orchestrated mode with `EE_CHECKPOINT_DIR`.
- [x] Host: `TurnPausedRecoverable` event + `RecoverableInfo` parsing from error data (plain errors still surface as `TurnFailed`).
- [x] CLI: `ThreadUiState::PausedRecoverable`, Resume/Discard commands (`:agents_resume`, `:agents_discard`), prompt retention for resume, paused state blocks new prompts, footer/picker labels.
- [x] Observability: `CheckpointSaved` / `TurnInterrupted` / `TurnResumed` / `ToolResultReused` events; recovery counters in `OrchestratorMetrics`.
- [x] Tests: paused-clock deadline resume, crash-at-boundary capture, hung-stream timeout, resumed counters/task reuse, duplicate-write replay blocking, provider mismatch + session-timeout rejection, cancellation cleanup, completed-turn cleanup, `/discard` flow, auto-resume, wire-level recoverable error data, host pause event, CLI pause/resume/discard flows, OpenRouter classification/backoff.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator recovery` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator checkpoint_store` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator provider_adapter_` passes.
- [x] `cargo test --quiet -p ee-agent-host recoverable_error` passes.
- [x] `cargo test --quiet -p ee-cli --features agents recoverable_pause` passes.
- [x] `cargo test --quiet -p ee-openrouter-agent` passes.
- [x] `cargo clippy` clean on all touched crates.

#### Follow-up (implemented)

Current resume path: the client re-sends the original prompt; the provider detects the pending checkpoint and continues it. This covers the ee TUI (which retains prompt blocks) but not clients that lose the prompt or generic ACP hosts.

Per ACP v1 session-setup spec: `session/load` restores a session AND replays the entire conversation via `session/update`; `session/resume` restores context with NO replay. Neither continues a paused turn itself — the client sends `session/prompt` afterward and the pending-checkpoint detection resumes the turn. Both host client APIs exist (`AgentConnection::load_session` / `resume_session`).

- [x] `session/load` replay conformance (see the dedicated follow-up below).
- [x] Route the SDK `session/resume` method in the ACP dispatcher (registry defines `ResumeSessionRequest`/`ResumeSessionResponse`; the dispatcher ignored it).
  - [x] Spec wire semantics: params are sessionId + cwd + mcpServers (+ additionalDirectories); NO prompt, NO replay; respond `{}` after restoring context.
  - [x] Provider restores from the checkpoint store (same fallback as `session/load`); the interrupted turn is then resumed by the next `session/prompt` re-send via the existing pending-checkpoint detection. A live session (same process) is reused as-is; without pending state the resume is rejected.
  - [x] Advertise `SessionCapabilities.resume` when recovery is enabled.
  - [x] Host `resume_session` already exists (capability-gated); wired into the TUI reconnect flow as the fallback when the agent does not advertise `loadSession`.
  - [x] Wire-level tests: `session/resume` restores without replay; next prompt resumes the paused turn; rejected without pending state.
- [x] Agent-advertised `/resume` slash command (plan alternative to the wire method; needs no protocol change).
  - [x] Provider detects a pending checkpoint on a `/resume` prompt and continues it without appending the command text as a user message.
  - [x] Advertise via `available_commands_update` (`discard_available_command` pattern; `resume_available_command` added to `ee-agent-protocol`).
  - [x] Covers the client-crash case: after `session/resume` or `session/load`, the client types `/resume` and the turn continues without the original prompt.
  - [x] Tests: `/resume` continues from checkpoint (command text never reaches the model transcript); `/resume` without pending checkpoint is an ordinary prompt.
- [x] TUI wiring for both lifecycle methods: reconnect flow after agent/process restart (session id is client-persisted), plus prompt persistence across TUI restarts for the resend path.
- [x] Generic-client resume: closed by the `/resume` command — any ACP host that lost the original prompt types `/resume` and the pending-checkpoint detection continues the turn (no prompt text required).

#### Follow-up: incomplete `session/load` (implemented)

`session/load` now meets the ACP v1 contract: the agent **replays the entire conversation** as `session/update` notifications (`user_message_chunk` / `agent_message_chunk`) before responding. The dispatcher registers the session provisionally and defers the response so every replayed update precedes it (FIFO outbound); a failed load removes the provisional session.

What shipped:

- [x] Conversation replay: the provider streams `user_message_chunk` / `agent_message_chunk` for the whole recorded conversation, then the load response follows. Replay ids are deterministic (`replay-u-<n>` / `replay-a-<n>`), unique per message.
- [x] Transcript persistence: `PersistedSession` (written by `close_session`) now carries the session's memory-bounded conversation log (max 256 messages, oldest dropped first), captured via an `UpdateSink` observer in the provider adapter (agent text chunks) plus user prompts recorded at submit.
- [x] Capability honesty: replay is implemented, so `loadSession` stays advertised.
- [x] Client wiring: `:agents_reconnect` in the TUI loads the client-persisted session record (workspace-keyed file under the platform state directory) via `AgentConnection::load_session`, buffering replay updates until the thread is registered; `session/resume` is the fallback when load is not advertised. Existing threads are rebound instead of duplicated.
- [x] Crash-restore reachability: post-close loads replay the persisted conversation; post-crash loads (pending checkpoint) replay the checkpoint's `ResumeState` transcript tail as a bounded fallback. New session ids skip ids present in the durable checkpoint store (cross-restart collision guard).
- [x] Tests: replay order and message ids; load after close; load after crash with a pending checkpoint; load with no persisted state; duplicate/already-registered load rejection; `session/resume` wire flows; `/resume` command flows; TUI reconnect (replay applied, load preferred over resume, last prompt restored); capability gating on the host side (existing `host_flows` tests).

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

- [x] Implement strategy types.
  - [x] Add `TurnStrategy` enum.
    - [x] Include `SimpleAnswer`.
    - [x] Include `ToolLoop`.
    - [x] Include `PlanThenExecute`.
    - [x] Include `ResearchThenEdit`.
    - [x] Include `ValidateThenReview`.
    - [x] Include `ParallelDelegation`.
  - [x] Add `StrategyDecision`.
    - [x] Include selected strategy.
    - [x] Include deterministic reason code.
    - [x] Include required capabilities.
  - [x] Add serialization tests for strategy types.
- [x] Implement strategy selector.
  - [x] Select `SimpleAnswer` when prompt requires no workspace/tool context.
  - [x] Select `ToolLoop` when prompt asks for file inspection or tool use.
  - [x] Select `PlanThenExecute` when prompt asks for implementation over multiple files.
  - [x] Select `ResearchThenEdit` when prompt asks for unknown codebase change.
  - [x] Select `ValidateThenReview` when task has code changes and validation tools are available.
  - [x] Select `ParallelDelegation` only when task graph has independent read-only or disjoint write scopes.
  - [x] Emit strategy decision event.
  - [x] Add selector tests for each strategy.
- [x] Implement strategy execution wrappers.
  - [x] Make `SimpleAnswer` run one model call with no tool execution.
  - [x] Make `ToolLoop` run standard loop engine.
  - [x] Make `PlanThenExecute` require task graph creation before tools.
  - [x] Make `ResearchThenEdit` run read-only tools before write tools.
  - [x] Make `ValidateThenReview` run validation and review after edits.
  - [x] Make `ParallelDelegation` use subagent manager with write-scope checks.
  - [x] Add tests that each wrapper respects cancellation and budget limits.
- [x] Implement final response builder.
  - [x] Add `FinalResponse` data model.
    - [x] Include changed files.
    - [x] Include validation commands and outcomes.
    - [x] Include unresolved risks.
    - [x] Include follow-up suggestions.
  - [x] Build final response from task graph, tool results, validation records, and memory provenance.
  - [x] Prevent claiming validation success without recorded passed command.
  - [x] Add tests for final response with no code changes.
  - [x] Add tests for final response with changed files and passing validation.
  - [x] Add tests for final response with failed validation.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator strategy` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator final_response` passes.
- [x] Tests prove final responses cannot claim unrecorded validation success.

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

- [x] Implement validation task planner.
  - [x] Infer validation tools from changed file types and project metadata.
  - [x] Create validation task nodes in task graph.
  - [x] Route validation commands through existing tool executor.
  - [x] Store validation results with command, status, output summary, and timestamp.
  - [x] Add tests for Rust file validation plan.
  - [x] Add tests for no validation tools available.
- [x] Implement reflection pass.
  - [x] Add `ReflectionConfig`.
    - [x] Include `enabled`.
    - [x] Include `max_review_iterations`.
    - [x] Include `max_fix_iterations`.
  - [x] Add one model review call after tool/edit loop when enabled.
  - [x] Feed changed files, diagnostics, validation results, and task state to review model request.
  - [x] Convert review findings into task graph items.
  - [x] Allow at most configured fix iterations.
  - [x] Add tests for one review pass finding issue.
  - [x] Add tests for review disabled.
- [x] Implement stuck detection.
  - [x] Track repeated identical model responses.
  - [x] Track repeated identical tool calls.
  - [x] Track repeated failed edit attempts.
  - [x] Track loop iterations with no task graph state change.
  - [x] Stop with `Stuck` reason when threshold exceeded.
  - [x] Add tests for repeated model response stop.
  - [x] Add tests for repeated tool call stop.
  - [x] Add tests for repeated failed edit stop.
- [x] Implement progress scoring.
  - [x] Add task completion confidence field.
  - [x] Update confidence from completed tools, validation pass, and review findings.
  - [x] Prevent final success when required tasks remain failed or blocked.
  - [x] Add tests for confidence updates.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator validation_planner` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator reflection` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator stuck_detection` passes.
- [x] Tests prove reflection cannot exceed configured iteration limits.

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

- [x] Implement trust labels.
  - [x] Add `TrustLevel` enum.
    - [x] Include `SystemPolicy`.
    - [x] Include `UserPrompt`.
    - [x] Include `ModelOutput`.
    - [x] Include `ToolOutputUntrusted`.
    - [x] Include `SubagentSummaryUntrusted`.
  - [x] Label every transcript and memory item.
  - [x] Add tests for trust labels on file/tool outputs.
- [x] Implement prompt-injection guard.
  - [x] Wrap untrusted content in model requests with explicit labels.
  - [x] Add policy reminder that untrusted content cannot modify instructions.
  - [x] Detect common injection phrases in untrusted tool output.
  - [x] Emit diagnostic event when suspicious text is detected.
  - [x] Add tests with file content containing “ignore previous instructions”.
  - [x] Add tests proving suspicious text does not alter policy decisions.
- [x] Implement sensitive-data guard.
  - [x] Detect secret-like keys and token-like values.
  - [x] Redact sensitive values before memory insertion.
  - [x] Redact sensitive values before trace export.
  - [x] Redact sensitive values before final response builder.
  - [x] Add tests for API key redaction.
  - [x] Add tests for env-var-like secret redaction.
- [x] Implement destructive action gate.
  - [x] Add side-effect subclasses for delete, move, overwrite, chmod-like operations, terminal kill, and external network request.
  - [x] Deny destructive subclasses by default.
  - [x] Require explicit policy allowance for destructive subclasses.
  - [x] Add tests for delete denied by default.
  - [x] Add tests for overwrite denied without configured allowance.
  - [x] Add tests for terminal kill denied outside owned terminal scope.
- [x] Implement workspace scope policy.
  - [x] Add allowed roots and file glob scopes to task policy.
  - [x] Narrow subagent scopes from parent scopes.
  - [x] Reject tool intents outside active scope before client bridge call.
  - [x] Add tests for root escape rejection.
  - [x] Add tests for subagent narrowed-scope enforcement.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator trust` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator prompt_injection` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator sensitive_data` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator destructive_policy` passes.
- [x] Tests prove untrusted tool output cannot change tool policy decisions.

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

- [x] Implement tool dependency graph.
  - [x] Add `ToolDependency` metadata to `ToolDefinition`.
    - [x] Include required prior data classes.
    - [x] Include produced data classes.
    - [x] Include affected path scope.
  - [x] Build dependency graph from planned tool intents.
  - [x] Reject cyclic tool dependencies.
  - [x] Add tests for dependency ordering.
  - [x] Add tests for cycle rejection.
- [x] Implement tool result cache.
  - [x] Add cache key from tool name, normalized args, session id, and scope.
  - [x] Store read-only tool results only.
  - [x] Add TTL or turn-scoped lifetime.
  - [x] Invalidate path-scoped cache entries on write/edit tool success.
  - [x] Add tests for read cache hit.
  - [x] Add tests for write invalidation.
  - [x] Add tests that write/execute results are not cached.
- [x] Implement parallel read-only tool execution.
  - [x] Group independent read-only tool intents.
  - [x] Run group concurrently under configured parallelism limit.
  - [x] Collect results in original intent order.
  - [x] Emit events for each started/completed tool.
  - [x] Add tests for concurrent execution with deterministic final ordering.
  - [x] Add tests proving write tools are serialized.
- [x] Implement retry classifier.
  - [x] Add `RetryPolicy`.
    - [x] Include max retries.
    - [x] Include transient error classes.
    - [x] Include backoff strategy using testable clock.
  - [x] Classify timeout, rate-limit, and temporary I/O as transient.
  - [x] Classify invalid params, policy denial, and permission denial as permanent.
  - [x] Add tests for transient retry.
  - [x] Add tests for permanent no-retry.
  - [x] Add tests for retry budget exhaustion.
- [x] Implement tool schema compiler.
  - [x] Generate provider-facing tool schemas from `ToolDefinition`.
  - [x] Validate generated schemas include names, descriptions, required fields, and side-effect metadata.
  - [x] Add snapshot tests for built-in tool schemas.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator tool_dependencies` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator tool_cache` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator parallel_tools` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator retries` passes.
- [x] Tests prove policy denials are never retried.

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

- [x] Implement subagent role library.
  - [x] Add built-in `researcher` role.
    - [x] Allow read-only tools.
    - [x] Deny writes and executes.
  - [x] Add built-in `code_reader` role.
    - [x] Allow file/search/symbol tools.
    - [x] Deny writes and executes.
  - [x] Add built-in `implementer` role.
    - [x] Allow writes only within assigned file scopes.
    - [x] Deny terminal execution by default.
  - [x] Add built-in `test_runner` role.
    - [x] Allow configured validation tools.
    - [x] Deny file writes.
  - [x] Add built-in `reviewer` role.
    - [x] Allow read-only and diagnostics tools.
    - [x] Deny writes.
  - [x] Add built-in `summarizer` role.
    - [x] Deny all tools by default.
  - [x] Add tests for default tool scopes of every role.
- [x] Implement fan-out/fan-in coordinator.
  - [x] Split ready independent tasks into subagent requests.
  - [x] Enforce max parallel subagents.
  - [x] Collect child summaries in deterministic task order.
  - [x] Merge completed summaries into parent transcript.
  - [x] Mark parent task blocked if required child fails.
  - [x] Add tests for parallel fan-out deterministic merge.
  - [x] Add tests for child failure blocking parent task.
- [x] Implement subagent result verifier.
  - [x] Require child summary to include cited files/tools when role requires evidence.
  - [x] Check cited files/tools exist in child event log.
  - [x] Reject child memory merge when citations are missing.
  - [x] Add tests for valid cited summary.
  - [x] Add tests for missing citation rejection.
- [x] Implement subagent quarantine.
  - [x] Store failed child output in quarantine state.
  - [x] Exclude quarantined output from normal memory context.
  - [x] Allow parent model to inspect bounded quarantine summary.
  - [x] Add tests that failed child memory is not merged.
- [x] Implement write-scope conflict detector.
  - [x] Track intended file scopes per subagent.
  - [x] Reject overlapping write scopes for concurrent subagents.
  - [x] Lock file scopes during active write task.
  - [x] Release locks after task completion/cancellation.
  - [x] Add tests for overlapping file conflict.
  - [x] Add tests for disjoint file scopes running concurrently.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator subagent_roles` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator fanout_fanin` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator subagent_verifier` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator write_conflicts` passes.
- [x] Tests prove failed subagent memory is quarantined by default.

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

- [x] Implement plan compiler.
  - [x] Parse model plan items into `TaskNode` values.
  - [x] Require task title, action, scope, and expected result.
  - [x] Infer dependencies from explicit model output.
  - [x] Reject cyclic dependencies.
  - [x] Reject tasks without executable action or verification criteria.
  - [x] Add tests for valid plan compilation.
  - [x] Add tests for vague task rejection.
  - [x] Add tests for dependency cycle rejection.
- [x] Implement task readiness scoring.
  - [x] Mark tasks ready only when dependencies complete.
  - [x] Mark tasks blocked when dependency fails.
  - [x] Compute progress percentage from task graph status.
  - [x] Add tests for progress scoring.
- [x] Implement milestone summaries.
  - [x] Generate bounded summary after configured number of events or completed tasks.
  - [x] Store summary in memory with provenance.
  - [x] Drop low-value raw observations after summary when memory pressure exists.
  - [x] Add tests for milestone summary creation.
  - [x] Add tests for compaction under memory pressure.
- [x] Implement issue checklist integration.
  - [x] Parse markdown checklist items from configured issue files.
  - [x] Match completed task criteria to checklist items by stable text or configured key.
  - [x] Require recorded passing validation before marking item complete.
  - [x] Use write/edit tool path to update checklist text.
  - [x] Add tests for checklist parse.
  - [x] Add tests for marking item only after criteria pass.
  - [x] Add tests that failed criteria do not mark item complete.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator plan_compiler` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator progress` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator milestones` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator issue_integration` passes.
- [x] Tests prove vague tasks are rejected before execution.

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

- [x] Implement context pack model.
  - [x] Add `ContextPack`.
    - [x] Include active task summary.
    - [x] Include relevant memory items.
    - [x] Include recent tool summaries.
    - [x] Include file references.
    - [x] Include policy reminders.
    - [x] Include budget snapshot.
    - [x] Include truncation metadata.
  - [x] Add `ContextItemProvenance`.
    - [x] Include source kind.
    - [x] Include source id.
    - [x] Include optional file path/range.
    - [x] Include trust label.
- [x] Implement context pack builder.
  - [x] Score memory relevance by task id, key match, source recency, and explicit dependency.
  - [x] Include policy reminders before untrusted content.
  - [x] Include newest high-value tool summaries.
  - [x] Exclude sensitive items.
  - [x] Enforce byte budget with deterministic truncation.
  - [x] Add tests for relevance ordering.
  - [x] Add tests for byte-budget truncation.
  - [x] Add tests for sensitive exclusion.
- [x] Implement memory compaction and decay.
  - [x] Merge repeated facts with same key and compatible provenance.
  - [x] Decay low-value stale observations.
  - [x] Preserve decisions, constraints, and validation results.
  - [x] Add tests for repeated fact merge.
  - [x] Add tests for preserving decisions during compaction.
- [x] Add optional semantic memory adapter trait.
  - [x] Define trait for external vector/index lookup without adding required embedding dependency.
  - [x] Add fake semantic adapter for tests.
  - [x] Merge semantic results into context pack with provenance.
  - [x] Add tests for adapter disabled behavior.
  - [x] Add tests for fake adapter result inclusion.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator context_pack` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator memory_compaction` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator semantic_memory` passes.
- [x] Tests prove context packs stay within byte budget and preserve provenance.

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

- [x] Implement model router.
  - [x] Add `ModelRoute`.
    - [x] Include route id.
    - [x] Include model adapter id.
    - [x] Include task kind constraints.
    - [x] Include cost/strength tier.
  - [x] Route simple summaries to cheap adapter.
  - [x] Route implementation/review tasks to strong adapter when configured.
  - [x] Route subagent roles to role-specific adapters when configured.
  - [x] Add tests for deterministic route selection.
- [x] Implement rate-limit adapter.
  - [x] Add provider-level semaphore/concurrency limit.
  - [x] Add request-per-window limiter using testable clock.
  - [x] Queue model calls when allowed by timeout budget.
  - [x] Fail fast when queue wait would exceed turn deadline.
  - [x] Add tests for concurrency limit.
  - [x] Add tests for per-window limit with paused time.
  - [x] Add tests for deadline-aware fail-fast behavior.
- [x] Implement streaming model support.
  - [x] Add streaming callback/event type for partial text.
  - [x] Add streaming callback/event type for partial reasoning.
  - [x] Merge streamed chunks into final transcript message.
  - [x] Emit ACP updates through `UpdateSink` as chunks arrive.
  - [x] Add tests for streamed text chunk ordering.
  - [x] Add tests for streamed reasoning chunk ordering.
  - [x] Add tests for stream cancellation.
- [x] Implement tool-call dialect adapters.
  - [x] Add OpenAI/OpenRouter-style function call normalization.
  - [x] Add Anthropic-style tool use normalization.
  - [x] Add local-model JSON tool-call normalization.
  - [x] Reject malformed tool-call dialect payloads with model error.
  - [x] Add fixtures and tests for each dialect.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator model_router` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator rate_limit` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator streaming` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator dialects` passes.
- [x] Tests prove shared provider rate limits apply across subagents.

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

- [x] Implement metrics model.
  - [x] Count model calls.
  - [x] Count tool calls by side-effect class.
  - [x] Count subagent spawns by role.
  - [x] Count cancellations.
  - [x] Count denied policy actions.
  - [x] Count budget-exceeded stops.
  - [x] Count bytes/tokens where known.
  - [x] Add tests for metrics increments.
- [x] Implement decision log.
  - [x] Record strategy decisions with reason codes.
  - [x] Record tool policy decisions with reason codes.
  - [x] Record routing decisions with reason codes.
  - [x] Record subagent delegation decisions with reason codes.
  - [x] Exclude hidden chain-of-thought and sensitive content.
  - [x] Add tests for decision log redaction.
- [x] Add dependency-boundary checks.
  - [x] Assert `ee-agent-orchestrator` does not depend on `ee-agent-host`.
  - [x] Assert `ee-agent-orchestrator` does not depend on `ee-cli`.
  - [x] Assert orchestrator examples/tests remain network-free by default.
- [x] Add default-safety regression suite.
  - [x] Test writes denied by default.
  - [x] Test executes denied by default.
  - [x] Test destructive operations denied by default.
  - [x] Test subagent depth limit default.
  - [x] Test memory byte limit default.
  - [x] Test prompt-injection guard enabled by default.
- [x] Run focused validation commands.
  - [x] Validate format.
  - [x] Validate orchestrator clippy.
  - [x] Validate orchestrator tests.
  - [x] Validate ACP server framework tests when adapter APIs change.
  - [x] Validate OpenRouter tests when model adapter code changes.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator metrics` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator decision_log` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator default_safety` passes.
- [x] `cargo clippy --quiet -p ee-agent-orchestrator --all-targets --all-features -- -D warnings` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator` passes.
- [x] `./scripts/test-workspace-summary.sh` passes after focused validation.

### Phase 11: Subagent model selection

Goal: let delegation choose which provider/model a subagent runs on, with the available models advertised to the delegating model and unknown selections rejected before spawn.

Overview: today every subagent inherits the parent's single injected `ModelAdapter`. This phase adds an optional per-role model selection: the runtime owns a small model registry, `SubagentRole` and the `delegate_task` tool carry an optional model id, and the delegating model sees the available models so it can pick. Default stays the parent adapter when unset. Router and rate-limit machinery land in Phase 9; this phase owns the registry, the delegation surface, and validation.

Rules:

- Default to the parent model when the role does not select one.
- Reject unknown model ids before the child task node is created.
- Advertise the model list to the model without leaking provider secrets or credentials.
- Record the selected model on the child task and in delegation events.
- Keep tests deterministic and network-free with scripted adapters.

#### Work items

- [x] Implement the model registry.
  - [x] Add `ModelRegistry` mapping model ids to `Arc<dyn ModelAdapter>`.
    - [x] Register adapters under explicit ids.
    - [x] Expose the advertised model list (ids plus optional display name/capability hints).
    - [x] Reject duplicate ids.
  - [x] Wire the registry into `OrchestratorRuntime` and `SubagentManager`.
    - [x] Keep single-adapter construction working unchanged (registry with one default entry).
  - [x] Add registry tests.
    - [x] Test duplicate id rejection.
    - [x] Test unknown id lookup fails.
- [x] Extend the delegation surface.
  - [x] Add optional `model` field to `SubagentRole`.
  - [x] Add optional `model` argument to the `delegate_task` tool schema.
  - [x] Add optional `model_id` to `SubagentRequest`.
  - [x] Expose available models to the delegating model.
    - [x] Include the advertised model list in `ModelRequest`.
    - [x] Include the available models in the `delegate_task` tool schema description or an enumerated argument.
  - [x] Resolve the child adapter before the child task node is created.
  - [x] Reject unknown model selection with a deterministic delegation error.
  - [x] Fall back to the parent adapter when no model is selected.
  - [x] Store the selected model id on the child `TaskNode`.
  - [x] Add tests for selection, fallback, and unknown-id rejection.
- [x] Surface routing decisions.
  - [x] Record the selected model id in a delegation event.
  - [x] Include the selected model in the child's `ModelRequest` diagnostic metadata.
  - [x] Add tests asserting the event and metadata for explicit and fallback selections.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-agent-orchestrator model_registry` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator delegate_model` passes.
- [x] Tests prove unknown model selections never create a child task node.
- [x] Tests prove unset selections fall back to the parent adapter.

### Phase 12: LLM compaction slash command

Goal: support ACP-compatible `/compact` so agents can shrink long session context into durable, high-signal summaries without client-side protocol special cases.

Overview: agents advertise `/compact` through `available_commands_update`; the TUI sends `/compact` as a normal `session/prompt`; providers detect the command, ask the configured model for a continuation summary, replace or augment their session context safely, and report what changed. Deterministic memory compaction remains responsible for removals of structured memory; LLM summaries are additive or history-replacing only at provider-owned boundaries.

Rules:

- Keep slash commands agent-advertised and agent-handled; `ee-cli` must not special-case `/compact` behavior beyond normal command display/input.
- Preserve the system prompt, recent useful tail context, decisions, constraints, validation results, and protected memory.
- Do not let LLM output delete protected memory or policy state.
- Redact secret-like values before sending history into compaction prompts or status messages.
- Keep compaction bounded by configurable input bytes, retained tail messages, and request timeout/cancellation.
- Avoid tool calls during compaction unless a later explicit phase needs tool-backed context refresh.
- Tests must be deterministic and network-free with fake/scripted model adapters.

#### Work items

- [x] Add shared slash-command parsing.
  - [x] Parse `/compact` and `/compact <instructions>` only when the first non-space character is `/`.
  - [x] Reject false positives such as `/compactness`.
  - [x] Preserve optional instruction text exactly after command whitespace normalization.
  - [x] Add parser tests for empty, normal prompt, exact command, command with instructions, and prefix collision cases.
- [x] Wire ACP command advertisement.
  - [x] Represent advertised commands as typed `AvailableCommand` values, including description and optional input hint.
  - [x] Emit `available_commands_update` after `session/new` when a provider exposes initial commands.
  - [x] Emit the same command list after `session/load` when restored providers expose initial commands.
  - [x] Advertise `/compact` from `ee-openrouter-agent` simple provider.
  - [x] Advertise `/compact` from `ee-agent-orchestrator` provider adapter.
  - [x] Add ACP server tests proving initial commands reach the client as session updates.
- [x] Implement simple OpenRouter history compaction.
  - [x] Add compaction config knobs for minimum message count, retained tail messages, and maximum compaction input bytes.
  - [x] Build a compaction prompt that asks for user goal, completed work, current state, important files/symbols, decisions/constraints, pending work, validation status, and risks/errors.
  - [x] Bound serialized history included in the compaction request.
  - [x] Redact secret-like values from the compaction request and from emitted status text.
  - [x] Call OpenRouter with no tools for compaction.
  - [x] Replace stored history with compacted summary plus a safe recent tail.
  - [x] Keep tool-call/tool-result pairs consistent when retaining tail messages.
  - [x] Emit a user-visible completion message with before/after message and byte counts.
  - [x] Add tests for no-op small history, bounded input, summary replacement, tail retention, empty-summary rejection, and cancellation.
- [x] Implement orchestrator compaction command.
  - [x] Detect `/compact` before entering the normal model-tool loop.
  - [x] Run deterministic `compact_memory` first.
  - [x] Build a provenance-rich compaction context from task graph, memory, recent events, validation facts, and budget state.
  - [x] Ask the configured model for a compact continuation summary without exposing tools.
  - [x] Store the summary as model-derived session memory without deleting protected keys.
  - [x] Emit report fields for merged duplicates, decayed observations, preserved protected items, summary bytes, and retained context bytes.
  - [x] Add tests proving protected memory survives and normal tools are not invoked.
- [x] Add optional TUI polish without changing protocol behavior.
  - [x] Show advertised command descriptions in an agents command list or footer hint.
  - [x] Use `AvailableCommand.input.hint` as a draft placeholder when cycling slash commands.
  - [x] Add pane tests only for display/input behavior, not compaction semantics.
- [x] Document compaction behavior.
  - [x] Document `/compact` and `/compact <instructions>` usage for OpenRouter agent users.
  - [x] Document compaction config environment variables.
  - [x] Explain that client sends `/compact` as a normal ACP prompt and provider owns history/memory changes.
  - [x] Document security limits: redaction, protected memory preservation, bounded input, and no tool calls.

#### Actionable criteria

- [x] `cargo test --quiet -p ee-acp-agent-server available_commands` passes.
- [x] `cargo test --quiet -p ee-openrouter-agent compact` passes.
- [x] `cargo test --quiet -p ee-agent-orchestrator compact` passes.
- [x] `cargo fmt --check -p ee-acp-agent-server -p ee-openrouter-agent -p ee-agent-orchestrator` passes.
- [x] `cargo clippy -p ee-acp-agent-server -p ee-openrouter-agent -p ee-agent-orchestrator --all-targets --all-features -- -D warnings` passes.
- [x] Tests prove `/compact` reaches providers as normal prompt text and no `ee-cli` compaction special case exists.
- [x] Tests prove compaction cannot remove protected decisions, constraints, or validation memory.
- [x] Tests prove compaction requests are bounded and network-free under test.

## Host-Bound Encrypted Secrets Store

### Phase 1: Secret-store contract and dependency boundary

Goal: establish a testable, frontend-local secrets API without adding editor-backend dependencies or exposing plaintext through configuration models.

Overview: add a private `secrets` module to `ee-cli`, keep its public surface limited to typed names, opaque references, and store operations, and inject host/keychain dependencies behind traits so all tests run without real platform secrets or machine identity.

Rules:

- Keep all secret-store code in `crates/ee-cli`; `xi-core-lib` remains config- and frontend-agnostic.
- Use a random 256-bit vault key stored in OS secure storage; never derive encryption material from a host fingerprint alone.
- Treat host fingerprint only as authenticated binding data; it is not a cryptographic secret.
- Never accept a secret value as positional CLI argument, config plaintext expansion, log field, debug output, or error interpolation.
- Use exact `secret://<name>` references only; do not support interpolation, concatenation, environment expansion, or recursive references.
- Make tests deterministic and network-free through fake keychain, fingerprint, filesystem, and terminal-input implementations.

#### Work items

- [x] Add private `mod secrets;` wiring in `crates/ee-cli/src/main.rs` and create `crates/ee-cli/src/secrets.rs`.
  - [x] Define `SecretName`, `SecretReference`, `SecretStore`, `SecretStoreError`, `HostBinding`, and `Keychain` types/traits in that module.
  - [x] Keep plaintext-bearing methods crate-private and expose secret values only as `Zeroizing<String>` or equivalent zeroizing buffers.
  - [x] Add unit tests proving invalid names fail before any keychain or filesystem operation.
- [x] Define one canonical secret-name grammar.
  - [x] Accept ASCII letters, ASCII digits, `.`, `_`, and `-`.
  - [x] Require first character to be an ASCII letter or digit.
  - [x] Enforce a 128-byte maximum name length.
  - [x] Reject empty names, whitespace, `/`, `\`, `:`, control characters, `..`, and names outside the grammar.
  - [x] Add table-driven tests for accepted and rejected boundary cases.
- [x] Define exact `secret://<name>` parsing and rendering.
  - [x] Require scheme lowercase `secret` and exactly one non-empty path segment.
  - [x] Reject authority components, query strings, fragments, percent-decoding, leading/trailing whitespace, and embedded secret URIs.
  - [x] Render accepted references in canonical `secret://<name>` form.
  - [x] Add parser/renderer round-trip and malformed-reference tests.
- [x] Add workspace dependency declarations required by implementation.
  - [x] Add `chacha20poly1305`, `keyring`, `sha2`, and `zeroize` with versions/features compatible with Rust 1.95.
  - [x] Add a terminal secret-input dependency only if `crossterm` cannot provide hidden input safely on every supported platform.
  - [x] Add a compile-backed unit test target that constructs the module with test doubles and proves no platform keychain backend is contacted.

#### Acceptance criteria

- [x] `cargo test --quiet -p ee-cli secrets::` passes.
- [x] Tests prove invalid secret names and malformed references cause zero keychain and filesystem calls.
- [x] Tests prove parsed secret references round-trip to one canonical string form.
- [x] `cargo clippy -p ee-cli --all-targets --all-features -- -D warnings` passes with new dependencies.

### Phase 2: Host binding and OS-secure vault-key lifecycle

Goal: bind every vault cryptographically to current host while retaining a non-derivable random encryption key protected by current OS user account.

Overview: derive a versioned SHA-256 host-binding digest from one stable platform machine identifier, load or create a random 32-byte vault key through OS keychain, and fail closed for unavailable keychain, unavailable host identity, or mismatch.

Rules:

- On macOS use platform UUID; on Linux use `/etc/machine-id`; on Windows use `MachineGuid`.
- Hash canonical binding bytes with domain separation before writing them to disk or using them as associated data.
- Store only host-binding digest and format version in vault metadata; never store raw machine identifiers.
- Use one random vault key per current OS-user plus secrets-store namespace, never a deterministic key.
- Do not add recovery, export, fingerprint override, or host-migration bypass paths in this feature.
- Map unavailable secure-store or host-binding sources to explicit safe errors without including credential material or raw host IDs.

#### Work items

- [x] Implement `HostBinding::current()` with platform-specific identifier readers and a common digest function.
  - [x] Prefix digest input with fixed domain separator `ee-secrets-host-binding-v1`.
  - [x] Trim only platform file line endings before hashing; preserve all other identifier bytes.
  - [x] Return a typed unavailable error when platform identity cannot be read or is empty.
  - [x] Add fake-source tests proving equal canonical identifiers yield equal digests and distinct identifiers yield distinct digests.
- [x] Implement a keychain adapter using service name `ee-secrets-v1` and one deterministic account name per current user-store namespace.
  - [x] Encode exactly 32 random bytes as stable text for keychain persistence.
  - [x] Validate decoded key length before use and classify malformed keychain content as corruption.
  - [x] Generate with OS CSPRNG only when keychain lookup reports absent.
  - [x] Persist newly generated key before returning it to caller.
  - [x] Add fake-keychain tests for load, create-once, malformed key, read failure, and write failure paths.
- [x] Bind vault-key use to host digest without leaking either input.
  - [x] Require both loaded vault key and current host digest before encrypt/decrypt operations.
  - [x] Return `HostBindingMismatch` only after authenticated verification fails against vault metadata.
  - [x] Ensure mismatch errors contain vault version and safe remediation text but no fingerprint digest, key, ciphertext, or plaintext.
  - [x] Add tests proving a copied vault plus different host digest cannot be opened even when same fake keychain key is present.

#### Acceptance criteria

- [x] `cargo test --quiet -p ee-cli secrets::host_binding` passes.
- [x] `cargo test --quiet -p ee-cli secrets::keychain` passes.
- [x] Tests prove key generation occurs exactly once for a missing key and never for an existing valid key.
- [x] Tests prove different host bindings fail closed without exposing either identifier or key bytes in error strings.

### Phase 3: Authenticated encrypted vault format and durable persistence

Goal: store named secrets in versioned, authenticated ciphertext with restrictive permissions and crash-safe replacement semantics.

Overview: persist a single XChaCha20-Poly1305 encrypted vault under user data directory, bind each record to vault version, host digest, and canonical secret name through associated data, and replace the file atomically.

Rules:

- Use XChaCha20-Poly1305 with a fresh 24-byte nonce for every record write.
- Store ciphertext, nonce, version, and host-binding digest only; never plaintext, encryption key, or raw host identifier.
- Include record name, vault format version, and host-binding digest in AEAD associated data.
- Reject unknown future vault versions, duplicate names, malformed base64, invalid nonce lengths, and trailing/unrecognized record fields.
- Place vault at `dirs::data_dir()/ee/secrets/v1.json`; create parent directories with mode `0700` and vault file mode `0600` on Unix.
- Write through a same-directory temporary file, flush file and parent directory where platform supports it, then atomically rename.

#### Work items

- [x] Define strict version-1 vault serialization types.
  - [x] Include `version`, `host_binding_digest`, and sorted secret-record collection.
  - [x] Include per-record canonical name, nonce, and ciphertext fields.
  - [x] Derive only serialization traits required for storage; do not derive `Debug` for types holding plaintext or vault keys.
  - [x] Add deserialize tests rejecting unknown fields, duplicate names, future versions, malformed encoded values, and invalid nonce/key sizes.
- [x] Implement AEAD encrypt/decrypt functions.
  - [x] Generate a new random nonce for each `set` call, including replacement of same name.
  - [x] Construct canonical associated-data bytes from version, host digest, and exact canonical name.
  - [x] Zeroize temporary plaintext and key copies after each operation.
  - [x] Map authentication failure to corruption/mismatch error without revealing whether record existed before verification.
  - [x] Add round-trip, nonce-uniqueness, swapped-name, modified-ciphertext, modified-nonce, and modified-host-digest tests.
- [x] Implement private data-path resolution and file permission enforcement.
  - [x] Resolve only through `dirs::data_dir()` and append `ee/secrets/v1.json`.
  - [x] Return a typed error when data directory cannot be resolved.
  - [x] Create missing parent directories before first write.
  - [x] On Unix, explicitly set parent mode `0700` and file mode `0600` after creation and replacement.
  - [x] Add temp-directory tests for path construction, first-write directory creation, and Unix modes under `#[cfg(unix)]`.
- [x] Implement atomic vault read-modify-write operations.
  - [x] Read and validate full existing vault before mutating any record.
  - [x] Sort records by canonical name before serialization for deterministic list output and stable diffs.
  - [x] Write complete serialized content to a unique temp file in vault directory.
  - [x] Flush temp content, rename only after successful write, and remove failed temp files best-effort.
  - [x] Add failure-injection tests proving an interrupted write preserves old readable vault and does not leave a valid partial replacement.
- [x] Implement `set`, `get`, `list`, and `delete` store operations.
  - [x] Make `set` replace only exact canonical name and retain unrelated records byte-semantically after reserialization.
  - [x] Make `get` return `NotFound` for missing name without creating vault/keychain entries.
  - [x] Make `list` decrypt no record plaintext and return sorted names only.
  - [x] Make `delete` remove only exact name and return `NotFound` without rewriting when absent.
  - [x] Add operation tests with multiple records, replacements, absent names, and corruption before mutation.

#### Acceptance criteria

- [x] `cargo test --quiet -p ee-cli secrets::vault` passes.
- [x] Tests prove stored vault JSON contains neither plaintext values nor raw host identifiers.
- [x] Tests prove tampering with ciphertext, nonce, record name, or host digest prevents decryption.
- [x] Tests prove failed writes retain prior decryptable data and Unix vault permissions are owner-only.

### Phase 4: `ee do secrets` command interface

Goal: provide a safe, scriptable command interface that creates, reads, lists, deletes, and diagnoses encrypted secrets without leaking values through normal terminal use.

Overview: add a `Secrets` branch under existing `DoCommands`, route every subcommand through `SecretStore`, require hidden terminal entry by default, and reserve explicit `--stdin`/`--force` paths for automated workflows.

Rules:

- Keep command surface under `ee do secrets`; do not add top-level commands.
- Read interactive values with terminal echo disabled; never use command-line value arguments.
- Permit `--stdin` only as explicit opt-in and cap input at 64 KiB.
- Emit raw secret from `get` only to non-terminal stdout unless caller passes `--force`.
- Write diagnostics/errors to stderr and never include secret values, ciphertext, keys, host digest, or raw machine ID.
- `list` returns names only; `status` returns only safe state/path/count information.

#### Work items

- [x] Add `DoCommands::Secrets` and typed `SecretsCommands` definitions in `crates/ee-cli/src/main.rs`.
  - [x] Add `set <name> [--stdin]`, `get <name> [--force]`, `list`, `delete <name>`, and `status` subcommands.
  - [x] Mark secret values absent from clap positional/option definitions.
  - [x] Add parser tests covering every command, all flags, and invalid flag combinations.
- [x] Implement safe secret input for `set`.
  - [x] Read hidden input from controlling terminal when `--stdin` is absent.
  - [x] Read at most 64 KiB from standard input when `--stdin` is present.
  - [x] Remove exactly one final line ending from stdin input and preserve all other bytes.
  - [x] Reject empty input after required final-line-ending normalization.
  - [x] Add fake-input tests for hidden-terminal selection, stdin selection, oversize rejection, newline handling, and empty value rejection.
- [x] Implement command dispatch and output behavior.
  - [x] Route `set`, `get`, `list`, `delete`, and `status` through one store-construction path.
  - [x] Print only acknowledgement/name for `set` and `delete`.
  - [x] Print sorted names one per line for `list`.
  - [x] Refuse `get` when stdout is a terminal and `--force` is absent.
  - [x] Print exact raw value plus one newline only when `get` is permitted.
  - [x] Add captured-stdio tests for stdout/stderr separation and safe error messages.
- [x] Implement safe status reporting and exit classification.
  - [x] Report vault path, vault presence, record count when readable, keychain availability, and host-binding verification state.
  - [x] Distinguish not found, user-input error, unavailable keychain, unavailable host binding, corrupted vault, host mismatch, and I/O failure with stable non-zero exits.
  - [x] Add tests asserting each class has deterministic exit status and excludes seeded secret text from output.

#### Acceptance criteria

- [x] `cargo test --quiet -p ee-cli cli_utility_commands_live_under_do` passes.
- [x] `cargo test --quiet -p ee-cli secrets_command` passes.
- [x] Tests prove `set` cannot receive a secret through CLI argument parsing.
- [x] Tests prove terminal `get` fails without `--force`, while piped `get` emits only raw value on stdout.
- [x] Tests prove all command errors redact seeded secret values.

### Phase 5: Trusted global config references and agent launch resolution

Goal: allow user-global agent configuration to supply a stored API key at launch while preventing workspace configuration from selecting or exfiltrating host secrets.

Overview: preserve config-layer provenance for agent environment values, retain `secret://` text in config inspection output, resolve approved references only immediately before ACP agent process construction, and feed resolved values into existing redaction collection.

Rules:

- Resolve secret references only in user XDG config or legacy user config fallback; reject them from system and ancestor workspace config layers.
- Keep project `.ee.toml` able to configure non-secret agent settings but unable to reference globally stored secrets.
- Preserve `secret://<name>` in `ee do config show` and `ee do config get`; never resolve references during display, schema generation, parsing, or validation-only paths.
- Resolve approved references only at agent process launch, after final config merge and before `AgentProcessConfig` creation.
- Abort whole target agent launch if any required referenced secret is missing, unreadable, host-mismatched, or corrupt.
- Initial integration scope is `[agents.servers.<id>.env]`; do not resolve MCP environment values or HTTP headers in this phase.

#### Work items

- [x] Add typed agent-environment value representation that preserves literal versus secret-reference origin.
  - [x] Replace raw merge-only representation with an internal value carrying source `ConfigLayerKind` and raw text/reference state.
  - [x] Preserve existing shallow merge behavior for ordinary literal environment values.
  - [x] Ensure higher-priority non-secret values replace lower-priority values exactly as before.
  - [x] Add merge tests for global literal, global secret reference, project literal override, and project secret-reference rejection.
- [x] Validate secret references against config provenance during layer merge.
  - [x] Parse exact secret URI values using shared `SecretReference` parser.
  - [x] Reject malformed `secret://` values in agent env with field path and safe reference text.
  - [x] Reject otherwise valid references from system or ancestor layers with field path and source-layer label.
  - [x] Leave normal strings containing `secret://` as literals unless entire value matches reference grammar.
  - [x] Add config-file tests for global accept, legacy-user accept, system reject, ancestor reject, malformed reject, and literal preservation.
- [x] Resolve agent secret references immediately before host configuration assembly.
  - [x] Add resolver accepting final `AgentServerSettings` typed values and `SecretStore` dependency.
  - [x] Return `BTreeMap<String, String>` only after all required references resolve successfully.
  - [x] Resolve into zeroizing temporary buffers and clone only final process-env values required by child spawn API.
  - [x] Fail target agent host initialization before spawning a child when any reference resolution fails.
  - [x] Add fake-agent-host and fake-secret-store tests proving resolved `OPENROUTER_API_KEY` reaches `AgentProcessConfig.env`.
- [x] Preserve redaction coverage for resolved environment values.
  - [x] Ensure `App::agents_secret_values()` receives resolved secret-like agent env values after launch configuration is constructed.
  - [x] Ensure error/status/stderr paths pass resolved values to existing `ee_agent_host::redact::redact_secret_values` logic.
  - [x] Add agent pane tests proving seeded resolved API key is replaced with `***` in stderr and never appears in diagnostics.
- [x] Update schema generation source metadata for agent environment secret references.
  - [x] Add schema description explaining exact `secret://<name>` syntax and user-global-only resolution boundary.
  - [x] Regenerate `schemas/ee-config.schema.json` through existing schema generator.
  - [x] Add schema test asserting agent `env` documentation exposes supported reference syntax without declaring new config shape.

#### Acceptance criteria

- [x] `cargo test --quiet -p ee-cli config_schema_includes_agents_and_mcp_fields` passes.
- [x] `cargo test --quiet -p ee-cli configured_secret_values_are_collected_for_redaction` passes.
- [x] `cargo test --quiet -p ee-cli secret_reference` passes.
- [x] Tests prove global `OPENROUTER_API_KEY = "secret://openrouter-api-key"` resolves only at agent launch.
- [x] Tests prove ancestor workspace configs cannot resolve, override with, or cause launch of secret references.
- [x] Tests prove `config show` and `config get` expose URI reference only, never seeded secret plaintext.

### Phase 6: End-to-end regression coverage and repository validation

Goal: lock behavior across CLI, vault, config merging, and agent launch so future refactors cannot weaken confidentiality, host binding, or workspace trust boundaries.

Overview: assemble end-to-end test fixtures from fake host/keychain/input/process layers, exercise OpenRouter API-key configuration through agent launch preparation, and run repository formatting, lint, schema, and package tests.

Rules:

- End-to-end tests must use fakes only; never access developer keychain, machine ID, network, or real OpenRouter endpoint.
- Seed tests with recognizable secret values and assert none appear in captured stdout, stderr, diagnostics, serialized config, or vault metadata.
- Test host mismatch and unavailable secure storage as normal expected failure modes.
- Keep existing plaintext environment configuration behavior unchanged.
- Do not add documentation-only, manual test, migration, export, or recovery tasks to this phase.

#### Work items

- [x] Add an end-to-end encrypted-secret-to-agent-env fixture.
  - [x] Create secret through fake CLI input and fake keychain/host binding.
  - [x] Configure global agent env with `OPENROUTER_API_KEY = "secret://openrouter-api-key"`.
  - [x] Build lazy agent host configuration without spawning a real process.
  - [x] Assert final agent env contains seeded plaintext only within fake process configuration.
  - [x] Assert captured config output, status output, agent stderr, and vault JSON omit seeded plaintext.
- [x] Add negative end-to-end fixtures.
  - [x] Assert missing referenced secret prevents fake process creation.
  - [x] Assert copied vault under different fake host binding prevents fake process creation.
  - [x] Assert unavailable keychain prevents fake process creation.
  - [x] Assert malformed/corrupt vault prevents fake process creation.
  - [x] Assert project config reference rejection prevents fake process creation while project literal env remains supported.
- [x] Add regression tests for legacy config behavior.
  - [x] Assert a literal `OPENROUTER_API_KEY` value remains accepted and reaches launch configuration unchanged.
  - [x] Assert an agent without any secret references launches through existing configuration path.
  - [x] Assert existing agent secret redaction tests retain behavior for literal secret-like env values.
- [x] Run generated-schema and package validation coverage.
  - [x] Add test asserting checked-in schema matches generated schema after reference documentation update.
  - [x] Add targeted test commands to existing CI-compatible package test set where command tests are organized.

#### Acceptance criteria

- [x] `cargo fmt --check` passes.
- [x] `cargo clippy -p ee-cli --all-targets --all-features -- -D warnings` passes.
- [x] `cargo test --quiet -p ee-cli` passes.
- [x] `cargo test --quiet -p ee-agent-host` passes.
- [x] `cargo run --quiet -p ee-cli -- do schema check` passes.
- [x] End-to-end tests prove a global OpenRouter secret reference reaches agent launch configuration but never any captured user-visible output.
- [x] End-to-end tests prove keychain failure, host mismatch, vault corruption, missing secret, and workspace reference each fail before child process creation.

## Unified Host-Local Workspace Trust Policy

Goal: reduce repetitive approval prompts with one bounded trust engine while preventing repository content, unknown tools, external paths, sensitive data, destructive operations, and malformed rules from granting authority.

Overview: replace independent command, MCP, and workspace trust implementations with one policy foundation. Rule matching remains operation-specific, but every rule shares host-local persistence, canonical workspace binding, agent scope, session-deny precedence, expiration, budgets, redaction, atomic persistence, explicit trust-store reload, and fail-closed behavior.

Rules:

- This is authoritative trust implementation plan for persistent workspace tool trust.
- Persistent grants are stored in host-local state, never repository-controlled `ee.toml`, XDG project config, system config, or agent-provided files.
- Host-local trust store is keyed by canonical workspace identity; copying repository config or trust files to another workspace grants nothing.
- Project configuration has no trust or capability-request fields in version one; host-local trust store alone controls authority.
- Every operation begins with validated normalized identity. Missing identity, unknown category, malformed config, invalid path, expired rule, exhausted rule, or tool metadata mismatch returns prompt.
- Session deny takes precedence over every persistent or session allow.
- Persistent grants are allow-only. Persistent deny is out of scope; deny once/session remains current in-memory behavior.
- Delete, rename, chmod, symlink creation, binary writes, secret access, VCS mutation, package install/script execution, publish, non-GET network access, and unknown tools remain prompt-only.
- Rule evaluator is pure. It does not write files, dispatch tools, consume budgets, mutate UI, or access system clock.

### Phase 1: Establish shared policy contracts and host-local trust store

Goal: provide one secure foundation before adding any persistent trust rule type.

Overview: create common operation, rule-scope, decision, persistence, and lifecycle contracts. Migrate existing session approval policy to use common precedence without changing current once/session behavior.

#### Trust-store schema

Host-local per-workspace store uses versioned TOML. This file is application-owned state, not project configuration:

```toml
schema_version = 1

[workspace]
# SHA-256("ee.workspace.v1\\0" + canonical_workspace_root_path_bytes).
# Store filename uses same digest. Moving workspace requires fresh approval.
identity = "sha256:…"

[policy]
workspace_enabled = false

[[command_allow]]
id = "cmd_…"
agent = "openrouter"                 # optional
executable = "git"
match = "argv_prefix"                # `argv_exact` | `argv_prefix`
argv = ["status"]
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[mcp_allow]]
id = "mcp_…"
agent = "openrouter"                 # optional
server = "ee"
transport_identity = "stdio:…"
tool = "ee_read_file"
tool_schema_version = 1
arguments_json = "{\"path\":\"src/main.rs\"}"
expires_at = "2026-08-08T12:00:00Z" # required for write/execute
max_uses = 20                         # required for write/execute

[[read_path_allow]]
id = "read_…"
agent = "openrouter"                 # optional
path_prefix = "src"
max_bytes = 262144

[[mcp_read_allow]]
id = "mcp_read_…"
agent = "openrouter"                 # optional
server = "ee"
transport_identity = "stdio:…"
tool = "ee_read_file"
tool_schema_version = 1
path_prefix = "src"
max_bytes = 262144

[[profile_allow]]
id = "profile_…"
agent = "openrouter"                 # optional
profile = "git_readonly"
expires_at = "2026-08-08T12:00:00Z"
max_uses = 20

[[write_allow]]
id = "write_…"
agent = "openrouter"                 # optional
operation = "create"                 # `create` | `modify`
path_prefix = "src/generated"
max_files = 5
max_total_bytes = 65536
max_file_bytes = 16384
expires_at = "2026-08-08T12:00:00Z"
max_uses = 5
```

Schema rules:

- `schema_version` is required and currently must equal `1`; unsupported versions load no effective rules and prompt.
- `workspace.identity` is required and must equal `SHA-256("ee.workspace.v1\\0" + canonical_workspace_root_path_bytes)` recomputed from current workspace; canonical root bytes use platform-native path encoding and are never serialized directly.
- `id` is generated stable identifier unique across all rule arrays. Conflicting duplicate ids invalidate every conflicting entry; unique valid entries continue loading.
- Rule array determines typed matcher. Unknown and cross-kind fields are rejected with `deny_unknown_fields`; they are never ignored.
- Every rule array supports optional `agent`, `expires_at`, and `max_uses`; variant validation decides whether expiry/use are mandatory.
- Optional `agent` scopes a rule to one configured agent; missing `agent` scopes rule to any configured agent in matching workspace.
- `argv_exact` may use `argv = []`; `argv_prefix` requires non-empty `argv`.
- `arguments_json` must parse as JSON object from validated arguments. Loader canonicalizes whitespace and object-key ordering before matching and serialization, but rejects duplicate keys, non-object values, sensitive data, binary attachments, and oversized payloads.
- `path_prefix` is workspace-relative canonical path segment sequence. Empty, root-wide, absolute, traversal, glob, regex, and protected prefixes are invalid.
- `expires_at` and finite `max_uses` are mandatory for `command_allow`, MCP `write`/`execute`, `profile_allow`, and `write_allow`; eligible read rules may omit both.
- Runtime usage counters are session-local and are never written into trust-store TOML.
- Linux trust directory mode is `0700`; trust document and temporary document modes are `0600`. Broader Unix modes reject loading. Non-Unix platforms must implement equivalent owner-only ACL verification before enabling persistent trust.
- Store serialization emits canonical key order and never emits raw workspace paths, secrets, environment values, file contents, or MCP argument previews beyond canonical `arguments_json` permitted by validation.

Rules:

- Trust state directory uses platform-local state location, such as `$XDG_STATE_HOME/ee/trust/` on Linux.
- Store filename derives from `SHA-256("ee.workspace.v1\\0" + canonical_workspace_root_path_bytes)`; raw workspace path never appears in filename or store document.
- Stored document contains schema version, hashed workspace identity, created metadata, and typed rule arrays.
- Store loader rejects workspace-identity mismatch, unsupported schema version, malformed entries, unsafe file permissions, or non-regular files.
- Store writer uses unique sibling temporary file, flush, atomic rename, restrictive permissions, and cleanup on every failure path.

#### Work items

- [x] Define shared trust-domain types in dedicated policy module.
  - [x] Add `TrustOperation` containing canonical workspace identity, optional agent id, transport, category, and validated operation-specific identity.
  - [x] Add categories `read`, `write_create`, `write_modify`, `execute`, and `unknown`.
  - [x] Add `TrustRuleScope` containing workspace identity, optional agent id, optional expiration, and optional maximum-use budget.
  - [x] Add `TrustDecision::{Allow, Prompt}` with redacted machine-readable reason and optional stable rule id.
  - [x] Add `TrustRule` tagged enum for command, MCP exact invocation, read path, write path, and curated profile variants.
- [x] Implement pure shared policy evaluator.
  - [x] Accept immutable effective rule set, session policy state, injected current time, injected usage snapshot, and normalized operation.
  - [x] Evaluate session deny before every persistent rule.
  - [x] Reject malformed, cross-workspace, cross-agent, expired, and exhausted rules before domain matcher execution.
  - [x] Delegate only operation-specific comparison to typed matchers.
  - [x] Return prompt when no validated rule matches without mutating usage state.
- [x] Implement host-local trust-store load and atomic write APIs.
  - [x] Derive store path using existing canonical path helper and exact `SHA-256("ee.workspace.v1\\0" + canonical_workspace_root_path_bytes)` input contract.
  - [x] Persist workspace identity inside document and verify it on every load.
  - [x] Create Linux trust directory with mode `0700` and document/temporary files with mode `0600`; fail closed on broader mode.
  - [x] Add platform abstraction that enables persistent trust on non-Unix only after owner-only ACL verification succeeds.
  - [x] Reject symlink, directory, group-writable, world-writable, and non-regular store paths.
  - [x] Implement append-or-reuse behavior using stable rule id to prevent duplicate grants.
  - [x] Return typed errors for identity mismatch, permission failure, parse failure, validation failure, write failure, rename failure, and unsupported platform ACL verification.
  - [x] Deserialize only schema version `1` and serialize every accepted rule back in canonical schema order.
  - [x] Reject unknown rule fields, cross-kind fields, duplicate rule ids, and invalid enum values rather than silently dropping them.
- [x] Add trust-store schema compatibility tests.
  - [x] Assert every documented rule array round-trips through parse, validation, canonical serialization, and reload.
  - [x] Assert unsupported schema version, missing workspace identity, cross-workspace identity, duplicate id, cross-kind field, invalid match mode, and invalid write operation produce no effective rule.
  - [x] Assert `argv_exact = []` is accepted while `argv_prefix = []` is rejected.
  - [x] Assert runtime usage counters do not appear in serialized trust-store document.
- [x] Adapt existing session approval policy to shared precedence contract.
  - [x] Preserve allow-once, allow-session, deny-once, and deny-session user-visible behavior.
  - [x] Expose session deny/allow lookup as injected evaluator input.
  - [x] Keep session state in memory and clear it on existing session teardown path.
- [x] Add foundation tests.
  - [x] Assert every unknown operation prompts.
  - [x] Assert session deny overrides matching persistent allow.
  - [x] Assert evaluator performs no filesystem, process, transport, UI, clock, or counter mutation.
  - [x] Assert copied store document and copied store file fail workspace-identity validation after canonical workspace root changes.
  - [x] Assert malformed, symlinked, insecure-permission, and cross-workspace stores yield empty effective rules and prompt.
  - [x] Assert failed write preserves prior store bytes and removes temporary file.

#### Actionable criteria

- `cargo test --quiet -p ee-cli trust_policy_foundation` passes.
- Automated store tests prove repository content cannot create effective persistent grants.
- Automated precedence tests prove session deny always overrides every persistent allow variant.

### Phase 2: Add exact structured terminal command trust

Goal: let user permanently approve one bounded terminal command for one host-local workspace without authorizing shell interpretation or arbitrary executable arguments.

Overview: implement command rule matcher as first shared-policy adapter. Terminal requests still use existing process ownership, output redaction, timeout, cancellation, and approval response paths.

Rules:

- Command identity is executable token plus structured argv tokens from `CreateTerminalRequest`; display text is never matched.
- Persistent execute rule must use either exact argv or non-empty argv prefix; command-only trust is prohibited.
- Exact empty argv is allowed only for an explicit `args_exact = []` rule created from a no-argument request.
- Shell wrappers `sh`, `bash`, `zsh`, `fish`, `dash`, `cmd`, `powershell`, and `pwsh` are ineligible.
- Request cwd must resolve to canonical workspace root; explicit external, relative, traversal, or symlink-escape cwd prompts.
- Execute rules require finite maximum-use budget and expiration.

#### Work items

- [x] Add command rule and invocation types.
  - [x] Define `CommandRule` with optional agent id, executable, explicit match mode, argv tokens, workspace scope, expiry, and maximum use.
  - [x] Define `CommandInvocation` from validated `CreateTerminalRequest` command, args, and canonical cwd.
  - [x] Reject control characters, empty tokens, invalid Unicode boundaries, shell wrappers, and command-only prefix rules during rule creation and load.
- [x] Implement pure command matcher.
  - [x] Match agent scope, canonical workspace identity, executable token, and explicit exact/prefix argv semantics.
  - [x] Require every prefix rule to contain at least one argument token.
  - [x] Return rule metadata only; do not spawn terminal or mutate approval state.
- [x] Integrate command policy into terminal approval path.
  - [x] Normalize terminal request after current request validation and before approval queue insertion.
  - [x] Evaluate shared policy with session deny, time, and usage state.
  - [x] Dispatch existing terminal creation only after allow decision.
  - [x] On `Allow for 1 hour / 20 uses`, derive narrow rule from full current argv, persist host-local rule, activate rule, then dispatch original request.
  - [x] On persistence failure, return current permission error and do not spawn terminal.
- [x] Add command trust tests.
  - [x] Assert `git status` exact/prefix rule matches only intended structured argv.
  - [x] Assert `git commit`, `git reset`, `git clean`, `sh -c`, external cwd, relative cwd, traversal, and symlink escape prompt.
  - [x] Assert command-only `git` rule is rejected.
  - [x] Assert trusted command preserves output cap, cancellation, timeout, ownership, and no-secret-display behavior.
  - [x] Assert persistence failure leaves terminal unspawned and usage budget unchanged.

#### Actionable criteria

- `cargo test --quiet -p ee-cli command_trust` passes.
- Automated terminal tests prove shell and mutable Git operations never bypass approval through command trust.
- Automated persistence tests prove command grants exist only in host-local workspace trust store.

### Phase 3: Add exact generic MCP invocation trust

Goal: let user permanently approve one validated MCP tool invocation without trusting an entire server, tool, or argument pattern.

Overview: implement MCP rule adapter using server identity, tool identity, schema version, canonical workspace scope, optional agent scope, and canonical exact JSON arguments.

Rules:

- Generic MCP trust never applies to terminal-create; terminal creation uses command trust only.
- Rule matches exact server id, transport identity, tool name, manifest schema version, and full canonical JSON object arguments.
- Rule creation runs after server identity, tool schema validation, and side-effect classification.
- Unknown server, unknown tool, missing manifest, unknown side-effect class, or schema-version mismatch prompts.
- Rule arguments must be JSON object, bounded in size, free of duplicate object keys, and free of secret-like keys/values or binary attachments.
- Execute and write MCP rules require finite expiry and use budget.

#### Work items

- [x] Add canonical MCP invocation and rule types.
  - [x] Define `McpInvocation` with agent id, server id, transport identity, tool name, manifest schema version, side-effect class, canonical workspace identity, and canonical JSON bytes.
  - [x] Define `McpExactRule` with same matching fields plus common scope.
  - [x] Canonicalize JSON by recursively sorting object keys while retaining array order.
  - [x] Parse with duplicate-key detection and reject top-level non-object values.
- [x] Implement MCP exact matcher.
  - [x] Match every identity field and canonical argument bytes exactly.
  - [x] Return no match for changed nested field, changed array order, changed server transport identity, changed schema version, or changed workspace/agent scope.
  - [x] Keep matcher independent from MCP transport dispatch.
- [x] Integrate MCP policy into stdio and ACP-native routes.
  - [x] Build invocation only after existing manifest/schema validation succeeds.
  - [x] Evaluate shared policy before approval queue insertion.
  - [x] Offer `Allow for 1 hour / 20 uses` only for eligible validated generic MCP prompt.
  - [x] Persist exact invocation rule to host-local store before dispatch.
  - [x] Render only server, tool, side-effect class, and redacted arguments in UI/transcript/status.
- [x] Add MCP trust tests.
  - [x] Assert identical invocation through stdio proxy and ACP-native route bypasses prompt after grant.
  - [x] Assert changed server, transport, tool, schema version, agent, workspace, nested value, or array order prompts.
  - [x] Assert secret-like, malformed, oversized, binary, duplicate-key, and non-object arguments never offer persistent allow.
  - [x] Assert terminal-create and unknown-side-effect tools cannot use MCP exact trust.

#### Actionable criteria

- `cargo test --quiet -p ee-mcp` passes.
- Automated MCP tests prove no server-wide, tool-wide, partial-argument, or cross-transport bypass exists.
- Automated UI tests prove sensitive MCP arguments cannot be persisted or displayed by trust flow.

### Phase 4: Add workspace-gated read trust and curated validation profiles

Goal: eliminate recurring prompts for bounded source reads and common validation commands after explicit workspace-local user approval.

Overview: add low-risk capability rules on shared engine. Workspace gate enables evaluation only; each read path/tool/profile remains separately constrained.

Rules:

- Workspace gate is stored host-local and defaults disabled.
- Workspace gate alone never permits an operation.
- Read rules require canonical in-workspace paths and deny secret-like, credential, private-key, secret-store, external, traversal, and symlink-escape paths.
- MCP read rules require ee-pinned manifest classification `read`, matching server/tool/schema, and validated bounded argument profile.
- Curated profile registry is application-owned and versioned; config stores profile ids only.
- Profiles include only `git status`, `git diff`, `git log`, `git show`, `git branch --show-current`, `cargo fmt --check`, `cargo test --quiet`, and `cargo clippy` with fixed safety flags.
- Profiles do not include VCS mutation, package install, package scripts, publish, network, or shell commands.

#### Work items

- [x] Add host-local workspace gate and read rule types.
  - [x] Define explicit workspace gate setting in trust-store document defaulting false.
  - [x] Define native read rules using canonical workspace-relative path prefixes and bounded read limits.
  - [x] Define `McpReadRule` using server, transport identity, tool, tool schema version, canonical workspace-relative path prefix, and bounded byte/result limits.
  - [x] Reject root-wide path prefix, globs, regex, absolute paths, traversal segments, protected paths, and unbounded limits.
- [x] Add protected-path classifier integration.
  - [x] Reuse existing secret-redaction and path-security helpers where available.
  - [x] Classify `.env`, `.env.*`, credentials, SSH material, private-key suffixes, configured secret-store paths, and any existing protected-path classes as ineligible.
  - [x] Apply classification before trust matching and before UI persistent-option display.
- [x] Add curated command profile registry and matcher.
  - [x] Define stable profile ids `git_readonly` and `rust_validate` with fixed structured executable/argv entries.
  - [x] Bind each entry to workspace cwd, timeout cap, output cap, finite use/expiry requirements, and execute category.
  - [x] Reject unknown profile ids and prevent profile registry mutation from workspace/project config.
- [x] Integrate read/profile policy decisions.
  - [x] Normalize native read, content search, directory list, diagnostics, MCP read, and terminal profile operations before evaluation.
  - [x] Build MCP read identity only after ee-pinned manifest classification, server transport identity validation, tool schema validation, and bounded argument extraction succeed.
  - [x] Require workspace gate plus matching read rule/profile id before allow.
  - [x] Preserve current prompts for unmatched, external, sensitive, oversized, unknown, and invalid requests.
- [x] Add read/profile tests.
  - [x] Assert workspace gate without matching rule/profile prompts.
  - [x] Assert matching source read and approved profile command bypass prompt.
  - [x] Assert `.env`, private-key-like, secret-store, external, traversal, symlink escape, Git mutation, package install, and shell wrapper prompt.
  - [x] Assert profile timeout, cancellation, output cap, and terminal ownership behavior remain unchanged.

#### Actionable criteria

- `cargo test --quiet -p ee-cli workspace_read_trust` passes.
- `cargo test --quiet -p ee-cli command_profiles` passes.
- Automated tests prove workspace gate cannot authorize a read, command, or MCP operation without its own matching rule.

### Phase 5: Add bounded create/modify write trust

Goal: reduce prompts for narrow routine text edits while keeping destructive or sensitive filesystem operations prompt-only.

Overview: implement write adapter after shared store, canonical paths, expiry, and usage ledger exist. Trust only regular UTF-8 text create/modify operations within strict path and size budgets.

Rules:

- Write rules distinguish `create` and `modify`; one operation never authorizes other.
- Rule uses canonical workspace-relative directory prefix, maximum file count, maximum aggregate bytes, maximum per-file bytes, finite expiry, and finite use budget.
- Root-wide, glob, regex, absolute, traversal, protected, and unbounded write rules are invalid.
- Delete, rename, mode change, symlink, special file, binary, non-UTF-8 content, protected path, and over-budget batch always prompt.
- Usage increments only after successful trusted write response; failure, cancel, denial, or connection close consumes no budget.

#### Work items

- [x] Define validated write operation and rule types.
  - [x] Normalize target paths, operation type, proposed file count, and byte deltas before policy evaluation.
  - [x] Canonicalize target parent and final target path without following an unsafe symlink outside workspace.
  - [x] Reject ineligible file kinds and protected paths before candidate rule creation.
  - [x] Validate finite nonzero caps against application safety maxima.
- [x] Implement write matcher and session-local usage ledger.
  - [x] Match workspace, agent, operation, canonical directory prefix, and every file/byte constraint.
  - [x] Inject usage snapshot into evaluator and update ledger only after successful operation response.
  - [x] Clear ledger through existing session close and connection teardown paths.
  - [x] Return prompt for exhausted budget without mutating persistent store.
- [x] Integrate write approval and persistence.
  - [x] Offer `Allow for 1 hour / 5 uses` only for eligible narrow create/modify requests.
  - [x] Derive rule narrower than or equal to application safety maxima from approved request.
  - [x] Persist rule before dispatch and activate it only after durable store success.
  - [x] Keep all ineligible operations on existing approval UI with no persistent option.
- [x] Add write trust tests.
  - [x] Assert matching `src/generated/` text create/modify within budget bypasses prompt.
  - [x] Assert operation mismatch, external/traversal/symlink escape, protected path, binary, delete, rename, mode change, and over-budget request prompts.
  - [x] Assert successful trusted operation consumes one use; failed/canceled/denied operation consumes none.
  - [x] Assert session teardown clears usage ledger.

#### Actionable criteria

- `cargo test --quiet -p ee-cli write_trust` passes.
- Automated write tests prove destructive, binary, sensitive, and external filesystem operations cannot bypass approval.
- Automated budget tests prove only successfully dispatched trusted writes consume authority.

### Phase 6: Add expiry, finite-use lifecycle, and unified audit visibility

Goal: prevent forgotten persistent grants from becoming indefinite execute/write authority and make every automatic decision explainable without leaking data.

Overview: complete common scope lifecycle for all rule variants with injected time, session-local counters, expiry-aware UI, and redacted rule audit metadata.

Rules:

- Read rules may be unlimited only when workspace gate and all scope constraints match.
- Execute and write rules require both expiration and finite maximum use.
- Expired or exhausted rules remain stored but evaluate as prompt; runtime never renews automatically.
- Clock is injected; no test uses wall-clock sleep.
- Audit output includes rule id/category/scope/remaining use only; never raw paths beyond approved display policy, command env, secret values, or MCP arguments.

#### Work items

- [x] Extend common scope validation and store serialization.
  - [x] Add schema fields for absolute UTC expiration and maximum successful uses.
  - [x] Reject invalid timestamp, past expiry, zero uses, use cap above safety maximum, unlimited execute/write, and expiration beyond maximum duration.
  - [x] Assign stable rule id at creation and retain it across reload.
- [x] Implement injected clock and lifecycle ledger.
  - [x] Add production clock implementation and deterministic fake clock for tests.
  - [x] Key usage ledger by workspace identity, session id, and rule id.
  - [x] Check expiry/use before allow decision and increment only after successful dispatch.
  - [x] Clear session rows on close and connection loss.
- [x] Add approval/status integration.
  - [x] Offer only `Allow for 1 hour / 20 uses` for execute actions and `Allow for 1 hour / 5 uses` for write actions; do not expose an unlimited persistent execute/write choice.
  - [x] Show redacted expiration and remaining-use metadata in approval and status surfaces.
  - [x] Emit redacted matched-rule audit event for automatic allow and prompt fallback.
- [x] Add deterministic lifecycle tests.
  - [x] Assert virtual clock permits rule before expiry and prompts after expiry.
  - [x] Assert execute/write grants allow exactly configured successful uses then prompt.
  - [x] Assert failed, canceled, denied, and disconnected requests do not consume use.
  - [x] Assert reload preserves valid expiry metadata and ignores invalid persisted scope.

#### Actionable criteria

- `cargo test --quiet -p ee-cli trust_lifecycle` passes.
- Automated virtual-time tests prove no expiration behavior depends on real time or sleeps.
- Automated audit tests prove secret-like values never occur in automatic-decision metadata.

### Phase 7: Run cross-transport security and compatibility matrix

Goal: prove unified policy produces identical bounded outcomes through native ACP, MCP stdio proxy, ACP-native MCP-over-ACP, config reload, cancellation, and session lifecycle.

Overview: add deterministic matrix fixtures after all adapters exist. Preserve existing tool validation, ownership, redaction, output caps, cancellation, and error behavior.

Rules:

- Tests use temporary directories, fake agents, fake MCP servers, injected clocks, and explicit task shutdown only.
- Tests assert approval queue state, fake dispatch records, and structured responses; no fixed sleeps or retry loops.
- Fixture secret values must be absent from host-local store, repository config, UI, transcript, logs, diagnostics, stdout, stderr, and error output.
- Schema generation must cover all host-local trust document variants without adding authority-granting project-config fields.

#### Work items

- [x] Add operation-category matrix tests.
  - [x] Cover workspace gate disabled, gate enabled without rule, matching rule, session deny, scope mismatch, expired rule, exhausted rule, malformed store, and identity mismatch for every eligible category.
  - [x] Cover terminal command, exact MCP invocation, native read, MCP read, curated profile, create write, and modify write.
  - [x] Cover prompt-only delete, rename, VCS mutation, package mutation, network mutation, secret access, external path, and unknown tool cases.
- [x] Add transport and lifecycle matrix tests.
  - [x] Assert equivalent operation identity yields same decision through direct ACP, stdio MCP proxy, and ACP-native MCP-over-ACP.
  - [x] Assert cancellation or connection close before approval resolution dispatches no operation, consumes no usage, and writes no rule.
  - [x] Assert session close clears once/session decisions and runtime budgets while persistent host-local rules remain scope-checked.
  - [x] Assert explicit trust-store reload applies valid host-local changes and fails closed on corruption; repository config reload never reloads or grants trust.
- [x] Add compatibility regression coverage.
  - [x] Assert existing terminal ownership, output truncation, path traversal, symlink escape, secret redaction, stale revision, and permission denial tests remain unchanged.
  - [x] Assert repository `ee.toml` trust-looking fields cannot grant effective authority.
  - [x] Assert host-local trust store remains excluded from repository config discovery; generated project schema contains no authority-granting trust fields.
  - [x] Add targeted package tests to existing CI-compatible suite.

#### Actionable criteria

- `cargo fmt --check` passes.
- `cargo clippy -p ee-cli --all-targets -- -D warnings` passes.
- `cargo test --quiet -p ee-cli` passes.
- `cargo test --quiet -p ee-agent-host` passes.
- `cargo test --quiet -p ee-mcp` passes.
- Automated matrix tests prove no repository-controlled configuration, unclassified tool, destructive operation, external path, sensitive value, expired rule, exhausted rule, or scope mismatch bypasses approval.
