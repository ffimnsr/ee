# ISSUES


# Fresh Divergence Scan — Prompt Input, Config Surface, Session Ops (2026-11-19)

Audit date: 2026-11-19. Scope: reference subsystems **not itemized** by Parts 1–8 — prompt intake (`session/inbox.ts`, `session/prompt.ts`), user-defined config surface (`config/plugin/agent.ts`, `config/plugin/command.ts`), session operations (`agent.ts` switching, `snapshot.ts` + `session/diff.ts`, `session/transfer.ts`, `session/stats.ts`, `session/background.ts` + `subagent-job.ts`), and boundary services (`file-access.ts`, `reference.ts`, `image.ts`, `flag.ts`). Parts 1–7 (calling layer) and Part 8 (tool surface, permission, PTY, MCP resources) are not reopened. Retry-class details (`model-retry.ts` unreachable budget, retry-after clamp, one-shot `notRun`, `x-should-retry`) were re-checked and are already covered by the landed Gap 5 work — no new item.

## Part 9 — Findings

- **Prompt inbox (queue + steer).** The reference persists every user input in a per-session inbox with a `delivery` field (`steer` by default, `queue` otherwise); steers are injected into the running turn, queued prompts are promoted when the turn settles, and control operations (move, compaction) queue the same way (`session/inbox.ts`). ee rejects a prompt while a turn runs with `AgentError::TurnAlreadyRunning` (`crates/ee-agent-host/src/error.rs`) — the user's only option is `/resume` after the stop gate.
- **User-defined agents.** Agents are markdown files under `{agent,agents,mode,modes}/**/*.md` in every config source (`.opencode/`, `~/.config/opencode/`, `.agents/`, `.claude/`), parsed with gray-matter frontmatter: `model` + `variant`, `system` prompt (file body), `description`, `mode: primary|subagent`, `hidden`, `color`, per-agent `steps` budget, per-agent permission rulesets, `request.headers`/`request.body` overrides, and `disabled` — hot-reloaded through a config observer, with `default_agent` selecting the primary (`config/plugin/agent.ts`). ee's delegate roles are hardcoded (`researcher, code_reader, implementer, test_runner, reviewer, rubber_duck, summarizer` in `prompt/tool_guidance.rs`); there is no user-defined agent surface at all.
- **User-defined commands.** Slash commands are markdown templates under `{command,commands}/**/*.md` with frontmatter (`description`, `agent`, `model`, `subtask`/`subagent`), `$ARGUMENTS`/`$1..$N` placeholder substitution, and `` !`cmd` `` shell interpolation resolved at execution time; a `subtask` command spawns a child session instead of prompting the current one (`config/plugin/command.ts`). ee's slash set is hardcoded in `ee-agent-protocol/src/slash_command.rs` and `ee-cli` (`/compact`, `/branch`, `/fork`, `/copy`, `/resume`, `/revert`).
- **Mid-session model/agent switch.** The reference exposes `switchAgent` and `switchModel` per session; command execution can switch both before prompting (`config/command.ts:99-103`). ee pins provider + model at session creation; only subagent routing (`set_model_router`) selects models, and the `models` tool landed read-only.
- **Workspace file snapshot + per-turn diff + selective revert.** The reference captures a content-addressed git-tree snapshot per turn in a separate object store (ignore-aware, 2 MiB untracked cap, periodic `objects.pack`), lists changed files and structured diffs between any two captures, and restores selected paths; `session/diff.ts` diffs the files a turn changed, and `file-retention.ts` prunes old stores. Part 8.3 noted "no workspace file snapshot store" but never itemized the work — `/revert` only restores conversation state.
- **Prompt attachments and mentions.** Prompt intake materializes file attachments from paths, `data:` URLs, and stored blob refs (size-capped), expands agent and skill mentions into attached content, runs images through a decode/resize/limits normalize pass (`image.ts`: configurable `autoResize`, max width/height/base64 bytes), and fires a `session.prompt` plugin hook that can rewrite the prompt before the turn (`session/prompt.ts`). ee accepts ACP text + content blocks only; no path/data-URL attachment materialization, no mention expansion, no image resize (Gap 27 landed dedupe only), no prompt-rewrite hook.
- **External directory read approval.** `file-access.ts` gives read tools a batched, one-time external-directory authorization: resolving a path outside the location asks once for the lexical boundary directory, after which reads (and sibling recovery) inside it proceed without per-file prompts. ee fails closed to configured workspace roots with no escape hatch — the user cannot grant "read anything under /etc" for one session.
- **Session transfer (export/import with redaction).** `session/transfer.ts` exports a session (info + settled messages) with a sanitize mode that redacts text, tool input/output, metadata, native state, and file/agent/skill mentions, and imports into another project with conflict detection. ee has a durable session log but no export/import surface and no redaction path over transcripts.
- **Usage analytics.** `session/stats.ts` aggregates per-day/per-model/per-tool token and cost totals (input, output, reasoning, cache read/write), tool call counts with success/failure and p50 durations, active-day streaks, and range queries over history. ee reports per-turn `UsageUpdate` events only; nothing aggregates session/cost history (verified: no `total_cost`/`sum_cost` surface anywhere).
- **Subagent jobs and outstanding work.** `session/background.ts`/`subagent-job.ts`/`subagent-completion.ts` track a session's (and all descendants') still-running background shells, image generations, and subagents; a child's background work keeps the parent session from reporting done, and completion notices wake the parent. ee's `delegate_task` is synchronous inside the parent turn, and Improvement 5 covered background terminals only — a subagent that outlives the delegate call has no completion path.
- **Reference sources.** `reference.ts` + `repository-cache.ts` register named references to local paths or **remote git repositories** (branch-pinned, cached on disk, refreshed daily, concurrency-bounded) so the agent can read other repos by name. ee has no named-reference or remote-repo cache concept.
- **Feature flags.** `flag.ts` resolves an experimental-flag registry from `OPENCODE_FLAGS` env (`-key`, `all`), config `experimental.<key>` / `all_flags`, and per-channel defaults — a kill-switch surface for rolling out experimental harness behavior. ee has per-feature config options but no flag registry with env override.

## Part 9 — Improvements (itemized)

Ordered by parity-gained-per-risk; numbering continues Part 8. Each item is independently shippable, lands with its own tests, and does not reopen Parts 1–8.

- [x] **I15 — Prompt inbox: queue + steer.** Persistent per-session inbox in `ee-agent-host` with delivery semantics. Problem/Fix/Acceptance:
  - [x] **Problem:** a prompt sent while a turn runs fails with `AgentError::TurnAlreadyRunning`; the user cannot queue follow-ups or steer mid-turn. Reference: `session/inbox.ts` (steer injects into the running turn; queued items promote at turn settle; cancel/steer/delivery-change on pending items; control ops like `/compact` queue the same way).
  - [x] **Fix:** add a bounded per-session inbox in the host between ACP `session/prompt` and the orchestrator: `delivery: steer` (default) injects the text as a system-reminder/user block at the next step boundary; `delivery: queue` holds the prompt and auto-promotes it when the turn settles; both bounded (e.g. 16 pending items, prompt-size capped). Mirror `TurnAlreadyRunning` only when the inbox is disabled or full. ACP surface: optional `delivery` field on `session/prompt`. **Landed (2026-11-19):** `PromptDelivery` (`steer` default, `queue`) parses top-level `delivery` and `_meta.delivery`; `ee-agent-acp-server` gains a bounded `PromptInbox` (`prompt_inbox_enabled`, `max_pending_prompts=16`, `max_prompt_bytes=256 KiB`), steer channels drained by the orchestrator loop at each step boundary, queue promotion with no client round-trip, cancel/close cleanup of queued items and steer waiters, and `AgentThread::send_prompt_with_delivery` + v2 ack-only delivery path in `ee-agent-host`; `/steer` now injects instead of cancelling.
  - [x] **Acceptance:** a prompt sent mid-turn with `steer` is visible to the model before the turn ends; a queued prompt runs automatically after the turn settles without a client round-trip; cancel is honored for not-yet-promoted items; `/compact` queued behind a running turn executes after it. (Covered by `inbox_flows`, `cancellation`, `loop_engine::tests::steer_message_is_visible_before_the_next_model_call`, and host `concurrency_tests::steer_delivery_joins_the_running_turn_without_taking_the_reservation`.)
