# PLAN-HARNESS-PARITY — Close the ee ↔ Harness2 gaps

Status: **implemented (2026-11-24)** · Scope: `ee-agent-harnessing`, existing orchestrator seams, and `ee-cli` agent-mode surfaces. Design parity for the agent-mode loop; benchmark adapters and LLM judges remain out of scope.

Sources:

- Previous adaptation: [`docs/PLAN-HARNESS.md`](PLAN-HARNESS.md).
- Vendored upstream: `harness-example/harness2/harness2/` (`core/library.py`, `core/selection.py`, `core/context.py`, `core/schedule.py`, `core/recursion.py`).
- Checklist: [`ISSUES.md`](../ISSUES.md) → Part 11.2.

## 0. Path vocabulary

- `<workspace>` — the project root passed as `--workspace` (default `.`). Active harness files live under `<workspace>/.agents/`.
- `<run-root>` — per-run harness state. Default: `<state-dir>/ee/harness-runs/<task-id>` (`harness_cmd.rs::default_run_root`), typically `~/.local/state/ee/harness-runs/<task-id>` on Linux. Override with `--run-root`.
- `<stage>` — one improver stage directory: `step-0-inv`, `step-0-dom`, `step-0-compose`, `reflect`, or `select`.
- `<run-root>/context/<stage>/` — one stage's evidence directory. D12's `library/INDEX.md` and library entries live here.
- Sweep per-task root — `<run-root>/tasks/<domain>/<task>/`; each task gets its own `context/`, `harness/`, `adopted/`, and `run-summary.json` beneath that prefix.
- `<library-root>` — persistent experience library. Single task defaults to `<workspace>/.agents/experience`; sweep defaults to `<run-root>/library/<mode>-k<k>`; override with `--library-root`.

`<run-root>` contains, among other things: `adopted/.agents`, `harness/base`, `harness/step-*`, `candidates/`, `context/`, `runs/`, `improver-cache/`, `agent-audit.log`, `agent-sessions/`, and `run-summary.json` (plus `stream-order.json` for sweeps). Deleting it discards resume state, not workspace files.

## 1. Verified gaps

| ID | Current ee behavior | Harness2 target | User impact |
|---|---|---|---|
| G1 | `/harness` runs the child with stdin `null`; reflection is executed but its write is declined, so the library never updates. Direct CLI also requires a separate `--yes`. | Reflection writes the library as part of the invocation; opt-out flags are explicit. | `/harness` learns nothing across tasks. |
| G2 | Chosen tree is written only to `<run-root>/adopted/.agents`; no apply/adopt command. | Selected harness is retained in the run/library, but the benchmark workspace is not mutated either. | ee agent mode has no supported way to activate the chosen harness. |
| G3 | Improver context gets only `library/cues.md`; no full prior harnesses. | `context/library/` contains `INDEX.md`, `EXPERIENCE.md`, and the full prior `harness/` trees. | Next-task improvers cannot inspect earlier adopted harnesses. |
| G4 | `HARNESS_DIR` is written by `save_entry` but has no production reader. | Library entries are browsed as full harness trees. | The archived harness is dead data; only cue/edit summaries transfer. |
| G5 | `run_stream` exists and is public, but only tests call it. No CLI/agent-mode task list. | `schedule.py` runs per-domain ordered streams with cross-domain concurrency. | No one-command next-task learning; users must run `/harness` repeatedly. |
| G6 | Single-task `/harness` reads the workspace experience library; `run_stream` has no per-task cutoff or own-entry exclusion. I43 claims run-identity scoping and stream wiring that the CLI does not expose. | Streaming retention: a task sees only earlier same-domain entries; libraries are scoped per run identity. | Resume/rerun can leak a task's own entry or future entries into its context. |
| G7 | Reflection outcome is not recorded; there is no opt-out and no status on the run. | Reflection is a normal post-task stage with `written/declined/disabled` recorded. | Automation and agent-mode runs cannot audit what was persisted. |
| G8 | Experience module docs claim a user-config mirror; `save_entry` only writes the workspace entry. | Not a Harness2 requirement; upstream keeps libraries under `runs/`. | Documentation overclaims; do not build a trusted-plane mirror without a separate decision. |

