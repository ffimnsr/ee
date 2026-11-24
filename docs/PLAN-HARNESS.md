# PLAN-HARNESS — Recursive Harnessing Adaptation (Harness2 → ee agents mode)

Status: **implemented (Part 11 phases H0–H4 landed 2026-11-23; parity closure H5–H9 landed 2026-11-24)** · Scope: `ee-agent-orchestrator`, a new `ee-agent-harnessing` crate, `ee-agent-remote` reuse, `ee-cli` surfaces. Follow-up parity closure: [`PLAN-HARNESS-PARITY.md`](PLAN-HARNESS-PARITY.md) (implemented 2026-11-24).

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

**D6 — Budget identity; invocation is the only gate.** Per task the loop records R = 4k rollouts and P = 3k improvement sessions and asserts the identity (mirror `ledger.py`); counts derive from session-log usage records, so a replay reproduces them. Follows harness2 itself: the harnessing pipeline is a driver the user runs (`/harness`, `ee do agent harness improve`) — running it is consent, exactly like running the `harness2` binary over a benchmark. No config opt-in, no flag registry (I26 out), no disabled command registration. `k` defaults to 1 (the paper's single-step default) when unspecified. Hard caps and the ledger are accounting, not gating.

**D7 — Lessons need contrasts.** The reflector may write at most one lesson per task, only from execution contrasts; verdicts are `helped | hurt | no observed effect | confounded`, with `confounded` preferred over invented attribution; `NO_ENTRY` is valid. Streaming retention: online, a task sees only earlier entries; leave-one-out reading exists only in the offline eval mode (I46).

**D8 — Improver roles are builtin and evidence-checked.** `harness_reflector` is a builtin role definition in the `subagent_roles.rs` pattern (code-controlled prompt, own context, bounded steps) and is used by manual `/reflect`. The driver roles (`harness_editor_inv`, `harness_editor_dom`, `harness_composer`, `harness_selector`) are fixed mission contracts in `ee-agent-harnessing` that run as isolated host sessions; their returned tree is checked by I34 rather than by a subagent evidence verifier, and the harness itself owns context and step bounds. They are not user-plane markdown agents either way. **Implementation note:** `ImproverRole::builtin_role_name` keys the session cache with those names; adding full subagent-role definitions is not required for the loop's correctness.

**D9 — Probes reuse the headless host.** Candidate re-rolls run through `ee-agent-remote` under matched model/env config, one rollout per candidate, with existing timeouts and cancellation.

**D10 — Measurement reuses the fixture harness.** Harness-edit regressions run on `evaluation.rs` fixtures + record/replay (deterministic checks only in CI; no LLM judge in the loop).

## 4. Component surface layout (target)

```
<workspace>/.agents/
  guardrails.md                   # H1 · always-on short constraints, untrusted plane
  tool_guidance.md                # H1 · optional override of built-in guidance, untrusted plane
  middleware.toml                  # H1 · declarative loop hooks, active when present and valid
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
  guardrails.md  tool_guidance.md  middleware.toml  scripts/   # H1 user mirrors, trusted user plane
  experience/                                                # read-only discovery; no implicit write mirror
```

Writes are workspace-scoped: `save_entry`/`save_entry_at` only write under the
resolved experience-library root. `$XDG_CONFIG_HOME/ee/experience` is read when
a caller supplies that root, but no invocation mirrors entries into it.

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

- `.agents/scripts/` + user-plane mirror; bounded discovery; extension allowlist (`sh`, `py`); catalog message on the workspace prompt plane, rendered only when a terminal tool is advertised: "prefer a script for repeated deterministic extraction/conversion/checking; run via the terminal tool under the existing approval gate".
- Execution stays the existing terminal approval path — no new execution surface.
- Tests: discovery bounds, allowlist enforcement, catalog rendering.

### I33 — Declarative middleware hooks

- `.agents/middleware.toml`, strict serde; unknown key or malformed file ⇒ loud warning + file ignored (fail closed). Initial bounded hook set, each executed at existing loop boundaries (`loop_hooks.rs`, stop gate, post-tool-result seam):
  - `warn_no_artifact_after_calls = N` — appends a warning to the tool result after N calls in a turn without a write artifact (the paper's biggest LAB case: deadline runs that never write files);
  - `hint_on_error_signature = [{ pattern, hint, max_uses }]` — bounded list; appends a targeted hint after a recurring error signature;
  - `verify_after_write = true` — appends a reopen-and-check reminder after each write.
- Invariants: hooks append text to results and never block a valid completion; every hook has a per-turn use cap. Active exactly when the file is present and parses — mirror of harness2's mount-after-validation, where a plugin exists because it ships in the harness tree; absent or invalid ⇒ loud warning + ignored.
- Tests: hook fires at exactly N calls; unknown keys ignored loudly; caps enforced; no hook can suppress a tool result.

### Acceptance for H1

- Each item independently shippable; hermetic tests per the delivery gates; `cargo check -p ee-cli` after orchestrator seam changes.

## 7. Phase H2 — harnessing-level recursion (validation, library, reflector)

**Goal:** the system can look at what a session did and write durable, contrast-backed lessons — the ablation-heavy component (−3.1/−1.8 when removed).

### I34 — Harness-edit validation layer

- New pure module `crates/ee-agent-orchestrator/src/harness_validation.rs` (later re-used by the harnessing crate):
  - trigger-line rule: every discovered skill must appear verbatim in always-on text (guardrails/tool_guidance override), else warning + drop from catalog (mirror `tree.py autotrigger` + the "most violated rule" note);
  - script-reference rule: a script named in guidance/skills must exist under an allowed root, within bounds and allowlist, **and pass a bounded syntax check** (`sh -n`, `python -m py_compile`) — mirror `tree.py _py_compiles`, run by the driver, never by an improver session;
  - frontmatter validation (reuse the I16/I17 parsers);
  - reserved-name and duplicate-name checks;
  - editable-surface naming rules (lowercase/digits/`_`/`-` components, mirror `read_tree(rejected=...)`): off-surface files are rejected by name in the error so the editor can fix them;
  - **always-on byte cap**: total always-on text (system + guardrails + tool guidance) has a hard budget with a stated reason (every token competes with the agent's output budget), and every diff summary surfaces `always_on_char_delta` so composition sees what prose costs.
- API: `validate(tree) -> Verdict { ok, repairs_applied, errors, census }` — census counts (files, skills, scripts, subagents) are recorded like `tree.py validate`'s return; repairs are recorded, never silent. The driver records census, always-on characters, and the delta for every accepted candidate, and includes them in the validator report carried into a retry.
- Second-pass detail from `recursion.py`: when a candidate is partially valid, the valid subset is written back into the session's `context/harness/` before the retry — the retry reads a fresh context copy, so it must carry this attempt's edits; a tree that comes back empty is rejected ("edit the files, do not delete them all").
- Tests: unit matrix mirroring the three "discard the proposal" rules from the paper's editor prompt, plus cap/reject/empty cases.

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
- Streaming retention: the catalog shows only entries with `seq` below the current task's; the reflector is the only writer. **Implementation note:** the online prompt catalog is domain-labeled (so a session sees every domain's cue with `[domain #seq]`) while driver evidence and leave-one-out filtering scope by the task domain; `HarnessTask::with_excluded_experience_seq` hides exactly one entry for leave-one-out without hiding later entries.
- Tests mirror the `user_memories.rs` suite (precedence, bounds, malformed skip, catalog bytes, retention).

### I36 — Reflector: `/reflect` + post-session hook

- New builtin role `harness_reflector` (D8). Input evidence mirrors `reflect_adoption`: the chosen harness + every non-base candidate's probe and harness diff + **a copy of the base harness for diffing** + a decisions file (composition notes) + an `adoption.json` mapping the selector's anonymous ids to rollout labels plus the selection's `<why>` — so the reflector can audit the reasoning, not just the verdict. Tool records + workspace snapshot diff come from the existing I19/I23 surfaces.
- The reflector session runs on a **curated copy** of the reflection context (mirror `_distil` copying the root minus the raw selection record): the adoption summary is in, raw selection internals are out.
- Output contract (mirror `selection.py` parsing): an `<edits>` block with one five-line row per changed component (`component / candidates / change / observed / verdict`) and at most one `<experience>` block (`cue / lesson / evidence / confidence`); `NO_ENTRY` replaces the block when contrasts are absent. Parsed lessons require all of cue/lesson/evidence non-empty; `confidence` is optional (`low | medium | high`, default medium) and invalid values are rejected; the edits block is stored in `EXPERIENCE.md` separately from the lesson block; the grader-verdict assert also runs on the parsed experience itself.
- Writes: through the existing approval-gated write path (`user_memory_write.rs` pattern); the entry and `INDEX.md` update are one transaction-ish write batch. `/reflect --library-root DIR` targets an explicit experience library; the default stays `<workspace>/.agents/experience`.
- Cost accounting (mirror the paper): selection and reflection spend is tracked **outside** the R/P ledger identity, on the run record — the identity stays R=4k / P=3k.
- Surface: manual `/reflect` for one-off sessions, and the reflector runs as the final stage of every `/harness` run — mirror of `schedule.py`, which reflects after each task in the stream. No automatic editor-session hook: out of scope.
- Guard: the evidence projection refuses grader/rubric/score material (D5).
- Tests: parser round-trips; verdict vocabulary enforced; one-lesson bound; write path honors approval; NO_ENTRY path writes edits-only entry; curated-copy exclusion.

## 8. Phase H3 — task-level recursion loop

**Goal:** the full Propose–Probe–Compose–Select driver in `ee-agent-harnessing`, invocation-gated like the harness2 binary, budget-asserted.

### I47 — Task adapter seam (added in the second pass)

- The loop's input contract, mirror `spec.py BenchmarkAdapter` (method/substrate/benchmark stay separate axes): one `HarnessTask` type supplying the task prompt (the current session's user prompt, answer-key-free by construction), source files (workspace roots + I20 attachments), `delivered(run)` (final response + files written per the I19 snapshot diff), `deliverable_text` / `deliverable_assets` (bounded render, exact names — never a whole run dir), `domain_of(task)` for library scoping, and the base harness dir. Single-task mode adapts the current session; stream mode (I43) feeds a task list.
- Tests: adapter contract unit tests; delivered() against snapshot diffs. **Implementation note:** `deliverable_text`/`deliverable_assets` are the adapter's bounded render accessors; the CLI channel fills asset content so selection and reflection see rendered artifacts.

### I37 — Evidence export (rollout views)

- `rollout_view.rs` (orchestrator seam): builds an improver-facing view from a session: bounded per-turn records (tool name, ok/error class, duration, result digest), final deliverable pointer, workspace diff summary. Redaction reuses the R12 redaction layer; truncation bounds mirror `context.py _write_rollout`. The CLI host builds the same shape from live session updates because child session logs are not reachable across the remote boundary; `rollout_view::build` remains the seam for in-process session-log hosts.
- **Implementation note:** selector evidence carries the adapter's rendered asset content (bounded), not just names, so the `OPEN THESE` rule has something to open.
- Context-dir builder in the harnessing crate (mirror `context.py build`): `context/task.md`, `context/source/`, `context/harness/`, `context/library/`, `context/rollouts/*.md`, `context/decisions/<label>.md` (composition notes carried into later stages, mirror sequential-mode `decisions`), plus a `mission.md` per stage. Build-time grader-leak guard (D5) asserted at **three points**, mirror `context.py`: the harness dir, the library dir, and the whole context root.
- Second-pass details from `context.py`:
  - the mission describes **only the evidence that landed** (the builder returns what it wrote; the mission never references an absent dir);
  - a `names_only` deliverables mode for oversized artifacts (file names instead of contents);
  - if no trajectory can be rendered for **any** rollout, the builder raises — "the proposer would be adapting on nothing";
  - trajectory and deliverable reader failures are themselves evidence (recorded), while adapter bugs raise as stage errors.
- Tests: hermetic; redaction asserts; truncation bounds; empty-evidence raise; leak asserts at all three points.

### I38 — Per-session harness source

- `HarnessSource { Workspace, Override(PathBuf) }` in `OrchestratorConfig`, defaulting to `Workspace`; the prompt-assembly seam resolves guardrails/tool-guidance/scripts/skills roots from the source. This is the only orchestrator change the probe stage needs: a probe session mounts the candidate directory without touching the workspace.
- **Implementation note:** the production driver mounts a candidate by giving each probe its own copied workspace under the run root and materializing the candidate `.agents` tree there; host sessions expose workspace roots but not `OrchestratorConfig`, so the override seam is exercised by tests and remains available to programmatic sessions. Both paths leave the user's workspace untouched.
- Tests: an override session renders candidate files; workspace sessions are byte-identical to today.

### I39 — Propose

- Driver: obtain k reference executions (existing runtime); build the evidence context dir; spawn `harness_editor_inv` and `harness_editor_dom` sessions (SubagentManager, evidence-checked); each returns an edited copy of the component files (their file-tool writes are scoped to a candidate dir via existing workspace-isolation patterns).
- Improver session isolation, mirror `proposer.py`: the session runs in a temp workspace with the context dir copied in and the edited harness subtree **harvested back** on completion; filesystem scope denies everything outside the session dir (reuse the `prepare_isolation` / `workspace_isolation.rs` patterns); the environment is scrubbed to a keep-list; per-session state roots are isolated. Improver sessions never see other tasks' state.
- **Implementation note:** the host's allowed root is the run root, so the original workspace is never mounted for a child session, and each session writes only its own candidate directory. Host configuration exposes a root list rather than a per-session deny scope; stronger per-session filesystem sandboxing and environment scrubbing remain deployment concerns (see risk table).
- Improver channel config, mirror `ChannelSpec`: the improver model, effort, step budget, and timeout are configurable and **independent of the executor model** (the paper's default improver differs from the executor, and its Figure 8 shows same-model also works — ee exposes both). Model-id form validated with a named error.
- Candidate acceptance: run the I34 validator; one retry with the validator message (and the partially-valid tree persisted back into the retry context); second failure inherits the current harness (recorded as fallback, mirror `recursion.py _run_stage`). An empty returned tree is rejected, not treated as KEEP_BASE.
- Tests: fake transport; validator-retry-inherits path; scoped writes; isolation asserts; empty-tree rejection.

### I40 — Probe

- Re-roll each accepted candidate once via the `ee-agent-remote` headless host under matched model/env config; capture a rollout view per probe; existing timeouts/cancellation apply.
- Tests: hermetic with fake transport; probe isolation (candidate files mounted via I38, workspace untouched).

### I41 — Compose

- `harness_composer` session reads the full pool (references + candidates + probes + diffs); emits one composed tree plus a notes block (`kept|revised|dropped -> <edit> -> <trajectory moment with counts>`), parsed and recorded. Merge / adopt-one / keep-base all valid; an edit from a non-delivering rollout is unsafe unless the rest of the evidence is overwhelming (stated rule).
- Validation (I34) gates the composed tree with the same retry/inherit rule.
- Tests: notes parser; composed-tree validation; unsafe-edit rule enforced by prompt contract + recorded fallback.

### I42 — Selection

- Pool = the k finals (anonymized, seeded shuffle ids — deterministic per task/k/mode, mirror `selection.py assign_ids`); the base run's deliverable is a valid candidate and the selector is told so.
- **k=1 bypasses the selector entirely** (mirror `trivial_selection`): the single final is delivered with the standard record schema, no selector session — the paper's "at k=1 both forms reduce to the basic contrastive step".
- Pre-anonymization filtering, mirror `_render_pool`: non-delivering candidates are dropped **with recorded causes** (`dropped` list); when a task has no file deliverable, the candidate is shown as its own trajectory (`evidence: trajectory` mode) instead of being dropped.
- Candidates are written **untrimmed** with a size header ("COMPLETE — nothing trimmed; navigate with grep/offsets") — truncation applies to improver rollout views, never to selector artifacts; **rendered evidence assets** are copied per candidate, adapter-named files only (a whole-run-dir copy would carry grader output), with an "OPEN THESE" note that rendered behavior beats reading source.
- Selector failure never raises: neutral fallback = a delivered base, else the first final; parse failures and empty pools are recorded errors. Grader-verdict assert runs on the selection context before the session.
- `harness_selector` returns a fenced ```choice block + ≤6-line `<why>` naming the deciding criterion and the runner-up's cost.
- Tests: id determinism; parse; neutral fallback incl. no-deliverable path; base-can-win; k=1 bypass writes the standard record; drop-with-cause recording.

### I43 — Ledger, driver surface, and stream

- Ledger per task: rollout and session counters derived from session-log usage records; `assert_identity` R=4k / P=3k with selection and reflection spend recorded outside the identity (mirror the paper); hard caps and typed budget errors; totals feed `usage_stats` so improvement spend is visible next to task spend.
- Recording discipline, mirror `_roll_many`: concurrent stages run under a bounded width, but the ledger is written after the pool drains, in stable label order — a concurrent run records the same sequence as a serial one. **Dead rollouts are recorded, never raised** (ok/delivered/error per run); if no base rollout delivers, the task short-circuits to a recorded `base-failed` state with modes skipped, not a crash; a mode exception records the traceback tail and continues to the next mode.
- Driver surface: `/harness k=N mode=parallel|sequential` slash command + a manual `ee do agent harness improve` CLI path, always registered and advertised — invocation is the only gate, matching harness2; `k` defaults to 1; dry-run mode prints the plan (references, candidates, compositions) without model calls. Driver options: improver model override (I39 channel), workers, domain/task selection for stream runs.
- Stream mode, mirror `schedule.py`: per-domain ordered tasks (concurrency across domains only, `--workers`); seeded shuffle built from sorted order so each domain's stream is independent of the others; a frozen domain order that is missing a domain is a loud error, not an arbitrary placement. Library roots are scoped per run identity (workspace + mode + k), so measurement runs never cross-contaminate the daily library — a scratch root for experiments, the workspace library for daily use.
- **Implementation note:** `run_stream` is the harnessing-crate driver API for callers that already hold a task list; the CLI and `/harness` surface run one task per invocation. Per-run library scoping is expressed with `HarnessTask::with_library_root` and the CLI `--library-root`.
- Tests: identity asserts; caps; dry-run hermetic; stable-order recording under concurrency; dead-rollout and base-failed paths; stream order determinism.

### I48 — Run state, resume, and improver session cache

- Run state per task (mirror `summary.json` + `_resume`): the driver writes a task summary covering every mode's result and selection; rerunning the same command resumes instead of re-spending.
- Resume validity, mirror `runmark.py`: a completion marker written by the driver only after the run was observed to return (output presence alone is not completion); a **harness fingerprint** (sorted content-addressed walk of the component tree, ignoring byproducts) and a **task-input fingerprint** (prompt + source files). A cached result is reused only when inputs are unchanged, the adopted run dir is intact, the harness tree is present, and its fingerprint matches — otherwise the task re-runs with a logged reason.
- Improver session cache, mirror `proposer.py _call_fingerprint`: improver replies are content-addressed by (label, mission, system, context digest); a resumed sweep reuses cached sessions instead of re-spending, and cached entries requiring a harvested workspace are only reused when the workspace is present. Every call writes an audit entry either way.
- Tests: fingerprint stability; inputs-changed re-run; harness-changed re-run; cache hit/miss; marker-only-after-completion.

## 9. Phase H4 — measurement

### I44 — Harness-edit regression suite

- Per-domain fixtures on `evaluation.rs` + record/replay: deterministic deliverable checks only (file exists, shape, content assertions); a harness edit that regresses a domain fails CI before it can enter the library.
- **Implementation note:** the fixtures live in `ee-agent-harnessing::regression` and run the real driver through a deterministic `FixtureChannel`, because the checks assert driver deliverables rather than editor evaluation outcomes; no LLM judge or network provider is involved.
- Tests: suite green on the base harness; a deliberately bad edit (missing trigger line) is caught.

### I45 — Steps and tool-error metrics

- Extend `usage_stats.rs` with per-task step counts and tool-error-rate rows, plus an optional trend view against harness adoption time (entries carry `seq`), surfacing the paper's E.4 signal (error rate should fall after refinement).
- Tests: deterministic from the session log.

### I46 — Leave-one-out library evaluation

- Offline harnessing-crate mode: re-run a task with its own entry excluded from the completed library; report per-domain deltas (mirror the paper's E.2 protocol). Eval-only; never an online path. The exclusion is by `(task domain, seq)`: `HarnessTask::with_excluded_experience_seq` hides exactly the task's own entry while later entries remain visible, so the delta measures that entry rather than the whole tail of the library.
- Tests: hermetic; exclusion correctness.

### I49 — Deterministic demo driver (added in the second pass)

- Mirror `demo.py`: a small deterministic task adapter + fake channel that exercises the **real** adaptation workflow end to end — reference runs, propose, probe, compose, select, reflect — with zero model credentials. Ships as a harnessing-crate example/test binary and doubles as the onboarding path for `/harness`.
- Tests: the demo run itself is the test (CI runs the full loop hermetically on every change to the driver).

## 10. Sequencing and dependencies

```
H0 ──────────────────────────────────────────────► shippable immediately, no deps
H1: I30, I31, I32, I33 ── independent (file presence/validity is their activation) ──►
H2: I34 ── independent; I35 after H1 files exist; I36 after I34+I35
H3: I47 first (input contract) → I37, I38 ── independent seams; I39→I40→I41→I42 chained;
    I43 (driver+stream) then I48 (resume/cache) last; whole phase requires H1 + H2 + ee-agent-remote (landed)
H4: I44 after H1; I45 anytime after I43's spend visibility exists; I46 after H2; I49 with the driver
```

Invocation-is-the-gate guarantee, matching harness2: the loop never runs unless called; component files are inert until present (guardrails/tool-guidance/scripts) or valid (middleware). H0 changes only prompt text.

## 11. Testing strategy

- Pure modules (validation, library, parsers, ledger): unit matrices mirroring the harness2 rules; property-ish bounds tests for every file-size/count cap.
- Session-level: `FakeAgentTransport` hermetic suites; `EE_TEST_DISABLE_USER_PLUGINS` discipline; no network, no provider keys, no real config tree.
- Cross-crate: `cargo check -p ee-cli` for orchestrator seams; targeted `cargo test --quiet -p <crate> <module>` per the escalation rules; full package only as the manual gate.
- Flaky-test rules apply throughout: improver drivers own every spawned session and await completion; no wall-clock sleeps; seeded shuffles make selection deterministic.

## 12. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Recursion multiplies model spend (R=4k rollouts per task) | no silent path — explicit invocation only, `k` visible in the command; hard caps; ledger visible in `usage_stats`; dry-run mode |
| Improver writes poison the harness (bad edits, injection) | D4 validation before adopt; untrusted plane labels; approval-gated writes; regression suite I44 |
| Grader/rubric leakage into improver context | D5 build-time guard + redaction reuse; tests assert refusal |
| Library bloat and stale lessons | one lesson per task; `NO_ENTRY` valid; leave-one-out eval detects non-transferring entries; future: consolidation pass (paper's own future work) |
| Selection prefers weaker deliverable (paper failure mode) | anonymized pool, base valid, criteria order fixed (H0.3), fallback to reference on parse failure |
| Wrong premise propagates across candidates | INV/DOM scoping keeps candidates independent; compose reads both trajectories, not just verdicts |
| Executor fetches eval data over the network (observed in the wild: a rollout downloaded its task's rubric unbidden) | leak asserts cover context dirs (D5); for benchmark-style runs the operator must apply egress restrictions — the loop itself does not firewall |
| Stale resume reuses results from a changed task or edited harness | I48 fingerprints (inputs + harness) and completion markers make reuse conditional and logged |
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
| `core/schedule.py` (stream order, resume, library retain) | driver + stream mode + run state (I43, I48) |
| `core/runmark.py` (completion marker, harness + inputs fingerprints) | run state and resume validity (I48) |
| `core/proposer.py` cache (`_call_fingerprint`) | improver session cache (I48) |
| `core/spec.py` (`BenchmarkAdapter`, `ChannelSpec`) | task adapter seam + improver channel (I47, I39) |
| `demo.py` (deterministic full-loop demo) | demo driver (I49) |
