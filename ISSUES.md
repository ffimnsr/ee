# ISSUES

## External MCP Server Trust (2026-11-26)

Goal: extend `ee do agent trust mcp grant|revoke|status` from the built-in `ee` manifest to configured external MCP servers over stdio and Streamable HTTP, with optional whole-server trust (`--yolo`) and explicit read, write/delete, and execute tiers, without weakening workspace trust, pinning, or deny precedence.

Scope and boundaries:

- Enforced for external stdio and Streamable HTTP servers that ee forwards to an ee-spawned agent (`ee-agent-orchestrator` provider path, `session/new` `mcpServers`).
- External ACP agents (opencode and similar) spawn their own MCP servers and own their tool calls; no ee grant can be enforced there. An ee MCP gateway is the only future route and is not part of this plan.
- Streamable HTTP is supported; deprecated HTTP+SSE stays unsupported, matching the `ee-mcp` transport policy. OAuth is out of scope; headers/secrets keep the existing resolver rules.
- `harness-example/opencode/packages/core/src/mcp/client.ts` supports local stdio and remote Streamable HTTP; ee matches those transports while keeping workspace-trust gating and pinning.
- MCP resources are not covered; only tools.
- Trust is a pinned authority grant, not a sandbox. The external server process/endpoint is outside ee's process boundary.

Rules:

- Workspace trust is the outer gate; an undecided or untrusted workspace makes every external grant inert.
- A grant is pinned to the canonical workspace identity, server id, canonical transport fingerprint, exact tool name, input-schema digest, and category. New tools, schema changes, or transport changes never inherit trust.
- Explicit tiers select exact tool names; `--yolo` is the only whole-server option and still pins the exact advertised list at grant time. No wildcard or glob expansion at match time. `tools/list` annotations are display-only and never authorize a category.
- Tiers: `read` is allowed whenever reads are allowed (Ask/Plan/Write) after scope checks; `write_create`, `write_modify`, and `delete` are allowed only when the session policy allows writes (Write mode); `execute` requires an explicit per-tool opt-in in tiered grants, or the whole-server `--yolo` opt-in, and is allowed only when the session policy allows execute (Write mode).
- Whole-server trust: `--yolo` pins every tool the server advertises at grant time as `any`; repeatable `--read` flags override those exact tools to `read`. `any` is allowed when the session policy allows writes (Write mode); Ask/Plan run only the explicit `read` pins. New tools advertised later never inherit trust. `--yolo` cannot be combined with `--create`/`--write`/`--delete`/`--execute-tool` because `any` already covers those tiers in Write mode.
- Permission rules, workspace scope, and mandatory safeguards still win. Trust only bypasses the class/subclass gate for the exact pinned tool.
- No `network` or `unknown` categories. There is no separate external unrestricted mode beyond `--yolo`, which still pins the current tool list and never expands to future tools.
- Store remains host-local and owner-only under `$XDG_STATE_HOME/ee/mcp-trust/`; repository configuration cannot create authority.

Proposed CLI:

```sh
# Discover exact names and schemas before granting (never required at match time)
ee do agent trust mcp tools --server atlas --json

# Whole-server trust: every tool advertised now, no per-tool prompts in Write mode
ee do agent trust mcp grant --server atlas --yolo

# Whole-server trust plus Ask/Plan reads for selected tools
ee do agent trust mcp grant --server atlas --yolo \
  --read query_graph --read search_content

# Explicit tiers when you do not want blanket trust
ee do agent trust mcp grant --server atlas \
  --read query_graph --read search_content --read read_file
ee do agent trust mcp grant --server atlas \
  --create save_note --write memory_store --delete forget_fact

# Remote Streamable HTTP server (headers resolve through the existing secret rules)
ee do agent trust mcp grant --server remote-atlas --yolo

# Execute tier: explicit per-tool opt-in when not using --yolo
ee do agent trust mcp grant --server atlas \
  --execute-tool run_command

# Existing status/revoke keep working; external transport is inferred from config
ee do agent trust mcp status --server atlas
ee do agent trust mcp revoke --server atlas --execute
```

