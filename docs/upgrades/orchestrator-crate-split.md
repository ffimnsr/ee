# Orchestrator crate split for build speed

Status: proposed. Plan only; no crate has been split yet.

## Problem

`ee-agent-orchestrator` is the slowest Rust unit in a cold workspace build:

| metric | value |
| --- | --- |
| cold unit time (`CARGO_BUILD_JOBS=2`, `opt-level = 0`, `debug = 0`) | 98.4 s |
| of which rustc frontend | 58.6 s |
| of which codegen | 39.8 s |
| top-level modules | 118 |
| LOC in `src/*.rs` | 51.6k |
| LOC including subdirectories and inline tests | 86.1k |
| intra-crate module edges (`crate::<mod>` references) | 713 |
| strongly connected components | 30 |
| largest SCC | **88 modules, 69.2k LOC** |

Consumers: `ee-agent-host`, `ee-agent-opencode`, `ee-agent-openrouter`,
`ee-chat-completions`, `ee-cli`, plus the crate's own `test-utils` dev-dep.
They use 66 distinct `ee_agent_orchestrator::` symbols; the crate re-exports
86 `pub use` blocks.

A crate split only helps if the new crates form a DAG. Today 88 of 118 modules
sit in one cycle, so the split work is **the de-cycling work**, not the crate
move. Frontend time is serial per crate, so N independent crates can compile in
parallel; codegen already parallelizes inside a crate through codegen units.

## Target layout

Seven crates. `core` is the only new lower layer; the facade keeps every
existing `pub use` so dependents do not change.

| crate | representative modules | ~LOC | depends on |
| --- | --- | --- | --- |
| `ee-agent-orchestrator-core` | `model`, `transcript_normalize`, `tools` (types only), `tool_dependencies`, `tool_schemas`, `tool_output`, `config`, `error`, `events`, `trust`, `sensitive_data`, `prompt_injection`, `workspace_scope`, `destructive_policy`, `policy` (pure), `budget`, `model_metadata`, `model_options`, `model_registry`, `streaming`, `structured_output`, `rate_limit`, `retries` | ~13k | `ee-agent-protocol`, serde |
| `ee-agent-orchestrator-runtime` | `runtime`, `loop_engine`, `loop_control`, `loop_hooks`, `strategy`, `prompt`, `prompt_attachments`, `mcp`, `context_pack`, `context_budget`, `context_planner`, `compaction`, `compaction_checkpoint`, `memory_compaction`, `tool_cache`, `parallel_tools`, `repair`, `repair_context`, `recovery`, `session_context`, `background_jobs` | ~20k | core, `ee-agent-acp-server` |
| `ee-agent-orchestrator-state` | `checkpoint`, `checkpoint_budget`, `checkpoint_store`, `session_log`, `session_log_sink`, `session_store`, `trace`, `observability`, `usage`, `usage_stats`, `opaque_state`, `transcript_log`, `memory`, `semantic_memory`, `user_memories`, `user_memory_write`, `workspace_snapshot`, `write_transaction`, `private_fs` | ~11k | core |
| `ee-agent-orchestrator-planning` | `tasks`, `plan_compiler`, `progress_scoring`, `progress`, `milestones`, `issue_integration`, `todo_plan`, `validation`, `command_intelligence`, `reflection`, `review_context`, `critique`, `completion`, `stop_gate`, `stuck`, `final_response`, `evaluation`, `replay` | ~12k | core, runtime |
| `ee-agent-orchestrator-subagents` | `subagents`, `subagent_handoff`, `subagent_roles`, `subagent_transcripts`, `subagent_verifier`, `child_registry`, `fanout_fanin`, `write_conflicts`, `delegation_quality`, `user_agents` | ~10k | core, runtime, planning |
| `ee-agent-orchestrator-plugins` | `plugin_runtime`, `approval_classifier`, `permission_rules` | ~7k | core |
| `ee-agent-orchestrator` (facade) | `provider_adapter`, `rubber_duck*`, `test_support`, `lib` re-exports | ~10k | all of the above |

Rules for modules not listed:

- Pure data types and constants referenced by two or more layers move to `core`.
- Anything that performs workspace I/O, subprocess work, or persistence moves
  to `state` or `runtime`, never `core`.
- `provider_adapter` stays last: it is the ACP `AgentProvider` glue and today
  cycles with `plugin_runtime` (`plugin_runtime <-> provider_adapter`).
- `test-utils` becomes a forwarded feature: facade enables the components'
  `test-utils`; each crate exposes only its own fixtures.

Expected crate DAG:

```
core
├── runtime
│   ├── planning
│   │   └── subagents
│   └── subagents
├── state
└── plugins
facade = core + runtime + state + planning + subagents + plugins
```

## Phase 0 — baseline and guardrail (1 day)

- Record `cargo build -p ee-agent-orchestrator --timings` and the full
  `cargo build --timings` numbers; keep the HTML reports.
- Add `scripts/check-orchestrator-layering.py`: builds the module graph from
  `crate::` references, computes SCCs, and fails when a forbidden edge appears
  or the largest SCC grows. This replaces "don't add cycles" with a checkable
  rule and is the exit gate for Phase 1.
- `build.jobs = 3` is the local default (4-core host, one core headroom).
  Measure `4` again only if memory stays comfortable under
  `scripts/run-tests-memguard.sh`.

Exit: reproducible baseline and a layering check that currently reports the
88-module SCC.

## Phase 1 — de-cycle inside the current crate (main work)

Do this before creating any crate. Target: largest SCC under ~15k LOC and the
crate's own graph acyclic at the intended crate boundaries.

