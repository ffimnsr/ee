# PLAN-HARNESS — Recursive Harnessing Adaptation (Harness2 → ee agents mode)

Status: **planned, not started** · Scope: `ee-agent-orchestrator`, a new `ee-agent-harnessing` crate, `ee-agent-remote` reuse, `ee-cli` surfaces.

Sources:

- Paper: arXiv 2610.14224v1 — *Harness²: Recursive Agent Harnessing for an Open World* (Google Cloud AI Research).
- Vendored implementation: `harness-example/harness2/harness2/` (`core/recursion.py`, `core/library.py`, `core/selection.py`, `core/tree.py`, `core/context.py`, `core/ledger.py`, `core/proposer.py`, `core/prompts.py`, `core/schedule.py`).
- Implementation checklist: [`ISSUES.md`](../ISSUES.md) → "Part 11 — Recursive Harnessing (Harness2 adaptation)".

Relationship to existing plans:

- [`docs/upgrades/PLAN-harness.md`](upgrades/PLAN-harness.md) is the **DSH prompt-sections parity plan** (phases 0–6 landed). That plan fixed *what the harness says*. This plan adapts Harness2's **improvement loop** — how the harness edits itself from execution evidence — and builds on the section registry that plan delivered.
- [`docs/upgrades/record-replay-harness.md`](upgrades/record-replay-harness.md) and [`crates/ee-agent-orchestrator/src/evaluation.rs`] supply the measurement layer this plan reuses.
- [`docs/upgrades/orchestrator-crate-split.md`](upgrades/orchestrator-crate-split.md) constrains placement: the orchestrator is a single 51k LOC unit; the new loop must not grow it.

---

## 1. What Harness2 does (condensed)

A *harness* is the editable scaffolding around a frozen model: prompts, memory, skills, tools, orchestration. Harness2 exposes **8 file-level components** (system prompt, guardrails, memory, tool guidance, skills, scripts, subagents, plugin) and improves them with two coupled loops, **without ground-truth verifiers**:

1. **Task-level recursion — Propose → Probe → Compose.**
   - *Propose*: run the current harness once for a reference execution; an improver session reads (task, harness, reference trajectory, library) and proposes two scoped candidate edits — an **INV** candidate (process: planning, tool use, context management, delegation) and a **DOM** candidate (domain: practitioner knowledge, artifact contracts, decision rules).
   - *Probe*: execute each candidate once on the same task under matched config; the behavioral **contrast** vs the reference (not a score) is the evidence.
   - *Compose*: a composer session merges supported edits, adopts one, or keeps the base; every decision cites a named trajectory moment. `KEEP_BASE` is a valid accepted outcome.
   - Scales two ways: **parallel** (k INV + k DOM candidates, k independent compositions from the shared evidence pool) and **sequential** (k chained steps, each conditioned on the accumulated history).
2. **Harnessing-level recursion — experience library.**
   - After each task, a reflector diffs every candidate against the base, records per-edit verdicts (`helped | hurt | no observed effect | confounded`), and writes **at most one** durable lesson (`cue / lesson / evidence / confidence`) into a per-domain library entry together with the full adopted harness tree.
   - Lessons are retained **only** when backed by execution contrasts; adoption alone never establishes transferability; `NO_ENTRY` is a valid result. Streaming retention: a task only ever sees entries from earlier tasks.

**Results that justify the order of work**: one contrastive step gains up to +11.5 (WorkBuddy), +4.5 (JobBench), +11.8 partial (LAB); k=5 recursion adds +3–4 more; tool-error rate drops 20–37%. Ablations (points lost when removed): **library −3.1/−1.8, scoped INV/DOM editors −2.9/−3.7, composition −1.6/−1.9** — all three components matter, so the plan does not skip any.

**Failure modes the paper admits** (must be encoded, not rediscovered):

