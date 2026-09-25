# Line Payload Binary Framing

## Purpose

Remove per-line serialization cost from the `update` path so line content — text
and syntax spans — reaches the frontend with far fewer bytes, allocations, and
per-line JSON objects.

Measured composition (Phase 0, see below) reordered this plan: syntax span JSON
is **86.2%** of an update frame for a syntax-enabled window, so span encoding
comes first. The text carrier work stays in scope for syntax-free windows (plain
text, VLF, syntax disabled) where JSON overhead is still ~54% of the frame.

Rope-native iteration stays parked (`CONVERT_TO_ROPE.md`): it cannot remove the
payload copy, and the payload copy is not the dominant term anyway.

## Revision note

- Split into a JSON blob carrier plus a later binary frame, with a base64 interim.
  Dropped: the interim is additive or lossy, never both correct and beneficial,
  and upgrade policy prefers no compatibility shims. The carrier switch and the
  frontend decode are one atomic phase.
- Text-first ordering. Revised after measuring: span JSON dominates update frame
  bytes and render time, so span encoding is Phase 1 and text-carrier work moves
  behind it.
- Latency check. A deeper decomposition (Phase 0 results below) shows the text
  carrier is about 5% of a syntax-enabled render and is not user-visible on the
  in-process channel. The impact ranking is: span payload shape, then span
  production, then text. Phases are ordered accordingly; the binary frame work is
  retained for byte and allocation hygiene plus syntax-free windows, not for
  perceived latency.
- Phase 1b split span production into cold parse/walk and repeated repaint work.
  The interactive cost was the repaint: every caret move and drag re-parsed and
  re-walked the window. Both are now eliminated; parse-only turns out to be ~20%
  of the cold cost, so incremental reparse is lower value than it first looked.

## Scope

In scope:

- `update` notification syntax span payload.
- `update` notification line text carrier.
- Text-carrying `UpdateOp` payloads (`ins`, `update`).
- Borrowed-read API on `TextStore`.
- In-process RPC channel frame type.
- Frontend line cache and span consumption.

Out of scope:

- Render planning, dirty spans, `line_cache_shadow` tactics.
- Wrap cache (`Lines`) and `WidthCache` behavior.
- Rope iteration model in `view/render.rs` (stays parked).
- Cursor and annotation encodings (stay JSON).
- Plugin `PluginUpdate` channel.

## Non-goals

- No change to editor semantics, selection truth, or view state.
- No new document model.
- No document text stored on the frontend.
- No binary migration for any channel other than `update`.
- No change to what is highlighted, only to how spans are encoded.

## Current state findings

### Span payload is the dominant cost

- Spans are render-critical: `ee-cli/src/ui/panels.rs:284` `render_buffer` calls
  `crate::highlight::Highlighter::scope_spans_in_range(line, syntax_spans,
  byte_start, byte_end)`, which maps scope strings to ratatui styles
  (`ee-cli/src/highlight.rs:32`). Dropping spans is not an option.
- Spans ship by default for grammar-backed languages:
  `xi-core-lib/src/tree_sitter_support.rs:224-233` gates `syntax_spans` on
  `gates.syntax && has_grammar && supports_any_query_kind(Highlights |
  Injections | Locals)`. No user config toggle found in `config.rs`.
- Encoding before Phase 1 was one JSON object per span:
  `crates/xi-core-lib/src/view/render.rs` emitted
  `{"start_byte": n, "end_byte": n, "scope": "dotted.scope.name"}`, roughly
  70-90 bytes per span, with the scope string repeated at every occurrence.
  Phase 1 replaced it with `encode_line_spans` (flat triples plus a per-update
  interned `scopes` table).
- Frontend cost: one owned `String` per span (`CoreSyntaxSpan.scope`,
  `ee-cli/src/backend.rs:321`).

### Backend wire shape is JSON nested in JSON

- `crates/xi-core-lib/src/client.rs:375` defines
  `Update { ops, pristine, annotations, vlf_total_lines }` with `Serialize`.
- `crates/xi-core-lib/src/client.rs:396` defines
  `UpdateOp { op, n, lines: Option<Vec<Value>>, first_line_number }`.
  Constructors at `:407-432`: `invalidate`, `skip`, `copy` carry no payload;
  `insert` and `update` carry `Vec<serde_json::Value>`.
- `crates/xi-core-lib/src/view/render.rs` `encode_line` builds one
  `serde_json::Value` per visual row: `text`, `spans` (flat triples), `cursor`,
  `ln`.
- Emission sites: `UpdateOp::insert(encoded_lines)` at `render.rs:429`,
  `UpdateOp::update(encoded_lines, ..)` at `render.rs:360`.
- Steady state is already cheap: when the shadow needs no render,
  `render.rs:221-231` emits a single `UpdateOp::copy(total_lines, 1)` with no
  payload. Only `Render` / `Update` segments carry payload.

### Per-line cost chain today

1. `RenderSource::read_range` returns `ReadResult::Ready(Cow<str>)`
   (`crates/xi-core-lib/src/text_store/render_source.rs:30-41`).
2. `encode_line` clones that `Cow` into an owned `Value::String`.
3. `Client::update_view` nests the `Update` into another `json!` value
   (`client.rs:42-50`).
4. `RawPeer::send` runs `serde_json::to_string(&Value)` and writes the framed
   message (`crates/xi-rpc/src/lib.rs:665-671`).
5. Frontend deserializes into `CoreLine` and copies the text twice.

### Store side already iterates chunks but returns owned text

- `crates/xi-core-lib/src/text_store/mod.rs:402`
  `TextChunk { text: String, byte_range: ByteRange }`.
- `:409` `TextChunkResult { Ready, Pending, Cancelled, Unsupported }`.
- `TextStore::read_byte_range` at `:450`, `iter_chunks` at `:465`.
  Implemented by `RopeTextStore` (`text_store/rope_store.rs:177`, `:209`) and
  `VlfStore` (`vlf/store/text_store.rs:45`).
- Chunks are already aligned to rope leaves / VLF pages, so the iteration shape
  needed for blob assembly exists. What is missing is a borrowed variant: the
  carrier type always owns a `String`.

### Transport traits are text-only, but the actual frontend link is a channel

- `crates/xi-rpc/src/transport.rs:35`
  `ReadTransport::read_message(&mut self, buf: &mut String)` — UTF-8 only.
- `:45` `WriteTransport::write_message(&mut self, data: &[u8])` — bytes accepted
  on write.
