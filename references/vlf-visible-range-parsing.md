# VLF Visible-Range Parsing Design

## Goal

Re-enable syntax-driven features in VLF mode without requiring a whole-buffer parse or a full-buffer clone.

Scope:

- visible-range tree-sitter parsing
- visible-range syntect fallback
- semantic motions and text objects that currently depend on full parse

Non-goals for first milestone:

- whole-file syntax trees for VLF buffers
- background parse of entire file before first render
- exact semantic coverage outside current viewport

## Current Constraint

VLF keeps file contents in `VlfStore` and only loads bounded byte windows. Existing syntax features in normal mode assume whole-buffer text is available:

- tree-sitter semantic selection and navigation parse `Rope::to_string()`
- syntect fallback expects line slices with enough prior state to reconstruct parser context

That is safe for normal buffers, but wrong for VLF because it defeats sparse loading and can scale to unbounded memory or CPU.

## Design Summary

Introduce visible-range parsing as viewport-scoped work owned by core.

Pieces:

1. `VisibleParseRequest`
   Carries `view_id`, viewport logical line range, overscan policy, generation token, language id, and parse budget.

2. `VisibleParseWindow`
   Resolved by `VlfStore` into bounded byte range plus metadata:
   `requested_lines`, `requested_bytes`, `expanded_bytes`, `decoded_text`, `starts_in_unknown_state`, `ends_in_unknown_state`.

3. Stateful lookback policy
   Expand parse window backward by bounded bytes/lines to recover parser state for multi-line constructs.
   Initial policy:
   - hard byte cap per request
   - hard line cap per request
   - stop expansion early at safe anchors when available

4. Parse cache
   Cache parse results by `(snapshot_id, generation, byte_range, language, parser_kind)`.
   Viewport parse wins over background parse.
   Evict by byte budget, not file size.

5. Partial-result protocol
   Return syntax spans only for visible lines.
   Mark edges with uncertainty flags when parse starts or ends inside incomplete state.

## Tree-Sitter Plan

### Input

Add core-side helper that reads bounded VLF text for a visible line range plus lookback slack:

- resolve `line_start..line_end` to byte range via `TextStore::line_to_byte`
- expand backward for state recovery
- expand forward for visible range plus small overscan
- decode through seam-safe `read_byte_range`

### Parse model

Use a fresh parser per request for first milestone.

Reason:

- avoids pretending incremental state is valid across disjoint windows
- keeps cancellation simple
- bounds memory to visible range

Later optimization:

- cache parse trees for overlapping windows
- incremental reparse only inside same cached window family

### Output

Return:

- syntax spans only for requested visible lines
- fold candidates only when fully contained in parsed window
- `parse_incomplete_start` / `parse_incomplete_end` flags for UI fallback decisions

### Re-enable criteria for tree-sitter features

- semantic selection and navigation read only visible-range parse output
- no call path may materialize full VLF text
- cancelled viewport generation must drop stale parse results
- tests cover multi-line constructs crossing page and parse-window boundaries

## Syntect Fallback Plan

Syntect remains fallback-only. In VLF it must never process from line `0..top+count` across full logical buffer.

Approach:

- feed syntect only visible lines plus bounded lookback lines
- maintain per-window checkpoint state in cache
- refuse fallback when checkpoint is unavailable within configured budget
- in that case render plain text for that viewport slice

This keeps syntax rendering bounded and preserves responsiveness when grammar state reconstruction would be too expensive.

## Semantic Feature Plan

Commands that require full parse today:

- expand/shrink selection
- sibling selection
- child selection
- parent-node boundary motions
- next/previous function, class, parameter, comment, test

Migration path:

1. gate them in VLF until visible-range parse exists
2. reimplement them against `VisibleParseWindow` output
3. return explicit "not in parsed range" status when target is outside available parse context
4. optionally trigger bounded prefetch when user repeats command in same direction

Rule: semantic feature may operate only when requested selection and target node are fully covered by current parse window.

## API Changes

Planned core interfaces:

- `VisibleSyntaxProvider::spans_for_viewport(view_id, line_start, line_end, generation)`
- `VisibleSyntaxProvider::semantic_context_for_selection(view_id, selection, generation)`
- `VisibleSyntaxResult { spans, incomplete_start, incomplete_end, generation }`
- `VisibleSemanticResult<T> { value, fully_covered, generation }`

`ee-tui` stays consumer only. Window resolution, parsing, caching, and uncertainty rules remain backend-owned.

## Budget and Cancellation

Per request budget must be explicit:

- max bytes decoded
- max lines decoded
- max parse time before cancel/yield
- max cached parse bytes per view and globally

Every request carries generation. Results with stale generation are dropped without UI mutation.

## Testing

Required regression coverage before re-enable:

- viewport parse over multi-line string/comment crossing window boundary
- viewport parse over UTF-8 seam at page boundary
- semantic motion inside fully covered window succeeds
- semantic motion outside covered window returns bounded status, not full parse
- syntect fallback in VLF never scans from file start
- memory budget test proves no whole-buffer clone for VLF syntax path

## Re-enable Checklist

- visible-range tree-sitter request path implemented
- bounded lookback and cancellation implemented
- syntax spans emitted only for visible lines
- semantic commands consume visible-range parse output
- syntect fallback uses bounded checkpoints only
- tests prove no whole-buffer parse or clone in VLF

Until all checklist items are complete, VLF should keep syntax and semantic parse-dependent features disabled.