- `--yolo` is the external whole-server grant. It pins the current tool list, not future tools, and applies only to external ids; the built-in `ee` path keeps its existing unrestricted-terminal meaning. `--read` may refine selected tools for Ask/Plan; `--yolo` alone runs only in Write mode. `--create`/`--write`/`--delete`/`--execute-tool` cannot be combined with `--yolo` because their behavior is already covered by `any` in Write mode.
- `--execute-tool` is the explicit external execute authorization when `--yolo` is not used. `--execute` stays the built-in `ee_terminal_create` flag and is rejected for external servers with a message pointing at `--execute-tool` or `--yolo`. `--yolo` keeps its existing unrestricted-terminal meaning for the built-in server.
- External transport is inferred from the resolved server config. `--transport` stays meaningful for the built-in server; an explicit mismatched transport, or `acp`, is rejected for external servers.
- New tier flags (`--read`, `--create`, `--write`, `--delete`, `--execute-tool`) are external-only; the built-in `ee` grant path is unchanged apart from `--yolo` now also selecting whole-server trust for external ids.
- Granting with no tool flags and no `--yolo` fails with a message pointing at `tools`, `--yolo`, and the available names. Unknown names fail with the closest bounded list.
- Re-granting replaces the read/write/delete/any pins but preserves existing execute pins, matching the built-in behavior; `revoke --execute` removes only execute pins.

Phases:

### Phase E0 — Contract, threat model, and file-size precondition

- [ ] **Problem:** `ee-agent-orchestrator/src/mcp/manager.rs` is already ~1.6k LOC, and the grant store/CLI files are close to the 1k threshold. Adding feature code directly would violate the file-size rule.
- [ ] **Fix:** split `mcp/manager.rs` into focused modules (tool resolution/classification, stdio/ACP transports, fake-server tests) until every file is under 1,000 LOC; keep the host-facing API unchanged.
- [ ] **Fix:** add the contract as doc comments/tests: external stdio + Streamable HTTP, exact pins, read/write/execute tiers, `--yolo` whole-server semantics, no network pins, no sandbox claim, external ACP agents out of scope.
- [ ] **Acceptance:** no touched code file exceeds 1,000 LOC; existing orchestrator MCP tests pass unchanged.

### Phase E1 — Shared fingerprint/digest helpers and schema v3

- [ ] **Fix:** add `crates/ee-mcp/src/trust.rs` with bounded, secret-free helpers shared by host and agent:
  - `stdio_transport_identity(command, args, resolved_cwd, sorted env names)` using SHA-256; form `external-stdio:sha256:<64hex>`; values never included.
  - `http_transport_identity(url, sorted header names)` hashing the full URL (query included, so tokens do not leak) plus header names; form `external-http:sha256:<64hex>`.
  - `tool_schema_digest(input_schema)` over canonical JSON (recursively sorted object keys, array order preserved, no whitespace), plus a bounded manifest digest over sorted `(name, digest, category)` entries.
- [ ] **Fix:** extend the store document to schema v3 with `origin = "builtin" | "external"`, optional `schema_digest`, and optional `manifest_digest`. Missing fields default to `builtin`/`None` so v1/v2 documents still load.
- [ ] **Fix:** add external-only `any` to the stored category set (`Read`, `WriteCreate`, `WriteModify`, `Delete`, `Execute`, `Any`). Relax `McpServerToolGrant::validate` only for external grants; keep the `ee_terminal_create`-only rule for builtin grants; reject `Network`/`Unknown` and `unrestricted` external pins.
- [ ] **Tests:** v2 document loads and saves as v3; external grant round-trips; stdio and HTTP fingerprints match canonical inputs and differ on command/args/cwd/env-name/url/header-name changes; builtin execute validation unchanged; duplicate ids/tools rejected; malformed/non-hex digests rejected; workspace identity mismatch rejected; bounded manifest caps (256 tools / 64 KiB) enforced.

### Phase E2 — Discovery and CLI