- Framing strategies: `NewlineWriter` at `:80`, `ContentLengthWriter` at `:113`.
- ee-cli uses neither. It embeds core in-process over mpsc channels that are
  `String`-typed on both ends:
  `ee-cli/src/backend.rs:23-25` `ChannelReader { rx: Receiver<String> }`,
  `:27-37` `read_message` uses `buf.push_str(&message)`,
  `:40-52` `ChannelWriter` converts back with `String::from_utf8(data.to_vec())`,
  `:365-367` both channels are `String` channels,
  `:371` `RpcLoop::new(ChannelWriter { .. })`,
  `:1299-1305` `xi_reader_thread` takes `Receiver<String>` and `Sender<String>`,
  plus `block_for_response` at `:1572` and `drain_sync_notifications` at `:1598`.
- Separate consumers exist and are unaffected: standalone core over stdio uses
  newline framing (`xi-core/src/main.rs:200`), plugin transports are stdio
  newline or content-length (`xi-core-lib/src/plugins/mod.rs:509`, `:521`).
- Newline framing cannot carry raw bytes. Content-length framing can.

### Frontend keeps text twice per line

- `ee-cli/src/backend.rs:237` `CachedLine { text: String, cursors, syntax_spans,
  logical_line }`; `:247` `LineSlot { Known(CachedLine), Invalid }`.
- `CoreLine` carries `text`, `cursor`, flat `spans`, `ln`; `CoreUpdate` carries
  the `scopes` table. `decode_spans` validates both and fails closed.
- `buffer/mod.rs` `BufState` holds both `line_cache: Vec<LineSlot>` and
  `lines: Vec<String>`, synced by cloning through `line_text_for_slot`
  (`ee-cli/src/buffer/mod.rs:196-201`). The legacy `XiClient` mirror that held the
  same pair (`backend.rs`) is gone: it was dead code and was deleted in the
  duplication follow-up.
- `buffer/bufstate.rs` `apply_update` rebuilds both vectors through the shared
  appliers in `buffer/apply.rs`; `copy` clones text, `invalidate` pushes one empty
  `String` per row.
- VLF consumes the same stream through `BufState`'s window branch and the shared
  `replace_vlf_window` helper, with a bounded window.

### Reuse points that already exist

- Chunk iteration for both stores: `read_byte_range` / `iter_chunks`.
- Rope leaf iteration without materializing: `find.rs:275` already uses
  `text.slice_view(..)` plus `Cursor` and `lines_raw()`.
- Viewport-bounded reads: `RenderSource::set_viewport` (`render_source.rs:72`)
  drives the VLF pager read-ahead window.
- Missing-text semantics already exist: `encode_line` omits `text` when a read is
  `Pending` or `Cancelled` (`render.rs:56-61`), and the frontend keeps its
  previous row.
- Span budget caps already exist: `VisibleSyntaxLimits` (`max_bytes`, `timeout`,
  `max_matches`, `max_captures`) in `crates/xi-core-lib/src/tree_sitter_support.rs:127`.
- Working probe for payload composition:
  `xi-core-lib/src/view/tests/render_find_tests.rs::syntax_span_render_perf_probe`
  (manual, `#[ignore]`).
- Bench harness precedent: `crates/xi-core-lib/benches/wrap.rs` with
  `[[bench]] name = "wrap" harness = false` (`crates/xi-core-lib/Cargo.toml:60`).
- Test peer for capture: `crates/xi-rpc/src/test_utils.rs:31` `DummyWriter`.

## Phase 0 results

Measured, not estimated. Command:

```
cargo test --quiet -p ee-xi-core-lib syntax_span_render_perf_probe -- --ignored --nocapture
```

Fixture: 200 lines of `let value_N = 42;`, language `rust`, warm queries, wrap 80,
full-window `request_lines(0, 199)`. Median of three runs.

```
text_bytes=3890
plain_payload_bytes=8346      # syntax disabled
syntax_payload_bytes=60370    # syntax enabled
span_payload_bytes=52024      # span_share=86.2%
spans=853                     # 4.3 spans per line, ~61 bytes per span
plain_render_us=188          syntax_render_us=3907
plain_serialize_us=13        syntax_serialize_us=77
span_production_us=5742      # single whole-chunk segment
span_production_segmented_us=1890   # per-line segments, the editor's path
```

Attribution for the syntax-enabled window (`syntax_render_us` = 3907 microseconds):

| Term | Cost | Share | Addressed by |
| --- | --- | --- | --- |
| Span production (parse plus query walk) | 1890 µs | 48% | Phase 1b landed the repeated-work removals; the walk itself is tracked in "Span production: recorded decisions" |
| Span payload shape (853 span objects, Value maps, harness clone) | ~1730 µs | 44% | Phase 1 (span encoding) |
| JSON serialization | 77 µs | 2% | Phase 1, partly |
| Text path (read, encode, per-line Values) | 188 µs | 5% | Phase 3 (text carrier) |

Conclusions:

- **Binary framing of line text would not meaningfully improve the editor.** The
  text path is 188 µs including store read, line encoding, and serialization; a
  perfect carrier eliminates well under 0.2 ms of a ~4 ms window, against a
  16.7 ms frame budget. On the in-process channel the copy is microseconds.
- Span payload shape is real backend work: 853 `serde_json::Value` objects per
  window, each a 3-entry map, inserted into 200 line maps.
- Span production is the single largest term and is untouched by any payload
  work. It is addressable only by parsing less: incremental reparse with
  `old_tree` reuse (the repository has none today; every tree-sitter call passes
  `None`), per-line span caching across renders, or fewer context lines. (The
  caching lever has since landed — see "Span production: recorded decisions"
  item 2 — taking the scroll-back repaint from ~2.2 ms to ~0.27 ms.)
- Segmentation matters 3x: the same walk costs 5742 µs with one whole-chunk
  segment versus 1890 µs with per-line segments. The editor's path already uses
  per-line segments, but `parse_from_document_start` for YAML (`render.rs:112`)
  still parses and walks from the top of the document on every render.
- Caveat: `render_us` includes the recording peer's deep clone of the payload
  Value tree, so the payload-shape term is an upper bound that mixes harness cost
  with backend cost. Serialization alone is 77 µs, which bounds the pure
  serialization share.
- The former probe was broken and ignored (panicked on unseeded wrap breaks), so
  no composition data existed before this measurement. It now reports the byte
  split, the timing split, the span count, and the span carrier comparison.

### Span carrier spike (measured)

Same 853 spans, warm queries, median of three runs. `zerocopy` is already in the
lockfile via `ahash`, so the dependency adds no new build units.

```
span encoding probe: spans=853 flat_scopes=5
json_bytes=49547       json_us=318          # pre-Phase-1 production: one JSON object per span
flat_json_bytes=6853   flat_json_us=75      # interned scope ids, flat integer arrays
records_bytes=11135    records_us=21        # zerocopy u32 records, 12 B/span
records_u16_bytes=6017 records_u16_us=21     # zerocopy u16 records, 6 B/span
records_decode_us=0    records_u16_decode_us=0
```