## 2. Fixed decisions

**D11 — Invocation consents to library writes; active adoption is the only separate approval.** Experience-library writes and active-harness writes are different operations, but they do not both need routine prompts.

- Running `harness improve` or `/harness` is consent to write the experience entry under `.agents/experience/` when reflection succeeds. No extra `--yes` and no `/harness yes`.
- Opt out with `--no-reflect` (skip the reflector stage) or `--reflection=decline` (run it but write nothing). Record the outcome in the run summary.
- Active `.agents` component writes happen only through the separate `harness adopt` or `/adopt` command, with diff, backup, and confirmation. That is one deliberate adoption decision, not a second routine approval.
- `harness improve` never applies, copies, or mutates active `.agents/guardrails.md`, `.agents/tool_guidance.md`, `.agents/scripts/`, `.agents/skills/`, `.agents/agents/`, `.agents/memories/`, or `.agents/middleware.toml`.
- No config-wide silent activation. The library write is automatic because the entry is data for future improvers; active instructions still require an explicit adopt.

**D12 — Full library context is bounded, not cue-only.** Keep the cue catalog as the quick index, but improvers get full prior entries.

Plain meaning: the next task's improver must be able to open the previous task's actual `guardrails.md`, `tool_guidance.md`, scripts, skills, and other harness files. Today it only sees a cue line.

```text
context/library/
  INDEX.md                              quick cue catalog
  rust-crates/
    1-fix-parser/
      EXPERIENCE.md                     the lesson and edit verdicts
      harness/
        guardrails.md
        tool_guidance.md
        middleware.toml
        scripts/...
        skills/...
        agents/...
```

- Copy only prior same-domain entries. The `harness/` tree is read-only evidence for the improver; it is not active `.agents` configuration.
- Path anchor: `context/` means `<run-root>/context/<stage>/`. D12 writes `<run-root>/context/<stage>/library/INDEX.md`, for example `context/step-0-inv/library/INDEX.md`, `context/step-0-compose/library/INDEX.md`, or `context/reflect/library/INDEX.md`.
- D12 does **not** write the persistent `<workspace>/.agents/experience/<domain>/INDEX.md`; that index is already rebuilt by `save_entry`/`rebuild_index`. The context index is a bounded, stage-local view of only the entries that were copied into that stage.
- `INDEX.md` is generated from the bounded cue catalog; full entries sit beside it.
- Caps: at most 16 entries, 1 MiB total, 64 KiB per file. If an entry cannot fit or its harness tree is malformed, skip the whole entry and note it in `INDEX.md`; never copy a partial tree.
- Redact every copied file with the existing redactor and run the grader/rubric/score refusal on every text. Path escapes and symlinks fail closed.
- The archived `HARNESS_DIR` becomes the source of truth for the full tree; `ExperienceEntry.path.parent()` is the entry directory.

**D13 — Active adoption is a managed-surface transaction.** `ee do agent harness adopt` validates, diffs, backs up, and atomically replaces only the managed harness surface.

- Default managed surface, all under `.agents/`: `guardrails.md`, `tool_guidance.md`, `middleware.toml`, `scripts/`, `skills/`, `agents/`.
- Complete-tree semantics: managed files absent from the adopted tree are removed; `--dry-run` shows every addition, removal, and modification.
- `.agents/experience/` is always preserved. `.agents/memories/` is preserved by default; `--include-memories` is required to replace it, with backup.
- Out of scope and byte-identical after adoption: top-level `<workspace>/skills/`, top-level `<workspace>/scripts/`, and `$XDG_CONFIG_HOME/ee/skills/`. Active-discovery also reads those locations, but they are not part of the `.agents` harness surface.
- Remove stale files only inside the managed surface. Unknown files and non-managed directories are preserved.
- Validate the candidate with the I34 validator before writing. `--dry-run` prints the diff and writes nothing.
- Backup the previous managed surface under `workspace/.agents/.backup/<timestamp>/` with a manifest.
- Interactive default asks for confirmation; non-interactive without `--yes` declines.
- This is an ee apply seam, not an upstream Harness2 behavior: upstream materialises a harness into execution workspaces and never writes the developer project. Loop parity is H5/H6/H8; H7 exists because ee agent mode runs in a live workspace.

