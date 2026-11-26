# Findings: Approval Flow and External-Agent (OpenCode) Parity

Static read-only review. No tests run. Every claim below cites the file it came
from; items under "Open questions" are **unverified**.

## Current shape (verified)

Gate chain, in order:

1. `PermissionBroker` — `crates/ee-agent-host/src/permission.rs`. Session-scoped
   registry of pending `session/request_permission` requests; resolves
   outstanding requests on turn cancel / connection drop.
2. Host policy — `crates/ee-cli/src/policy/`. `evaluate` is pure; time and usage
   are injected. Precedence: safeguards → mandatory-confirm → session deny →
   persistent deny → session allow → persistent allow → rule defaults.
3. Prompt queue — `crates/ee-cli/src/app/agent_bridge/app_approval.rs`,
   `prompt.rs`, `approval_always.rs`.
4. Remote registry — `crates/ee-agent-remote/src/permissions/approvals.rs`.
   Every resolution appends one audit line; expiry, client error, malformed
   response, and dropped sender all end in deny.

Risk classifier — `crates/ee-agent-orchestrator/src/approval_classifier.rs`:

- `ApprovalClassifierMode::{Off, Auto}`; `Off` is the default.
- `DEFAULT_APPROVAL_CLASSIFIER_TIMEOUT = 5s`,
  `MAX_APPROVAL_CANDIDATE_BYTES = 4 * 1024`.
- One bounded, non-streaming model call returning
  `{ "risk": "low"|"medium"|"high", "reason": ... }`. Only `low` auto-approves;
  timeout, transport error, and malformed reply fail safe to the prompt.
- Deterministic hard blocks run first and never reach a model.

Marker — `crates/ee-agent-protocol/src/approval_meta.rs`:

- `_meta` key `ee/approval_classifier`, `version = 1`, payload
  `{ auto_approved: true }`. Advisory by contract.

Marker consumer — `crates/ee-cli/src/app/agent_bridge/app_trust.rs::classifier_auto_approval`:

- Requires `classifier_auto_approved(meta)`, `workspace_trusted()`,
  `trusted_mode_allows(session_id)`, a resolvable `agents.servers` entry, and
  finally `matches!(name, "ee-agent-opencode" | "ee-agent-openrouter")` on the
  configured command's `file_name`.

Always-allow — `crates/ee-cli/src/app/agent_bridge/approval_always.rs`:

- Workspace-wide (`agent: None`), no expiry, no use budget, host-local
  per-workspace trust store only, offered only while the workspace decision is
  `trusted`. `mark_mandatory_confirmation` strips these options.

Session state — `crates/ee-cli/src/policy/session.rs`:

- `BTreeSet<(String session_id, String fingerprint)>` for allow and deny; deny
  wins; cleared on session teardown.

## Improvements, ranked

1. **Binary-name trust check is spoofable and brittle.**
   `app_trust.rs::classifier_auto_approval` trusts `Path::file_name` against a
   hardcoded `matches!`. Any binary named `ee-agent-opencode` self-approves
   through `_meta`; a renamed build or a new orchestrator-backed agent silently
   loses auto-approve with no diagnostic.
   Fix: verify the canonical executable path against an ee-owned install
   location, or HMAC the marker with a per-install key issued in a handshake
   nonce. Replace the `matches!` with a config field on
   `OrchestratorConfig::servers` so a new backing agent is data, not code.

2. **Classifier re-runs per call; no verdict cache.**
   Identical `(tool, argument shape, path, cwd)` pays 5 s plus tokens on every
   occurrence, inside the interactive path.
   Fix: bounded short-TTL LRU keyed on the existing `OperationIdentity` /
   `approval_fingerprint`, invalidated on workspace-trust or rule change. Cache
   only `low`; never cache `medium` or `high`.

3. **No promotion path from user answers.**
   A manual "Allow always" on a call the classifier already rated `low` teaches
   nothing. Repeated identical manual approvals never reduce prompt count.
   Fix: after N consistent manual allows of one identity, offer the prepared
   `AlwaysRulePreview` confirmation proactively; feed accept/reject counters
   into `DecisionLog`.

