# Record/replay harness for provider transports

Status: implemented (ISSUES.md Part 8, I13).

## Problem

Provider regression tests either hit the network (slow, flaky, credential-bound)
or hand-build fixture bodies inside each test. When a provider changes its
response shape (a new Gemini revision, an external agent behind an
OpenAI-compatible route), the first signal was a broken live session, and the
hand-built fixtures did not exercise the real event framing.

## Design

`ee-chat-completions` now funnels every model exchange through one seam:

```
codec → EndpointTransport::send_json → HttpBackend
                                        ├── LiveBackend      (reqwest)
                                        ├── RecordingBackend (wraps any backend)
                                        └── Replay           (cassette)
```

- `transport::backend` defines `PreparedRequest` (method, URL, final headers,
  encoded body), `HttpResponse` (status, headers, ordered chunk source), and the
  `HttpBackend` trait. `HttpResponse` is what a codec actually needs, so a
  fixture can reproduce a live response without constructing
  `reqwest::Response`.
- `transport::cassette` owns the on-disk format and every bound. Credentials are
  redacted from `Authorization`, provider key headers, and credential-bearing
  query parameters; hop-by-hop and framing headers are dropped because bodies
  are stored decompressed.
- `transport::recording` buffers a response (bounded by
  `MAX_RECORDED_BODY_BYTES`, under the profile's chunk budget) and appends the
  exchange to a `Recorder`.
- `transport::replay` serves a cassette. Requests match on **method + exact
  body**, with the URL path/query as a tie-breaker, so a test may rewrite the
  endpoint origin to an unreachable host. Repeated calls replay the recorded
  response again; a recorded sequence (`429` then `200`) is consumed in order, so
  retry behavior is reproducible. Unknown requests fail closed with a diagnostic
  listing the fixture's routes.

No production code path changes behavior: `LiveBackend` is the default, and the
per-phase timeouts (headers and idle chunk) apply exactly as before.

## Recording a real exchange

The example records one buffered or streaming call:

```sh
EE_RECORD_ENDPOINT=https://generativelanguage.googleapis.com/v1beta/openai/chat/completions \
EE_RECORD_MODEL=gemini-2.5-flash \
EE_RECORD_API_KEY=... \
EE_RECORD_OUT=crates/ee-chat-completions/tests/fixtures/gemini-chat-stream.json \
EE_RECORD_STREAM=1 \
cargo run -p ee-chat-completions --example record_exchange
```

Optional variables: `EE_RECORD_API_KEY_ENV` (name of the variable holding the
key, default `EE_RECORD_API_KEY`), `EE_RECORD_PROMPT`, `EE_RECORD_LABEL`, and
`EE_RECORD_STREAM` (`1`, `true`, or `yes`). The credential is read at send time,
never printed, and stripped from the cassette.

Review a cassette before committing it: headers and query credentials are
redacted, but request bodies still contain the prompt that was sent.

## Replaying in a test

```rust
let cassette = Cassette::from_json(include_str!("fixtures/gemini-chat-stream.json"))?;
let replay = Replay::with_chunk_size(cassette, 7); // split SSE frames across reads
let transport = EndpointTransport::with_backend(profile, token, Arc::new(replay));
let client = ChatCompletionsClient::from_transport(transport);
let turn = client.complete_streaming(&messages, &[], &mut sink).await?;
```

`tests/record_replay.rs` covers buffered replay, streaming replay with split
chunks, deterministic repeated calls, recorded retry sequences, fail-closed
unknown requests, disk round-trips, and a drift guard that rebuilds the request
body with `request_body` so a fixture is only re-recorded when the builder
changes. `fixtures_carry_no_credentials` asserts no fixture contains a bearer
value.

## Bounds

| Bound | Value |
| --- | --- |
| Cassette file | 64 MiB |
| Exchanges per cassette | 4096 |
| One request or response body | 32 MiB |
| Headers per side | 128 |
| Replay chunk size | 1..=64 KiB (default 512 B) |

Anything outside a bound fails closed on load or record, so a corrupt fixture
cannot exhaust memory.