- selection can prefer a weaker deliverable (picked a 55 over a 95) — coverage vs precision;
- a wrong premise propagates across candidates — probes must challenge shared assumptions;
- a single rollout is weak evidence — prefer a `confounded` verdict over invented attribution;
- library reuse is adaptation, not copying (only 2–10% of modified lines survive from an entry).

## 2. Current ee state (verified via Atlas, 2026-11-23)

Harness2's 8 components vs ee today:

| Harness2 component | ee state | Files |
|---|---|---|
| C1 system prompt | present | provider `DEFAULT_SYSTEM_PROMPT` (`ee-agent-opencode/src/config.rs`, `ee-agent-openrouter/src/config.rs`); DSH section registry landed (`prompt/sections.rs`, `prompt/render.rs`); user agents (I16) |
| C2 guardrails | **missing as a component** — constraints merged into system prompt | — |
| C3 memory | present | `user_memories.rs` (I27 `.agents/memories`), `user_memory_write.rs` CRUD, `semantic_memory.rs` optional adapter |
| C4 tool guidance | **hardcoded const, not editable** | `prompt/tool_guidance.rs::TOOL_GUIDANCE` |
| C5 subagents | present | `subagents/manager.rs`, `subagent_roles.rs`, evidence checks in `subagent_verifier.rs` |
| C6 skills | present (read-only catalog) | `prompt/skills.rs` + `load_skill` tool |
| C7 scripts | **no convention** | — |
| C8 plugin / middleware | **two halves, no editable seam**: code-level `plugin_runtime/` exists (Cordis-style `agent.*` services, bridge gates tool/approval, turn-scoped contributions); no declarative file-level hooks an improver could edit | `plugin_runtime/{host,bridge,effects,events,services}.rs`, `loop_hooks.rs` |

Existing machinery this plan reuses (no duplication):

- **Evidence**: append-only `session_log.rs` already records `Usage(UsageRecord)` and `Tool(ToolRecord)` per execution; `usage_stats.rs` (I23) derives deterministic analytics from it; `workspace_snapshot.rs` (I19) gives per-turn workspace diffs; `session_transfer.rs` (I22) has export + redaction.
- **Improver-shaped building blocks**: `reflection.rs` (bounded self-review, budget slots, findings→task graph), `rubber_duck.rs` + `critique.rs` (contrasting critic with schema/citation verification), `subagent_roles.rs` (evidence-checked role spawns), `markdown_config.rs` (shared bounded frontmatter discovery behind I16/I17), `user_memory_write.rs` (agent-authored memory-file writes through approval gates).
- **Probe substrate**: `ee-agent-remote` headless host (R1–R13) can re-roll a task without the editor.
- **Measurement**: `evaluation.rs` hermetic fixture harness + `ee-chat-completions` record/replay provider.

Gaps (nothing exists for these — verified zero graph matches):

- no post-task **experience library** (cue/lesson/evidence, verdict vocabulary, streaming retention);
- no **reflector** that turns a session into library entries;
- no **Propose/Probe/Compose/Selection** loop and no per-session harness override to mount candidates;
- no **harness-edit validation** (trigger-line rule, script existence, autotrigger repair);
- no **budget ledger** for improvement loops; no improver-facing **evidence export** with grader-leak guards.

## 3. Fixed decisions

**D1 — New crate: `ee-agent-harnessing`.** The improvement loop (propose/probe/compose/select/reflect drivers, context-dir builder, library store, ledger) lives in a new crate, per the orchestrator split direction. The orchestrator gains only small seams, each independently shippable:

1. per-session `HarnessSource` override (I38),
2. declarative middleware hook interpreter (I33),
3. an evidence-export projection helper (I37),
4. validation module (I34) — pure, unit-testable.

**D2 — All new editable components are bounded files on the existing planes.** Workspace files under `.agents/` ride the untrusted plane (trust-labeled, cannot override system/developer/user policy — same headers as `skills.rs`); personal mirrors under `$XDG_CONFIG_HOME/ee/` ride the trusted user plane. Discovery, bounds, and warning-skip discipline reuse `markdown_config.rs`. No component is ever injected without a trust label.