**D14 — Stream is manifest-driven and explicit.** Add `ee do agent harness sweep` rather than making `/harness` silently fan out.

- The manifest is a JSON list of task specs: `id`, `prompt`, `domain`, optional `workspace`, `source_roots`, `attachments`. The library root is an invocation-level option, never per task.
- `run_stream` remains the scheduler; the CLI becomes a real caller.
- Per-domain task order is frozen in `run-root/stream-order.json`; tasks within a domain run sequentially, different domains run concurrently up to `--workers`.
- `/harness` remains a single-task command. Its output points at the adopted path and the experience entry, not at a hidden sweep.
- Minimal manifest:

```json
{
  "tasks": [
    {
      "id": "cli-rust-crates-1a2b3c",
      "prompt": "Fix the failing parser test.",
      "domain": "rust-crates",
      "workspace": "."
    }
  ]
}
```

**D15 — Library identity and retention.** Read and write the same library root, and expose only earlier same-domain entries.

- Single-task default: workspace `.agents/experience` (current behavior).
- Sweep default: `<run-root>/library/<mode>-k<k>` to avoid contaminating the daily library; `--library-root` overrides both modes.
- Before a task, compute `library_before_seq` from the registry and exclude that task's own prior entries. Do not expose entries from later tasks.
- Single-task reruns also exclude the task's own prior entries unless `--include-own-experience` is explicitly set.
- Keep leave-one-out evaluation offline; it continues to use `with_excluded_experience_seq`.

**D16 — Approval is recorded, not inferred.** Extend the run record with:

- `library_root`
- `reflection_write`: `written | declined | failed | disabled`
- `reflection_source`: `invocation | flag | none`
- `adopt_approval`: `prompt | flag | declined | none` on the adopt operation
- `reflected_path` (already present)
- `adopted_path` (already present)

No prompt text or lesson content goes in telemetry; the fields are status and path only.

## 3. Phases and items

### Phase H5 — Full library read parity

- [ ] **I50 — Experience entry harness accessor and bounded loader.**
  - **Problem:** the experience entry stores `harness/`, but nothing loads it; `HARNESS_DIR` has only the writer and one test reference.
  - **Fix:** add a bounded `library_context` module (new file under `crates/ee-agent-harnessing/src/`) that resolves `entry.path.parent().join(HARNESS_DIR)`, loads it with `HarnessTree::load_components`, validates bounds, and returns `Option<HarnessTree>`. Missing or malformed harness trees are skipped with a warning; symlinks, UTF-8 errors, oversized files, and path escapes fail closed for that entry.
  - **Acceptance:** round-trip test writes an entry with `save_entry` and reloads the full tree; malformed/missing harness entries are skipped without failing the context build; no path outside the entry directory can be read.

- [ ] **I51 — Full library evidence in the improver context.**
  - **Problem:** `StageEvidence.library_cues` is only cue lines; `context/library/cues.md` is the only library evidence.
  - **Fix:** add `LibraryEvidence { cues, entries }` to `StageEvidence`. Load it once per task in `run_task` (same place as `library_cues`) and pass it to INV, DOM, composer, and reflector stages. Extend `context::build` to write:
    - `context/library/INDEX.md` (bounded cue catalog),
    - `context/library/<domain>/<seq>-<slug>/EXPERIENCE.md`,
    - `context/library/<domain>/<seq>-<slug>/harness/<rel>`.
    Use the D12 caps, redaction, and grader guards. Keep `library/cues.md` for compatibility only if existing tests need it; otherwise remove it from the mission and tests.
  - **Acceptance:** a second task's context contains the first task's full `harness/` tree and `EXPERIENCE.md`; the mission lists `library/` only when files landed; caps truncate deterministically; malformed entries are skipped; grader material is refused at the library, harness, and context-root checks.

### Phase H6 — Approval and persistence parity