| Carrier | Bytes | Encode µs | Decode |
| --- | --- | --- | --- |
| JSON objects (pre-Phase-1 production) | 49547 | 318 | JSON parse plus 853 owned span objects |
| Interned flat JSON arrays (Phase 1, landed) | 6853 | 75 | JSON parse plus per-line `Vec<u32>` |
| zerocopy u32 records | 11135 | 21 | pointer cast, 0 µs |
| **zerocopy u16 records** | **6017** | **21** | pointer cast, 0 µs |

Conclusions:

- Fixed-width records are not a byte win over smart flat JSON by construction:
  `u32` records (11135 B) lose to flat JSON (6853 B) because small integers are
  cheaper as text than as fixed widths. The 16-bit variant flips that (6017 B)
  because line-relative offsets and scope ids fit in `u16` for realistic windows.
- `u16` records win on all three axes at once: 88% fewer bytes than today, 3.5x
  faster to encode than flat JSON, and free to decode (no parse, no allocation).
- Only 5 distinct scopes appear in a 200-line Rust window, so interning is the
  single largest byte win. Record width matters second.
- Fallback contract: `encode_u16` returns `None` when a line-relative offset or
  the scope table exceeds `u16`, so the caller emits `u32` records for that
  payload. No silent clamping in the 16-bit path.
- Spike location: `crates/xi-core-lib/src/span_payload.rs`. `ScopeTable` is
  production (used by `encode_line_spans`); the fixed-width record layout is
  `#[cfg(test)]`, pending the Phase 3 byte frame. Nine unit tests cover layout,
  endianness, interning, line indexing, round-trip, unaligned decode,
  partial-record rejection, and the `u16` overflow fallback.

## Phase plan

Phase 0 — measurement gate (done)
Phase 1 — span encoding: interned scopes, flat span arrays (**landed**)
Phase 1b — span production reduction (**landed**)
Phase 2 — borrowed chunk read on `TextStore` (**landed**) (additive)
Phase 3 — atomic carrier switch for text (payload, frame, decode in one change)
  (**landed**)
Phase 4 — optional: write without the intermediate arena

### Phase 1 results (landed)

Backend emits `scopes` once per update and per-line flat `[start, end,
scope_id, ...]` triples; `ee-cli` decodes them in `decode_spans` and fails closed
on misaligned triples, inverted ranges, or unknown scope ids. Measured on the
Phase 0 fixture, same probe, before to after:

```
span_payload_bytes   52024 -> 8123    (-84%)
syntax_payload_bytes 60370 -> 16469   (-73%)
syntax_render_us      3907 -> 2782    (-29%)
syntax_serialize_us     77 -> 39
span_share            86.2% -> 49.3%
```

Span production is unchanged (`~1930 µs`), so the remaining window cost is now
split between production and the residual flat-array encoding. `zerocopy`
fixed-width records stay test-gated for Phase 3: on a JSON transport their
packed blobs would need base64, which measures larger than the flat carrier
(6017 B packed becomes ~8023 B base64 against 6836 B flat).

Layout: the wire payload types, `decode_spans`, `scope_table`, and the `LineSlot`
conversion and merge live in `ee-cli/src/backend/update.rs`, re-exported by
`backend.rs`. The op-to-cache application lives in `ee-cli/src/buffer/apply.rs`
(`rope_update`, `line_texts_for_cache`, `replace_vlf_window`), used by the live
`BufState` path. `backend.rs` is 680 LOC after the dead `XiClient` mirror was
deleted, back under the repository 1000 LOC bar. `crates/xi-core-lib/src/span_payload.rs`
owns `ScopeTable` plus the test-gated record layout.

#### Duplication follow-up (done)

The frontend had two copies of every cache-applying path: `BufState`'s and the
legacy `XiClient` mirror's. They had already drifted: `XiClient` did not clear
cached cursors on `copy` ops (a `copy` re-emits content the core considers
unchanged while cursor positions may have moved, so its rows could keep a stale
caret), and it rebuilt line texts by re-walking the cache instead of using the
pass it had just run. The VLF paths were also still wedging the buffer on a
rejected payload (they take the window before applying and only restored it in the
"no insert rows" branch).

What shipped: `buffer/apply.rs` owns the apply rules once (`rope_update`,
`line_texts_for_cache`, `replace_vlf_window`, with the helper restoring the cache
on rejection), `BufState` calls it, and the `XiClient` mirror was **deleted** — it
carried `#[allow(dead_code)]` and had no inbound references outside its own impls
(grep plus the Atlas graph), so nothing constructed it and nothing migrated to it.
Gone with it: XiClient's own `PartialEq`/`Eq`, its thread spawn that hosted
`XiCore` + `RpcLoop` (the live host is `buffer/construction.rs`), and
`format_location_message`, whose only production caller was the mirror (the live
path shows locations in a picker). Total: `backend.rs` 1484 -> 680 LOC, and one
implementation of every apply rule instead of two.

#### Review pass (second pass on Phase 1)

An adversarial review of the landed phase found five real defects. Fixed here:

1. **A rejected payload emptied the frontend's line cache and wedged the buffer.**
   Both appliers (`Backend::apply_update`, `BufState::apply_update`) took
   `line_cache`/`lines` with `std::mem::take`, then returned early on any decode
   error, leaving the caches empty. The next payload's `copy`/`skip` ops would fail
   against an empty cache, no range read as invalid, so the app never re-requested
   the rows. Both op passes are now pure (`update.rs::lines_from_ops`,
   `bufstate.rs::apply_rope_ops`) and the caller commits only on success, so a
   rejected update is a rejected message. Pinned by
   `notifications::rejected_update_keeps_the_cached_lines`.
2. **Unordered triples printed duplicated text.** `decode_spans` validated
   alignment, inversion, and scope ids, but not order, and `highlight.rs` walks
   forward-only with a cursor it does not clamp against. A triple starting before
   the previous one ended now rejects; touching spans stay valid.
3. **The "one `Arc<str>` per scope" criterion was not actually built.**
   `Arc::from(scope.as_str())` ran once per span, allocating exactly as the `String`
   path did. Scope names are now resolved once per payload into a shared
   `Vec<Arc<str>>` (`update.rs::scope_table`) and every span clones from it, on all
   three call paths (`lines_from_ops`, `apply_rope_ops`, `vlf_window_from_ops`).
   The exit criterion is now true as written.
4. **Span offsets were unbounded.** `[0, 1_000_000, 0]` was accepted and silently
   clamped to EOL by `highlight.rs`, styling a whole line from a malformed message.
   `decode_spans` now takes the served line length: a span start beyond the line
   (plus the two bytes a stripped trailing line ending can occupy) rejects, and a
   span end is clipped to the line.