1. Introduce `src/contracts/` (or `src/core/`) as the only module allowed to be
   referenced from everywhere: model/transcript types, tool call/definition
   types, side-effect subclasses, workspace scopes, trust labels, config
   structs, event envelopes, error kinds.
2. Break the measured cycles in this order (reference counts are
   `a -> b` / `b -> a`):

   | cycle | refs | fix |
   | --- | --- | --- |
   | `tools <-> destructive_policy` | 9 / 3 | move `SideEffectSubclass` into contracts; policy consumes tool metadata by reference |
   | `model <-> tools` | 8 / 4 | move `ToolCall`, `ToolDefinition`, result envelopes into contracts |
   | `tools <-> workspace_scope` | 5 / 3 | move `WriteScope`/path-scope types into contracts |
   | `model <-> transcript_normalize` | 5 / 4 | fold normalization into the model module or move `Transcript` into contracts |
   | `events <-> strategy` | 4 / 1 | `strategy` owns its event type; `events` keeps the generic envelope |
   | `memory <-> trust` | 4 / 1 | `trust` is a contracts type; `memory` consumes it |
   | `final_response <-> progress` | 3 / 2 | both belong to planning; merge or move the shared DTO into planning-internal module |
   | `loop_engine <-> parallel_tools` | 3 / 1 | `parallel_tools` takes a tool-executor trait defined in core |
   | `config <-> tools` | 2 / 6 | `config` keeps plain values; tool limits live in `ToolConfig` inside contracts |
   | `config <-> prompt` | 2 / 1 | prompt limits move to `PromptConfig` in contracts |
   | `plugin_runtime <-> provider_adapter` | 2 / 9 | provider adapter consumes a plugin-host trait from the plugins crate |
   | `session_log <-> session_log_sink` | 2 / 3 | move the sink trait into the state crate's internal `types` module |
   | `runtime <-> subagents` | 2 / 1 | runtime consumes a `SubagentExecutor` trait; the subagent manager implements it |
   | `checkpoint <-> runtime` | 1 / 15 | checkpoint stores DTOs only; runtime serializes/restores |

3. Re-run the layering check after each cycle set; keep the crate compiling and
   tests green after every step.
4. Freeze the target edge matrix: core has no upward imports; runtime imports
   core; state imports core; planning imports core + runtime; subagents imports
   core + runtime + planning; plugins imports core; facade imports all.

Exit: local SCCs only, layering script green, full crate test suite green.

## Phase 2 — extract `core`

- Create the crate, move modules, add `pub use ee_agent_orchestrator_core::*;`
  in the existing crate. Visibility shim keeps all 66 external symbols valid.
- Add a `core` feature only if a module is genuinely optional; otherwise plain
  dependency.
- Gate: dependents compile unchanged; `cargo tree -d` clean.

## Phase 3 — extract `state` and `plugins`

- Leaves first: both depend on `core` only, so they compile in parallel with
  runtime.
- Move `test-utils` fixtures with their modules; forward the feature.
- Gate: checkpoint/session-log/plugin tests and dependents' plugin tests green.

## Phase 4 — extract `subagents`, then `planning`

- `subagents` depends on `runtime` + `planning`, so extract planning first if
  the edge matrix proves it. Otherwise split `runtime` first and keep both as
  facade-only for one phase.
- Gate: delegation, fan-out, verification, and plan-compiler suites green.

## Phase 5 — extract `runtime`, leave the facade

- Facade keeps `provider_adapter`, rubber-duck/evaluation glue, `test_support`,
  and the re-export surface.
- Gate: `ee-agent-host` and all provider crates compile; ACP smoke tests pass.

## Phase 6 — cleanup

- Remove the `--allow(dead_code)` shims that existed only for the monolithic
  lib; re-export only the public surface in the facade.
- Update `docs/upgrades`, `AGENTS.md` build section, and the workspace member
  list.
- Publish path deps and musl/macOS release validation (CI release.yml).

## Expected effect

| scenario | today | after split (estimate) |
| --- | --- | --- |
| cold unit | 98.4 s (58.6 frontend + 39.8 codegen) | ~60-75 s at the default `jobs = 3`; lower at `jobs = 4` |
| edit `plugin_runtime` | recompiles the 70k-LOC crate | recompiles plugins crate + dependents |
| edit a `core` type | recompiles everything | recompiles all layers (worst case unchanged) |

Caveats:

- Total CPU work does not drop; cross-crate boundaries add rlib/metadata
  overhead and can duplicate some monomorphization. Wall-time gain comes from
  parallel frontends across crates.
- On this 4-core host the default `jobs = 3` gives roughly a 3x frontend
  ceiling, limited by the dependency DAG. Measure `jobs = 4` only if memory
  stays comfortable under `scripts/run-tests-memguard.sh`.
- Edits to `core` or `runtime` still cascade; the split mainly isolates leaf
  subsystems (plugins, state, planning, subagents) and shortens cold builds.

## Risks and rollback

- **Visibility churn:** moving items to `core` forces `pub(crate)` items to
  become `pub`. Keep `core` API small and byte-bounded by DTOs; do not move
  behavior just to break a cycle.
- **Feature unification:** `test-utils` must not leak test fixtures into
  production builds; use per-crate optional features and dev-dependencies.
- **Incremental churn:** an unstable `core` API invalidates every dependent on
  each change; land Phase 1 cycle fixes and the core type freeze before moving
  behavior.
- **Publish/lint:** workspace path deps need versions for release; clippy and
  `cargo doc` must run per crate.
- Rollback is per phase: each extraction keeps the facade re-exports, so any
  phase can be reverted without touching dependents.