**D3 — Middleware is declarative TOML, not plugin code.** `.agents/middleware.toml` describes a bounded hook set interpreted at existing loop boundaries (`loop_hooks.rs`, stop gate). The code-level extension path stays `plugin_runtime`; improver sessions edit TOML, never author plugins. Unknown keys: logged warning + whole-file ignore (fail closed).

**D4 — Validate before adopt.** Mirror `tree.py validate`: every harness edit passes a validation layer before it can be adopted or written to the library — skill trigger-line rule, script existence + bounds + extension allowlist, frontmatter shape, reserved names, autotrigger repair. One retry with the validator message; second failure inherits the current harness. `KEEP_BASE` is a valid, recorded outcome.

**D5 — Improver sessions see evidence, never verdicts.** Evidence context dirs are built from redacted session-log projections, workspace diffs, and the harness tree; a build-time guard refuses any context containing grader/rubric/score material (mirror `context.py assert_no_grader_verdicts`). Trajectory files are data, not instructions — existing `prompt_injection.rs` labeling applies.

**D6 — Budget identity and flag gating.** Per task the loop records R = 4k rollouts and P = 3k improvement sessions and asserts the identity (mirror `ledger.py`); counts derive from session-log usage records, so a replay reproduces them. The whole loop is off by default (`k = 0`), surfaced only behind I26 feature flags, and every stage has hard caps.

**D7 — Lessons need contrasts.** The reflector may write at most one lesson per task, only from execution contrasts; verdicts are `helped | hurt | no observed effect | confounded`, with `confounded` preferred over invented attribution; `NO_ENTRY` is valid. Streaming retention: online, a task sees only earlier entries; leave-one-out reading exists only in the offline eval mode (I46).

**D8 — Improver roles are builtin and evidence-checked.** `harness_editor_inv`, `harness_editor_dom`, `harness_composer`, `harness_selector`, `harness_reflector` are builtin role definitions in the `subagent_roles.rs` pattern (code-controlled prompts, own context, bounded steps, evidence verification on return). They are not user-plane markdown agents.

**D9 — Probes reuse the headless host.** Candidate re-rolls run through `ee-agent-remote` under matched model/env config, one rollout per candidate, with existing timeouts and cancellation.

**D10 — Measurement reuses the fixture harness.** Harness-edit regressions run on `evaluation.rs` fixtures + record/replay (deterministic checks only in CI; no LLM judge in the loop).

## 4. Component surface layout (target)

```
<workspace>/.agents/
  guardrails.md                   # H1 · always-on short constraints, untrusted plane
  tool_guidance.md                # H1 · optional override of built-in guidance, untrusted plane
  middleware.toml                  # H1 · declarative loop hooks, flag-gated
  scripts/<name>.{sh,py}          # H1 · deterministic helpers, extension allowlist
  skills/<name>/SKILL.md           # existing · unchanged
  agents/<name>.md                # existing (I16) · unchanged — this IS the C1 editability surface:
                                   #   a primary agent's body is its system prompt, so improver
                                   #   sessions edit C1 by editing these files, never code consts
  memories/                        # existing (I27) · unchanged
  experience/<domain>/             # H2 · per-domain improvement library
    INDEX.md                       #   one line per entry: cue summary + adoption seq
    entries/<seq>-<task-slug>/
      harness/<rel>               #   full adopted component copies (never summaries)
      EXPERIENCE.md               #   edits verdict rows + cue/lesson/evidence/confidence

$XDG_CONFIG_HOME/ee/
  guardrails.md  tool_guidance.md  middleware.toml  scripts/  experience/   # H1/H2 mirrors, trusted user plane
```