5. **`zerocopy` was a production dependency for `#[cfg(test)]`-only code.** Moved
   to `dev-dependencies` of `ee-xi-core-lib`; the production path ships the JSON
   carrier, as the spike concluded.

Also in this pass: `CoreSyntaxSpan` lost its now-dead `Deserialize` derive (nothing
reads a span object off the wire any more), and the `scopes` table is built once per
payload rather than per line.

Test gaps closed: the drag/selection-change claim ("0 span rows") was asserted only
by the manual probe, so it now has a normal test
(`render_find_tests::selection_drag_repaint_omits_spans`) alongside the caret test,
which additionally asserts that rows without a valid cache entry still carry spans.
Malformed-payload coverage now includes an empty scope table, a zero-width span with
an unknown id (id validation precedes the zero-width drop), overlapping triples,
spans past the line end, clipped trailing spans, and cache survival across a
rejection.

Recorded limitation (no code change): an `update` op that patches a row the
frontend cannot read (`LineSlot::Invalid`) is accepted with whatever spans it
carries, so if the backend omitted spans because its shadow still considered the
row valid, that row stays unstyled until an edit invalidates it. No in-tree path
reaches that state (rows become `Invalid` only through an explicit `invalidate` op,
which clears the backend shadow too), and rejecting it instead would risk a
non-converging loop for lines that legitimately have no spans. A future frontend
cache-loss path must force a re-render or carry an explicit validity bit.

### Phase 1 — Span encoding

Goal: cut span bytes and per-span allocations without changing what is
highlighted. Measured carrier: 16-bit zerocopy records, see Phase 0 results.

Work items, backend:

- [x] Promote `crates/xi-core-lib/src/span_payload.rs` (`ScopeTable`) as the
      production interning table.
- [x] Emit a per-update scope interning table (`Update::scopes`): distinct scope
      strings once, spans reference them by index.
- [x] Encode spans as flat `[start, end, scope_id, ...]` triples per line,
      line-relative, key omitted when a line has none.
- [x] Keep `encode_line`'s contract: no spans for lines without them.
- Deferred by decision: distinct-scope cap. See "Distinct-scope cap: deferred"
  below — no cap ships in Phase 1.

Work items, frontend:

- [x] Decode spans into `CoreSyntaxSpan { start_byte, end_byte, scope: Arc<str> }`
      resolved from the update table, so `highlight.rs` keeps matching on scope
      strings and the per-span `String` allocation disappears. The table is
      resolved once per payload through `scope_table`, so spans of the same scope
      share one `Arc<str>`.
- [x] Cache the decoded spans in `CachedLine` in the same shape as before, so
      `copy` ops keep working with spans from earlier updates.
- [x] Fail closed on malformed payloads: `decode_spans` rejects misaligned
      triples, inverted ranges, out-of-order (overlapping) triples, span starts
      beyond the served line, and unknown scope ids; span ends are clipped to the
      line so a trailing line ending cannot be styled. See the review pass above.

Tests:

- Span decode round-trip: repeated scopes share one table entry; prefix matching
  in `highlight.rs` produces identical styling for the existing fixtures.
- Table lifecycle: a `copy` op spanning an update boundary keeps earlier spans
  valid.
- Rejection: out-of-range scope id, table length mismatch, span offsets beyond
  line length (start rejects, end clips), overlapping triples, cache survival
  across a rejected payload.
- Payload: probe shows a material reduction in `span_payload_bytes` with
  `syntax_payload_bytes` still above `plain_payload_bytes`.

### Distinct-scope cap: deferred

Recorded decision. No distinct-scope cap ships in Phase 1.

Why the table cannot grow with document content:

| Bound | Evidence | Effect on the scope table |
| --- | --- | --- |
| `max_captures = 4096`, `max_matches = 2048` | `tree_sitter_support.rs:66-72`, enforced at `:444`, `:466`, `:682` (`VisibleSyntaxWalk::exhausted`) | Caps emitted spans per window, so table growth per update is bounded |
| `max_bytes = 128 KiB` plus parse timeout | same limits, checked at `:325` | An oversized window emits no spans at all |
| `MAX_VISIBLE_INJECTION_DEPTH = 4` | `:73`, used at `:378` | Bounds scope mixing through injected languages |
| Scope names originate in query captures, not in the document | `:476`, `:722` (`scope: scope.to_string()`) | A file cannot invent scopes; it only selects among captures its language pack defines |

Measured on the Phase 0 fixture: 853 spans intern to 5 distinct scopes. Table
width is a property of the loaded query set, not of file size.

Failure mode if a query pack were pathological: a larger `scopes` array in that
update's payload — transient bytes, dropped with the payload. Scope ids are
`u32` on this carrier, so there is no wrap or overflow, and the frontend already
rejects out-of-range ids (fail closed).

Why no cap yet: every overflow policy is user-visible. Dropping beyond-cap spans
under-highlights lines, clearing spans drops the whole window's color, and
keeping them makes the cap a no-op. Choosing that behavior without a measured
case would put speculative policy in the render path.

Trigger to implement:

1. A grammar or query pack with thousands of distinct captures (generated or
   vendored packs, deep markdown to HTML to JS injection chains), **and**
2. probe evidence that the `scopes` table is a material share of
   `syntax_payload_bytes`.

When the trigger fires, prefer one of:

- Cap distinct scopes (for example 512) and drop spans that only reference
  beyond-cap scopes: deterministic, cosmetic-only degradation, table stays
  bounded.
- Structural fix once Phase 3 lands: intern scopes per document instead of per
  update, so only new scopes ship and ids stay stable. That removes the item
  rather than capping it.

Exit criteria:

- [x] `span_payload_bytes` reduced by at least half on the Phase 0 fixture;
      landed at -84%.
- [x] No per-span owned `String` on the decode path (`Arc<str>`, shared per
      scope name within an update).
- [x] Highlighted output unchanged on existing render tests
      (`ee-cli/src/tests/render.rs`, `highlight.rs` unit tests).
- [x] Existing span fixtures behave exactly as before
      (`view/tests/render_find_tests.rs`, `event_context` syntax-refresh test).

### Phase 1b — Span production reduction

Largest measured term. Measurement split it into two very different costs: the
cold parse/walk, and the same work repeated on every repaint.

Work items:

- [x] Measure where production time goes: parse versus query walk. Result:
      parse-only is 390-545 µs of a 1905-2999 µs segmented production, so the
      walk and per-segment clipping carry roughly 80% of the cost.