- [ ] **Fix:** extract the MCP env/header resolver currently private to `app/agents_mcp` into a shared helper (reuse-first, no behavior change), so the CLI can resolve one configured server with the same secret/workspace-trust rules.
- [ ] **Fix:** add `crates/ee-cli/src/mcp_trust/discovery.rs` (child module of `mcp_trust.rs`, keeping the parent file stable): resolve one server from the effective config, start it with `ee_mcp::McpClientManager` under a bounded current-thread Tokio runtime for both stdio and Streamable HTTP, call `list_tools`, print names/digests/annotations, and always `shutdown()` the manager.
- [ ] **Fix:** add `ee do agent trust mcp tools --server <id> [--json]`. Print `readOnlyHint`, `destructiveHint`, `idempotentHint`, and `openWorldHint` as informational fields only; never auto-select or auto-classify from them. Do not start a server when the id is the built-in `ee`; point at `ee tools manifest` instead.
- [ ] **Fix:** extend `McpTrustCommands::Grant` with `--yolo` (external whole-server trust) plus repeatable `--read`, `--create`, `--write`, `--delete`, `--execute-tool`; keep existing `--execute` semantics for the built-in server only and reject `--execute` for external servers; keep `--yolo` as the existing unrestricted-terminal grant for the built-in server; allow `--yolo` with `--read` only (reject `--yolo` with write/delete/execute tier flags as redundant); infer external stdio vs Streamable HTTP from the resolved config; reject explicit `acp`; require the workspace to be trusted before discovery/spawn.
- [ ] **Fix:** build the grant from the discovered tool list: `--yolo` assigns `any` to every tool, then `--read` flags override those exact tools to `read`; tier-only grants assign exact categories; exact names only; digest from `tool_schema_digest`; transport identity from the shared fingerprint helper; `origin = "external"`; and a `manifest_digest`; preview the pinned tools before writing; print an explicit whole-server trust warning for external `--yolo`.
- [ ] **Tests:** CLI parsing for every new flag and the `--yolo` conflict matrix; `--yolo --read` produces read pins plus `any` for the rest; discovery with fake stdio and HTTP transports (no real process/network); unknown tool names rejected; duplicate category assignments rejected; explicit `acp` rejected; workspace untrusted rejected; built-in grant path byte-for-byte unchanged.

### Phase E3 — Forward the pinned manifest to ee-spawned agents

- [ ] **Fix:** in `app/agents_mcp`, after resolving servers, load effective external grants only when the workspace is trusted; select grants whose `transport_identity` matches the resolved server's current fingerprint; attach a bounded, secret-free `_meta.ee.mcp_trust` manifest to the forwarded `McpServer::Stdio` or `McpServer::Http` entry:
  ```json
  {
    "grant_id": "mcp_server_...",
    "transport_identity": "external-stdio:sha256:...",
    "manifest_digest": "sha256:...",
    "tools": [
      {"name": "query_graph", "schema_digest": "sha256:...", "category": "read"}
    ]
  }
  ```
- [ ] **Fix:** merge the trust metadata with the existing `_meta.ee.cwd` entry on stdio instead of replacing it; set `_meta` on HTTP entries too; never include env/header values, URL credentials, or secrets; cap the manifest at 256 tools / 64 KiB and fail closed by not attaching trust when the cap is exceeded.
- [ ] **Fix:** refresh/reload the grant snapshot before each `session/new`; revoking a grant or untrusting the workspace must remove the manifest from the next session.
- [ ] **Tests:** session/new carries the manifest for matching stdio and HTTP external grants; no manifest for builtin `ee`, untrusted workspace, config repoint, transport mismatch, or missing grant; env/header values and URL secrets are absent; stdio cwd and trust metadata coexist; cache reload behavior covered.

### Phase E4 — Orchestrator verifies the manifest

- [ ] **Fix:** add `crates/ee-agent-orchestrator/src/mcp/trust.rs` with a bounded manifest parser and verifier: validate digest forms, cap tools/bytes, verify the manifest's server name matches the descriptor's server name, and recompute each discovered tool's schema digest.
- [ ] **Fix:** carry the parsed manifest on `McpServerDescriptor`; add `McpTransportKind::Http` and accept `McpServer::Http` in `descriptor.rs` (reject SSE). Keep `Debug` redacted and never persist env/header values.
- [ ] **Fix:** build HTTP discovery on `ee_mcp::transport::build_http_transport` (URL + headers, no OAuth), with the same timeout/cancellation/close handling as stdio. `manager.rs` stays the orchestrator of connections; transport code lives in the split modules from Phase E0.
- [ ] **Fix:** in tool resolution, mark a discovered tool trusted only when name, schema digest, and category all match a pin. Everything else keeps the existing conservative external classification.
- [ ] **Fix:** classify trusted tools as: `read -> Read/None`, `write_create -> Write/None`, `write_modify -> Write/Overwrite`, `delete -> Write/Delete`, `execute -> Execute/None`, `any -> Write/Overwrite`, plus a `trusted_external` tier marker on the tool definition for policy and diagnostics.
- [ ] **Tests:** exact match trusted; name match with schema drift untrusted; new tool untrusted; `any` pin trusted; `readOnlyHint` never changes classification or trust (`readOnlyHint=true` on an untrusted or `any` tool stays conservative/`any`); manifest with unknown category rejected; oversized manifest rejected; stdio and HTTP descriptors both verify; no secrets in diagnostics; descriptor Debug redacted.