- [x] **I16 — User-defined agents via markdown.**
  - [x] **Problem:** delegate roles and the primary agent's system prompt/model are hardcoded; users cannot define project or personal agents (role, prompt, model, tool/permission profile) without code changes.
  - [x] **Fix:** discover `<workspace>/.agents/agents/**/*.md` and `$XDG_CONFIG_HOME/ee/agents/**/*.md` with YAML frontmatter (`model`, `variant`, `description`, `mode: primary|subagent`, `hidden`, `steps` budget, per-agent permission rules, `disabled`), body = system prompt. Feed the registry: primary agents become ACP-selectable, subagent-mode agents become `delegate_task` roles alongside the built-ins. Reuse the I3 ruleset evaluator for per-agent permission rules. Bounded (e.g. 32 agents, 32 KiB prompt each); fail closed on frontmatter errors with a logged skip. **Landed (2026-11-19):** `user_agents.rs` discovers both roots recursively with canonical-path containment, YAML frontmatter via `serde_yaml_ng`, 32-agent / 32 KiB-prompt / 96 KiB-file bounds, workspace-over-user precedence, and warning-based skips for malformed, disabled, oversized, or duplicate definitions. `UserAgentRegistry` is shared between `OrchestratorRuntime` and `SubagentManager`: primary agents are selected by the advertised `/primary-agent [name]` command (a distinct name so the CLI's `/agent` model alias is untouched), while `mode: subagent` definitions resolve in `delegate_task` after the immutable built-ins. The I3 ruleset evaluator moved to `ee-agent-orchestrator::permission_rules` and is re-exported by the host; subagent roles and primary sessions carry a deny-biased `ToolPolicy.permission_rules` overlay that runs before host approval and before any ACP `session/request_permission` round-trip. `steps` caps the root loop; `model`/`variant` resolve through `model#variant` then the base id; selection is persisted and restored on `session/load`; workspace agent prompts ride the untrusted workspace plane.
  - [x] **Acceptance:** a project agent file with `mode: subagent` appears as a `delegate_task` role and its body becomes the child's system prompt; `disabled: true` removes it; a malformed file is skipped with a warning; per-agent permission rules are enforced by the host broker before ACP round-trips. (Covered by `user_agents::tests`, `subagents::tests::roles_tests::user_defined_subagent_role_uses_file_body_and_permissions`, `subagents::tests::delegate_tests::user_defined_subagent_role_runs_with_its_file_prompt`, `policy::tests::agent_permission_rules_deny_before_host_approval_and_match_resources`, `runtime::tests::core_tests::user_agent_selection_sets_model_steps_and_permission_overlay`, and `provider_adapter::tests::session_tests::agent_command_selects_a_user_defined_primary_agent_without_a_model_call`.)
- [x] **I17 — User-defined slash commands via markdown templates.**
  - [x] **Problem:** the slash set is hardcoded; users cannot add project/personal commands ("prepare release", "run flaky suite", review prompts).
  - [x] **Fix:** discover `commands/**/*.md` in the same roots; frontmatter: `description`, optional `agent`, optional `model`, optional `subtask: bool`; body is a template with `$ARGUMENTS`/`$1..$N` substitution. Execution reuses I15 delivery and I16 agent selection: default prompts the current session, `subtask` spawns a child session via the existing subagent machinery, `agent`/`model` switch (I18) before prompting. Shell interpolation (`` !`cmd` ``) is opt-in and runs under the existing terminal approval gate. Hot reload on workspace file events or next prompt. **Landed (2026-11-19):** `user_commands.rs` discovers `<workspace>/.agents/commands/**/*.md` then `$XDG_CONFIG_HOME/ee/commands/**/*.md` with workspace-over-user precedence, 64-command / 64 KiB-template / 128 KiB-file bounds, reserved-name rejection, and warning-based skips. `markdown_config.rs` owns the shared bounded frontmatter/discovery path for agents and commands. Templates expand `$ARGUMENTS`, `$1`..`$999`, and `$$` before prompting; inline commands apply `agent` through the I16 primary selector and `model` through the I18 route, then rebuild the prompt bundle so the same turn uses the selected agent and model. `subtask: true` routes through `SubagentManager::spawn` (same budget, task graph, cancellation, and evidence verification as `delegate_task`), defaulting to the read-only researcher role. The template set is reloaded on every prompt and advertised through `available_commands_update` only when it changes, so completion replaces built-ins plus user commands. Shell interpolation is deliberately rejected at discovery with a warning because this provider has no approval-gated shell path; it never runs unapproved.
  - [x] **Acceptance:** `/mycommand arg1 arg2` substitutes placeholders and prompts with the expanded template; `subtask: true` runs as a child session that reports back through the existing subagent-completion path; unknown/malformed command files are skipped; commands appear in tab-completion alongside builtins. (Covered by `user_commands::tests`, `markdown_config::tests`, `provider_adapter::tests::session_tests::user_command_expands_template_and_advertises_completion`, `user_command_applies_agent_and_model_before_prompting`, and `subtask_user_command_runs_through_the_subagent_manager`.)
- [x] **I18 — Mid-session model and agent switch.**
  - [x] **Problem:** provider + model are pinned at session creation; switching models mid-conversation requires a new session (losing transcript) or editing config and restarting.
  - [x] **Fix:** host-level switch on idle sessions: resolve the target model against the existing catalog/model registry, rebuild the provider profile, and keep the transcript (already provider-agnostic through `ModelMessage` normalization; cross-provider native state replay is already producer-gated per Gap 25). Surface as an ACP method and a `/model` / `/agent` slash command; a switch to an incompatible route (e.g. structured-output mismatch mid-session) fails closed with actionable guidance. Pairs with the read-only `models` tool (I8). **Landed (2026-11-19):** new ACP extension `session/set_model` (`SetSessionModelRequest`/`Response`, registry-backed, rejected while a turn runs), `AgentProvider::set_model`, `AgentThread::set_model`, `/model <id>` and `/agent <id>` in `ee-cli`; `OrchestratorRuntime` stores a per-session selected model resolved at each loop/compaction/resume path, provider labels update on switch, persisted snapshots retain `model_id`, `/branch` inherits it, and unknown/disabled/non-chat targets fail with named errors. Native state remains producer-gated by `replayable_for`.
  - [x] **Acceptance:** switching models between turns keeps the full transcript and sends the next request on the new route; native state from the old provider is replayed only where the landed Gap 25 rules allow; switch during a running turn is rejected; incompatible target models fail with a named error. (Covered by `provider_adapter::tests::mid_session_model_switch_keeps_the_transcript_and_routes_the_next_turn`, `unknown_model_switch_fails_closed_with_the_target_named`, `model_switch` server tests, and the existing `replayable_for` producer gate.)
- [x] **I19 — Workspace file snapshot, per-turn diff, and selective revert** (closes Part 8.3's open note).
  - [x] **Problem:** `/revert` restores conversation state only; there is no workspace file snapshot store, so a bad agent turn cannot be file-reverted, and users cannot see what files a turn touched without running git.
  - [x] **Fix:** capture a content-addressed snapshot tree at turn start/end over the workspace roots (git-object store under the session state dir when a repo exists, bounded to tracked-plus-small-untracked files, ignore rules respected); record snapshot ids on the turn's messages; add `turn_diff` (files + structured patches between a turn's start/end snapshots) and extend `/revert <seq>` with an optional `--files` mode restoring selected paths from the pre-turn snapshot; retention sweep on session close reuses the session-log prune pattern. Non-git workspaces either fail closed to git repos or use a bounded file-copy fallback — decide in design. **Landed (2026-11-19):** `WorkspaceSnapshotStore` (`workspace_snapshot.rs`) captures SHA-256 content-addressed blobs + manifests under `<session-log-dir>/snapshots/<session>` (in-memory when no durable dir), using `git ls-files` for tracked plus size-capped untracked files and the `ignore` walker as the bounded non-git fallback; bounded by file count, per-file bytes, and total bytes with a `truncated` flag. `OrchestratorRuntime` captures start/end around each ordinary turn, annotates the turn's user/assistant messages with `ee_snapshot_start`/`ee_snapshot_end`, exposes `turn_diff` via `/turn-diff [N]`, and `/revert N --files [paths...]` restores selected paths (or every changed file) from the pre-turn snapshot without touching conversation state. `purge_session_artifacts` prunes snapshot storage with the session.
  - [x] **Acceptance:** a turn that edits three files reports exactly those files in `turn_diff`; `/revert N --files a.rs` restores `a.rs` byte-identically while leaving `b.rs` and the conversation untouched; snapshots of ignored files are excluded; snapshot storage is pruned with session lifecycle like `SessionLog::purge_artifacts`. (Covered by `workspace_snapshot::tests`, `provider_turn_diff_reports_exactly_the_files_the_turn_changed`, `snapshot_command::tests`, and `checkpoint_command::tests::files_mode_restores_selected_paths_and_leaves_the_conversation_untouched`.)
- [x] **I20 — Prompt attachments, mention expansion, and image normalization.**
  - [x] **Problem:** prompt intake is text + inline blocks; no file-path attachment materialization, no `@file`/`@skill`/`@agent` mention expansion, and attached images are sent as-is (Gap 27 dedupe only; a 4 MiB screenshot costs context and can overflow).
  - [x] **Fix:** in the host prompt path, accept an optional `attachments` array (paths and/or `data:` URLs) with per-item size caps and workspace-root or external-read (I21) authorization; expand `@path`, `@skill:name` mentions into attached content with the existing trust-level tagging; add a deterministic image normalize pass (decode, downscale to configured max dimensions, re-encode, enforce max base64 bytes; passthrough for undecodable formats) before the request projection. Attachments land in the transcript as ordinary content blocks so replay/compaction rules apply unchanged. **Landed (2026-11-19):** `prompt_attachments.rs` reads `_meta.attachments` entries as path strings, `{path}`, or base64 `{dataUrl, mediaType}` objects; paths resolve through canonical workspace roots and anything outside fails closed with the named I21 external-directory message. `@path` files and `@skill:name` skills expand into bounded workspace-plane instructions (`WorkspaceInstructionUntrusted` for repository content, `UserInstructionTrusted` for personal skills), bare `@handles` stay text, and mentions dedupe. Images are decoded, downscaled to `prompt_attachments.max_image_dimension`, re-encoded as PNG, and checked against `max_image_base64_bytes`; undecodable formats pass through. `OrchestratorConfig::prompt_attachments` carries per-item, total, mention, and image caps; text/binary attachments become ordinary `ContentBlock::Resource` values and images become `ContentBlock::Image`, so existing replay and compaction paths apply unchanged.
  - [x] **Acceptance:** attaching a 4000x3000 screenshot sends a bounded, downscaled image; `@skills/atlas/SKILL.md` inlines that file under the untrusted-workspace trust level; an attachment outside workspace roots without approval fails closed with the I21 message; total attachment bytes are capped per prompt. (Covered by `prompt_attachments::tests`, `provider_adapter::tests::session_tests::prompt_attachment_image_is_downscaled_before_the_model_call`, and the `max_image_dimension` regression in `large_image_is_downscaled_and_reencoded`.)
- [x] **I21 — External directory read approval.**
  - [x] **Problem:** tools fail closed to workspace roots; the user cannot grant reads outside them (another checkout, `/etc/hosts`, a dependency source tree) without reconfiguring roots.
  - [x] **Fix:** add an approval boundary in the host policy path: a read-class tool resolving a path outside all roots requests one batched external-directory permission for the lexical ancestor directory; approval persists per session (and optionally to `agent-permissions.json` via the I3 allow-always path). Reads inside an approved directory proceed without further prompts; writes and execute-class tools remain fail-closed to workspace roots — no external write approval. **Landed (2026-11-19):** `external_reads.rs` holds canonical session grants (`AgentPaneState::external_read_roots`, in-memory only, max 32) and intercepts read-class proxy calls (`ee_read_text_file`, list/read-buffer, diagnostics, symbols, references, dependency maps, git file diff) before dispatch; the deferred call resumes through the same read path after approval (`DeferredExternalRead` in `ApprovalKind::ExternalRead`, allow-once / allow-directory-for-session / deny). `path_in_read_scope` and `path_in_effective_read_scope` widen only read guards; write/execute validation and `current_text_revision`/`read_current_text` stay on workspace roots. `/permissions` lists the grants and `/permissions revoke-read <dir|all>` removes them. The orchestrator policy now lets host-approved read tools leave the `WorkspaceScope` (the host owns the approval); non-host reads and all write/execute scope escapes stay denied.
  - [x] **Acceptance:** reading `/etc/hosts` prompts once for `/etc`; subsequent reads under `/etc` do not prompt; write/execute tools outside roots still fail closed; the approval list is visible and revocable per session; approvals never persist to disk unless the user picks allow-always. (Covered by `external_reads::tests`, `proxy_external_read_session_grant_covers_later_reads_in_the_directory`, `proxy_phase1_path_tools_reject_outside_workspace_when_denied`, `read_outside_workspace_fails_closed_when_denied`, and `policy::tests::host_approved_reads_reach_host_external_directory_approval`.)
- [x] **I22 — Session export/import with redaction (transfer).**
  - [x] **Problem:** sessions cannot be moved between machines or shared without shipping raw logs; no redaction path exists over transcripts.
  - [x] **Fix:** `export_session` over the existing session log producing a bounded JSON document (info + settled messages + usage), with a redaction mode covering prompt text, tool inputs/outputs, metadata, native state, and memory references; `import_session` validates, resolves conflicts by id (fail on existing), and creates a resumable session. Share/backup use; redacted exports are the default suggestion. **Landed (2026-11-19):** `session_log` now records per-response `UsageRecord`s (token/cost) from the loop, so exports and later analytics read one append-only stream. `session_transfer.rs` exports a bounded JSON document (`schema_version`, session info, settled non-prompt-plane messages, aggregate and per-model usage, `truncated`/`redacted` flags); redaction replaces every message body, clears tool-call ids and routing metadata, folds per-model rows into one `redacted` row, and keeps usage totals; provider native state and workspace-memory references are never exported (Gap 25 producer gating). `import_session` validates version/size, refuses a target with existing records (id in the error), writes messages plus usage into the target log, and `/import-session` seeds the current empty session's replay conversation and model transcript. `/export-session [path] [--include-content]` defaults to redacted and bounded; `/import-session <path>` restores history for the next turn. `session/load` now restores replay/model planes from the durable log when no live session survived, so imported and restarted sessions stay resumable.
  - [x] **Acceptance:** export→import round-trips a resumable session with usage totals intact; redacted export contains no user text, tool I/O bodies, or absolute paths; importing an existing session id fails closed with the id named; exports cap at a bounded size. (Covered by `session_transfer::tests`, `session_transfer_command::tests` including the live in-memory export, `durable_session_load_replays_the_logged_transcript`, and the refreshed command advertisement test.)
- [x] **I23 — Usage and cost analytics surface.**
  - [x] **Problem:** per-turn `UsageUpdate` events exist but nothing aggregates them; users cannot see session cost to date, per-model spend, or tool call success/duration stats without replaying logs.
  - [x] **Fix:** extend the session-log/telemetry layer with an aggregation query: per-session and per-model token/cost totals (input, output, reasoning, cache read/write), per-tool call counts with success/failure and bounded duration percentiles; expose as a host query surfaced by a read-only `usage_stats` tool and/or `AgentThread` API method. Deterministic from the session log — no new recording state. **Landed (2026-11-19):** the log now records `UsageRecord` per model response (I22 foundation) and `ToolRecord` per completed execution (name, success, error kind, duration, turn/iteration); `usage_stats.rs` aggregates totals, per-model rows, and per-tool calls/failures/p50/p95/max from those records only. The read-only `usage_stats` built-in tool is registered per session when logging is enabled and returns bounded text plus structured JSON (top 32 models, top 16 tools by call count). No arguments, no secrets, no client handles.
  - [x] **Acceptance:** a session with two models reports each model's totals; tool stats include p50 duration for the N most-called tools; the query is computed from the existing log (no dual bookkeeping); read-only tool surface carries no secrets or client handles. (Covered by `usage_stats::tests` and the usage/tool records emitted from the loop.)
- [x] **I24 — Async subagent completion and outstanding-work tracking.**
  - [x] **Problem:** `delegate_task` blocks the parent turn until the child finishes; Improvement 5 backgrounded terminals only. A long child (test runs, large refactors) either holds the parent turn hostage or never reports back if spawned detached.
  - [x] **Fix:** register delegate children as background jobs in the same registry as background terminals (Improvement 5): a `background: true` delegate returns a child id immediately, the child's completion injects a bounded system-reminder notice at the parent's next step boundary (or wakes an idle parent session via the existing ACP update path), and a turn does not settle while outstanding child jobs exist (session-close cancels them). Reuses the `BACKGROUND_JOBS` runtime section for status projection. **Landed (2026-11-19):** `BackgroundJobRegistry` now tracks detached delegates beside terminal jobs (`register_subagent`/`finish_subagent`/`cancel_subagents`/`take_subagent_notices`/`wait_for_change`), `delegate_task` accepts `background: true` and returns a `delegate-N` id immediately while a detached `manager.spawn` reports completion, the loop refuses to settle while delegates run/wait, injects bounded notices at the next step, and `session/cancel`/`session/close`/`/discard` cancel outstanding children; the runtime `BACKGROUND_JOBS` section lists job id, role, description, status, and internal child id.
  - [x] **Acceptance:** a background delegate returns immediately and the parent receives the child's result summary automatically on the next step; `session/cancel` and `session/close` cancel outstanding children; the runtime snapshot lists running children with their role/description; a parent never settles with unreported outstanding child work. (Covered by `background_delegate_returns_immediately_and_reports_completion`, `turn_waits_for_background_subagents_and_injects_the_notice`, and the cancel/close paths calling `clear_background_jobs`.)
- [x] **I25 — Named reference sources (local dirs + cached remote repos).**
  - [x] **Problem:** the agent can only read the current workspace and explicitly-approved paths; there is no way to pin "compare against repo X at branch Y" without the user cloning it manually.
  - [x] **Fix:** a config-level `[[agents.references]]` table: `name` → local path or `{ repository, branch }`; remote repos are cloned into a bounded cache under the state dir on first use and refreshed on a daily throttle; references are read-only roots (never write roots) and surfaced in the prompt pack like the workspace roots section. Path resolution inside read tools accepts the reference name as a prefix. **Landed (2026-11-19):** `[agents.references]` parses local path strings and `{ repository, branch }` tables (validated names/branches, max 16, later layers replace a name) in `agents_settings.rs`/`raw.rs` plus the regenerated config schema. `agent_references.rs` resolves `<name>/relative` and `<name>:relative` prefixes for read-class proxy calls and `fs/read_text_file`; local entries read in place, remote entries clone with `--depth 1` into `<state>/references/<name>` and refresh via fetch/reset at most once per 24 h, bounded by a 30 s timeout and a 256 MiB checkout budget. Failures are named errors (`reference \`name\` unavailable: ...`). Reference roots are read-only: they are not added to workspace/write roots, every write validator still requires an allowed absolute workspace path, and read guards treat them as in-scope without external approval. Every outbound prompt carries a bounded reference-section block listing the `<name>/path` prefixes. `ee-mcp` read-class argument validation accepts reference prefixes while write tools keep `require_absolute`.
  - [x] **Acceptance:** a prompt referencing the named source reads files from the cached clone without leaving the cache dir; a second session reuses the cache without re-cloning until the refresh window passes; writes into reference roots fail closed; a fetch failure degrades to a named error, not a hang. (Covered by `agent_references::tests` for local resolution, prefix parsing, clone-once/refresh-window, missing-remote named error, and prompt bounds; `proxy_reference_prefix_reads_configured_sources_without_approval` for host reads and write fail-closed; `read_tools_accept_reference_prefixes_but_writes_stay_absolute` for the MCP boundary.)
- [x] **I27 — File-backed memory entries (`.agents/memories`).**
  - [x] **Problem:** durable memory is either the opt-in SQLite workspace-fact store (host-owned, not portable) or session notes; there is no human-editable, repo-committable memory surface, so curated project knowledge cannot travel with a checkout or be reviewed like code.
  - [x] **Fix:** discover `<workspace>/.agents/memories/**/*.md` then `$XDG_CONFIG_HOME/ee/memories/**/*.md` with YAML frontmatter (`name`, `description`, `tags`, `importance: low|normal|high|critical`, `disabled`); body is the entry content. Workspace entries win over personal entries by name. Discovery reuses the bounded markdown path (`128` entries, `64 KiB` file, `32 KiB` body, `16` tags, depth `4`); malformed/oversized/duplicate/disabled files warn and skip. The prompt carries a bounded catalog (`8 KiB`) ranked by importance then name, and the read-only `load_memory` tool loads one entry by name or searches by query/tags with deterministic relevance-then-importance ordering (`24 KiB` tool cap). Workspace entries are created/replaced by `save_memory` and removed by `delete_memory`; both are host-approved writes that confirm through an ACP elicitation before mutating, reject path-like names, stay inside a workspace root, and write atomically. Personal entries stay read-only. `@memory:<name>` mentions expand the content through the existing mention path. Workspace entries ride the untrusted workspace plane; personal entries ride the trusted user plane. Files are the persistence layer, so entries survive sessions and can be committed or reviewed.
  - [x] **Acceptance:** an entry appears in the prompt catalog with tags and importance; `load_memory` returns it by name and ranks tag/text matches; `save_memory` creates and updates entries while preserving omitted metadata; `delete_memory` removes the file only after confirmation; a declined confirmation leaves the file untouched; a malformed file is skipped with a warning; workspace definitions win over personal ones; `@memory:name` inlines the content with the correct trust label; user memory tool output is bounded. (Covered by `user_memories::tests`, `user_memory_write::tests`, `tools::tests::load_memory_loads_by_name_and_searches_by_tag`, `save_memory_requires_confirmation_and_writes_the_entry`, `delete_memory_requires_confirmation_and_removes_the_entry`, `delete_memory_declined_keeps_the_entry`, `prompt_attachments::tests::memory_mention_is_injected_with_its_scope_trust`, and `provider_adapter::conversation::tests::prompt_bundle_injects_a_bounded_memory_catalog`.)

Explicitly not adopted from this scan (same policy as Part 8): the managed/org policy layer (`managed-policy.ts` — enterprise feature, no product requirement), the per-session env-var table (`session/environment.ts` — the persistent PTY covers env persistence), the event logger (`event-logger.ts` — `tracing` covers it), and voice input. Reopen only with an explicit design decision.

# Remote Agent Mode Control — Headless Host, MCP Driver Surface, and Web Chat (2026-11-19)

Plan date: 2026-11-19. Basis: verified code audit (2026-11-18/19 passes) plus fetched Codex app-server and Claude Code cloud/Remote-Control docs. Goal: let ee agent mode be driven remotely — by another agent (e.g. hermes) over MCP, and by a human over a web chat UI — without the editor TUI.

Architecture (one new backend crate, three thin surfaces):

```
hermes agent ──MCP stdio──┐
                          ├── ee-agent-remote (embeds ee-agent-host::AgentManager) ──ACP── ee-agent-openrouter / ee-agent-opencode
web browser ──WS + static──┘
```

Verified anchors (not assumptions):

- `ee-agent-host` is UI-free (no ratatui/crossterm/ee-cli deps in its `Cargo.toml`) and is the designed host surface: manager, connection, session, `AgentEvent` stream, permission broker (`permission.rs` — declarative `PermissionRules` before any ACP round-trip, "empty rules ask every time"), `mcp_over_acp` proxy hosting, `redact.rs`.
- `ClientRequestHandler` + `HandlerCapabilities` (`inbound.rs`) is the designed extension point for agent→client fs/terminal/elicitation requests; fail-closed when a capability is not advertised. Today only ee-cli (`agent_bridge/bridge_ui.rs`), the critic, and tests implement it.
- ACP method registry is complete (`ee-agent-protocol/src/registry.rs`): `session/new|load|list|delete|resume|close|set_mode|set_config_option|prompt`, `session/cancel` notification, `session/request_permission`, elicitation, fs/terminal client methods, `mcp/connect|message|disconnect`.
- No steer exists in ACP: the framework rejects a second prompt while one is active per session (`ActivePromptError::AlreadyActive`, `ee-agent-acp-server/src/server.rs` via `dispatch.rs`). Follow-up queueing is Part 9 I15 (inbox) work, not this plan.
- Prompt concurrency cap: `DEFAULT_MAX_CONCURRENT_PROMPTS = 4` per connection, across sessions (`connection.rs:71`).
- `ee --mcp-proxy` is **not** standalone — it dials back to a live editor via `EE_MCP_PROXY_SOCKET`/`EE_MCP_PROXY_TOKEN` (`main.rs:251`). The reusable MCP machinery is `ee_mcp::EeMcpProxy` (rmcp 3.x `ServerHandler`) plus `tool_governance`; the workspace pins rmcp 3.x and upstream `agent-client-protocol-rmcp` wants rmcp ^2.x (documented SDK gap in `mcp_over_acp/mod.rs`), so any new MCP server follows the local rmcp 3.x pattern.
- CLI grammar is `ee do agent <sub>` (`args.rs:126`: `shell|trust|setup`) — new subcommands follow it.
- Durable session history: `session_log_dir` is provider config, opt-in but a defaulted path resolves to the state dir (`ee-agent-openrouter/src/orchestrated/tests.rs:55-75`). Remote history tools must confirm it is enabled.
- Trust profiles `mcp_safe_read` / `git_readonly` / `terminal_readonly` are host-local, bounded read grants on stdio and ACP routes (`args.rs:172`).
- `FakeAgentTransport` behind the `test-utils` feature is the hermetic test path — no network, no provider keys.

External patterns adopted (from fetched official docs, 2026-11-18): Codex app-server — JSON-RPC over stdio/ws/unix, approvals as server→client requests, `/healthz`+`/readyz`, token-file ws auth, bounded queues with `-32001` overload, schema generation discipline; Claude Code — follow-up injection into running sessions, session pull-in (`--teleport` analog = resume), remote steering of a locally-running session. Explicitly out of scope here: ACP-to-ACP relaying (bridge acting as agent to an outer ACP client) — excluded by decision; and any hosted relay service (phone→home machine without a tunnel) — later phase, needs relay infra.

## Part 10 — Issues (itemized)

Ordered by dependency; R1–R2 are foundations, R3–R6 the headless bridge, R7–R9 the MCP driver surface, R10–R11 the web surface, R12–R13 hardening. Each item is independently shippable, lands with its own tests, and does not reopen Parts 1–9.

- [x] **R1 — `ee-agent-remote` crate skeleton.** New backend crate embedding `AgentManager` as a headless host; the single place remote-control logic lives.
  - [x] **Problem:** `ee-agent-host` is UI-free and could serve any frontend, but only `ee-cli` drives it; there is no headless entrypoint, and ee-cli files are already large (1k LOC rule forbids growing the wiring there).
  - [x] **Fix:** new workspace member `crates/ee-agent-remote` depending on `ee-agent-host` (and later `ee-mcp`); `lib.rs` stays a module map only; one module per concern (transport, dispatch, events, permissions, config). Add the `test-utils` feature mirroring the host's so tests use `FakeAgentTransport`. The crate never depends on `ee-cli`, and nothing in `ee-agent-host` may depend on it (no cycles).
  - [x] **Acceptance criteria:** crate builds clean (`cargo check -q -p ee-agent-remote`); `cargo clippy -q -D warnings` passes; `cargo fmt --all -- --check` passes; no `ee-cli`/ratatui/crossterm dependency edges; a smoke unit test constructs a manager config with a fake transport.
  - [x] **Edge cases:** keep each module under the 1k LOC threshold (split dispatch per concern early); `test-utils` must not leak into default features; workspace feature unification — the host's `test-utils` stays dev/test-only exactly as today.

- [x] **R2 — Remote control protocol registry and fixtures.** A pinned JSON-RPC 2.0 method registry for the remote surface, mirroring the `ee-agent-protocol` registry discipline.
  - [x] **Problem:** without a single registry, the MCP wrapper, ws transport, and web client each invent request/notification names and drift; also no wire-level conformance fixtures exist for the remote surface.
  - [x] **Fix:** registry module in `ee-agent-remote` with method-name constants and typed params/results: `initialize`/`initialized` handshake; `session/list|new|resume|close|set_mode`; `turn/start|interrupt` (deliberately **no steer** — ACP has none; the mid-turn inbox is Part 9 I15); `permissions/pending|respond`; `history/tail`. Notifications: `session/update` passthrough, `turn/completed`, `status/changed`. Server→client request: `permission/requestApproval`. Documented error codes including `-32001` overload. JSON fixture files under `tests/fixtures/` parsed by round-trip tests, one per method, exactly like `ee-agent-protocol`'s fixture discipline.
  - [x] **Acceptance criteria:** every dispatcher route resolves through the registry (no string literals at call sites); each fixture parses into the typed params and serializes back byte-stable; unknown methods return the typed `method_not_found` error; the registry unit test asserts no name collides with an ACP wire name (avoids confusion when both surfaces are debugged together).
  - [x] **Edge cases:** JSON-RPC batch arrays (ACP v2 forbids them; v1 rejects them with a typed error) — pick one behavior (reject with typed error in v1) and fixture it; protocol version field on `initialize` with a documented compat policy (bump only on breaking change); request ids must be echoed exactly, including string ids.

- [x] **R3 — Headless `ClientRequestHandler`.** Implement the agent→client fs/terminal/elicitation surface without the editor so agents keep file and shell tools in headless mode.
  - [x] **Problem:** inbound agent requests (`fs/read_text_file`, `fs/write_text_file`, terminal family, elicitation) are currently answered only by ee-cli's bridge handler; a headless host fails closed on all of them, so remote-driven agents could talk but not act.
  - [x] **Fix:** a headless handler in `ee-agent-remote` advertising explicit `HandlerCapabilities` (`fs_read`, `fs_write`, `terminal`, `elicitation_*`) backed by bounded filesystem/terminal primitives with the same caps discipline as the existing bounded ops (entry/byte/depth caps on copies, absolute-path rules, output redaction on terminal env); reads pre-approved via the `mcp_safe_read`/`git_readonly`/`terminal_readonly` trust profiles mapped into `PermissionRules`; writes and terminal creation require the broker round-trip (R5). Where a primitive only exists inside ee-cli today, extract it to the host or remote crate rather than duplicating logic.
  - [x] **Acceptance criteria:** hermetic fake-transport test: agent `fs/read_text_file` inside workspace succeeds under `mcp_safe_read`; read outside workspace without a grant fails closed with the typed error; `fs/write_text_file` without an approval answer ends denied after timeout; terminal create is gated and kill honors the owned-terminal rule.
  - [x] **Edge cases:** no editor buffer semantics — headless writes go directly to disk with approval gates, a documented, intentional divergence from the editor path (editor routes through buffer/edit/save); decide and fixture behavior for symlink escapes and absolute paths outside roots; elicitation in headless mode v1 = typed "unsupported" (fails closed) unless forwarded via R5-style approval.

- [x] **R4 — Bridge dispatch and event fan-out.** Map manager/thread commands to the R2 registry and pump `AgentEvent`s out as ordered notifications with reconnect cursors.
  - [x] **Problem:** `AgentEvent`s are in-process channel messages; remote consumers need ordered, resumable notification streams per session, plus honest turn lifecycle answers (including the no-steer constraint).
  - [x] **Fix:** dispatcher module translating `session/*` and `turn/*` registry methods into `AgentManager`/`AgentThread` calls; an event pump that converts each `AgentEvent` into the matching notification; per-session bounded ring buffer with a monotonically increasing cursor so a reconnecting client can resume from its last seen cursor; `turn/completed` synthesized from thread state. `turn/start` while a prompt is active returns the typed `AlreadyActive` error (mirrors `ActivePromptError`) — the error message points drivers at `turn/interrupt` or waiting; queueing is I15, not silently invented here.
  - [x] **Acceptance criteria:** end-to-end hermetic stdio test with `FakeAgentTransport`: `initialize` → `session/new` → `turn/start` → ordered `session/update` stream → `turn/completed`; a second `turn/start` on the same session while active returns the typed error; `turn/interrupt` cancels and the turn ends interrupted; full replay from cursor 0 after a simulated reconnect reproduces the same event sequence.
  - [x] **Edge cases:** notification ordering vs response ordering (responses may overtake the final notification — clients must treat `turn/completed` as authoritative); backpressure: ring-buffer overflow drops oldest notifications but never the cursor sequence, and a full outbound queue returns `-32001` overload instead of blocking the pump; slow-consumer disconnect must not stall other sessions on the same host; cursor gaps after overflow are reported so clients know to re-sync via `history/tail`.

- [x] **R5 — Permission approval round-trip.** Surface broker approvals to the remote consumer as a first-class request/answer flow, fail-closed by default.
  - [x] **Problem:** the permission broker currently resolves through the ee-cli UI; remote drivers (hermes, web) have no way to see or answer pending approvals, so every gated operation would time out denied in headless mode — safe but useless.
  - [x] **Fix:** `permission/requestApproval` emitted as a server→client JSON-RPC request with the broker's typed `PermissionRequestInfo` (redacted via `redact.rs`); `permissions/pending` + `permissions/respond` registry methods for poll-based drivers (MCP surface); a configurable response timeout that ends in deny (fail-closed, same as empty rules); `PermissionRules` still evaluated first — declarative `allow` answers without a round-trip, `deny` always wins; every remote decision appended to an audit log line (who, what, decision, timestamp).
  - [x] **Acceptance criteria:** hermetic test: a write-class operation surfaces one approval request; `respond(allow)` lets it proceed and `respond(deny)` fails it with the typed error; expiry with no answer ends denied; a rules-based `allow` short-circuits with no request emitted; audit log entries exist for both the auto and manual paths.
  - [x] **Edge cases:** two connected clients both answer — first answer wins, later answers get a typed `already_resolved`; client disconnect while pending — request stays pending until timeout (deny), never auto-approved; redaction must strip secrets/PII from approval payloads before they cross the remote boundary; malformed `respond` payloads fail closed.

- [x] **R6 — `ee do agent serve` entrypoint and headless config resolution.** CLI wiring plus the config/registry path a headless host needs.
  - [x] **Problem:** there is no subcommand that starts the host without the editor, and agent-server discovery/config resolution (`[agents.servers]`, registry download, secret resolution) lives in ee-cli modules — a headless process cannot reach it without dragging the frontend crate in.
  - [x] **Fix:** new `ee do agent serve` subcommand (new small command file in ee-cli, main.rs only routes to it), `--stdio` default transport for v1; extract the agent-server config resolution + registry discovery into a crate the remote host can consume (or move it into `ee-agent-host`), reusing the same config layers and `secret://` resolution so editor and headless agree on configured agents; no editor startup, no TUI deps in the serve path.
  - [x] **Acceptance criteria:** `ee do agent serve --stdio` runs the R3/R4/R5 bridge against configured agents from a clean shell with no editor running; `--help` documents transport flags; a config with no `[agents]` servers produces a clear typed error, not a panic; clippy/fmt clean; the new command file stays well under the 1k LOC rule.
  - [x] **Edge cases:** `secret://` references must resolve headless or fail with the same typed secret errors as the editor path (never prompt interactively in serve mode); registry download on first headless run needs bounded timeouts and a no-network failure mode; concurrent `ee do agent serve` and editor session on the same config must not corrupt state_dir (both are separate ACP clients of separate agent processes — verify no shared mutable files beyond the documented stores).

- [x] **R7 — MCP server for agent drivers (`ee do agent serve-mcp`).** rmcp 3.x `ServerHandler` exposing the host core to MCP clients like hermes.
  - [x] **Problem:** agent drivers speak MCP, not the raw remote protocol; today ee's MCP server surface is the editor proxy (`ee --mcp-proxy`), which is not standalone and exposes editor tools, not agent-control tools.
  - [x] **Fix:** new MCP stdio server as a second thin surface over the same `ee-agent-remote` host core (one process per driver connection, embedding the bridge); follow the `ee_mcp::EeMcpProxy`/`tool_governance` rmcp 3.x pattern for handler structure, tool metadata, argument validation, and result mapping; hermes config: `[mcp_servers.ee-agent] command = "ee" args = ["do", "agent", "mcp"]`.
  - [x] **Acceptance criteria:** an MCP client test harness (reuse `ee-mcp/tests/manager_flows.rs` patterns) completes initialize → tools/list → tools/call round-trips; every tool schema validates its args and fails closed on malformed input; tool results bounded (byte-capped, truncated with a marker); a driver can drive a full fake-agent turn end-to-end (send task, poll updates, get completion).
  - [x] **Edge cases:** one MCP server process = one host = its own agent connections (no sharing across driver processes — document it); tool-result size caps because MCP results cross a stdio boundary; server startup failure (no agents configured) surfaces as MCP error, not a silent hang; rmcp stays on the workspace-pinned 3.x line (do not pull the ^2.x SDK).

- [x] **R8 — MCP tool set for agent drivers.** The concrete tools hermes calls.
  - [x] **Problem:** drivers need deterministic, poll-based control: what is the agent doing, start new work, read history — the raw JSON-RPC surface maps to tools but each needs bounded, typed contracts.
  - [x] **Fix:** tools over the R2 registry: `ee_status` (connection state, configured agents, loaded sessions); `ee_sessions_list`; `ee_session_new` (cwd, agent id, mode); `ee_task_send` (session id + text → turn started, returns immediately, typed `AlreadyActive` error if a turn runs); `ee_updates_poll` (session id + cursor → active tool, text tail, plan state; capped items per call); `ee_turn_interrupt`; `ee_session_history` (durable-log tail, requires R6-verified `session_log_dir`). All inputs validated; all outputs capped.
  - [x] **Acceptance criteria:** each tool has a hermetic test against `FakeAgentTransport`; `ee_task_send` → `ee_updates_poll` → completion observed in a single test; `ee_session_history` returns log entries after a completed turn when the durable log is enabled and a typed "log disabled" error when not; cursors are stable across polls (no missed or duplicated updates in the assertion).
  - [x] **Edge cases:** `ee_updates_poll` on an idle session returns an empty delta, not an error; very long turns — pagination caps with continuation cursors; driver passing a stale session id gets a typed not-found, never a panic; text tails must pass the same redaction as R5 payloads.

- [x] **R9 — MCP permission tools and remote permission mode.** Driver-visible approval flow with an explicit trust gate.
  - [x] **Problem:** an MCP driver is a program, not the human user — letting hermes auto-answer approvals silently would break the fail-closed posture; but requiring a human for every write makes remote agent work useless when the user has pre-decided.
  - [x] **Fix:** tools `ee_permissions_pending` and `ee_permission_respond` reusing R5 semantics verbatim (timeout→deny, rules first, audit); plus a config `remote_permission_mode`: `ask` (default — approvals surface to whoever the driver relays to), `rules` (only declarative `PermissionRules`/trust profiles answer; anything unmatched stays pending until timeout), `deny` (all gated ops refused immediately). No mode ever blanket-auto-approves writes or terminal from agent context alone.
  - [x] **Acceptance criteria:** hermetic tests cover all three modes; in `rules` mode an unmatched write-class op ends denied after timeout with zero approval requests emitted; in `ask` mode the pending list and respond flow work end-to-end; mode parse errors fail closed to `ask` semantics (surface everything) rather than silently widening.
  - [x] **Edge cases:** mode set per driver config, not per prompt (a prompt must not be able to escalate its own mode); `ee_permission_respond` for an already-resolved request returns typed `already_resolved`; audit log records the mode in force for each decision.

- [x] **R10 — WebSocket transport, token auth, health endpoints, overload semantics.** Network surface for the web client (and any future non-stdio driver).
  - [x] **Problem:** stdio only reaches same-machine child processes; the web UI and remote-driver cases need a socket transport, and an unauthenticated local listener is a remote-code-execution hinge (agent can run terminals and write files).
  - [x] **Fix:** implement the R2 protocol over WebSocket on the serve command: bind `127.0.0.1` by default; bearer token from a token file (0600) compared in constant time, required before any `initialize`; `GET /healthz` and `GET /readyz`; reject requests carrying an `Origin` header on the health routes (codex pattern); bounded outbound queues with `-32001` overload and retry guidance; non-loopback binding refuses to start without an explicit flag and prints the SSH-tunnel/reverse-proxy+TLS guidance instead (plain ws is loopback/tunnel-only, exactly like codex `--listen ws://`).
  - [x] **Acceptance criteria:** token-less and wrong-token connections are rejected before any method executes; health endpoints answer 200 without Origin and 403 with Origin; a slow consumer triggers `-32001` on further requests while the host stays responsive for other clients; reconnect with the R4 cursor replays missed notifications; non-loopback bind without the explicit flag fails with guidance text.
  - [x] **Edge cases:** multiple simultaneous ws clients fan out to the same session streams (permission first-answer-wins per R5); token file rotation requires reconnect — document that tokens are read per connection; health endpoint must not leak session or config data; ws idle timeout that does not kill in-flight turns.

- [x] **R11 — Web chat UI (Rust → WASM only, no JS/TS).** Human-facing chat over the same host.
  - [x] **Problem:** the only human surface for agent mode is the TUI; a browser (local or tunnelled) is the natural remote human surface, and RULE.md requires it to be a thin frontend over backend state, not a second brain. Constraint: the frontend must be Rust compiled to WASM — no JavaScript or TypeScript source anywhere in this repo.
  - [x] **Fix:** a separate frontend crate (e.g. `crates/ee-web-frontend`) compiled to `wasm32-unknown-unknown` with Rust web deps only (wasm-bindgen family — Leptos, Yew, or raw web-sys; Rust bundler such as trunk if needed); the serve command serves the compiled `.wasm` (+ toolchain-generated bootstrap glue only) as static assets and `WS /ws` speaking the R2 protocol; all state derives from notifications and method responses; every mutation is a registry method call; backend emits data, frontend renders. Token auth identical to R10. The separate crate exists because the wasm target triple differs from the host crates' — it must not drag native deps into the workspace's default target graph.
  - [x] **Acceptance criteria:** browser flow works end-to-end against a fake-agent backend: list sessions, open one, send a message, watch streamed updates render in order, answer a permission prompt, see `turn/completed`; reconnect restores the transcript from cursor + `history/tail`; the frontend crate builds with `cargo build --target wasm32-unknown-unknown` and no node/npm/yarn step exists; a checked-in guard (CI grep or test) fails if any hand-written `.js`/`.ts` file appears in the frontend crate — the only permitted non-Rust files are toolchain-**generated** loader glue and the static HTML shell; the served frontend contains no logic that duplicates backend policy (no client-side permission rules); frontend rendering logic is unit-testable as plain Rust (render-model functions take protocol types and produce view state).
  - [x] **Edge cases:** wasm-bindgen emits a small generated JS loader — acceptable because it is generated, never authored (document this rule in the crate README); wasm binary size (set a cap, enable size optimizations — `opt-level = 'z'`/wasm-opt if available in the Rust toolchain path); all agent-produced text is set via DOM text nodes (agent output is untrusted input — no `innerHTML`/raw HTML/markdown execution in v1, render as text); token delivery to the browser (printed URL with one-time token vs manual paste — pick one, document); viewport/perf with long transcripts (cap rendered lines, older content via history paging); streaming throughput at the wasm boundary (batch notification application per frame tick, no per-delta DOM thrash); framework choice must keep the 1k LOC module rule — split components like any Rust module.

- [x] **R12 — Redaction and audit hardening pass.** Everything crossing the remote boundary passes the redaction layer; decisions are auditable.
  - [x] **Problem:** remote surfaces widen who can observe agent output (updates, approvals, history) and who can trigger gated actions — without a systematic pass, secrets in tool output, terminal env, or transcripts can leak across the boundary, and remote-triggered actions leave no accountable trail.
  - [x] **Fix:** single choke point where every outbound notification, approval payload, tool result, and history page passes `redact.rs` (reuse, not a second redactor); the audit log (from R5) gains the remote command surface: every `turn/start`, `turn/interrupt`, and permission decision with client identity, method, and outcome; audit append is failure-tolerant (a failed append is a logged warning, never a dropped turn) but visible in `ee_status`.
  - [x] **Acceptance criteria:** fixture-driven tests assert known secret shapes (env values, auth headers, `secret://` echoes) never appear in outbound payloads from any of the three surfaces; audit entries exist for a scripted full session including one approval; a redaction regression test seeded from the `ee-chat-completions` cassette redaction cases.
  - [x] **Edge cases:** redaction must run before ring-buffer insertion (not only at send), so buffered replays are already clean; audit log rotation/size cap so it cannot grow unbounded; redaction false positives (legitimate content mangled) are acceptable over false negatives — document the posture.

- [x] **R13 — Timeouts, cancellation, and lifecycle hardening.** Bounded remote-driven sessions.
  - [x] **Problem:** remote clients are flakier than a local TUI: connections drop mid-turn, drivers hang, ws clients vanish — without explicit lifecycle rules the host leaks agent turns, terminals, and sessions, or silently kills work a user wanted to continue.
  - [x] **Fix:** per-request timeouts on the registry surface (typed timeout errors, never hangs); `turn/interrupt` propagates to ACP `session/cancel` on every disconnect-with-interrupt path; on client drop mid-turn the turn **continues** (Claude cloud pattern) and completion lands in the ring buffer + durable log for later pickup; idle ws connections time out without affecting in-flight turns; host shutdown cancels active prompts and fails pending requests cleanly (mirror the ACP framework's transport-close semantics from `ee-agent-acp-server`).
  - [x] **Acceptance criteria:** hermetic tests: client drop mid-turn → turn completes and `history/tail` + cursor replay show the completion; `turn/interrupt` while an approval is pending cancels the turn and resolves the approval as cancelled; host shutdown leaves no zombie fake-agent prompt (assert transport close reached the agent); every timeout produces the typed error, not a panic or silent stall.
  - [x] **Edge cases:** double interrupt (second is a typed no-op/already-done); interrupt racing turn completion (whichever wins, the other path must stay consistent — completion notification or interrupted completion, never both); long-running background terminals outliving a turn follow the existing owned-terminal kill gate, not a new remote rule.

## Part 10.1 — Cross-references and delivery gates

- I15 (prompt inbox: queue + steer) is the natural follow-up to this plan's no-steer limitation; once it lands, the R2 registry gains a delivery hint on `turn/start` and the MCP `ee_task_send` gains queue semantics.
- Unified Harness Plan Phase 4b Gap 2 (checkpoint branch surface) — when the ACP/CLI surface lands, mirror it as one more registry method here rather than a remote-specific invention.
- Audit Improvement 9 (risk-tiered auto-approval classifier) composes with R9's `rules` mode; classifier outcomes must never widen `deny` mode.
- Explicitly out of scope: ACP-to-ACP relaying (excluded by decision), hosted always-on relay service, multi-tenant auth (OAuth), and mobile push. Reopen only with an explicit design decision.

Delivery gates for each item: `cargo fmt --all -- --check`; `cargo clippy -q -D warnings` on touched crates; targeted `cargo test --quiet -p <crate> <module>` per the escalation rules (`scripts/run-ee-cli-touched-tests.sh` for ee-cli touch points); hermetic tests via `FakeAgentTransport` only — no network, no provider keys, no real `~/.config` plugin tree (`EE_TEST_DISABLE_USER_PLUGINS` discipline); mem-guarded full package only when the change is crate-wide.

# Recursive Harnessing — Harness2 Adaptation (2026-11-23)

Source: arXiv 2610.14224v1 (Google "Harness²: Recursive Agent Harnessing for an Open World") + vendored clone `harness-example/harness2/`. **Detailed implementation plan: [docs/PLAN-HARNESS.md](docs/PLAN-HARNESS.md)** — schemas, file maps, fixed decisions D1–D10, sequencing, and the harness2→ee module map live there; items below are summaries. Distinct from `docs/upgrades/PLAN-harness.md` (DSH prompt-sections parity, landed): this part adapts Harness2's improvement loop — the harness editing itself from execution evidence — and builds on the section registry that plan delivered.

## Part 11 — Phases and items

### Phase H0 — Prompt-content transfers (no new machinery)

- [x] **I28 — Editor-pattern guidance transfers.**
  - [x] **Problem:** built-in tool guidance and role prompts contain none of the improver discipline Harness2 encodes (divergence-first edits, evidence-moment rule, `KEEP_BASE` as a valid answer, form preference order skill > script > subagent > hook > always-on prose, deliverable-shape rule).
  - [x] **Fix:** add these blocks to `prompt/tool_guidance.rs` guidance and the delegation/review sections; hermetic tests assert rendering conditions and section byte budgets. Plan §5.
  - [x] **Acceptance:** guidance blocks render only when relevant tools are advertised; no config or schema changes; targeted module tests green.
- [x] **I29 — Selector criteria into the reviewer role.**
  - [x] **Problem:** review prompts lack the fixed artifact-choice criteria order (delivery → conformance → fidelity → internal consistency → operational fitness → finish) and the "plainer artifact is valid" rule.
  - [x] **Fix:** write the criteria order into the `reviewer` role body in `subagent_roles.rs`. Plan §5 (H0.3).
  - [x] **Acceptance:** reviewer role test asserts criteria presence and order; no behavior change beyond role text.

### Phase H1 — Editable component surface completion

- [x] **I30 — Guardrails file component.**
  - [x] **Problem:** always-on constraints are merged into the system prompt; users and improvers cannot edit a bounded guardrails component.
  - [x] **Fix:** discover `.agents/guardrails.md` + `$XDG_CONFIG_HOME/ee/guardrails.md` via `markdown_config.rs`; render one trust-labeled untrusted section after the system prompt; 32 KiB cap; absent file emits nothing. Plan §6.
  - [x] **Acceptance:** prompt bundle carries labeled guardrails; workspace-over-user precedence; malformed/oversized skipped with warning; hermetic tests.
- [x] **I31 — Tool-guidance override file.**
  - [x] **Problem:** `TOOL_GUIDANCE` is a hardcoded const; not user- or improver-editable.
  - [x] **Fix:** `.agents/tool_guidance.md` (untrusted, trust-labeled) augments/overrides the builtin section per tool; const stays default fallback; bounds mirror skills. Plan §6.
  - [x] **Acceptance:** override wins with label; absent tree emits nothing; builtin fallback intact; tests mirror `prompt/skills.rs` suite.
- [x] **I32 — Scripts convention.**
  - [x] **Problem:** no bounded scripts component; agents hand-roll repeated deterministic operations.
  - [x] **Fix:** `.agents/scripts/` + user mirror, extension allowlist (`sh`, `py`), bounded discovery, catalog line into tool guidance; execution stays the existing terminal approval path. Plan §6.
  - [x] **Acceptance:** discovery bounds and allowlist enforced; catalog renders; no new execution surface.
- [x] **I33 — Declarative middleware hooks (active when a valid file is present).**
  - [x] **Problem:** no file-level middleware an improver can edit; prose cannot enforce behavior ("prose can be stopped reading; a hook cannot").
  - [x] **Fix:** `.agents/middleware.toml` strict-parse; bounded hook set executed at existing loop boundaries: `warn_no_artifact_after_calls`, `hint_on_error_signature` (bounded), `verify_after_write`; per-turn use caps; unknown key ⇒ loud warning + file ignored; active exactly when the file is present and parses — mirror of harness2's mount-after-validation, no config gate. Plan §6, decision D3.
  - [x] **Acceptance:** hook fires at exactly N calls; caps enforced; hooks append text and never block valid completion; malformed or absent file ⇒ byte-identical sessions.

### Phase H2 — Harnessing-level recursion (validation, experience library, reflector)

- [x] **I34 — Harness-edit validation layer.**
  - [x] **Problem:** no validation before adopting harness edits — the paper's three discard rules (skill trigger line, script existence, frontmatter) and autotrigger repair have no ee equivalent.
  - [x] **Fix:** pure module `harness_validation.rs`: trigger-line rule (skill named verbatim in always-on text), script-reference existence/bounds/allowlist plus bounded syntax check (`sh -n`, `python -m py_compile`), frontmatter validation, reserved/duplicate names, editable-surface naming rules (off-surface files named in the error), autotrigger repair with recorded repairs, and an **always-on byte cap** with `always_on_char_delta` surfaced in every diff summary. Plan §7, mirror `tree.py validate`.
  - [x] **Acceptance:** unit matrix covers every discard rule including the byte cap; census counts recorded; repairs are recorded, never silent; partially-valid tree persisted into the retry context; one retry with validator message then inherit (adopted by I39/I41).
- [x] **I35 — Experience library `.agents/experience/`.**
  - [x] **Problem:** no per-domain durable record of what harness edits did — lessons, verdicts, or adopted trees; "experience" has zero graph matches.
  - [x] **Fix:** `user_experience.rs` mirroring `user_memories.rs`: `experience/<domain>/{INDEX.md, entries/<seq>-<slug>/{harness/, EXPERIENCE.md}}`; verdict vocabulary `helped|hurt|no-observed-effect|confounded`; `NO_ENTRY` valid; bounded cue catalog per session; `load_experience` read-only tool; streaming retention (only earlier `seq` visible online). Plan §7, mirror `library.py`.
  - [x] **Acceptance:** tests mirror the `user_memories.rs` suite plus retention; one lesson per task bound; whole adopted tree stored, never a summary.
- [x] **I36 — Reflector: `/reflect` command + post-session hook.**
  - [x] **Problem:** sessions end without contrast-backed lesson extraction; existing `reflection.rs` is in-turn self-review only, nothing persists across tasks.
  - [x] **Fix:** builtin `harness_reflector` role reads the full improvement record — chosen harness + non-base candidate probes + a base-harness copy for diffing + adoption.json (anon-id map + the selection `<why>`) + decisions notes + session tool records/workspace diff; the session runs on a curated copy (raw selection record excluded, mirror `_distil`); emits `<edits>` verdict rows + at most one `<experience>` block, lessons require cue+lesson+evidence all non-empty, edits stored separately, grader-verdict assert also on the parsed experience; writes through the existing approval-gated path (pattern of `user_memory_write.rs`); grader/rubric/score material refused at projection build (decision D5); selection+reflection spend tracked outside the R/P ledger identity; manual `/reflect` for one-off sessions, and the reflector runs as the final stage of every `/harness` run (mirror `schedule.py` reflect-after-each-task). No automatic editor-session hook — out of scope. Plan §7, mirror `selection.py` reflector.
  - [x] **Acceptance:** parser round-trips; vocabulary enforced; one-lesson bound; curated-copy exclusion; `NO_ENTRY` writes edits-only entry; write honors approval; leak guard tested including on the parsed lesson.

### Phase H3 — Task-level recursion loop (new crate `ee-agent-harnessing`; requires H1+H2)

- [x] **I47 — Task adapter seam (second pass).**
  - [x] **Problem:** the loop has no input contract — nothing defines what a "task" is, what counts as delivered, or how deliverables render for selection.
  - [x] **Fix:** `HarnessTask` mirror of `spec.py BenchmarkAdapter`: task prompt (current session's user prompt), source files (workspace roots + I20 attachments), `delivered(run)` (final response + I19 snapshot diff), `deliverable_text`/`deliverable_assets` (bounded render, exact names, never a whole run dir), `domain_of` for library scoping, base harness dir. Plan §8.
  - [x] **Acceptance:** adapter contract unit-tested; delivered() agrees with snapshot diffs; single-task mode adapts the current session.

- [x] **I37 — Evidence export (rollout views).**
  - [x] **Problem:** the append-only session log has no improver-facing projection — bounded per-turn records, redaction, grader-leak guards, and the improver context-dir layout do not exist.
  - [x] **Fix:** `rollout_view.rs` seam in the orchestrator + context-dir builder in the harnessing crate (`context/{task,source,harness,library,rollouts,decisions,mission}.md`); redaction reuses the R12 layer; truncation bounds mirror `context.py _write_rollout`; leak asserts at harness dir, library dir, and context root; mission text describes only the evidence that landed; names-only deliverables mode for oversized artifacts; zero renderable trajectories ⇒ loud error ("adapting on nothing"). Plan §8.
  - [x] **Acceptance:** hermetic; redaction, truncation, and three-point leak asserts; empty-evidence raise; mission lists only present dirs.
- [x] **I38 — Per-session harness source.**
  - [x] **Problem:** components resolve from the workspace only; a probe cannot mount a candidate harness without mutating the workspace.
  - [x] **Fix:** `HarnessSource { Workspace, Override(PathBuf) }` in `OrchestratorConfig`; prompt assembly resolves guardrails/tool-guidance/scripts/skills roots from it; default `Workspace` keeps today's behavior byte-identical. Plan §8, decision D1 seam.
  - [x] **Acceptance:** override session renders candidate files; workspace sessions unchanged; small additive seam with tests.
- [x] **I39 — Propose stage.**
  - [x] **Problem:** no scoped improver sessions that edit component files from reference-run evidence.
  - [x] **Fix:** driver obtains k references, builds the evidence dir, spawns evidence-checked `harness_editor_inv` + `harness_editor_dom` roles whose file writes are scoped to a candidate dir — sessions run in isolated temp workspaces (context copied in, edited harness harvested back, deny-outside fs scope, scrubbed env, per-session state roots, mirror `proposer.py`); improver model/effort/steps/timeout configurable independently of the executor (mirror `ChannelSpec`); I34 validation with one retry carrying the partially-valid tree back into the retry context, second failure inherits current harness (recorded fallback); empty returned tree rejected. Plan §8, mirror `recursion.py _run_stage`.
  - [x] **Acceptance:** fake-transport tests; validator-retry-inherits path; scoped writes; isolation asserts; empty-tree rejection; INV and DOM prompts stay scoped (ablation: scoping worth 2.9–3.7 pts).
- [x] **I40 — Probe stage.**
  - [x] **Problem:** no matched re-roll of a candidate harness.
  - [x] **Fix:** one headless re-roll per candidate via the `ee-agent-remote` host under matched model/env config, mounting the candidate through I38; existing timeouts/cancellation; rollout view captured per probe. Plan §8, decision D9.
  - [x] **Acceptance:** hermetic; candidate files mounted, workspace untouched; probe isolation asserted.
- [x] **I41 — Compose stage.**
  - [x] **Problem:** candidates are alternatives with no evidence-tied merge step.
  - [x] **Fix:** `harness_composer` reads the full pool + diffs, emits one composed tree plus notes `kept|revised|dropped -> <edit> -> <trajectory moment with counts>`; merge/adopt-one/keep-base all valid; non-delivering-rollout edits unsafe unless overwhelming evidence; I34 gates the composed tree. Plan §8, mirror the composer prompt.
  - [x] **Acceptance:** notes parser; validation retry/inherit; rule recorded in notes and enforced by prompt contract.
- [x] **I42 — Selection stage.**
  - [x] **Problem:** no anonymized best-of-k deliverable pick, so k>1 recursion has no final answer rule.
  - [x] **Fix:** pool = k finals with seeded-shuffle anon ids (deterministic per task/k/mode); base deliverable is a valid candidate and the selector is told so; **k=1 bypasses the selector session** (single final, standard record, mirror `trivial_selection`); non-delivering candidates dropped with recorded causes before anonymization; no-file-deliverable tasks fall back to trajectory-as-candidate; candidates written untrimmed with size headers; rendered evidence assets copied per candidate (adapter-named files only, never the whole run dir); selector failure never raises — neutral fallback = delivered base, else first final; fenced ```choice + ≤6-line `<why>`. Plan §8, mirror `selection.py`.
  - [x] **Acceptance:** id determinism; parse and neutral-fallback tested; drop-with-cause recorded; k=1 bypass writes the standard record; base-can-win path exercised.
- [x] **I43 — Budget ledger + driver surface.**
  - [x] **Problem:** no spend accounting for improvement loops and no default-off way to run one.
  - [x] **Fix:** ledger derived from session-log usage records asserting R=4k rollouts / P=3k sessions per task, with selection+reflection spend tracked outside the identity; hard caps, typed budget errors, totals visible in `usage_stats`; ledger written in stable label order after concurrent pools drain (concurrent run records the same sequence as a serial one); dead rollouts recorded never raised; no-delivering-base task short-circuits to a recorded `base-failed` state; mode exceptions record the traceback tail and continue; `/harness k=N mode=parallel|sequential` slash command + manual CLI path, always registered and advertised — invocation is the only gate, matching harness2 (`k` defaults to 1); dry-run prints the plan without model calls; stream mode: per-domain ordered tasks, concurrency across domains only (`--workers`), seeded shuffle from sorted order so each domain's stream is independent, stale frozen domain order is a loud error; library roots scoped per run identity (workspace + mode + k), scratch root for measurement runs. Plan §8, decisions D6/D10, mirror `ledger.py` + `schedule.py`.
  - [x] **Acceptance:** identity asserts; caps; dry-run hermetic; stable-order recording under concurrency; dead-rollout and base-failed paths; stream determinism; improvement spend visible next to task spend.
- [x] **I48 — Run state, resume, and improver session cache (second pass).**
  - [x] **Problem:** a crashed or re-run sweep re-spends every rollout and improver session, and stale results (changed task inputs, edited adopted harness) can be silently reused.
  - [x] **Fix:** per-task run summary (all modes + selections) so rerunning resumes; completion marker written only after a run is observed to return; harness fingerprint (sorted content walk, byproducts ignored) + task-inputs fingerprint (prompt + source files); resume only when inputs unchanged, adopted artifacts intact, harness tree present and fingerprint-matching — else re-run with a logged reason; improver replies cached content-addressed by (label, mission, system, context digest) with workspace-present checks, every call audited either way. Plan §8, mirror `runmark.py` + `proposer.py _call_fingerprint` + `schedule.py _resume`.
  - [x] **Acceptance:** fingerprints stable; inputs-changed and harness-changed re-run; cache hit/miss; marker only after completion; audit for every call.

### Phase H4 — Measurement

- [x] **I44 — Harness-edit regression suite.**
  - [x] **Problem:** a harness edit can regress a domain with nothing catching it before it enters the library.
  - [x] **Fix:** per-domain fixtures on `evaluation.rs` + the record/replay provider; deterministic deliverable checks only (no LLM judge in CI); suite must stay green on the base harness. Plan §9.
  - [x] **Acceptance:** suite green on base; a deliberately bad edit (missing trigger line) is caught.
- [x] **I45 — Steps and tool-error metrics.**
  - [x] **Problem:** `usage_stats` (I23) lacks per-task step counts and tool-error-rate trend against harness adoption time — the paper's E.4 refinement signal.
  - [x] **Fix:** extend `usage_stats.rs` with step-count and tool-error-rate rows plus an optional trend view keyed by experience `seq`; surfaced through the existing `usage_stats` tool. Plan §9.
  - [x] **Acceptance:** deterministic from the session log; trend visible after I35 lands.
- [x] **I46 — Leave-one-out library evaluation.**
  - [x] **Problem:** no way to verify experience entries actually transfer rather than accumulate.
  - [x] **Fix:** offline harnessing-crate mode re-running a task with its own entry excluded from the completed library, reporting per-domain deltas; eval-only, never an online path. Plan §9, mirror the paper's E.2 protocol.
  - [x] **Acceptance:** hermetic; exclusion correctness; delta report.
- [x] **I49 — Deterministic demo driver (second pass).**
  - [x] **Problem:** the full loop cannot be exercised end to end without provider credentials, so CI and onboarding both skip the real workflow.
  - [x] **Fix:** mirror `demo.py`: a small deterministic task adapter + fake channel runs reference runs, propose, probe, compose, select, and reflect through the real driver code, zero model keys; ships as a harnessing-crate example/test binary. Plan §9.
  - [x] **Acceptance:** the demo run itself is a CI test; full loop executes hermetically on every driver change.

## Part 11.1 — Sequencing and gates

- H0 lands immediately with no dependencies; H1 items I30–I33 are independent — file presence/validity is their activation; H2 order I34 → I35 → I36; H3 order I47 (input contract) → I37/I38 seams → I39→I40→I41→I42 → I43 (driver + stream) → I48 (resume/cache) last; the loop has no config or flag gate — invocation (`/harness`, CLI) is the gate, matching harness2, `k` defaults to 1; H4 starts after H1 (I44), after H2 (I46), and with the driver (I49).
- Invocation-is-the-gate: nothing in H1–H3 runs unless the file exists (guardrails/tool-guidance/scripts), parses (middleware), or the user runs the command; H0 changes only prompt text.
- Delivery gates identical to Part 10.1: fmt, clippy on touched crates, targeted `cargo test --quiet` per escalation rules, hermetic `FakeAgentTransport` tests with `EE_TEST_DISABLE_USER_PLUGINS` discipline, mem-guarded full package only as the manual gate.
- Explicitly out of scope: LLM-judge score loops (label-free by design, decision D5), plugin-code authoring by improvers (D3), model-weight changes, and replaying completed tasks online (streaming retention only; leave-one-out is offline eval).

## Part 11.2 — Parity closure and approval write path (2026-11-24)

Audit found landed H0–H4 code still leaves four parity gaps: `/harness` cannot persist reflection, the adopted tree is never applied, the improver context gets only cue lines instead of the full library, and `run_stream` is not wired to a CLI. Detailed plan: [docs/PLAN-HARNESS-PARITY.md](docs/PLAN-HARNESS-PARITY.md). Decisions D11–D16 make library writes invocation-consented; only active `.agents` adoption requires a separate `harness adopt`/`/adopt` approval.

### Phase H5 — Full library read parity

- [x] **I50 — Experience entry harness accessor and bounded loader.**
  - [x] **Problem:** `save_entry` writes `.agents/experience/<domain>/entries/<seq>-<slug>/harness/`, but `HARNESS_DIR` has no production reader.
  - [x] **Fix:** add a bounded `library_context` loader that resolves `entry.path.parent()/harness`, validates the tree, caps files/bytes/depth, rejects symlinks and escapes, and skips malformed entries with a warning.
  - [x] **Acceptance:** round-trip write/load test; malformed/missing tree skipped without failing the context; no path outside the entry can be read.
- [x] **I51 — Full library evidence in the improver context.**
  - [x] **Problem:** `StageEvidence` carries only `library_cues`; `context/library/cues.md` is the only prior-task evidence.
  - [x] **Fix:** add bounded `LibraryEvidence` entries and copy `EXPERIENCE.md` plus the full `harness/` tree into `<run-root>/context/<stage>/library/<domain>/<seq>-<slug>/`; generate stage-local `library/INDEX.md`; caps 16 entries / 1 MiB total / 64 KiB per file; redaction and grader guards on every file. Do not rewrite `<workspace>/.agents/experience/<domain>/INDEX.md` here.
  - [x] **Acceptance:** second task sees first task's full tree; stage-local INDEX exists; persistent workspace index untouched by context build; truncation deterministic; malformed skip; grader material refused.

### Phase H6 — Approval and persistence parity

- [x] **I52 — Invocation-consented reflection write and opt-out.**
  - [x] **Problem:** `/harness` child stdin is `null`; reflection is always declined, and the current `--yes` gate makes every run need a second approval.
  - [x] **Fix:** default `--reflection=accept`; `--reflection=decline` runs reflection without writing; `--no-reflect` skips the stage. Remove `--yes` from `improve`; no `/harness yes` token. Record `reflection_write`/`reflection_source` in the run summary. Invocation is the consent boundary.
  - [x] **Acceptance:** `/harness` writes without an extra prompt; `--no-reflect` skips; `--reflection=decline` writes nothing; active `.agents` unchanged in all `improve` paths.
- [x] **I53 — Shared library-root resolution.**
  - [x] **Problem:** `HarnessTask.library_root` and the CLI writer root are separate; sweep/run identity scoping is not wired.
  - [x] **Fix:** resolve one `LibraryLocation` per invocation: single task defaults to workspace, sweep defaults to `<run-root>/library/<mode>-k<k>`, `--library-root` overrides; pass the same value to reader and writer and record it.
  - [x] **Acceptance:** read root == write root; scratch roots do not contaminate the workspace library.

### Phase H7 — Active harness adoption

- [x] **I54 — `ee do agent harness adopt`.**
  - [x] **Problem:** chosen tree stops at `<run-root>/adopted/.agents`; no validated apply path exists.
  - [x] **Fix:** new `harness_adopt_cmd.rs` with `--run-root`/`--experience` source, `--workspace`, `--dry-run`, `--yes`, `--include-memories`; validate, diff, back up, atomically apply only the `.agents` managed surface (`guardrails.md`, `tool_guidance.md`, `middleware.toml`, `scripts/`, `skills/`, `agents/`), remove managed files absent from the adopted tree, preserve `.agents/experience` and `.agents/memories` by default. Top-level `skills/`, top-level `scripts/`, and user-config skills are out of scope.
  - [x] **Acceptance:** dry-run no writes and shows removals; invalid tree rejected; backup; stale `.agents/skills`/`.agents/scripts` removal; unrelated files preserved; top-level skills/scripts and `$XDG_CONFIG_HOME/ee/skills` unchanged; symlink rejected; non-TTY without `--yes` declines.

- [x] **I58 — Agent-mode `/adopt` surface.**
  - [x] **Problem:** CLI adoption is outside the agent session; `HarnessLedgerReport.run_root` is recorded as `None`, so no in-session apply UX exists.
  - [x] **Fix:** report `run_root` through the child harness report/session log; add `/adopt [--include-memories]` that resolves the last run root, runs `harness adopt --dry-run`, shows the diff, confirms via `ClientBridge` elicitation, then applies with `--yes` only on Accept. Fixed argv; no model text in shell args.
  - [x] **Acceptance:** diff shown; Accept applies; Cancel/decline writes nothing; missing run root errors clearly; no model API key required.

### Phase H8 — Task stream parity

- [x] **I55 — Task manifest and `ee do agent harness sweep`.**
  - [x] **Problem:** `run_stream` has no production caller.
  - [x] **Fix:** add `Sweep(Box<HarnessSweepArgs>)` with `--tasks <JSON>`, `--k`, `--mode`, `--workers`, `--run-root`, `--library-root`, `--no-reflect`, `--dry-run`; manifest is invocation-level library scoped; parse into `HarnessTask` and call `run_stream`; new `harness_sweep_cmd.rs`; nonzero exit on task failure.
  - [x] **Acceptance:** two domains concurrent, tasks within domain sequential; frozen order reused; bad manifest fails before model calls; failed task reported without aborting completed domains.
- [x] **I56 — Stream retention and own-entry exclusion.**
  - [x] **Problem:** `run_stream` does not set `library_before_seq` or exclude the current task's prior entry.
  - [x] **Fix:** compute the cutoff before each task, add an explicit excluded-task set to `HarnessTask`, filter cues and full library entries by both, and reconcile the library on resume.
  - [x] **Acceptance:** task B sees task A; task A rerun does not see its own old entry; future entries invisible; concurrent domains isolated.

### Phase H9 — Verification and documentation

- [x] **I57 — Parity regression suite and docs.**
  - [x] **Problem:** the parity contract crosses approval, filesystem, library, and stream boundaries with no single suite.
  - [x] **Fix:** hermetic `FakeAgentTransport` tests for approval outcomes, full library context, adopt safety, stream order/retention, and task A → B → C learning; CLI tests; update plan/checklist; assert `HARNESS_DIR` has a production reader; correct the `user_experience` user-mirror claim or add an explicit user-scope write path (default stays workspace-only).
  - [x] **Acceptance:** all H5–H8 contracts covered without network; user-mirror documentation matches code; fmt/clippy/targeted tests; every touched code file under 1,000 LOC.

## MCP Server Trust (2026-11-24)

Goal: one explicit host-local trust decision covers the pinned tool surface of an MCP server without weakening the existing per-operation safeguards.

Rules:

- Workspace trust is the outer gate; an undecided or untrusted workspace makes every MCP server grant inert.
- Grants are pinned to the canonical workspace identity, server id, transport identity, and an application-owned manifest of `(tool, schema version, side-effect category)` entries.
- Read tools with no separate approval requirement and every workspace write tool (edits, content-bearing writes, filesystem create/copy/move/delete, and workspace-memory mutations) are eligible. External-network tools are never eligible. Terminal execution is a separate explicit opt-in that adds only `ee_terminal_create` as an execute tool.
- A persistent deny, safeguard, mandatory-confirm rule, session deny, or untrusted workspace outranks a server grant.
- New tools, schema changes, transport changes, and workspace revocation require a new approval; grants never follow server-advertised runtime changes.
- The store is host-local (`$XDG_STATE_HOME/ee/mcp-trust/`), owner-only, atomic, and keyed by the workspace digest; repository configuration cannot create authority.
- Scope is the built-in `ee` MCP server (stdio proxy and ACP-native routes). External `[mcp.servers]` entries are forwarded to agents and remain outside the editor trust store.

Work items:

- [x] Add `McpServerTrust`, pinned `McpServerGrant`/tool grants, candidate preview, and manifest-derived tool eligibility.
- [x] Add the evaluator `mcp_server_allow` layer between workspace gating and session/persistent allows; deny and mandatory-confirm layers still win.
- [x] Add the owner-only workspace-keyed MCP trust store with strict parsing, duplicate rejection, identity checks, atomic writes, and revocation.
- [x] Add `ee do agent trust mcp grant|revoke|status` with workspace-trust enforcement.
- [x] Add the `Trust MCP server (pinned tools)` approval option with preview/confirmation and persistence-failure denial.
- [x] Manage server grants from `/permissions` list/inspect/revoke/reset and include them in the summary.
- [x] Cover pinned matching, cross-transport and cross-workspace misses, persisted-tool drift, deny precedence, untrusted inertness, store permissions, malformed documents, and end-to-end approval/auto-allow behavior.
- [x] Second pass: carry the validated MCP identity through content writes, filesystem mutations, and workspace-memory prompts; gate exact always-allow separately from server trust; keep write safety maxima; reject execute/network candidates; prove cross-tool persistence end to end.
- [x] Add a separate terminal-execution opt-in (`--execute`, `Trust MCP server + terminal (run commands)`) that extends the server grant with `ee_terminal_create`; keep it out of the general grant, require validated argv, preserve command safeguards/denies/mandatory confirms, preserve execute pins on re-trust, and support execution-only revoke.
- [x] Update README and CHANGELOG.

Acceptance:

- [x] `cargo test --quiet -p ee-cli --features agents mcp_trust` passes.
- [x] `cargo test --quiet -p ee-cli --features agents mcp_server_trust` passes.
- [x] Existing trust, always-allow, command, and permission-matrix suites still pass.
- [x] No general grant authorizes execute, external-network, or unpinned tools; terminal execution requires its own validated-command opt-in; every pinned read or write tool covered by a grant dispatches without a per-tool prompt.

## ACP v2 Client Tool Continuity (2026-11-25)

Goal: keep ACP v1 as the host default while making ACP v2 sessions functionally complete. ACP v2 removes `fs/read_text_file`, `fs/write_text_file`, and the `terminal/*` client methods; the replacement is the application-owned `ee` MCP server over `session.mcp.acp` (or the stdio `ee --mcp-proxy` fallback). v2 sessions must never silently lose client tools, and agent-owned v2 command approvals must reuse the same workspace safe-read policy as v1 terminal requests.

Scope and non-goals:

- ACP v1 remains the host default (`AgentConnectionOptions::default()` stays `ProtocolVersion::V1`) until v2 leaves Preview and every shipped agent passes the v2 gate below.
- Removed ACP v1 client methods are never re-served on a v2 connection. No protocol method-to-MCP translation shim; agents call MCP tools.
- ACP v2 agent-owned execution is advisory: the agent runs the command. The host auto-allows only curated safe-read commands and otherwise prompts or answers deny; it never claims enforcement it does not have.
- MCP terminal tools keep the existing `CreateTerminalRequest` approval, safeguards, workspace trust, and MCP server-trust path.

### Phase V0 — Baseline invariants

- [x] **Problem:** the v1/v2 split, default version, and removed-method behavior are spread across crates without a single pinned contract, so later phases could regress them silently.
- [x] **Fix:** add focused tests that pin: host connection default is `V1`; v2 negotiates v2 and rejects removed client methods before sending; v1 still advertises `fs.readTextFile`/`writeTextFile`/terminal; `ee-agent-protocol` accepts v1+v2 and rejects unknown versions.
- [x] **Acceptance:** the baseline tests pass unchanged before and after the migration phases; no production caller selects v2 until Phase V3.
- [x] **File-size guard:** if a touched file is at or near 1,000 LOC (for example `ee-agent-host/src/connection/driver.rs`), extract the new logic into a child module instead of growing it.

### Phase V1 — ACP-native MCP server survives v2 conversion

- [x] **Problem:** `mcp_servers_to_v2` drops `McpServer::Acp` with a warning (`ee-agent-host/src/connection/driver.rs`). The SDK supports it behind `unstable_mcp_over_acp`, which the workspace already enables, so an armed ACP-native `ee` server silently disappears on v2.
- [x] **Fix:** move the v1→v2 MCP server conversion into a dedicated `ee-agent-host/src/connection/session_servers.rs` helper; map `Stdio`, `Http`, and `Acp` (preserving `name`, `serverId`, and `_meta`); keep unknown transports dropped with a warning; have `driver.rs` delegate.
- [x] **Fix:** unit-test `Acp` conversion field-for-field, plus stdio/http conversion and unknown-transport dropping. Assert the ACP entry is not duplicated and keeps the `ee` server identity.
- [x] **Acceptance:** a v2 `session/new` request built from an armed ACP-native `ee` entry carries `McpServer::Acp`; v1 behavior is unchanged; targeted host tests pass.

### Phase V2 — v2 session continuity and fallback guarantee

- [x] **Problem:** `new_session` can return `EeProxyMode::Disabled` on v2 when the agent lacks `session.mcp.acp` and no stdio fallback exists, silently leaving the agent without client tools. `resume_session` never appends the `ee` proxy entry and always reports `Disabled`.
- [x] **Fix:** share one session-setup helper for `new_session` and `resume_session`: append the ACP-native `ee` entry when the agent advertises ACP MCP, else the stdio fallback when provided, else `Disabled`; return the resolved mode on both paths.
- [x] **Fix:** when the connection is v2 and the ee proxy is armed but no `ee` entry could be advertised, fail closed with a typed `invalid_params` error naming `[mcp.proxy] enabled = true` or `session.mcp.acp` support. Do not fail when the user explicitly disabled the proxy.
- [x] **Fix:** thread `ee_proxy_stdio_fallback` through `AgentManager::resume_session`, the CLI host command, and the reconnect call site; keep v1 resume behavior otherwise unchanged.
- [x] **Fix:** tests for: v2 + armed + ACP support → `Acp`; v2 + armed + no ACP + fallback → `Stdio`; v2 + armed + neither → typed error; v2 + disarmed → no entry and no error; resume advertises the same entry and reports its mode; v1 new/resume unchanged.
- [x] **Acceptance:** a v2 session can never be created or resumed with an armed proxy and no MCP route for client tools; error text is actionable; targeted host and CLI reconnect tests pass.

### Phase V3 — Per-agent v2 opt-in

- [x] **Problem:** the host always connects with `ProtocolVersion::V1`; there is no supported way to test v2 per agent, and flipping the global default is premature while v2 is Preview.
- [x] **Fix:** add `protocol = "v1" | "v2"` to `[agents.servers.<id>]` with default `v1`; parse, validate, merge, serialize, and document it; reject unknown values with a config error that names the server id.
- [x] **Fix:** carry the resolved per-agent version in `AgentManagerConfig` and apply it in `AgentConnection` options; unknown/missing entries keep `V1`; the global default stays `V1`.
- [x] **Fix:** tests: default is v1; `protocol = "v2"` makes that agent's `initialize` use `protocolVersion: 2`; a second agent without the field stays v1; invalid values fail config validation; schema/docs updated.
- [x] **Acceptance:** v2 is opt-in per agent only; no production default changes; v1 agents are unaffected; config round-trips.

### Phase V4 — v2 command permissions reuse workspace safe-read

- [x] **Problem:** v2 `session/request_permission` command subjects are flattened into a title plus `rawInput` and always prompt; they never consult the workspace-trust safe-read allowlist, so trusted `mk lf`-style commands still ask on v2 while v1 auto-allows them.
- [x] **Fix:** carry a typed `PermissionCommand { command, cwd }` alongside the v1-shaped tool call in `PermissionRequestInfo` (host); v1 requests set `None`, v2 command subjects set `Some`, and tool-call/unknown subjects stay `None`.
- [x] **Fix:** add a conservative simple-command parser (ASCII spaces only; reject quotes, escapes, expansions, redirections, pipes, separators, globs, control characters, empty tokens) and evaluate the parsed executable+argv through the existing `safe_read` profile with the canonical primary-workspace cwd.
- [x] **Fix:** in the editor permission path, auto-answer `AllowOnce` only for a trusted workspace with a `safe_read` match; answer a reject option for a safeguard or persistent/session deny; prompt for everything else (unknown parse, untrusted workspace, non-safe-read profiles, missing allow/reject options, pre-thread arrival).
- [x] **Fix:** never auto-select `AllowAlways` or mint a rule/use for a v2 agent-owned command; record a system notice naming the built-in safe-read decision.
- [x] **Tests:** trusted workspace `mk lf` auto-allows; untrusted workspace prompts; `mk run`, forwarded args, quotes/pipes/`$()`/redirections prompt; `/dev/null` safeguard denies when a reject option exists; cwd escape prompts; persistent deny denies; missing AllowOnce prompts; v1 permission behavior unchanged.
- [x] **Acceptance:** v2 safe-read commands match v1 auto-allow on trusted workspaces without weakening deny/safeguard/mandatory-confirm precedence; advisory nature is documented.

### Phase V5 — Documentation, gates, and default policy

- [x] **Fix:** update `README.md` and `wiki/config.md`: v1 default, per-agent v2 opt-in, the v1→MCP tool mapping table, the armed-proxy fallback requirement, and the advisory nature of v2 agent-owned command approvals.
- [x] **Fix:** correct the stale ACP batch claim in Part 10 (`ACP v2 stdio allows them` → v2 forbids batches) and record this migration.
- [x] **Validation:** per phase run `cargo fmt --all --check`, `cargo clippy -q -p <crate> --all-targets -- -D warnings`, and the narrowest targeted `cargo test --quiet` filters; final manual gate is the touched host/CLI suites, not an unprompted full workspace run.
- [x] **Acceptance:** every phase's tests and docs are in place; `AgentConnectionOptions::default()` is still `V1`; no removed v1 method is served on v2; every touched code file is under 1,000 LOC.

### v2 default-flip gate (not part of this work)

Do not flip the global host default to v2 in these phases. Reopen only when ACP v2 leaves Preview, every shipped agent passes v2 with either `session.mcp.acp` or the stdio fallback, and V0–V5 acceptance holds.

## Standard MCP JSON Configuration (2026-11-26)

Goal: load the ecosystem-standard `mcpServers` JSON shape from `.agents/mcp.json` and `$XDG_CONFIG_HOME/ee/mcp.json` alongside explicit `[mcp.servers]` TOML, without weakening trust or merge rules.

Rules:

- Standard JSON is an import layer. ee never rewrites it; `ee do config setup mcp` continues to manage TOML only.
- The user standard file is `$XDG_CONFIG_HOME/ee/mcp.json`; the workspace standard file is `<dir>/.agents/mcp.json` for every directory in `.ee.toml` discovery, and `root = true` stops it.
- Within one directory, standard JSON merges before `.ee.toml`, so an explicit `[mcp.servers.<id>]` entry wins on a duplicate id.
- Workspace JSON env/header values keep `Ancestor` provenance, so `secret://` references still require workspace trust.
- HTTP+SSE, ambiguous `command` + `url` entries, invalid URLs, oversized files, and malformed entries are reported and skipped per server; one bad entry cannot disable the rest.

Work items:

- [x] Add `config/mcp_json.rs`: `command`/`args`/`env`/`cwd` and `url`/`headers`/`timeout_ms`, `transport`/`type` inference, `disabled`/`enabled`, relative `cwd` resolution, a 1 MiB size cap, and per-entry failure isolation.
- [x] Add ordered `ConfigSource` discovery so user and ancestor standard files interleave with TOML layers and honor `root = true`.
- [x] Surface imported files in `ee do config doctor` and cover parser, precedence, provenance, root-boundary, disabled, malformed, relative-cwd, and report cases.
- [x] Update README, wiki config reference, and CHANGELOG; regenerate the checked-in schema for the already-landed per-agent `protocol` field so the config schema drift test passes.

Acceptance:

- [x] `cargo test --quiet -p ee-cli --features agents --lib mcp_json` and `config::` pass.
- [x] `cargo clippy --quiet -p ee-cli --features agents --lib --tests -- -D warnings` passes.
- [x] `cargo fmt --all --check` passes.

## Workspace Terminal Execution Trust (2026-11-26)

Goal: let a user explicitly authorize prompt-free native ACP `terminal/create` commands for one trusted workspace, mirroring the MCP `--execute` opt-in without widening plain workspace trust into implicit arbitrary code execution.

Rules:

- Workspace trust alone is not enough. The grant is a separate explicit opt-in on the same owner-only, host-local workspace trust document; repository configuration cannot create it.
- Revoking workspace trust clears and inerts the terminal grant. `ee do agent trust workspace revoke --execute` removes only the terminal grant and keeps the workspace trust decision.
- Applies only to native ACP v1 `terminal/create`. MCP `ee_terminal_create` keeps its separate `ee do agent trust mcp grant --execute` opt-in; neither grant crosses into the other route.
- Auto-allow requires a validated simple command: shell syntax and shell wrappers are rejected, the command text/args normalize to structured executable + argv, the executable must be a bare `PATH` name, every path-like argv operand must resolve inside the primary workspace root, and the canonical cwd must stay inside that root. Explicit-path executables, external/traversal operands, ambiguous option-embedded paths, extra workspace roots, and external directories still prompt.
- A leading environment assignment, loader/path environment overrides, an invalid request, a read-only agent mode (`agents.read_only_modes`, default `ask`/`plan`), a safeguard, a persistent deny, or a mandatory-confirm rule still wins; the grant is implicit consent for routine commands, not for unbounded ones.
- ACP v2 agent-owned command permissions stay on the existing curated `safe_read` reply path: the agent runs those commands itself, so the host cannot enforce workspace containment. MCP terminal tools remain covered by the MCP execute grant.

Work items:

### Phase T1 — Host-local terminal grant and CLI

- [x] Extend the workspace trust document with `terminal_execution` in schema v2, accepting v1 documents as "not granted", preserving owner-only/atomic/identity invariants, and writing v2 only.
- [x] Add `WorkspaceTrustStore::terminal_execution` and `set_terminal_execution`; `set_decision(Untrusted)` clears the grant, and granting it requires a trusted workspace.
- [x] Add `ee do agent trust workspace grant --execute`, `ee do agent trust workspace revoke --execute`, and a status line that distinguishes granted, not granted, and inert (workspace not trusted).
- [x] Tests: v2 round trip, v1 compatibility, untrusted clearing, grant-requires-trust, malformed/identity behavior unchanged, and CLI flag parsing.

### Phase T2 — Native terminal evaluation

- [x] Add `parse_terminal_command` so simple ACP v1 command text (`"mk lf"`) and structured `command` + `args` normalize identically; reject shell syntax, shell wrappers, and leading environment assignments.
- [x] Use the shared parser for command policy identity, persistent command trust, and the new terminal execution gate; keep shell wrappers and invalid cwd ineligible.
- [x] Add `workspace_execute_enabled` to the policy evaluator as a built-in allow after safeguards, persistent/session denies, and mandatory confirms; only `Execute` + validated `Command` identities match.
- [x] Add `app_terminal_trust.rs`: require workspace trust, the host-local terminal grant, a non-read-only agent mode, a canonical primary-workspace cwd, a bare PATH executable, and no execution-affecting environment overrides; reject MCP-originated prompts so the routes stay separate.
- [x] Confine path-like argv operands to the primary workspace root for auto-allow (canonical/lexical resolution, `..` traversal, absolute paths, flag-embedded paths, and shell-wrapper operands); ambiguous cases fall back to approval.
- [x] Wire `request_bridge_approval` and summarize the automatic decision as `trusted workspace terminal`.
- [x] Tests: native `touch` auto-spawns in the workspace root and subdirectory; untrusted, no-grant, shell-wrapper, external-cwd, env-override, read-only-mode, persistent-deny, external/traversal operand, explicit-path executable, option-embedded path, wrapper-operand, and MCP-route requests still prompt or deny; simple shell-text safe-read still auto-allows.

### Phase T3 — Boundaries and documentation

- [x] Document the grant in `README.md` and `CHANGELOG.md`, including the warning that it authorizes arbitrary user-level commands inside the workspace, is not a sandbox, and does not cover MCP or ACP v2.
- [x] Record the separate-route boundary and the deferred ACP v2 broad-execution decision in this section.
- [x] Run `cargo fmt --all --check`, targeted `cargo clippy`, and the terminal/trust/policy test filters; verify every touched code file stays under 1,000 LOC.

Acceptance:

- [x] `cargo test --quiet -p ee-cli --features agents --lib workspace_trust` passes.
- [x] `cargo test --quiet -p ee-cli --features agents --lib terminal_exec_trust` passes.
- [x] `cargo test --quiet -p ee-cli --features agents --lib command_trust` and `safe_read_profile` pass unchanged except for intentional parser coverage.
- [x] `cargo clippy --quiet -p ee-cli --features agents --lib --tests -- -D warnings` passes.
- [x] Plain workspace trust remains inert for terminal execution; the explicit grant and its revoke are observable through `ee do agent trust workspace status` and tests.

Deferred boundaries:

- MCP `ee_terminal_create` keeps its own pinned `ee do agent trust mcp grant --execute` grant; the native workspace grant never auto-allows proxy-originated terminal calls, and the MCP grant never auto-allows ACP v1 `terminal/create`.
- ACP v2 agent-owned command permissions keep the curated `safe_read` reply path. Widening them would be advisory only because the agent, not ee, runs the command; revisit only with an explicit v2 execution contract.

### Phase T4 — Sandboxed trusted auto-run and classifier hand-off (2026-11-26)

Context: real `ee agent shell` sessions showed approval fatigue because common `cd <dir> && <command>` lines fell back to the prompt, and the opt-in approval classifier never reached the host. Native bridge tools did not carry the classifier marker, and `AutoApprove` only skipped a telemetry event.

- [x] Accept `cd <absolute workspace directory> && <simple command>` for terminal trust eligibility by normalizing only the prefix; the remainder must still pass the strict simple-command parser, the `cd` target must canonicalize inside the primary workspace, and any other shape stays on the approval path.
- [x] Set `auto_sandboxed` on every auto-dispatch path (workspace grant, built-in allowlist, approval mode, and classifier) so auto-run terminal commands go through the OS sandbox; an unavailable sandbox backend keeps the prompt.
- [x] Add the shared `_meta` classifier marker in `ee-agent-protocol`; mark native write/terminal bridge tools classifier-eligible without changing host-approval policy semantics, and forward the marker on ACP write and terminal requests.
- [x] Honor classifier auto-approval only for built-in orchestrator binaries (`ee-agent-opencode`, `ee-agent-openrouter`) on a trusted workspace in a non-read-only mode; mandatory confirms, safeguards, persistent denies, and other agents still prompt.
- [x] Preserve host `CARGO_HOME`/`RUSTUP_HOME` as read-only toolchain homes for sandboxed spawns, because the sandbox replaces `HOME` with a fresh temp dir and would otherwise make rustup reinstall (network-denied) on every command.
- [x] Tests: `cd`-prefix acceptance and out-of-workspace rejection, classifier agent gating and metadata forwarding, native gated-tool eligibility, toolchain-home passthrough, plus unchanged terminal trust, mandatory-confirm, and always-allow suites.
- [x] `cargo fmt --all -- --check`, targeted `clippy -D warnings`, and terminal/policy/orchestrator tool filters pass.

### Phase T5 — Unrestricted terminal execution grant (`--yolo`, 2026-11-26)

Context: real agent turns routinely emit pipes, redirects, globs, and shell wrappers. The simple-command grant rejects all of them, so users still approve most commands. `--yolo` adds an explicit, host-local stronger grant without weakening the default layers.

- [x] Bump the workspace trust document to schema v3 with `terminal_execution = "none" | "simple" | "unrestricted"`; v1 and v2 documents stay readable, with a v2 boolean `true` mapping to `simple`.
- [x] Add `ee do agent trust workspace grant --yolo` (implies terminal execution), preserve an existing unrestricted grant across plain `--execute` and trust re-grants, and clear either grant with `ee do agent trust workspace revoke --execute`; status prints simple/unrestricted.
- [x] Add `workspace_execute_unrestricted` to the policy evaluator and honor it for `Execute` operations even when the shell text has no normalizable command identity; explicit/session denies, mandatory confirms, and safeguards still run before it.
- [x] Keep the yolo path limited to native ACP v1 terminal, a workspace-contained cwd, a trusted workspace, and non-read-only agent modes; MCP terminal tools and ACP v2 command permissions stay on their own routes.
- [x] Force yolo auto-runs through the terminal sandbox when `mode != off`; `mode = off` is called out as an explicit unsandboxed combination in the grant output and docs.
- [x] Tests: unrestricted storage round trip and v2 compatibility, CLI `--yolo` parsing, shell-syntax auto-run, read-only prompt fallback, and no MCP route crossing.

## Durable Agent Session Transcripts and Archive (2026-11-26)

Goal: keep the full local transcript for every agent session — live and archived — in the user data directory, rotate large transcript journals like logrotate, make the provider durable session log on by default with an explicit opt-out, and add a safe prune command. Large tool results stay inline-bounded (2,000 lines / 50 KiB) with the overflow retained by the session log, matching the upstream OpenCode `ToolOutput` behavior.

Rules:

- Transcript storage is owner-only (`0700` dirs, `0600` files), workspace-scoped by the existing `WorkspaceIdentity` hex digest, and lives under `$XDG_DATA_HOME/ee/agent-sessions/transcripts/<workspace-hex>/`.
- The live transcript is never trimmed or capped in memory or on disk. The journal rotates at a byte threshold; old segments stay until an explicit prune.
- `agent-sessions.json` keeps the live/archived session registry only. It may keep a full transcript only as a write-failure fallback; the durable journal is canonical.
- Archived sessions survive restart and are listed by `/archive list`; restoring one reconnects it through `session/load` while the archived transcript stays authoritative for the restore window (the bounded replay applies only after the user starts a new turn).
- The provider durable session log defaults to the same user data root. `EE_SESSION_LOG_DISABLE=1` (and `EE_SESSION_LOG_DURABLE=false`) opt out. Full tool output overflow is retained there; only the inline tool result is bounded.
- `ee do agent sessions prune` never touches a session listed as live in the client registry, never follows symlinks, and defaults to a 30-day cutoff with `--dry-run`.

Work items:

### Phase A1 — Rotating transcript journal store

- [x] Add `crates/ee-cli/src/agent_session_store.rs` with owner-only workspace/session paths, `meta.json`, an append-only JSONL journal (`Snapshot`/`Set`/`Append`/`Truncate` records), and size-based segment rotation that never deletes transcript content.
- [x] Replay every rotated segment plus the active journal on load; tolerate a torn final line, repair it before the next append, and reject malformed mid-stream records.
- [x] Persist archived state and timestamps in metadata, and delete one session directory only on explicit request.
- [x] Tests: append/update/truncate round trip, rotation across segments with >1,500 items restored exactly, archived metadata, owner-only modes, torn tail, malformed mid-stream rejection, and session-id path hardening.

### Phase A2 — Live and archived transcript wiring

- [x] Remove the 1,500-item transcript trim and the 200-line stderr trim; the pane keeps the full transcript in memory.
- [x] Persist every live thread's full transcript to the journal and store only registry metadata plus a write-failure fallback in `agent-sessions.json`; bump the persisted workspace schema and keep v2 documents readable.
- [x] Restore transcripts from the journal on reconnect, migrate legacy JSON transcript fields into the journal, and prefer the journal over the fallback.
- [x] Persist archived threads in the workspace registry with metadata-only records, keep them across restart, list them in `/archive list`, and restore them by reconnecting with the archived transcript; the bounded load replay is ignored until the next user turn.
- [x] Delete a session's journal on explicit `/delete`; a failed archived restore returns the record to the archived list.
- [x] Integration tests: restart restores a live transcript from the journal; archive → restart → `/archive list` → restore works; delete removes the journal; legacy JSON transcript migrates; archived restore failure keeps the archive record.

### Phase A3 — Default durable session log and tool-output bounds

- [x] Default the OpenCode and OpenRouter provider session-log root to `$XDG_DATA_HOME/ee/agent-sessions` and add `EE_SESSION_LOG_DISABLE=1` plus `EE_SESSION_LOG_DURABLE=false` opt-outs.
- [x] Keep the existing 2,000-line / 50 KiB inline tool-result bound; confirm the retained overflow path is the default durable session log and that tool-output metadata still reports truncation and the saved path.
- [x] Tests: default root is present, both opt-outs disable it, explicit `EE_SESSION_LOG_DIR` wins, and a large tool result is bounded inline while the full body is retained.

### Phase A4 — `ee do agent sessions prune`

- [x] Add `ee do agent sessions prune [--older-than-days N] [--all] [--dry-run]` under the existing `do agent` grammar.
- [x] Prune stale transcript archive session directories by metadata age and stale provider session-log directories by modification age; protect live session ids from the client registry and skip symlinks.
- [x] Print a deterministic summary (transcripts, session logs, bytes; `would prune` for dry run) and return nonzero on unsafe path or I/O failure.
- [x] Tests: CLI parsing, old vs young entries, live-session protection, `--all`, dry run writes nothing, symlink refusal, and idempotent second run.

### Phase A5 — Docs, changelog, and validation

- [x] Document transcript rotation, archive restore, opt-out, and prune in `README.md`, `CHANGELOG.md`, and the relevant config docs.
- [x] Run `cargo fmt --all -- --check`, targeted clippy, store/prune/agent-pane/reconnect tests, and the provider config tests; verify every touched code file stays below 1,000 LOC.

Acceptance:

- A >1,500-item live transcript restores byte-for-byte after restart; old journal segments remain until prune.
- Archived transcripts survive restart and can be restored through `/archive list` and `/archive restore`.
- The durable provider session log is on by default under the user data directory and can be disabled by either opt-out.
- A 100 KiB tool result keeps only the bounded inline preview plus the retained overflow path.
- `ee do agent sessions prune --dry-run` lists the same entries as a real run and deletes nothing; live sessions are never removed.
- `cargo fmt --all -- --check`, targeted `cargo clippy`, and the phase test filters pass.

## Workspace Terminal Sandbox (2026-11-26)

Goal: run prompt-free native ACP v1 `terminal/create` commands inside an OS sandbox on Linux and macOS, so the workspace terminal execution grant is no longer only an argv inspection layer. This narrows the existing caveat in [Workspace Terminal Execution Trust](#workspace-terminal-execution-trust-2026-11-26): operands are validated, but accepted commands can still reach anything the user account can reach.

Inputs: the Zed sandboxing documentation (`https://zed.dev/docs/ai/sandboxing`, Linux and macOS sections), the current terminal spawn path (`app/agent_bridge/terminal.rs`, `persistent_shell.rs`), and the existing `ee-plugin-runtime` seccomp/resource code (not reusable as-is; its filter denies `fork`).

Rules:

- Applies only to native ACP v1 `terminal/create`. Default: sandbox commands auto-allowed by the workspace terminal execution grant. User-approved commands keep their explicit consent path; a later `all` mode may sandbox those too. MCP `ee_terminal_create` and ACP v2 command permissions stay on their existing routes.
- Linux uses a runnable **non-setuid** `bwrap` only. Probe with `bwrap --ro-bind / / -- echo working`; reject any setuid-root binary because sandbox setup would run root-privileged with model-influenced arguments. If `bwrap` is missing, blocked by user namespaces, or fails the probe, fall back to an approval prompt; never silently run unsandboxed unless an explicit config opt-in says so.
- Ubuntu 23.10+ blocks unprivileged user namespaces through AppArmor. Document the `bwrap-userns-restrict` profile install (Ubuntu 25.04+ ships it) and surface the probe result in `ee do doctor`.
- macOS uses Apple Seatbelt through `sandbox-exec`. Probe at startup; treat the deprecated backend as unavailable if it disappears. No clean supported alternative exists for wrapping arbitrary host toolchains.
- Default access model matches the Zed baseline: broad filesystem reads; writes only inside the primary workspace plus a fresh temporary directory exposed through `TMPDIR`/`TMP`/`TEMP`; `.git` metadata readable but never writable; outbound network denied; Unix-domain IPC sockets and runtime dirs unavailable by default.
- `sandboxed` auto-runs use the per-command process path (`sh -c`), not the reusable persistent shell. bwrap mounts and `.git` protection are fixed at sandbox creation, and `/tmp` must be fresh per command. The persistent shell remains for the unsandboxed approved path; sandboxing a persistent shell would freeze mounts and leak temp state across commands.
- Resolve all bind paths to canonical, absolute paths first; prefer fd-pinned binds (`--bind-fd`/`--ro-bind-fd`) where supported to close rename/symlink TOCTOU. bwrap policy is creation-time: a `.git` directory created after sandbox start cannot be retroactively protected.
- `.git` protection covers the workspace Git directory, the linked-worktree gitdir pointer file, and common worktree metadata. Blocking `.git` must survive broader write approvals.
- Network starts at deny (`--unshare-net`). All-or-nothing network can be granted later; a host-specific allowlist requires an HTTP/HTTPS proxy plus accepting that SSH/raw socket clients may not work. Never present proxy env vars as enforcement when direct sockets are still reachable.
- Sandboxing is terminal-tool scoped, not an account boundary. It does not cover language servers, proc macros, tasks, plugins, the regular terminal, or commands the user runs later. Document the side-channel examples from the Zed doc.
- Do not claim sandboxing is unbreakable. Kernel/OS bugs, bwrap/Seatbelt bugs, and over-broad write/network grants remain.

Proposed config:

```toml
[agents.terminal_sandbox]
mode = "granted"   # off | granted | all
read = "broad"     # broad | workspace
network = "deny"   # deny | allow (S3)
on_unavailable = "prompt" # prompt | deny
write_paths = []   # additional host write paths, canonicalized (S3)
```

Work items:

### Phase S1 — Linux bubblewrap backend

- [x] Add shared `crates/ee-sandbox` (files kept under 1,000 LOC): `SandboxSpec`, backend probe, and deterministic Linux bwrap argv builder. `ee-cli` terminal spawn and `ee-agent-host` agent launch consume the same crate; no sandbox logic in either caller.
- [x] Probe `bwrap`, reject setuid-root binaries, run the `--ro-bind / /` smoke test, and cache capability plus probe error for doctor/status output.
- [x] bwrap profile: `--ro-bind / /`, rw bind the canonical primary workspace, ro-bind the workspace Git directory/pointer after the rw bind, private `--tmpfs /tmp` with `TMPDIR` set, `--proc /proc`, `--dev /dev`, `--unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup --unshare-net`, `--die-with-parent --new-session`, no `/run` or `$XDG_RUNTIME_DIR`, sanitized env, and the validated command as the final argv.
- [x] Add `env -u`/`--clearenv` style env scrubbing with the existing secret-name filter; set `PATH`, `HOME`, and temp vars explicitly; keep request env only when the terminal trust gate already allows it.
- [x] Route auto-granted terminal spawns through the sandboxed per-command path; keep the persistent shell for user-approved unsandboxed commands. Extend `AgentTerminals::spawn`/`PersistentShellPool` plumbing with an optional sandbox spec instead of reading process globals.
- [x] Fail closed to the approval path when the probe, spawn, or bwrap setup fails; expose the reason in the status line, and allow `on_unavailable = "deny"`.
- [x] `ee do doctor` reports bwrap path, setuid state, userns probe, and Ubuntu AppArmor guidance.
- [x] Tests: argv/profile snapshots; setuid rejection; Linux integration tests gated on a working non-setuid bwrap (write inside succeeds; outside write denied; `.git` write denied; symlink/rename escape denied; temporary dir fresh per command; network denied; Unix socket connect denied; env secret absent; sandbox-unavailable falls back to prompt).

### Phase S2 — macOS Seatbelt backend

- [x] Add the `sandbox-exec` probe and generated Seatbelt profile backend behind the same `SandboxSpec` interface.
- [x] Profile: `(deny default)`, allow the baseline process and sysctl operations, broad `file-read*`, writes only under the workspace and the per-command temp directory, explicit `.git` write denial, network denied, and a minimal `mach-lookup` allowlist for developer tooling. Exclude launchd/LaunchServices, pasteboard, and audio services.
- [x] Set `TMPDIR`/`TMP`/`TEMP` to the per-command temp path; sanitize env like S1.
- [x] Fail closed to approval when `sandbox-exec` is missing or the profile fails to apply.
- [x] Tests: profile generation snapshots; probe gating; macOS integration tests gated on `sandbox-exec` (workspace write succeeds; outside write denied; `.git` write denied; network denied; Mach service outside the allowlist denied; unavailable backend prompts).

### Phase S3 — Sandbox grants and status

- [x] Add `network = "deny" | "allow"` enforcement. Start with all-or-nothing; defer host allowlists to a proxy phase and document SSH/raw socket limitations.
- [x] Add canonical `write_paths` grants with fd-pinned validation where supported; reject non-canonical, symlinked, or escaping paths.
- [x] Show sandbox mode, backend, effective network, writable paths, and probe failures in `ee do agent trust workspace status` and `ee do doctor`.
- [x] Decide whether AI-triggered sandbox escalation is representable. ACP v1 has no sandbox request; start with static grants/config and approved-command fallback, not fabricated host/path requests.
- [x] Tests: network deny vs allow, write-path grant accepted/escape rejected, status output, config parsing and defaults.

### Phase S4 — ACP v2 agent-process containment

- [x] Add an opt-in agent-process sandbox profile to `ee-agent-host` (`AgentProcessConfig`/`process.rs`): same backend as S1/S2, broad reads, rw workspace plus explicit agent state/cache dirs and a private temp dir, `.git` read-only, and network allowed for model API calls.
- [x] Keep ACP v2 `session/request_permission` safe-read policy unchanged. The sandbox enforces filesystem containment for commands the agent executes itself; do not auto-allow broader v2 commands because a sandbox exists.
- [x] Pass the resolved sandbox spec through the agent launch config and expose `[agents.sandbox] mode = "off" | "workspace-write"` (opt-in initially) plus a status/doctor line.
- [x] Avoid nested sandboxes: client `terminal/create` spawns its own sandbox sibling, and MCP terminal tools stay on the MCP grant route.
- [x] Document that the agent process keeps network access for model calls, so ACP v2 command containment is filesystem-only until a proxy/network phase; no host-specific enforcement is claimed.
- [x] Tests: agent launch argv/profile snapshots; gated integration spawning a shell under the agent profile (outside write denied, workspace write allowed, `.git` write denied, network behavior documented); unavailable backend leaves v2 commands on safe-read/ask without claiming containment.

### Phase S5 — Shared resource limits

- [x] Extend shared `ee-sandbox` with `ProcessLimits` and a generic `ProcessTreeMonitor` extracted from `ee-plugin-runtime`; the plugin runtime keeps its `Limits` adapter and termination reasons compatible.
- [x] Add `ProcessLimits` for sandboxed terminal commands: wall time, aggregate RSS, CPU seconds, open files, file size, and process count; zero/absent means unlimited. Apply kernel rlimits before exec (bwrap `--rlimit-*` where supported, Unix `setrlimit` in `pre_exec` otherwise) and kill the whole process group on wall-time or aggregate breach.
- [x] Sample the entire process tree, not just the shell pid: Linux `/proc` descendants and macOS child enumeration plus process accounting. Surface the breach reason in terminal output/status; never silently truncate or kill without a reason.
- [ ] Optional Linux cgroup v2 backend (`systemd --user` scope or a delegated cgroup) for aggregate memory/CPU/PID limits where available; fall back to rlimits plus sampling. macOS uses rlimits plus sampling. _(Deferred: rlimits plus process-tree sampling is the current backend; tracked in Deferred boundaries.)_
- [x] Add `[agents.terminal_sandbox.limits]` config with unlimited defaults and a `strict` preset; document that builds may need raised limits. Keep the existing output-byte cap independent.
- [x] Tests: process-tree sampling, wall-time kill, rlimit application, plugin monitor compatibility, sandboxed command breaches, and no regression with limits disabled.

### Phase S6 — Docs, boundaries, and final validation

- [x] Document modes, defaults, failure behavior, Ubuntu setup, macOS deprecation, `.git` protection, temp behavior, ACP v2 agent-process containment, resource-limit presets, and the exact scope boundary in `README.md`, `CHANGELOG.md`, and doctor text.
- [x] Document that sandboxing is not applied to language servers, proc macros, tasks, plugins, the regular terminal, later user-run builds, or non-terminal agent routes; include the Zed side-channel examples.
- [x] Record the persistent-shell vs per-command tradeoff, bwrap creation-time `.git` caveat, ACP v2 filesystem-only guarantee, and network/proxy limitations in the docs and config reference.
- [x] Run `cargo fmt --all -- --check`, targeted `cargo clippy`, sandbox unit tests, Linux/macOS gated integration tests, terminal/trust suites, plugin-runtime tests, and verify every touched file stays below 1,000 LOC.

Acceptance:

- A prompt-free granted command can write only inside the workspace and its per-command temp directory; outside writes, `.git` writes, network, and Unix-socket IPC are blocked.
- With the ACP v2 agent-process sandbox enabled, commands the agent runs itself cannot write outside the workspace or `.git`; network stays available for model calls and this limitation is documented.
- Missing, setuid, or unusable bwrap and missing `sandbox-exec` produce an approval prompt (or denial under `on_unavailable = "deny"`), never a silent unsandboxed run.
- Workspace, Git metadata, and agent state/cache paths are canonicalized before sandbox creation; the creation-time caveat is documented.
- Sandboxed commands do not reuse the persistent shell; each command gets fresh temp state and a fresh mount policy.
- Resource limits default to unlimited, and a configured wall-time or aggregate breach terminates the whole process group with a visible reason.
- `ee do doctor` and `ee do agent trust workspace status` report backend capability and effective policy.
- Full targeted test gates and file-size checks pass.

Deferred boundaries:

- Host-specific network allowlists and HTTP/HTTPS proxy enforcement (include raw-socket/SSH caveats when added); ACP v2 agent network stays all-or-nothing.
- Windows/WSL sandboxing; native Windows out of scope.
- ACP v2 per-command interception without an agent-process sandbox: ACP v2 has no client terminal methods, so the host cannot wrap an agent-owned command directly.
- MCP terminal tools keep their own `ee do agent trust mcp grant --execute` route; no cross-route nesting.
- Optional Linux cgroup v2 aggregate limits remain future work; current limits use inherited rlimits plus process-tree sampling. The plugin runtime now delegates breach policy and process-tree sampling to the shared `ee-sandbox` monitor while retaining its platform-specific raw samplers and Windows job objects.

## Unified Agent Trust CLI (2026-11-26)

Goal: put every agent-related trust decision under one command root, and stop MCP trust commands from inferring a server id.

Rules:

- Canonical commands are `ee do agent trust workspace grant|revoke|status` and `ee do agent trust mcp grant|revoke|status`.
- Trust commands live only under `ee do agent trust ...`; there are no top-level `do trust` or `do mcp` aliases.
- `ee do agent trust mcp grant` and `revoke` require an explicit `--server <id>`. No default server is applied, so a command can never silently trust the built-in server or every configured server.
- `--transport all` means both stdio and ACP transports for the selected server, never all servers.
- Only the built-in `ee` server has an application-owned pinned tool manifest; other server ids stay rejected until they gain one.
- Existing `ee do agent trust grant|revoke --profile` profile grants stay in the same trust namespace.

Work items:

- [x] Add `Workspace` and `Mcp` branches under `AgentTrustCommands` and route only the canonical `ee do agent trust ...` commands through the same dispatch helpers.
- [x] Remove the temporary top-level `ee do trust` and `ee do mcp` aliases; only the canonical `ee do agent trust workspace|mcp` commands remain.
- [x] Rename the agent-driver MCP server command from `ee do agent mcp` to `ee do agent serve-mcp`, freeing `ee do agent mcp` for server management.
- [x] Add non-interactive `ee do agent mcp add|list|rm` management over the existing scoped `[mcp.servers]` config helpers; `add` requires `--command` or `--url`, secret-like env/header values require `secret://`, and `list` never prints env/header values or URL queries.
- [x] Remove the implicit `--server ee` default from MCP grant and revoke; require the id explicitly and update help text.
- [x] Keep `--transport` scoped to the selected server and document that `all` is transport-wide, not server-wide.
- [x] Add CLI parse tests for both canonical paths, the missing-server rejection, and removed top-level aliases.
- [x] Add MCP terminal `--yolo` parity: `ee do agent trust mcp grant --server ee --yolo` stores an unrestricted execute pin, auto-runs arbitrary `ee_terminal_create` text only in a non-read-only mode with a workspace cwd and a ready sandbox, and preserves the pin across simple re-trust until `revoke --execute`.
- [x] Extract the shared unrestricted-terminal gates (read-only mode, sandbox readiness, workspace cwd) used by workspace yolo and MCP yolo, plus one shared sandbox-policy notice for both grant CLIs.
- [x] Run `cargo fmt --all -- --check`, targeted `cargo clippy`, and the CLI/trust/MCP test filters.