- [x] Stop producing spans on cursor-only repaints: the `update` op path (text and
      syntax still valid) now encodes rows without `spans`, and the frontend keeps
      the spans it already decoded.
- [x] Stop invalidating `SYNTAX_VALID` on selection changes: spans are a pure
      function of the text, and edits invalidate them through `after_edit`, so a
      caret move or drag repaint now takes the cursor-only path.
- [x] Keep every change behind the existing `VisibleSyntaxLimits` budgets. No
      budget was raised or bypassed.
- Decisions recorded for the three evaluation items: see "Span production:
  recorded decisions" below.

Measured repaint cost (probe, 200-line Rust window, median of two runs, before to
after):

| Repaint | Before µs | After µs | Before bytes | After bytes | Span rows |
| --- | --- | --- | --- | --- | --- |
| Caret move (line 2) | 822-907 | 12-13 | 576 | 380 | 2 to 0 |
| Caret jump (line 100) | 1119-1171 | 15 | 591 | 395 | 2 to 0 |
| Drag, 100 lines selected | 1691-1722 | 86-93 | 9135 | 4267 | 101 to 0 |

Exit criteria:

- [x] Repaint span production eliminated for the interactive paths: 0 rows carry
      `spans` after a caret move, caret jump, or drag selection (was 2, 2, and 101).
- [x] Highlighted output unchanged on existing render tests: rows keep their spans
      on the frontend, and new regression tests pin both behaviors
      (`caret_only_repaint_omits_spans_and_keeps_them_for_renders`,
      `selection_change_keeps_syntax_valid_for_cursor_only_repaints`,
      `update_op_without_spans_keeps_cached_spans`).
- [x] No cross-buffer tree or span cache leaks: no cache was introduced; the
      change removes work rather than memoizing it.

Note on the original exit criterion: "`span_production_segmented_us` reduced by
at least 40% on the Phase 0 fixture" is a cold single-shot measurement, and the
walk still dominates it, so it cannot move much without rewriting the walk. The
measurement above replaced it, because the interactive repaint is where the cost
was paid per keystroke.

### Span production: recorded decisions

Work left after Phase 1b happens in true `Render` segments only (edited lines and
newly visible rows). Measured before deciding:

```
scroll back to a previously rendered window:  2176-2297 us, 159 span rows, 15,460 B
parse-only share of segmented production:     389-545 us of 1905-2160 us (~20%)
YAML window via backend_syntax_spans_for_segment:
    offset 0:    26,912 us      offset 2,400: 26,793 us      offset 4,800: 26,733 us
YAML 200-line window via chunk_syntax_spans directly: 1.6 ms
```

1. **Incremental reparse (`old_tree`): deferred.** Parse is ~20% of production
   and the walk carries the rest, so reusing trees caps out around a fifth of the
   remaining cost. It also needs edit deltas plumbed into the syntax path, which
   the render call does not currently receive. Trigger: a profile where parse
   dominates production (for example a grammar whose query walk is cheap), or a
   `max_bytes`-sized window where parse time alone exceeds the frame budget.
2. **Span caching keyed by revision and line range: landed.** Scroll-back re-renders
   a whole window because the plan discards rows outside the viewport; a per-view
   cache now serves those rows from the walk that already produced them. Measured
   with the probe, same fixture, same 159 span rows and 15,460 B payload:

```
scroll back to a previously rendered window:  2176-2297 us  ->  263-271 us
```

   The cold path is unchanged by design: `syntax_render_us` stays at ~2.3-2.4 ms on
   the same fixture, because the first walk of a window still happens exactly once.
   The cache only removes the repeated walk.

   Design notes worth keeping (details in `view/syntax_cache.rs`):

   - Entries record **rows**, not requests. The return pass re-segments the window
     (the cold pass asks for `0..199`, the return asks for `0..12` and `12..199`),
     so an exact-window key never hits the flow the cache exists for. A request is
     served when the entries together cover every row it asks for.
   - Invalidation is `after_edit` (text) plus `rewrap` (visual line numbering), which
     is the same signal a revision key would use; language and syntax toggles are
     part of an entry's identity. `set_dirty` from plan drift deliberately keeps
     entries — the text has not changed, and the re-render that follows is the point.
   - Each entry also stores the byte offset its first row resolved to, and is dropped
     when that offset no longer matches, so a mutation path that forgets to
     invalidate becomes a miss instead of stale highlighting.
   - Bounded at 8 windows / 4,096 rows per view.
   - Rows served have always been produced by an actual walk, never re-derived, so
     the cache cannot synthesize spans the uncached path would not have produced.
   - The render loop now collects its plan segments up front (a handful of small
     structs) because the cache needs `&mut self` per segment while the shadow
     iterator holds `&self`. `plain_render_us` stays at 172-204 us across runs, so
     the collect is not measurable against the encode/serialize work around it.
3. **YAML `parse_from_document_start`: hypothesis disproved; keep the code and
   investigate the real driver.** The original concern was that parsing from line
   0 makes YAML windows `O(document)`. Measured through the real path
   (`backend_syntax_spans_for_segment`) the cost is flat at about 26.7 ms for
   offsets 0, 2,400, and 4,800 on a 5,000-line document, so the prefix length is
   not the driver. A direct `chunk_syntax_spans` call on a 200-line YAML window
   costs 1.6 ms, so the 26 ms lives in the render path's own work rather than in
   the parse/walk of the window. Red flag: 26 ms per YAML `Render` segment is the
   largest per-render cost measured anywhere in this plan (13x a Rust window at
   the same line count) and would be user-visible on YAML edits and scrolls.
   Next step before touching the parse window: profile
   `backend_syntax_spans_for_segment` for YAML to separate context-line
   collection, chunk slicing, and span assembly. This is pre-existing behavior,
   not a Phase 1b regression. Tracked in `ISSUES.md` under "Editor Syntax Span
   Performance".

### Phase 2 — Borrowed chunk read on `TextStore` (additive)

Landed, additive only: nothing in production calls the new method yet and the
wire is untouched. `ChunkBytes<'a>` and `TextStore::read_chunk_bytes` live in
`text_store/mod.rs`; the shared conformance set is
`crates/xi-core-lib/src/text_store/conformance.rs` (`#[cfg(test)]`).

Work items:

- [x] Add a borrowed carrier next to the existing owned one:

      pub enum ChunkBytes<'a> {
          Ready { bytes: Cow<'a, [u8]>, byte_range: ByteRange },
          Pending,
          Cancelled,
          Unsupported,
      }

- [x] Add `fn read_chunk_bytes(&self, range: ByteRange) -> ChunkBytes<'_>` to
      `TextStore` (`text_store/mod.rs`).