Bounds follow the existing discipline (per-file 64 KiB, catalog 8 KiB, bounded scan counts, warning-skip on malformed/oversized/duplicate).

## 5. Phase H0 — prompt-content transfers (no new machinery)

**Goal:** adopt the paper's improver prompt patterns into text ee already renders. Lands as pure prompt-content changes with hermetic tests; no schema, no files, no flags.

### Work items

- **H0.1 Editor-pattern guidance blocks** in `prompt/tool_guidance.rs` (and the reviewer/delegation sections that already exist):
  - divergence-first: the cheapest fix is where runs disagreed — fix the earliest observed divergence;
  - evidence-moment rule: every proposed change must name the trajectory moment it addresses;
  - `KEEP_BASE`: leaving the harness unchanged is a valid answer when evidence does not argue for edits;
  - form preference order: skill (triggered, costs nothing until invoked) > script (deterministic ops) > subagent (large input, small answer) > middleware hook > always-on prose (the expensive option; if you add some, cut some);
  - deliverable-shape rule: harness changes HOW the agent works, never WHAT shape it delivers.
- **H0.2 Grounding rule** in the same guidance: verified workspace facts are written as facts ("in this workspace, role X in file A maps to position Y"), anchored and labeled, not as rediscovery procedure; a guessed fact is worse than none; procedure is written for the domain, facts are labeled as workspace-specific.
- **H0.3 Selector criteria** into the `reviewer` role body (`subagent_roles.rs`) in order: delivery → conformance → fidelity (check claims against source) → internal consistency → operational fitness → finish; length and polish are not merits; picking the plainer artifact is valid.

### Acceptance

- Guidance tests (`prompt/tool_guidance.rs::tests`, `subagent_roles` role tests) assert the new blocks render only with the relevant tools advertised and stay within section byte budgets.
- No behavior change beyond prompt text: full targeted module tests green, no new config.

## 6. Phase H1 — editable component surface completion

**Goal:** close the C2/C4/C7/C8 gaps so the harness is a set of bounded, trust-labeled files — the substrate every later phase edits.

### I30 — Guardrails file component