- [ ] **I52 — Invocation-consented reflection write and opt-out.**
  - **Problem:** `/harness` cannot approve reflection because its child stdin is `null`; the current `--yes` gate makes every run need a second approval; approval provenance is unrecorded.
  - **Fix:** make the experience write part of the invocation:
    - CLI: default `--reflection=accept`; `--reflection=decline` runs the reflector but writes nothing; `--no-reflect` disables the reflector stage entirely. Remove `--yes` from `improve` (it belongs to `adopt` only).
    - `/harness`: no `yes` token. Pass `--reflection=accept` by default; accept an optional `no-reflect` token for a dry/audit run.
    - `CliWriter` records `reflection_write` and `reflection_source` in the run summary.
    - Invocation remains the consent boundary, matching D6 and Harness2's automatic library update.
  - **Acceptance:** `/harness` writes the entry without an extra prompt; `--no-reflect` skips the stage; `--reflection=decline` runs reflection but writes nothing; `--reflection=accept` is the default; every active `.agents` component stays byte-identical in all `improve` paths; tests cover all outcomes.

- [ ] **I53 — Library root resolution shared by reader and writer.**
  - **Problem:** `HarnessTask.library_root` and the CLI writer's `library_root` are separate values; a mismatch can cause a write that the next task cannot read. The current CLI also ignores the I43 run-identity scoping claim.
  - **Fix:** resolve one `LibraryLocation` per invocation: single task defaults to the workspace library, sweep defaults to `<run-root>/library/<mode>-k<k>`, and `--library-root` overrides. Pass the resolved path into both `HarnessTask` and the `ReflectionWriter`. Record it in the run summary. Add a test that asserts the writer path and the next task's reader path are identical.
  - **Acceptance:** single-task and sweep roots behave as specified; scratch roots never contaminate the workspace library; read/write consistency is asserted; a mismatched root is impossible by construction.

### Phase H7 — Active-harness adoption

- [ ] **I54 — `ee do agent harness adopt`.**
  - **Problem:** the chosen tree stops at `<run-root>/adopted`; users must copy it manually and can easily copy stale or invalid files.
  - **Fix:** add a dedicated command, kept out of `harness_cmd.rs` (currently 987 LOC) in a new `harness_adopt_cmd.rs`:
    - source: `--run-root <dir>` (reads `<run-root>/adopted/.agents` with `HarnessTree::load_components`) or `--experience <domain>/<seq>` (reads `entry.path.parent()/harness`);
    - target: `--workspace <dir>` (default `.`);
    - `--dry-run` diff, `--yes`, `--include-memories`;
    - validate with I34; reject invalid trees;
    - back up the current managed surface;
    - apply only D13 managed paths, remove stale managed files, preserve `.agents/experience` and other unknown files;
    - print `added/removed/modified` and the backup path.
  - **Acceptance:** dry-run writes nothing and shows removals; invalid trees are rejected; backup contains the previous managed files; stale `.agents/skills`/`.agents/scripts` entries are removed; `.agents/experience` and `.agents/memories` survive; top-level `skills/`, top-level `scripts/`, and `$XDG_CONFIG_HOME/ee/skills` stay byte-identical; symlink/path-escape attempts fail; non-TTY without `--yes` declines; a temp-workspace test proves the applied tree equals the adopted managed surface.

- [ ] **I58 — Agent-mode `/adopt` surface (explicit ACP confirmation).**
  - **Problem:** CLI adoption is outside the agent session; an agent-mode user has no approval UI and `HarnessLedgerReport.run_root` is currently recorded as `None`.
  - **Fix:** include `run_root` in the child harness report/session log; add a `/adopt [--include-memories]` slash command that resolves the last run root, runs `ee do agent harness adopt --dry-run` first, shows the diff, asks through the existing `ClientBridge` elicitation pattern, and only on Accept invokes the apply with `--yes`. Cancel/decline writes nothing. Fixed argv only; never interpolate model text into shell arguments.
  - **Acceptance:** dry-run diff is shown before confirmation; Accept applies; Cancel/decline leaves active `.agents` byte-identical; missing run root gives a clear error; the command remains available without a model API key.

### Phase H8 — Task stream parity

