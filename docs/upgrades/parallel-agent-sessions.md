# Parallel agent sessions

EE supports concurrent top-level agent sessions with bounded provider work and host-owned write coordination.

## Supported matrix

| Sessions | Connection model | Prompt behavior |
| --- | --- | --- |
| Different configured agent ids | Isolated `AgentConnection` instances | Prompts run concurrently, subject to each connection limit. |
| Multiple sessions using one configured agent id | Shared `AgentConnection` and driver | Prompts run concurrently up to `agents.max_concurrent_prompts`; excess sessions wait FIFO. |
| Multiple prompts in one session | One session-local turn slot | Second prompt is rejected while current turn is active. TUI follow-ups use session-local queue. |

Default `agents.max_concurrent_prompts` is `4`. Valid values are `1` through `32`:

```toml
[agents]
enabled = true
default_agent = "assistant"
max_concurrent_prompts = 4
```

Agents pane shows session state independently: `queued`, `running`, `awaiting permission`, `awaiting elicitation`, `cancelling`, `paused`, or `failed`. Footer `conn:active/limit +queued` reports shared-connection saturation; active sessions are not described as blocked.

Lifecycle create/load/resume work is separately bounded to four operations. Independent failures remain attached to originating agent and session.

## Cancellation and shutdown

Cancellation targets exact session and request. Cancelling queued prompt removes it before provider dispatch. Connection shutdown stops intake, resolves queued and active waiters, closes session interactions, and releases host-owned write leases.

## Concurrent writes

Top-level sessions may write concurrently only when canonical host-owned path scopes are disjoint. Overlapping file or ancestor/descendant scopes fail closed before approval or mutation. Approval belongs to exact connection, session, and turn; approval never transfers between sessions.

Apply-time revision checks protect dirty editor buffers and user changes made after lease acquisition. Leases release on completion, denial, cancellation, session close, connection loss, and shutdown.

## Metrics and privacy

Per-session footer and transcript groups show provider-reported context usage, cost when supplied by ACP, elapsed turn latency, token counts when supplied, failure state, and cancellation state. Unknown values remain omitted rather than displayed as zero.

Status and scheduler events contain only agent/session ids, counters, durations, token usage, stop/failure category, and bounded error text. They never include prompt text, model output, raw tool output, or workspace file content.

## Starting multiple ACP agents

Configure distinct server ids, then start exact agents independently:

```toml
[agents]
enabled = true
default_agent = "root"

[agents.servers.root]
label = "Root"
command = "root-acp-agent"
args = ["acp"]

[agents.servers.reviewer]
label = "Reviewer"
command = "review-acp-agent"
args = ["acp"]
```

Use `:agents_new root` and `:agents_new reviewer`. Run `:agents_new` without an id to use `default_agent` or sole configured server; when neither is unambiguous, ee opens server picker. Each different agent id owns isolated connection and subprocess. Multiple sessions using same id share that id's connection and concurrency limit.

### Same ACP binary, different startup models

Two ids may launch same binary with different startup arguments or environment. They remain isolated ids, connections, processes, sessions, and native provider state:

```toml
[agents.servers.fast]
label = "Fast model"
command = "vendor-acp-agent"
args = ["--model", "<FAST_MODEL_ID>", "acp"]

[agents.servers.deep]
label = "Deep model"
command = "vendor-acp-agent"
args = ["acp"]
env = { VENDOR_MODEL = "<DEEP_MODEL_ID>" }
```

Start with `:agents_new fast` and `:agents_new deep`. Keep credentials outside project config; use provider-supported secure environment or authentication flow.

### Per-session provider model selection

ACP agents may advertise session config options after handshake. Inspect active session with `:agents_config`, then set advertised model/config value:

```text
:agents_config_set <config_id> <value>
```

Example shape: `:agents_config_set <PROVIDER_MODEL_CONFIG_ID> <ADVERTISED_MODEL_VALUE>`. ee accepts only ids and select/boolean values advertised by active session, validates value, and sends ACP session config change to that session. It does not invent model ids, copy setting to other sessions, or rewrite startup config. Composer equivalent: `/config set <config_id> <value>`.