- [x] Implement for `RopeTextStore`: a range that fits inside one leaf is handed
      out as `Cow::Borrowed` over that leaf; a range crossing leaves falls back
      to `Cow::Owned` built by the same chunk-walk `collect_text` already uses, so
      content and range stay identical to the owned carrier. No new rope API, and
      no behavior change on the owned path.
- [x] Implement for `VlfStore` over the existing page cache. **Finding: this path
      cannot borrow today, so it inherits the trait default.** Pager reads hand out
      owned `PageBytes`, the decoded page cache sits behind a `RefCell`, and that
      cache is an evicting LRU, so `&self` has nothing stable to lend. The default
      body of `read_chunk_bytes` wraps `read_byte_range`, which keeps
      page-decoding behavior identical (a thin wrapper cannot force a decode the
      owned carrier would not also do) and gives every future store a correct
      fallback. Rationale recorded in the `vlf/store/text_store.rs` header.
- [x] Keep `read_byte_range` and `iter_chunks`. No caller deleted.
- [x] Conformance tests shared by both stores
      (`text_store/conformance.rs::assert_chunk_bytes_conformance`, called from
      `rope_store::chunk_bytes_tests` and `vlf::store::tests::read_tests`): whole
      document, empty ranges at both ends and in the interior, out-of-bounds,
      interior codepoint-safe splits, multibyte content, repeated reads, carrier
      agreement on every range `iter_chunks` reports, and byte-identity with the
      owned carrier at every step. Store-specific tests add the borrow proof
      (pointer identity inside the leaf), the crossing-leaf owned fallback, the
      row-vs-window borrow boundary, the VLF owned-pages fact, seam-range reporting,
      `Pending` propagation, and the inverted-range verdict split.

Exit criteria:

- [x] Both stores pass the shared conformance set.
- [x] No production caller yet; no wire change.

#### Review pass (second pass on Phase 2)

1. **The borrowed carrier could hand out invalid UTF-8 where the owned carrier
   panics.** `chunk.is_char_boundary` was not checked, so a mid-codepoint range that
   fit inside one leaf was borrowed and returned as bytes, while the same request
   spanning two leaves panicked in `collect_text`. The borrow is now gated on both
   ends being char boundaries, so the borrowed carrier fails exactly where the owned
   one does. Pinned by
   `borrowed_carrier_rejects_mid_codepoint_range_like_the_owned_one` plus the owned
   parity pin.
2. **`VlfStore` duplicated the owned wrapper.** With the trait default in place the
   explicit impl was byte-identical, so it was deleted and the reasoning moved to the
   store's module header.
3. **The conformance set asserted a property the VLF store does not have.**
   `iter_chunks` decodes page steps without seam adjustment while range reads do
   adjust, so the shared set now asserts only carrier agreement per reported range.
4. **`Pending` was tolerated, never forced.** A real fixture now refuses the read
   (`page_size * 4` is `max_read_size`) so both carriers must answer `Pending`.
5. **Doc contracts were wrong.** `ChunkBytes` promised that a loaded page is
   borrowed (it is not), and never said that `byte_range` can be wider than the
   request (it is, at VLF seams, and overlay reads echo inverted ranges). Both are
   stated on the enum and the trait method now, and store-specific verdicts
   (rope `Unsupported` vs VLF empty `Ready` for inverted ranges) are pinned in the
   store tests instead of being implied by the shared set.

Findings that shape Phase 3 (recorded, no code):

- Text leaves are 511-1024 bytes (`xi-rope::rope`). A whole window always crosses
  leaves and takes the owned fallback, so blob assembly must read **row-by-row or
  chunk-by-chunk**, not window-by-window, if it wants borrows. Pinned by
  `row_sized_range_in_realistic_document_borrows` (row borrows on a 200-line
  document; the whole document does not).
- Borrows are valid only while `&self` is held, which is why the carrier is
  lifetime-parameterized. Blob assembly copies into its scratch buffer inside the
  same call; nothing may hold a `ChunkBytes` across a store call.
- Rope reads panic on mid-codepoint ranges (`slice_to_cow` semantics). Conformance
  offsets go through `safe_offset`; seam snapping stays the owned/VLF path's job.

### Phase 3 — Atomic carrier switch for text (payload, frame, decode in one change)

One change, one commit. Splitting this leaves a broken or wasteful intermediate
state, so it does not get split. Scoped to text: spans are already compact after
Phase 1, and this phase is measured at ~5% of a syntax-enabled window, so its
justification is bytes and allocation hygiene plus syntax-free windows (plain
text, VLF, syntax disabled) and pathological long lines — not perceived latency.

Work items, backend:

- [x] Introduce a text blob scratch buffer. Landed as
      `crates/xi-core-lib/src/text_blob.rs`: `TextBlob` collects row bytes and their
      lengths, `into_frame` emits the wire frame, `decode_frame` reads it back, and
      `MAX_BLOB_BYTES = 4 MiB` caps it. No source-document offset is recorded: rows
      are handed out positionally, so an offset would be dead weight on the wire.
- [x] Replace per-line text Values with references into the frame. **Deviation from
      the plan, measured into place:** the first cut used a per-row
      `"blob": [off, len]` slice, and measuring it showed that ~15 B of JSON
      metadata per row costs *more* than the short JSON string it replaces (total
      bytes +11% on source-like text). The shipped shape is one `"blob": true` flag
      per op plus a frame that carries the row lengths itself
      (`varint count || varint len_i... || bytes`), and rows are consumed in op
      order: a row without `text` takes the next frame row, a row with `text` keeps
      its own. That removes the per-row metadata entirely and turns the carrier into
      a real byte win (see the measurement below).
- [x] Assemble the frame during the same segment pass that builds spans, reading
      **per row** through the new `RenderSource::read_bytes` (a rope row inside one
      leaf is lent, anything else is the same owned copy the text carrier made; VLF
      uses the trait default over `read_range`). `Pending`/`Cancelled`/`Unsupported`
      leave the row without text, preserving the keep-previous behavior.
- [x] Keep `RenderSource::read_range` for the text carrier (text-only transports
      and rows the blob cannot serve). VLF wrapped facade rows share the same fill
      path: they pass `Some(text)` like rope rows, so they use the blob when the
      peer supports it.
- [x] Enforce the blob cap. **Deviation from the plan, recorded:** instead of
      splitting ops, a row that would exceed `MAX_BLOB_BYTES` falls back to the
      per-line text carrier inside the same update. That cannot truncate or corrupt
      anything (it is exactly the pre-Phase-3 carrier for that row), needs no
      framing changes, and is reached only by a pathological window; op splitting
      remains available if a profile ever shows the cap being hit routinely.

Work items, transport:

- [x] Extend the RPC traits with byte-frame methods, defaulted so nothing existing
      breaks: `ReadTransport::read_binary_message`, `WriteTransport::
      write_binary_message`, and the capability predicate
      `WriteTransport::supports_binary_frames` (default `false`). `NewlineWriter`,
      `ContentLengthWriter`, and plugin stdio transports keep the defaults, so the
      capability predicate and the carrier choice stay in step.
- [x] Add the capability predicate used for carrier selection: it travels
      `WriteTransport -> RawPeer -> Peer -> Client::supports_binary_frames`, which
      is what the renderer consults before building a blob.
- [x] Implement both methods on the in-process pair: `ChannelWriter` carries
      `Frame::{Text, Binary}` and reports support; `xi_reader_thread`,
      `block_for_response`, and `drain_sync_notifications` read that enum. The
      frontend -> core direction stays `String`: no byte frame flows that way, so
      `ChannelReader` keeps the trait's unsupported default for them (recorded
      deviation from "both directions", with no functional difference today).
- [x] Frame layout: the notification is a normal JSON `update` whose rows carry
      `blob` slices, and the blob follows as one raw frame. **Deviation:** the
      payload does not declare a length; the presence of any `blob` slice is what
      tells the frontend to read the frame, and the slices are validated against
      the frame that actually arrived. That is the stronger check (the plan's
      "meta length <= frame length" rule is implied by it) and removes a field
      that could disagree with the bytes.
- [x] Update `RULE.md` under "Current Protocol Decisions".

Work items, frontend:

- [x] Move the update payload types and decode out of `ee-cli/src/backend.rs`
      into `ee-cli/src/backend/update.rs`, re-exported so `crate::backend::*`
      paths keep working. Done during Phase 1 (the file was over the repository
      1000 LOC bar); the op-to-cache application went to `buffer/apply.rs` in the
      duplication follow-up, which also deleted the dead `XiClient` mirror and took
      `backend.rs` to 680 LOC.
- [x] Store line text once where the payload allows: this phase left the text
      carrier alone (Phase 3 scope).