- [ ] **I55 — Task manifest and `ee do agent harness sweep`.**
  - **Problem:** `run_stream` has no production caller, so there is no streaming next-task loop in agent mode.
  - **Fix:** add a `Sweep(Box<HarnessSweepArgs>)` subcommand with `--tasks <JSON>`, `--k`, `--mode`, `--workers`, `--run-root`, `--library-root`, `--no-reflect`, `--dry-run`. Parse and validate the manifest into `HarnessTask` values; call `run_stream`; print a bounded per-task summary and return nonzero if any task failed. Keep the implementation in a new `harness_sweep_cmd.rs`; reuse `run_stream` rather than duplicating scheduling.
  - **Acceptance:** a two-domain manifest runs domain streams concurrently and tasks within a domain sequentially; frozen order is reused; a bad manifest fails before any model call; a failed task is reported and does not abort completed domains; no network or provider keys in hermetic tests.

- [ ] **I56 — Stream retention, own-entry exclusion, and library handoff.**
  - **Problem:** `run_stream` does not set `library_before_seq` or exclude the current task's prior entry, so reruns can leak future/own lessons.
  - **Fix:** before each task in `run_stream`, resolve the registry cutoff and set `library_before_seq`; add an explicit excluded-task set to `HarnessTask`; filter library evidence and cues by both. Ensure a completed task's library write is visible to the next task in the same domain and not to a concurrent different domain. On a resumed task, reconcile the library from the saved reflection path or skip with the existing completed entry.
  - **Acceptance:** task B sees task A's full library entry; task A rerun does not see its own old entry; future entries are invisible; concurrent domains do not share cues; resume does not duplicate or lose entries.

### Phase H9 — Verification and documentation

- [ ] **I57 — Parity regression suite and docs.**
  - **Problem:** these seams cross crate, CLI, slash-command, filesystem, and approval boundaries; existing tests cover pieces, not the parity contract.
  - **Fix:** add hermetic `FakeAgentTransport` tests for: full library context, approval outcomes, adopt safety, stream order/retention, and end-to-end task A → task B → task C learning. Add `harness adopt`/`sweep` CLI tests. Update `docs/PLAN-HARNESS.md` status and the Part 11 checkboxes. Reassert `HARNESS_DIR` has a production reader and that active discovery still reads only root `.agents`. Resolve G8: either remove the `user_experience` module's user-mirror claim or add an explicit `--scope user` write path; default remains workspace-only and no trusted-plane mirror is implicit.
  - **Acceptance:** the full H5–H8 contract is exercised without network; the user-mirror claim matches code; `cargo fmt --all -- --check`; clippy on touched crates; targeted tests via `scripts/run-ee-cli-touched-tests.sh`; `scripts/run-affected-tests.sh` for dependents; file-size check keeps every modified code file under 1,000 LOC.

## 4. Acceptance matrix

| Contract | Test |
|---|---|
| Reflection write | `/harness` writes by default; `--no-reflect` skips; `--reflection=decline` writes nothing; active `.agents` unchanged |
| No accidental active write | After any `improve` invocation, active `.agents` component hashes are unchanged |
| Full library context | Second task's `context/library/<domain>/<seq>/harness/` matches the first entry's tree |
| Library bounds | 16-entry/1 MiB/64 KiB caps, deterministic truncation, malformed skip |
| Apply transaction | dry-run no-op; validation reject; backup; stale managed removal; experience/memories preserved; symlink rejected |
| Workspace skill scope | `.agents/skills` follows the adopted tree; top-level `skills/`, top-level `scripts/`, and user-config skills stay byte-identical |
| Agent adopt | `/adopt` shows diff; Accept applies; Cancel/decline writes nothing; run root from session log |
| Stream order | frozen JSON order; same-domain sequential; cross-domain concurrent |
| Retention | own and future entries excluded; prior same-domain entries visible |
| Library identity | reader root == writer root for single-task and sweep |
| Approval provenance | run summary records source/status/path, never lesson text |
| File hygiene | all touched files < 1,000 LOC; new CLI code lives in separate modules |

## 5. Sequencing and gates

1. H5 (I50 → I51) — makes the archived tree useful.
2. H6 (I52 → I53) — makes reflection persistent and consistent.
3. H7 (I54 → I58) — gives users an explicit active-apply path and an in-session approval UI; independent of H8.
4. H8 (I55 → I56) — wires the existing scheduler and retention rules.
5. H9 (I57) — parity regression and docs.

Gates after each phase: `cargo fmt --all -- --check`; clippy on touched crates; targeted `cargo test --quiet` per the escalation rules; hermetic fake-transport tests; mem-guarded run only when a crate-wide change requires it.