### Phase E5 — Policy tiers

- [ ] **Fix:** add the trusted-external tier check to `PolicyEngine::check` via a small helper module (keep `policy.rs` stable):
  - trusted read: allowed when `allow_read`;
  - trusted write/create/modify/delete: allowed when `allow_write`, bypassing only the destructive-subclass gate for the exact pin;
  - trusted `any`: allowed when `allow_write` (whole-server trust, non-read-only sessions only);
  - trusted execute: allowed when `allow_execute`; no external execute pin exists without the explicit grant opt-in, and the pin is verified against the live tool schema before it can authorize.
- [ ] **Fix:** Ask/Plan continue to set `allow_write = false` and `allow_execute = false`, so trusted writes and executes stay denied there. Write mode admits them.
- [ ] **Fix:** `check_with_arguments` still enforces permission rules and workspace scope for trusted tools; a deny rule or out-of-scope path still blocks. No host-approval round-trip is added.
- [ ] **Tests:** trusted read works in Ask; trusted write/delete denied in Ask and allowed in Write; trusted `any` denied in Ask and allowed in Write; trusted execute denied in Ask and allowed in Write only when pinned with `--execute-tool`; untrusted external tool unchanged; permission deny and scope denial still win; builtin ee proxy behavior unchanged.

### Phase E6 — Status, revoke, drift, and diagnostics

- [ ] **Fix:** `status --server <id>` prints origin, transport kind/fingerprint, manifest digest, and per-tool category/digest; `revoke --server <id>` drops the whole grant; `revoke --execute` drops only execute pins.
- [ ] **Fix:** surface bounded orchestrator diagnostics when a manifest is ignored: fingerprint mismatch, manifest digest mismatch, per-tool schema drift, tool missing from `tools/list`, and tools beyond cap. Diagnostics never include env/header values, URL credentials, or secrets.
- [ ] **Fix:** document/ensure invalidation on workspace untrust, server config change (command/args/cwd/env names, URL, header names), schema drift, server id repoint, and tool removal. Grants never follow runtime advertisement changes.
- [ ] **Tests:** status output for external grants; revoke variants; config repoint produces no trust; schema drift produces no trust; diagnostics are bounded and secret-free; builtin status/revoke unchanged.

### Phase E7 — Docs, changelog, and validation

- [ ] **Fix:** update `README.md`, `wiki/config.md` if applicable, and `CHANGELOG.md`: external grant scope, stdio + Streamable HTTP support, SSE/OAuth exclusions, `--yolo` semantics and read-only-mode boundary, examples, execute warning, external-ACP-agent boundary, and the explicit "trust is not a sandbox" statement.
- [ ] **Fix:** tick this section in `ISSUES.md` as phases land; keep the built-in `ee` grant docs aligned.
- [ ] **Validation:** targeted tests only, quiet mode:
  - `cargo test --quiet -p ee-mcp trust`
  - `cargo test --quiet -p ee-cli --features agents mcp_trust`
  - `cargo test --quiet -p ee-cli --features agents agent_mcp`
  - `cargo test --quiet -p ee-agent-orchestrator mcp`
  - `cargo test --quiet -p ee-agent-orchestrator policy`
  - `cargo clippy --quiet -p ee-mcp -p ee-cli -p ee-agent-orchestrator --all-targets -- -D warnings`
  - `cargo fmt --all -- --check`
- [ ] **Acceptance:**
  - `ee do agent trust mcp grant --server atlas --yolo` pins every advertised tool; Write-mode calls run with no prompts/denials; Ask/Plan run no external tools.
  - `ee do agent trust mcp grant --server atlas --yolo --read query_graph` keeps whole-server trust in Write mode and runs `query_graph` in Ask/Plan.
  - `--read`/`--create`/`--write`/`--delete` pins work in Write mode and stay denied in Ask/Plan.
  - `--execute-tool` alone authorizes execute for the exact pinned tool; Ask/Plan deny it; Write mode allows only that tool.
  - stdio and Streamable HTTP external servers both work; SSE and OAuth are rejected with actionable errors.
  - Schema drift, config repoint, new tools, revoke, and workspace untrust all remove trust.
  - Built-in `ee` grants, terminal `--execute`/`--yolo`, and existing tests are unchanged.
  - No secrets in the store, manifest, diagnostics, or logs; every touched code file is under 1,000 LOC.

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