4. **`TrustRuleScope` carries expiry and a use budget; always-allow sets
   neither.**
   Broad always-allow for a terminal command is permanent until revoked.
   Fix: default new always-allow rules to a use budget or a short expiry with an
   explicit `never_expire` opt-out. `RuleState` and the store already model this.

5. **Write prompts show no diff.**
   `ApprovalPrompt::write` detail is `path (N bytes)`. `PreparedWrite` already
   holds the full content and `WriteExpectation::{Blind, MustNotExist,
   ExpectRevision}`.
   Fix: bounded unified diff in the prompt, cap lines with an expand path;
   render `MustNotExist` as "creates file" and `ExpectRevision` as
   "replaces revision X".

6. **Classifier verdict is coarse.**
   One of three levels, no category, no confidence.
   Fix: per-category verdict (`read` / `write` / `exec` / `network` / `memory`)
   so a read class can auto-approve while `exec` stays prompt-only; require a
   declared `confidence` and reject payloads that omit it.

7. **Remote approvals lose context.**
   `MAX_SUMMARY_CHARS = 240` in
   `crates/ee-agent-remote/src/permissions/approvals.rs`; no structured detail,
   no diff, no batch answer.
   Fix: carry the structured `ApprovalPrompt` payload (kind, path, diff,
   candidate rules) and answer by option id instead of free-form choice.

8. **Session keys are unversioned strings.**
   `(session_id, fingerprint)` in `policy/session.rs`.
   Fix: typed key carrying category and schema version, mirroring the rule
   identity discipline in `policy/rules/`.

## External agents (OpenCode)

Verified: `crates/ee-agent-opencode/**` contains no permission or approval logic.
A search for `request_permission`, `PermissionOption`, and `permission` matches
only `approval_classifier` config plumbing (`src/config.rs`, `src/config/manifest.rs`,
`src/adapter.rs:approval_classifier_config`). The OpenCode agent runs in-process
through the orchestrator and therefore inherits ee's host gate verbatim.

1. **OpenCode's own `permission` configuration is unconsumed.**
   OpenCode ships per-agent permission rulesets (bash patterns, edit/write,
   webfetch categories, `ask|allow|deny`). ee only enforces its own host broker
   and only translates ee-side markdown agents.
   Fix: translate the `permission` block from every discovered config source
   into host `PermissionRules` at discovery time, so an external-agent config
   keeps its own semantics. This is frontend-config-owned per RULE.md.

2. **No `session/request_permission` fallback for opaque external agents.**
   An agent not listed under `agents.servers` fails
   `classifier_auto_approval` and gets a prompt that does not understand its own
   option ids.
   Fix: accept the agent's own option list and map to `AllowOnce` / `DenyOnce`
   via `PermissionOptionKind`; never synthesize an "always" the agent did not
   offer.

3. **External agents can silently lose "Allow always".**
   `attach_always_allows` returns early when `command_invocation_for_request`
   fails to parse, so the option vanishes with no reason shown.
   Fix: surface "no always-allow candidate: <reason>" in the prompt instead of a
   silent drop.

4. **Approval modes do not propagate.**
   `ToolApprovalMode::{Default, Autopilot, Bypass}` (`approval.rs`) is ee-only;
   an OpenCode-managed subtask cannot inherit the parent's no-prompt intent.
   Fix: propagate the mode into the ACP session config option at spawn.

## Open questions (not verified)

- Whether `DecisionLog` records accepted and rejected auto-approvals with enough
  detail to audit a prompt-skipping decision.
- Depth of `permission_rules.rs::PermissionTarget` matching versus the host
  `evaluate` precedence chain.
- How the frontend renders `ApprovalPrompt.options` (scroll, keybinding, whether
  a long option list is truncated).

## Suggested next step

Take items 1, 4, and 5 first: security, blast-radius, and UX, each localized to
one module. File them as a new PART 10 section in `ISSUES.md`; PART 8 is closed
per its own header.