- [ ] Store line text once: `Arc<str>` per row, or a slice into an `Arc`-held
      blob. **Deferred with rationale:** the payload no longer duplicates text
      (that was this phase's goal), and what remains is `BufState`'s own
      `line_cache` text plus its `lines: Vec<String>` flattening
      (`buffer/apply.rs::line_texts_for_cache`). Removing that pair means changing
      the render/edit surface (`get_line`, `whole_text`, `line_range_owned`, edits),
      which is a frontend representation change with no payload benefit — its own
      change, gated on a profile showing the flattening cost.
- [x] Preserve `apply_update` semantics exactly: `copy` clears cursors and reuses
      previous content; `invalidate` yields empty rows; `update` merges cursors with
      replaced text (the blob is decoded through the same `row_text` helper the
      per-line carrier uses, so a row's text source does not change what the cache
      ends up holding).
- [x] Preserve the VLF bounded-window path behavior: `replace_vlf_window` and
      `vlf_window_from_ops` take the blob and validate it before touching the
      window; a rejected payload restores the previous window.

Validation rules, fail closed on every frame (all enforced in
`backend/update.rs::validate_blob_slices` + `row_text`, before anything is
committed):

- every `BlobSlice.off + len` within the frame, no overflow
- `n == lines.len()` for payload ops (unchanged, per op)
- offsets monotonic within an op
- a slice requires a frame; a row cannot carry both carriers
- a slice must be UTF-8

Violation rejects the payload, keeps the previous cache, and surfaces an
error/alert. No panics, no silent truncation.

Tests (landed):

- Payload equivalence between the per-line carrier and the blob carrier for ASCII,
  multibyte text, CRLF, tabs, trailing-newline shapes, and one 1 MB single line:
  `xi-core-lib/src/view/tests/text_blob_tests.rs`. Empty rows are covered there too
  (a zero-length slice must still be followed by its frame — a regression found by
  the app-level tests while this phase was landing).
- Byte cost, measured both ways
  (`blob_carrier_byte_cost_is_measured_both_ways`), 200-line window, compact
  frame:

```
source-like rows:    text 10233 B  vs blob 2306 B json + 5951 B frame = 8257 B  (-19%)
escape-dense rows:   text  9460 B  vs blob 6290 B (json + frame)       = 6290 B  (-34%)
```

  The first cut of this carrier used a per-row `"blob": [off, len]` slice and
  measured **+11% total bytes** on the source fixture: ~15 B of JSON metadata per
  row costs more than the ~9 B a short JSON string costs, so the slice form only
  paid off for escape-dense rows (every tab, quote, and backslash costs two JSON
  bytes and one frame byte). That measurement is why the shipped frame carries the
  row lengths itself and flags the blob once per op: with the per-row metadata gone,
  the JSON keeps only the cursor/`ln` envelope and the carrier wins on bytes for
  ordinary source text too. Both numbers are pinned in the test.
- Store carriers: `RenderSource::read_bytes` equals `read_range` for rope and VLF,
  lends a one-leaf rope row, and reports `Pending` when the pager refuses the read
  (`rope_store` and `vlf/store/tests/render_source_tests.rs`).
- Round-trip for the in-process pair in both frame kinds, blob/text decode
  equivalence on the cache, `update`-op frame rows vs an inline `text` override,
  cursor-only ops keeping cached text, rejection cases (frame declared but absent,
  too few/too many frame rows, non-UTF-8 row, a blob row with neither text nor a
  frame row, truncated frame, trailing bytes), reader alert when a declared frame
  is missing or not binary, VLF window with frame rows, and cache survival at the
  app level: `ee-cli/src/tests/blob_frames.rs`.
- Frame format itself (`text_blob.rs`): round trip, empty frame, header cost, and
  fail-closed decode for truncated/trailing/overrunning frames.
- Carrier fallback: a text-only peer gets per-line JSON with no flag and no binary
  frame (`text_only_transport_keeps_the_per_line_carrier`).
- Payload-shape tests updated for the new fields across
  `tests/{notifications,vlf_cache,vlf_edit,app,lsp_edits}.rs`.

Exit criteria:

- No per-line owned `String` on the in-process insert/update path: the backend
  encodes frame rows only (no per-row `String`), and the frontend allocates exactly
  one `String` per row for the cache it owns. The `lines` flattening duplicate is
  recorded above as a separate, deferred change.
- Text crosses the boundary once: store chunk -> frame -> frontend cache.
- Rope and VLF share one fill path (`encode_line`'s frame branch) and one decode
  path (`row_text`).
- Malformed frames are rejected without panicking and without corrupting the
  cache (pinned at both the decode and app levels).

Byte reduction, the plan's stated motivation, is delivered by the compact frame:
-19% total payload on source-like rows and -34% on escape-dense rows, measured in
the test above. The per-row-slice intermediate that measured +11% is recorded in
the same place so the reason for the frame's shape is not lost.

### Phase 4 — Vectored write (optional)

- [x] Put the row lengths in the frame and flag the blob once per op. Landed inside
      Phase 3 after the per-row-slice measurement made it necessary: `TextBlob::
      into_frame` writes `varint count || varint len_i... || bytes`, ops carry a
      `blob` flag, and `xi_core_lib::text_blob::decode_frame` validates the header
      against the payload.
- [ ] Emit the frame header plus row slices directly from rope leaves / VLF pages
      instead of the scratch buffer, so a render never copies row bytes at all.
- [ ] Gate on the probe showing the arena copy still dominates update cost.

Out of scope unless measurements say otherwise.

## Carrier selection

Carrier is chosen by transport capability, not by frontend version.

- In-process channel (`ChannelReader` / `ChannelWriter`): binary frames
  supported, blob carrier used.
- Content-length framed peers (`ContentLengthWriter`): length framing can carry
  raw bytes, binary frames supported.
- Newline framed peers (`NewlineWriter`, including the standalone
  `xi-core/src/main.rs` binary): binary frames unsupported, per-line JSON carrier
  retained. Raw bytes cannot ride a newline-terminated text frame.

Consequence: the per-line JSON carrier does not disappear in Phase 3. It becomes
the fallback for newline-framed transports. That is a transport constraint, not a
backward-compatibility shim.

## Deferred: frontend capability negotiation

Not built now. There is exactly one consumer of the `update` line payload:
ee-cli, in-process, shipped from the same workspace, so version skew is
impossible and no negotiation is needed.

If a second or out-of-process frontend ever consumes line payloads:

- [ ] Extend `client_started` params (`ee-cli/src/backend.rs:375-382`,
      `buffer/construction.rs:31-38`) with a capability list such as
      `update_blob_v1`.
- [ ] Emit the blob carrier only when advertised, per peer.

Until then, adding it would be dead negotiation surface.

## Decisions

1. Op vocabulary is frozen. Only payload carriers change.
2. The `copy` path stays payload free. It must never regress into resending text
   or spans.
3. Missing text always means "keep previous row", never "empty row".
4. Blob lifetime is one update. The frontend must not hold blob slices past the
   next update unless it copies them.
5. Caps are enforced backend side. Overflow splits ops. Content is never
   truncated silently.
6. Scope interning preserves the scope strings for prefix matching; ids are
   per-update and never reused across updates, so cached spans from earlier
   updates stay valid.
7. Carrier is selected by transport capability. No frontend-version negotiation
   until a second frontend exists.
8. Backend remains the source of truth (`RULE.md`). The frontend only caches and
   decodes.
9. No distinct-scope cap yet. Scope-table width is bounded by the loaded query
   assets, not by document content, so a cap needs a measured trigger before it
   ships; see "Distinct-scope cap: deferred".

## File size gate

Per repository rule, code over 1000 LOC is not modified directly.

- `ee-cli/src/backend.rs` 1529 LOC and `xi-core-lib/src/linewrap.rs` 1278 LOC:
  new code goes into new modules only. The `update` payload moved to
  `ee-cli/src/backend/update.rs` during Phase 1; the remaining Phase 3 frame work
  lands in `src/backend/frame.rs`.
- `xi-core-lib/src/text_store/rope_store.rs` 881 LOC and
  `xi-core-lib/src/view/render.rs` 801 LOC are approaching the limit: keep edits
  small or extract first.

## Test contract summary

- Phase 1: span decode round-trip, table lifecycle across copy ops, id rejection,
  probe shows span bytes reduced while highlighting output is unchanged.
- Phase 1b / span cache: `view/tests/syntax_cache_tests.rs` covers the scroll-back
  hit, edit and rewrap invalidation, language separation, partially covered
  requests walking instead of serving a partial window, and the size bounds.
- Phase 2: shared store conformance for borrowed chunks.
- Phase 3: payload equivalence across encodings and document shapes; frame
  validation and rejection paths; carrier fallback for newline transports; update
  application semantics unchanged for rope and VLF, including cursor merge and
  invalid rows.
- Every phase: `cargo test --quiet`, `cargo clippy`, `rustfmt` clean before
  handing off.

## Overall exit criteria

- Update frames no longer carry per-line `serde_json::Value`.
- No per-span `String` and no per-line owned `String` on the in-process
  insert/update path.
- Text crosses the boundary once: store chunk -> frame -> frontend cache.
- VLF and rope share one fill and one decode path.
- Unsupported or malformed frames degrade to the per-line JSON carrier or to
  keep-previous rows, never to dropped or corrupted content.
- Highlighting output is unchanged for existing fixtures.

## Rollback

Phase 1 and Phase 3 are separable. Phase 3 is atomic, so its rollback is
reverting one commit; keep it isolated from unrelated refactors. Phase 2 is
purely additive and can stay in place regardless.

## Risks and open questions

- Scope interning adds a table whose lifetime must survive `copy` ops. Per-update
  ids with `Arc<str>` handles avoid cross-update id reuse, at the cost of one
  allocation per distinct scope per update.
- Some grammars emit many distinct scopes (injection-heavy files). The
  distinct-scope cap and the existing `VisibleSyntaxLimits` caps bound this.
- Blob slices pin a whole update buffer per row while visible. Measure frontend
  memory before preferring slices over `Arc<str>` copies; the naive assumption
  may be backwards.
- The channel element type change touches several `String`-typed helpers in
  `ee-cli/src/backend.rs`. Mechanical but wide; do it as the first commit of
  Phase 3 with no behavior change, so the carrier switch stays reviewable.
- VLF page misses must not block assembly on IO. `Pending` handling is the
  contract, and it must keep the existing stale-row behavior.
- The VLF wrapped facade rows may not be expressible as blob slices if they
  synthesize text rather than reading it. Verify before promising full coverage.
- The probe is a manual `#[ignore]` test. Keep it working; it is the only
  measurement of payload composition in the repository.
- Probe caveat: `syntax_render_us` includes the recording peer's payload clone,
  so the payload-shape term is an upper bound. If precision matters, add a
  non-cloning peer before drawing fine-grained conclusions from it.

## References

- `CONVERT_TO_ROPE.md` — why rope-native render iteration alone does not remove
  the copy.
- `references/helix-deep-research.md` — viewport state outside the text
  structure; render only visible regions.
- `crates/xi-core-lib/src/text_store/mod.rs` — store boundary and chunk types.
- `crates/xi-rpc/src/transport.rs` — framing strategies.
- `RULE.md` — backend and frontend ownership, protocol decisions.