- Discovery: `.agents/guardrails.md` + `$XDG_CONFIG_HOME/ee/guardrails.md` via `markdown_config.rs`; body = always-on constraints; 32 KiB cap; workspace-over-user precedence with trust labels.
- Render: one untrusted section after the system prompt (registry slot, additive per the DSH plan's D1); absent file emits nothing; oversize/malformed = warning-skip.
- Tests: prompt bundle carries a labeled guardrails section; precedence; bounds; skip-with-warning.

### I31 — Tool-guidance override file

- `.agents/tool_guidance.md` (untrusted) augments/overrides the builtin `TOOL_GUIDANCE` section for the same tools; builtin text remains the default fallback; bounds and trust headers mirror skills.
- Tests mirror `prompt/skills.rs` tests (absent tree emits nothing, override wins, label present).

### I32 — Scripts convention

- `.agents/scripts/` + user-plane mirror; bounded discovery; extension allowlist (`sh`, `py`); catalog line rendered into tool guidance: "prefer a script for repeated deterministic extraction/conversion/checking; run via the terminal tool under the existing approval gate".
- Execution stays the existing terminal approval path — no new execution surface.
- Tests: discovery bounds, allowlist enforcement, catalog rendering.

### I33 — Declarative middleware hooks

- `.agents/middleware.toml`, strict serde; unknown key or malformed file ⇒ loud warning + file ignored (fail closed). Initial bounded hook set, each executed at existing loop boundaries (`loop_hooks.rs`, stop gate, post-tool-result seam):
  - `warn_no_artifact_after_calls = N` — appends a warning to the tool result after N calls in a turn without a write artifact (the paper's biggest LAB case: deadline runs that never write files);
  - `hint_on_error_signature = [{ pattern, hint, max_uses }]` — bounded list; appends a targeted hint after a recurring error signature;
  - `verify_after_write = true` — appends a reopen-and-check reminder after each write.
- Invariants: hooks append text to results and never block a valid completion; every hook has a per-turn use cap; gated behind the I26 flag `agents.middleware`.
- Tests: hook fires at exactly N calls; unknown keys ignored loudly; caps enforced; no hook can suppress a tool result.

### Acceptance for H1

- Each item independently shippable; hermetic tests per the delivery gates; `cargo check -p ee-cli` after orchestrator seam changes.

## 7. Phase H2 — harnessing-level recursion (validation, library, reflector)

**Goal:** the system can look at what a session did and write durable, contrast-backed lessons — the ablation-heavy component (−3.1/−1.8 when removed).

### I34 — Harness-edit validation layer

- New pure module `crates/ee-agent-orchestrator/src/harness_validation.rs` (later re-used by the harnessing crate):
  - trigger-line rule: every discovered skill must appear verbatim in always-on text (guardrails/tool_guidance override), else warning + drop from catalog (mirror `tree.py autotrigger` + the "most violated rule" note);
  - script-reference rule: a script named in guidance/skills must exist under an allowed root, within bounds and allowlist;
  - frontmatter validation (reuse the I16/I17 parsers);
  - reserved-name and duplicate-name checks.
- API: `validate(tree) -> Verdict { ok, repairs_applied, errors }`; repairs are recorded, never silent.
- Tests: unit matrix mirroring the three "discard the proposal" rules from the paper's editor prompt.

### I35 — Experience library

- New module `user_experience.rs` mirroring `user_memories.rs`: discovery of `experience/<domain>/entries/*/EXPERIENCE.md`, `INDEX.md` rebuild-on-write, bounded catalog (cue summaries) injected per session scoped by domain, `load_experience` read-only tool mirroring `load_memory`.
- Entry schema (frontmatter + body blocks):

```markdown
---
seq: 7                      # adoption order, streaming retention key
domain: rust-crates
task: <task slug or session id>
confidence: medium
---
## Edits
component: tool_guidance.md
change: added reopen-and-check rule
observed: candidate re-opened outputs; base did not (run c2, step 11)
verdict: helped
## Experience
cue: deliverables written but never reopened for verification
lesson: add a verify-after-write guardrail naming the artifact type
evidence: edits row above; contrast between run c2 and base run b1
```

- `NO_ENTRY` entries (edits rows but no lesson) are valid and retained.
- Streaming retention: the catalog shows only entries with `seq` below the current task's; the reflector is the only writer.
- Tests mirror the `user_memories.rs` suite (precedence, bounds, malformed skip, catalog bytes, retention).

### I36 — Reflector: `/reflect` + post-session hook

- New builtin role `harness_reflector` (D8). Input: a redacted projection of the session (tool records + workspace snapshot diff from the existing I19/I23 surfaces) plus the harness files in effect.
- Output contract (mirror `selection.py` parsing): an `<edits>` block with one five-line row per changed component (`component / candidates / change / observed / verdict`) and at most one `<experience>` block (`cue / lesson / evidence / confidence`); `NO_ENTRY` replaces the block when contrasts are absent.
- Writes: through the existing approval-gated write path (`user_memory_write.rs` pattern); the entry and `INDEX.md` update are one transaction-ish write batch.
- Surface: manual `/reflect` slash command first; an automatic post-session hook later, flag-gated.
- Guard: the evidence projection refuses grader/rubric/score material (D5).
- Tests: parser round-trips; verdict vocabulary enforced; one-lesson bound; write path honors approval; NO_ENTRY path writes edits-only entry.

## 8. Phase H3 — task-level recursion loop

**Goal:** the full Propose–Probe–Compose–Select driver in `ee-agent-harnessing`, off by default, flag-gated, budget-asserted.

### I37 — Evidence export (rollout views)

- `rollout_view.rs` (orchestrator seam): builds an improver-facing view from a session: bounded per-turn records (tool name, ok/error class, duration, result digest), final deliverable pointer, workspace diff summary. Redaction reuses the R12 redaction layer; truncation bounds mirror `context.py _write_rollout`.
- Context-dir builder in the harnessing crate (mirror `context.py build`): `context/task.md`, `context/source/`, `context/harness/`, `context/library/`, `context/rollouts/*.md`, plus a `mission.md` per stage. Build-time grader-leak guard (D5).
- Tests: hermetic; redaction asserts; truncation bounds.

### I38 — Per-session harness source

- `HarnessSource { Workspace, Override(PathBuf) }` in `OrchestratorConfig`, defaulting to `Workspace`; the prompt-assembly seam resolves guardrails/tool-guidance/scripts/skills roots from the source. This is the only orchestrator change the probe stage needs: a probe session mounts the candidate directory without touching the workspace.
- Tests: an override session renders candidate files; workspace sessions are byte-identical to today.

### I39 — Propose

- Driver: obtain k reference executions (existing runtime); build the evidence context dir; spawn `harness_editor_inv` and `harness_editor_dom` sessions (SubagentManager, evidence-checked); each returns an edited copy of the component files (their file-tool writes are scoped to a candidate dir via existing workspace-isolation patterns).
- Candidate acceptance: run the I34 validator; one retry with the validator message; second failure inherits the current harness (recorded as fallback, mirror `recursion.py _run_stage`).
- Tests: fake transport; validator-retry-inherits path; scoped writes.

### I40 — Probe

- Re-roll each accepted candidate once via the `ee-agent-remote` headless host under matched model/env config; capture a rollout view per probe; existing timeouts/cancellation apply.
- Tests: hermetic with fake transport; probe isolation (candidate files mounted via I38, workspace untouched).

### I41 — Compose

- `harness_composer` session reads the full pool (references + candidates + probes + diffs); emits one composed tree plus a notes block (`kept|revised|dropped -> <edit> -> <trajectory moment with counts>`), parsed and recorded. Merge / adopt-one / keep-base all valid; an edit from a non-delivering rollout is unsafe unless the rest of the evidence is overwhelming (stated rule).
- Validation (I34) gates the composed tree with the same retry/inherit rule.
- Tests: notes parser; composed-tree validation; unsafe-edit rule enforced by prompt contract + recorded fallback.

### I42 — Selection

- Pool = the k finals (anonymized, seeded shuffle ids — deterministic per task/k/mode, mirror `selection.py assign_ids`); the base run's deliverable is a valid candidate and the selector is told so.
- `harness_selector` returns a fenced ```choice block + ≤6-line `<why>` naming the deciding criterion and the runner-up's cost; parse failures fall back to the reference deliverable.
- Tests: id determinism; parse; fallback; base-can-win path.

### I43 — Ledger and driver surface

- Ledger per task: rollout and session counters derived from session-log usage records; `assert_identity` R=4k / P=3k; hard caps and typed budget errors; totals feed `usage_stats` so improvement spend is visible next to task spend.
- Driver surface: `/harness k=N mode=parallel|sequential` slash command + a manual `ee do agent harness improve` CLI path, both flag-gated (I26) and default-off; dry-run mode prints the plan (references, candidates, compositions) without model calls.
- Tests: identity asserts; caps; dry-run hermetic.

## 9. Phase H4 — measurement

### I44 — Harness-edit regression suite

- Per-domain fixtures on `evaluation.rs` + record/replay: deterministic deliverable checks only (file exists, shape, content assertions); a harness edit that regresses a domain fails CI before it can enter the library.
- Tests: suite green on the base harness; a deliberately bad edit (missing trigger line) is caught.

### I45 — Steps and tool-error metrics

- Extend `usage_stats.rs` with per-task step counts and tool-error-rate rows, plus an optional trend view against harness adoption time (entries carry `seq`), surfacing the paper's E.4 signal (error rate should fall after refinement).
- Tests: deterministic from the session log.

### I46 — Leave-one-out library evaluation

- Offline harnessing-crate mode: re-run a task with its own entry excluded from the completed library; report per-domain deltas (mirror the paper's E.2 protocol). Eval-only; never an online path.
- Tests: hermetic; exclusion correctness.

## 10. Sequencing and dependencies

```
H0 ──────────────────────────────────────────────► shippable immediately, no deps
H1: I30, I31, I32 ── independent ──► I33 needs I26 (flags)
H2: I34 ── independent; I35 after H1 files exist; I36 after I34+I35
H3: I37, I38 ── independent seams; I39→I40→I41→I42 chained; I43 last;
    whole phase requires H1 + H2 + ee-agent-remote (landed) + I26
H4: I44 after H1; I45 anytime after I43's spend visibility exists; I46 after H2
```

Default-off guarantee: nothing in H1–H3 changes a session's behavior unless the corresponding file exists (and, for middleware/recursion, the flag is on). H0 changes only prompt text.

## 11. Testing strategy

- Pure modules (validation, library, parsers, ledger): unit matrices mirroring the harness2 rules; property-ish bounds tests for every file-size/count cap.
- Session-level: `FakeAgentTransport` hermetic suites; `EE_TEST_DISABLE_USER_PLUGINS` discipline; no network, no provider keys, no real config tree.
- Cross-crate: `cargo check -p ee-cli` for orchestrator seams; targeted `cargo test --quiet -p <crate> <module>` per the escalation rules; full package only as the manual gate.
- Flaky-test rules apply throughout: improver drivers own every spawned session and await completion; no wall-clock sleeps; seeded shuffles make selection deterministic.

## 12. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Recursion multiplies model spend (R=4k rollouts per task) | default `k=0`, flag-gated, hard caps, ledger visible in `usage_stats`; dry-run mode |
| Improver writes poison the harness (bad edits, injection) | D4 validation before adopt; untrusted plane labels; approval-gated writes; regression suite I44 |
| Grader/rubric leakage into improver context | D5 build-time guard + redaction reuse; tests assert refusal |
| Library bloat and stale lessons | one lesson per task; `NO_ENTRY` valid; leave-one-out eval detects non-transferring entries; future: consolidation pass (paper's own future work) |
| Selection prefers weaker deliverable (paper failure mode) | anonymized pool, base valid, criteria order fixed (H0.3), fallback to reference on parse failure |
| Wrong premise propagates across candidates | INV/DOM scoping keeps candidates independent; compose reads both trajectories, not just verdicts |
| Orchestrator growth past maintainability | D1 new crate; seams are small, additive, independently testable |

## Appendix A — harness2 → ee module map

| harness2 (Python) | ee target |
|---|---|
| `core/tree.py` (SurfaceSpec, validate, autotrigger, diff_summary) | `harness_validation.rs` + `workspace_snapshot.rs` diffs |
| `core/library.py` (Library, INDEX, retain, EXPERIENCE.md) | `user_experience.rs` (I35) |
| `core/selection.py` (assign_ids, pool, choice/why, experience parsing) | harnessing crate select/reflect parsers (I36, I42) |
| `core/context.py` (build, mission, rollout writing, grader guards) | evidence export + context-dir builder (I37) |
| `core/recursion.py` (parallel/sequential, stage retry/fallback) | harnessing crate driver (I39–I41) |
| `core/proposer.py` (agentic session runner, audit, concurrency) | SubagentManager role spawns + audit via session log |
| `core/ledger.py` (R=4k, P=3k, identity) | ledger module (I43) |
| `core/prompts.py` (INV/DOM/composer/selector/reflector) | builtin role bodies (D8) + H0 guidance transfers |
| `core/schedule.py` (stream order, resume, library retain) | driver + streaming retention (I43, I35) |
