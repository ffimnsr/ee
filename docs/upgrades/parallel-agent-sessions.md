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