Full agent config reference: [Config and schema](../../wiki/config.md#agent-process-examples).

## Rubber duck critic controls

### One ee-owned OpenRouter root and critic

`ee-openrouter-agent` owns both model calls inside one process. Configure contrasting root/critic model ids and families at startup; keep API key in secure process environment, never checked-in config:

```toml
[agents.servers.openrouter]
label = "OpenRouter root + critic"
command = "ee-openrouter-agent"

[agents.servers.openrouter.env]
OPENROUTER_MODEL = "<ROOT_MODEL_ID>"
OPENROUTER_MODEL_FAMILY = "<ROOT_MODEL_FAMILY>"
OPENROUTER_RUBBER_DUCK_MODEL = "<CRITIC_MODEL_ID>"
OPENROUTER_RUBBER_DUCK_MODEL_FAMILY = "<CRITIC_MODEL_FAMILY>"
OPENROUTER_RUBBER_DUCK_MODE = "manual"
```

Modes: `off`, `manual`, `automatic`; default `manual`. Missing, same-id, same-family, or malformed critic identity disables only rubber duck; root turns remain usable. Bounds default to 2 calls/session, 65,536 context bytes, 32,768 output bytes, and 90,000 ms timeout. Matching CLI flags and `OPENROUTER_RUBBER_DUCK_*` variables may override them. Invalid bounds fail startup parsing. API key never enters model metadata.

### Different external ACP root and critic

Config can identify separate processes:

```toml
[agents]
enabled = true
default_agent = "root"

[agents.servers.root]
command = "root-acp-agent"
args = ["acp"]

[agents.servers.critic]
command = "critic-acp-agent"
args = ["acp", "--read-only"]

[agents.rubber_duck]
mode = "manual"
external_agent_id = "critic"
max_calls = 2
max_context_bytes = 65536
max_output_bytes = 32768
timeout_ms = 90000
```

This route is **manual-only**. Production Agents UI resolves `external_agent_id` and intercepts exact `/rubber-duck [question]` before normal root prompt dispatch. Host starts only selected critic process in an isolated ephemeral ACP session, forwards bounded redacted review context through read-class EE tools, verifies structured report, closes critic connection, then sends canonical verified evidence to root for one synthesis turn. `internal_model_id` and `external_agent_id` are mutually exclusive.

Automatic external critique also fails closed: current production broker constructors lack host-owned sandbox or immutable-snapshot proof. Host-forwarded read tools are insufficient because agent-native filesystem/terminal tools remain outside ee control. External automatic rollout stays disabled until enforced read-only sandbox contract and adversarial fixtures pass.

## `/rubber-duck [question]`

Exact provider command:

```text
/rubber-duck [question]
```

Command must be exact; `/rubber-ducking` is not accepted. Provider must advertise support. Manual request makes extra critic model call over bounded observed context, verifies structured report, then gives root one bounded no-tools synthesis call. This adds provider cost and latency; configured timeout and per-session call budget apply. Cost appears only when provider reports it; unknown stays unknown.

Automatic internal mode evaluates deterministic host facts at four boundaries:

- plan: `PlanThenExecute` multi-file plans, or API/security/persistence/migration/destructive/high-coupling impact, before first write
- implementation: non-trivial changed scope, diagnostics, incomplete/skipped validation, or recovery, before final synthesis
- failure: at least two repeated failures before equivalent repair
- tests: behavioral change with changed files but no adjacent selected tests

Typed automatic skip reasons: `manual_only`, `cancelled`, `pure_question`, `formatting_only`, `trivial_mechanical_edit`, `already_evaluated`, `missing_revision`, and `no_material_signal`. Unavailable contrast, exhausted call budget, quarantine, timeout, or critic failure also prevent useful critique without changing root state.

Root remains sole decision owner and sole writer. Critic cannot write, execute, delegate, approve, grant trust, select completion state, or claim validation. Critic report is advisory evidence for root synthesis, **not validation evidence** and never completion proof.

### Privacy and ownership

Critic receives bounded context needed for selected target. External forwarding can disclose redacted workspace-derived context to separate agent/provider with its own authentication, billing, retention, model, and native-tool policies. ee does not copy credentials/provider state between processes. Sensitive-text redaction removes known secrets and common credential forms, but redaction is not substitute for reviewing external provider policy.

Critic events retain only bounded safe metadata: target/reason, identities, finding counts, latency, token/cost counters, and policy version. No prompts, critique text, workspace content/paths, terminal output, credentials, provider config, native state, or hidden reasoning.

## Replay gate and rollout policy

Rollout order:

1. `ManualInternal` remains production default.
2. Passing replay gate grants internal backend `AutomaticInternalEligible`; it does not alter production default or config automatically. Enabling `automatic` remains deliberate rollout decision.
3. External ACP remains `ExternalManualSandboxGateRequired`: production UI supports explicit manual dispatch, but automatic use stays disabled until host-owned sandbox or immutable-snapshot proof plus adversarial fixtures establish isolation.

Hermetic gate requires exact 15-fixture set in [`rubber_duck_tasks.json`](../../crates/ee-agent-orchestrator/tests/fixtures/replay/v1/rubber_duck_tasks.json) and thresholds in [`rubber_duck_baseline.json`](../../crates/ee-agent-orchestrator/tests/fixtures/replay/v1/rubber_duck_baseline.json):

| Metric | Passing threshold |
| --- | ---: |
| complex quality gain | at least 250 |
| trivial skip rate | at least 90% |
| false positives | at most 1 |
| duplicate work | 0 |
| policy violations | 0 |
| internal model/agent calls | at most 32 |
| internal latency | at most 320 ms |
| internal input tokens | at most 850 |
| internal output tokens | at most 330 |
| internal estimated cost | at most 1,550 micro-USD |
| false successes | 0 |
| completion regressions | 0 |

Independent hard requirements: critic mutation, execute, delegate, approval-prompt, and policy-violation counts must remain zero across internal and external fixtures. Internal automatic eligibility uses 11 internal fixtures; four external fixtures record cross-agent attribution separately and cannot make internal quality/cost pass. Quality comes from verified finding keys compared with fixed fixture oracle keys. Host validation/completion comes from root replay and cannot be upgraded by critic output. Pinned Rust thresholds reject weakened baseline policy. Run deterministic gate:

```sh
cargo test --quiet -p ee-agent-orchestrator --features test-utils deterministic_run_baseline_gate_and_stable_summaries_pass
```

Gate failure blocks automatic-internal eligibility. Replay evidence supports rollout decision; critic report remains advisory and never validation evidence.