## 6. File map

| Area | File |
|---|---|
| Library reader/context | new `crates/ee-agent-harnessing/src/library_context.rs`; extend `context.rs`, `driver.rs`, `driver/stages.rs` |
| Approval | `crates/ee-cli/src/harness_cmd.rs` (minimal wiring), new `harness_adopt_cmd.rs`, `crates/ee-agent-orchestrator/src/provider_adapter/harness_command.rs` |
| Library scope | `crates/ee-agent-harnessing/src/task.rs`, `driver/stream.rs`, `crates/ee-cli/src/harness_cmd.rs` |
| Active adoption | new `crates/ee-cli/src/harness_adopt_cmd.rs`; reuse `harness_validation` and `HarnessTree`; new `provider_adapter/adopt_command.rs`; extend `harness_command.rs`/`session_log.rs` run-root reporting |
| Sweep | new `crates/ee-cli/src/harness_sweep_cmd.rs`; reuse `driver/stream.rs` |
| Experience reader | `crates/ee-agent-orchestrator/src/user_experience.rs` (accessor only; keep file under 1K) |
| Docs/checklist | this file, `docs/PLAN-HARNESS.md`, `ISSUES.md` Part 11.2 |

`harness_cmd.rs` is already 987 lines: do not add new feature logic there. Split or add sibling modules before wiring I52/I54/I55. `user_experience.rs` is 824 lines: add only the minimal accessor there; put the tree copy and bounds in `library_context.rs`.

## 7. Definition of done

- `/harness` persists experience entries by default; `--no-reflect`/`--reflection=decline` opt out; active `.agents` still requires `adopt`.
- A subsequent task's improver context contains the prior entry's full `EXPERIENCE.md` and `harness/` tree, bounded and guarded.
- `ee do agent harness adopt` applies a validated tree to the managed `.agents` surface with diff, backup, explicit approval, and no experience/memory loss.
- `/adopt` provides the same operation inside agent mode with ACP confirmation; no silent apply.
- `ee do agent harness sweep` runs a manifest with frozen domain order and correct library retention.
- `HARNESS_DIR` has a production reader; active component discovery still reads only root `.agents`.
- I57's regression suite passes, and all touched code files remain under 1,000 LOC.

## 8. Implementation result (2026-11-24)

H5–H9 landed with these concrete shapes:

- **H5** — `library_context` is the production `HARNESS_DIR` reader; `run_task` loads one bounded `LibraryEvidence` and every INV/DOM/composer/reflector context gets `library/INDEX.md` plus full `EXPERIENCE.md` and `harness/` trees. `cues.md` was replaced by the stage-local index.
- **H6** — `--reflection=accept` is the CLI default and invocation is consent; `--reflection=decline` runs the reflector without a writer; `--no-reflect` skips the stage. `/harness` passes `--reflection accept` and an optional `no-reflect`. The run summary records `library_root`, `reflection_write`, `reflection_source`, and `reflected_path`. `/reflect` now accepts `--library-root DIR` and resolves the same explicit library root; its default remains `<workspace>/.agents/experience`.
- **H7** — `harness adopt` validates, diffs, backs up, and applies the managed `.agents` roots with complete-tree semantics. `/adopt` resolves the last run root from the session log, shows the dry-run diff, and applies only after an Accept elicitation.
- **H8** — `harness sweep --tasks` validates the manifest before host startup, defaults the library to `<run-root>/library/<mode>-k<k>`, and reports base-failed or no-deliverable tasks as failures. `run_stream` freezes the per-domain cutoff and excludes each task's own prior entries.
- **H9** — `tests/parity.rs` covers approval outcomes, full-library context, forward-only A → B → C learning, and the `HARNESS_DIR` production reader without network access. The `/adopt` handler is exercised end to end with a fake `ee` executable and a scripted Accept/Decline elicitation.

Deviations from the design text: the library root is the experience directory
itself (`<workspace>/.agents/experience`) rather than a workspace-shaped root;
the stage-local index replaces `library/cues.md`; adoption stages and renames
one managed root at a time instead of swapping the whole `.agents` directory;
and `/adopt` reads the run root from the harness ledger report recorded by the
child process.
