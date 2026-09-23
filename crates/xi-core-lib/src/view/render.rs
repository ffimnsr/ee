//! `impl View` methods: render.
use super::*;

impl View {
    pub(super) fn encode_line(
        &self,
        line: VisualLine,
        text: Option<&dyn RenderSource>,
        syntax_spans: &[VisibleSyntaxSpan],
        last_pos: usize,
        logical_line: usize,
    ) -> Value {
        let start_pos = line.interval.start;
        let pos = line.interval.end;
        let mut cursors = Vec::new();
        let mut selections = Vec::new();
        for region in self.selection.regions_in_range(start_pos, pos) {
            // cursor
            let c = region.end;

            if (c > start_pos && c < pos)
                || (!region.is_upstream() && c == start_pos)
                || (region.is_upstream() && c == pos)
                || (c == pos && c == last_pos)
            {
                cursors.push(c - start_pos);
            }

            // selection with interior
            let sel_start_ix = clamp(region.min(), start_pos, pos) - start_pos;
            let sel_end_ix = clamp(region.max(), start_pos, pos) - start_pos;
            if sel_end_ix > sel_start_ix {
                selections.push((sel_start_ix, sel_end_ix));
            }
        }

        let mut hls = Vec::new();

        if self.highlight_find {
            for find in &self.find {
                let mut cur_hls = Vec::new();
                for region in find.occurrences().regions_in_range(start_pos, pos) {
                    let sel_start_ix = clamp(region.min(), start_pos, pos) - start_pos;
                    let sel_end_ix = clamp(region.max(), start_pos, pos) - start_pos;
                    if sel_end_ix > sel_start_ix {
                        cur_hls.push((sel_start_ix, sel_end_ix));
                    }
                }
                hls.push(cur_hls);
            }
        }

        let mut result = json!({});

        if let Some(text) = text {
            if let ReadResult::Ready(chunk) = text.read_range(start_pos, pos) {
                result["text"] = json!(chunk);
            }
            // `Pending`/`Cancelled` reads leave `text` out of the payload; the
            // frontend keeps its previous line content until the next repaint.
        }
        let encoded_syntax_spans = self.encode_syntax_spans(syntax_spans);
        if !encoded_syntax_spans.is_empty() {
            result["syntax_spans"] = Value::Array(encoded_syntax_spans);
        }
        if !cursors.is_empty() {
            result["cursor"] = json!(cursors);
        }
        // Logical line number (0-based), only on the first visual row of a
        // logical line: `line_num` is `None` on wrapped continuation rows,
        // letting frontends leave the gutter blank there.
        if line.line_num.is_some() {
            result["ln"] = json!(logical_line.saturating_sub(1));
        }
        result
    }

    pub(super) fn encode_syntax_spans(&self, syntax_spans: &[VisibleSyntaxSpan]) -> Vec<Value> {
        syntax_spans
            .iter()
            .filter(|span| span.end_byte > span.start_byte)
            .map(|span| {
                json!({
                    "start_byte": span.start_byte,
                    "end_byte": span.end_byte,
                    "scope": span.scope,
                })
            })
            .collect()
    }

    pub(super) fn backend_syntax_spans_for_segment(
        &self,
        text: &dyn RenderSource,
        start_line: usize,
        line_count: usize,
        language_name: &str,
        syntax_enabled: bool,
    ) -> Vec<Vec<VisibleSyntaxSpan>> {
        // Non-rope sources (VLF) stay gated here: the windowed
        // `vlf_visible_syntax_spans` memo served the deleted `vlf_chunks`
        // channel, and the current frontend consumes no line syntax spans
        // from the `update` stream (no TUI highlighter). Revisit when a
        // consumer exists — the windowed parse + memo are unchanged.
        let Some(rope) = text.as_rope() else {
            return vec![Vec::new(); line_count];
        };
        if !syntax_enabled || line_count == 0 {
            return vec![Vec::new(); line_count];
        }

        let parse_from_document_start = language_name.eq_ignore_ascii_case("yaml");
        let context_start_line = if parse_from_document_start {
            0
        } else {
            start_line.saturating_sub(BACKEND_SYNTAX_CONTEXT_LINES)
        };
        let context_lines = if parse_from_document_start {
            self.lines.iter_lines(rope, context_start_line).collect::<Vec<VisualLine>>()
        } else {
            let context_line_count = line_count
                + start_line.saturating_sub(context_start_line)
                + BACKEND_SYNTAX_CONTEXT_LINES;
            self.lines
                .iter_lines(rope, context_start_line)
                .take(context_line_count)
                .collect::<Vec<VisualLine>>()
        };

        if context_lines.is_empty() {
            return vec![Vec::new(); line_count];
        }

        let chunk_start = context_lines.first().map(|line| line.interval.start()).unwrap_or(0);
        let chunk_end = context_lines.last().map(|line| line.interval.end()).unwrap_or(chunk_start);
        if chunk_end <= chunk_start {
            return vec![Vec::new(); line_count];
        }

        let chunk_text = rope.slice_to_cow(chunk_start..chunk_end).into_owned();
        let segments = context_lines
            .iter()
            .map(|line| {
                line.interval.start().saturating_sub(chunk_start)
                    ..line.interval.end().saturating_sub(chunk_start)
            })
            .collect::<Vec<_>>();

        let rendered_syntax = chunk_syntax_spans(
            language_name,
            &chunk_text,
            &segments,
            VisibleSyntaxLimits {
                timeout: BACKEND_SYNTAX_TIMEOUT,
                ..VisibleSyntaxLimits::default()
            },
        );
        let skip = start_line.saturating_sub(context_start_line);
        let mut segment_syntax =
            rendered_syntax.into_iter().skip(skip).take(line_count).collect::<Vec<_>>();
        while segment_syntax.len() < line_count {
            segment_syntax.push(Vec::new());
        }
        segment_syntax
    }

    pub(super) fn send_update_for_plan(
        &mut self,
        text: &dyn RenderSource,
        client: &Client,
        plan: &RenderPlan,
        pristine: bool,
        language_name: &str,
        syntax_enabled: bool,
    ) {
        // Wrap-aware visual math, find, and plugin annotations are rope-backed
        // in Stage A; non-rope sources (VLF) render without them (Phase 2
        // enables cursor/selection annotations behind a feature gate).
        let rope = text.as_rope();

        // every time current visible range changes, annotations are sent to frontend
        let start_off = match rope {
            Some(rope) => self.offset_of_line(rope, self.first_line),
            None => render_line_byte(text, self.first_line),
        };
        let end_off = match rope {
            Some(rope) => self.offset_of_line(rope, self.first_line + self.height + 2),
            None => render_line_byte(text, self.first_line + self.height + 2),
        };
        // Drive the source's read-ahead window from the rendered range (the
        // VLF pager + syntax semantic window; rope ignores it).
        text.set_viewport(start_off, end_off);
        let visible_range = Interval::new(start_off, end_off);
        let annotations = if let Some(rope) = rope {
            let selection_annotations =
                self.selection.get_annotations(visible_range, self, rope).to_json();
            let find_annotations =
                self.find.iter().map(|f| f.get_annotations(visible_range, self, rope).to_json());
            let plugin_annotations =
                self.annotations.iter_range(self, rope, visible_range).map(|a| a.to_json());

            iter::once(selection_annotations)
                .chain(find_annotations)
                .chain(plugin_annotations)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        // A shadow whose line count differs from the plan (stale spans from a
        // pre-store init render, a span-less shadow after a degenerate
        // zero-height render, or an approximate height change while the VLF
        // index builds) truncates the plan iterator at the shadow edge: lines
        // beyond it never get Render segments and the frontend would be told
        // `copy` forever. Re-seed like `set_dirty` to force a full-window
        // render. Rope-backed views keep shadow == plan via
        // `lc_shadow.edit`/`set_dirty` on edits, so this is a no-op there.
        let shadow_lines = self.lc_shadow.spans().iter().map(|s| s.n).sum::<usize>();
        let plan_lines = plan.spans.iter().map(|(n, _)| *n).sum::<usize>();
        if shadow_lines != plan_lines {
            self.set_dirty(text);
        }

        if !self.lc_shadow.needs_render(plan) {
            let total_lines = self.render_height(text);
            let update = Update {
                ops: vec![UpdateOp::copy(total_lines, 1)],
                pristine,
                annotations,
                vlf_total_lines: vlf_total_lines_for(text),
            };
            client.update_view(self.view_id, &update);
            return;
        }

        // send updated find status only if there have been changes
        if self.find_changed != FindStatusChange::None {
            if let Some(rope) = rope {
                let matches_only = self.find_changed == FindStatusChange::Matches;
                let store = rope_render_source(rope);
                client.find_status(self.view_id, &json!(self.find_status(&store, matches_only)));
            }
            self.find_changed = FindStatusChange::None;
        }

        // send updated replace status if changed
        if self.replace_changed {
            if let Some(replace) = self.get_replace() {
                client.replace_status(self.view_id, &json!(replace))
            }
        }

        let mut b = line_cache_shadow::Builder::new();
        let mut ops = Vec::new();
        let mut line_num = 0; // tracks old line cache

        for seg in self.lc_shadow.iter_with_plan(plan) {
            match seg.tactic {
                RenderTactic::Discard => {
                    ops.push(UpdateOp::invalidate(seg.n));
                    b.add_span(seg.n, 0, 0);
                }
                RenderTactic::Preserve | RenderTactic::Render => {
                    // Depending on the state of TEXT_VALID, SYNTAX_VALID and
                    // CURSOR_VALID, perform one of the following actions:
                    //
                    //   - All the three are valid => send the "copy" op
                    //     (+leading "skip" to catch up with "ln" to update);
                    //
                    //   - Text and syntax are valid, cursors are not => same,
                    //     but send an "update" op instead of "copy" to move
                    //     the cursors;
                    //
                    //   - Text or syntax are invalid:
                    //     => send "invalidate" if RenderTactic is "Preserve";
                    //     => send "skip"+"insert" (recreate the lines) if
                    //        RenderTactic is "Render".
                    if (seg.validity & line_cache_shadow::TEXT_VALID) != 0
                        && (seg.validity & line_cache_shadow::SYNTAX_VALID) != 0
                    {
                        let n_skip = seg.their_line_num - line_num;
                        if n_skip > 0 {
                            ops.push(UpdateOp::skip(n_skip));
                        }
                        let line_offset = match rope {
                            Some(rope) => self.offset_of_line(rope, seg.our_line_num),
                            None => render_line_byte(text, seg.our_line_num),
                        };
                        let logical_line = match rope {
                            Some(rope) => rope.line_of_offset(line_offset),
                            // No wrapping: visual line == logical line.
                            None => seg.our_line_num,
                        };
                        // Wrapped VLF emits more rows than the segment's
                        // logical count; the shadow tracks rows (frontend
                        // cache units), keeping the plan/height drift
                        // consistent for the reseed on the next repaint.
                        let mut span_rows = seg.n;
                        if (seg.validity & line_cache_shadow::CURSOR_VALID) != 0 {
                            // ALL_VALID; copy lines as-is
                            ops.push(UpdateOp::copy(seg.n, logical_line + 1));
                        } else {
                            // !CURSOR_VALID; update cursors.  `iter_lines`
                            // reports logical line numbers 1-based.
                            let start_line = seg.our_line_num;
                            let mut running_logical = match rope {
                                Some(rope) => {
                                    rope.line_of_offset(self.offset_of_line(rope, start_line)) + 1
                                }
                                None => start_line + 1,
                            };

                            let encoded_lines = if rope.is_none()
                                && let Some(cols) = self.facade_wrap_cols()
                            {
                                wrapped_facade_rows(text, start_line, seg.n, cols)
                                    .into_iter()
                                    .map(|(l, logical)| {
                                        self.encode_line(
                                            l,
                                            /* text = */ None,
                                            &[],
                                            text.len_bytes(),
                                            logical,
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            } else {
                                let syntax_spans = self.backend_syntax_spans_for_segment(
                                    text,
                                    start_line,
                                    seg.n,
                                    language_name,
                                    syntax_enabled,
                                );
                                self.iter_render_lines(text, start_line)
                                    .take(seg.n)
                                    .zip(syntax_spans)
                                    .map(|(line, syntax)| {
                                        if let Some(n) = line.line_num {
                                            running_logical = n;
                                        }
                                        self.encode_line(
                                            line,
                                            Some(text),
                                            &syntax,
                                            text.len_bytes(),
                                            running_logical,
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            };

                            let logical_line_opt =
                                if logical_line == 0 { None } else { Some(logical_line + 1) };
                            // Wrapped VLF emits more rows than the segment's
                            // logical count; the shadow tracks rows (frontend
                            // cache units), keeping the plan/height drift
                            // consistent for the reseed on the next repaint.
                            if rope.is_none() && self.facade_wrap_cols().is_some() {
                                span_rows = encoded_lines.len();
                            }
                            ops.push(UpdateOp::update(encoded_lines, logical_line_opt));
                        }
                        b.add_span(span_rows, seg.our_line_num, seg.validity);
                        line_num = seg.their_line_num + span_rows;
                    } else if seg.tactic == RenderTactic::Preserve {
                        ops.push(UpdateOp::invalidate(seg.n));
                        b.add_span(seg.n, 0, 0);
                    } else if seg.tactic == RenderTactic::Render {
                        let start_line = seg.our_line_num;
                        let mut running_logical = match rope {
                            Some(rope) => {
                                rope.line_of_offset(self.offset_of_line(rope, seg.our_line_num)) + 1
                            }
                            None => seg.our_line_num + 1,
                        };
                        let encoded_lines = if rope.is_none()
                            && let Some(cols) = self.facade_wrap_cols()
                        {
                            // Wrapped VLF (Phase 4): the width pass emits one
                            // row per visual segment of each logical line; the
                            // span count (logical) differs from the row count,
                            // so the shadow naturally drifts and the mismatch
                            // seed re-renders the window on the next repaint.
                            // Backend syntax stays on the VLF windowed path
                            // (empty spans here), matching Phase 1 gating.
                            wrapped_facade_rows(text, start_line, seg.n, cols)
                                .into_iter()
                                .map(|(l, logical)| {
                                    self.encode_line(l, Some(text), &[], text.len_bytes(), logical)
                                })
                                .collect::<Vec<_>>()
                        } else {
                            let syntax_spans = self.backend_syntax_spans_for_segment(
                                text,
                                start_line,
                                seg.n,
                                language_name,
                                syntax_enabled,
                            );
                            self.iter_render_lines(text, start_line)
                                .take(seg.n)
                                .zip(syntax_spans)
                                .map(|(line, syntax)| {
                                    if let Some(n) = line.line_num {
                                        running_logical = n;
                                    }
                                    self.encode_line(
                                        line,
                                        Some(text),
                                        &syntax,
                                        text.len_bytes(),
                                        running_logical,
                                    )
                                })
                                .collect::<Vec<_>>()
                        };
                        // The shadow tracks rows (frontend cache units). VLF
                        // rows can differ from the segment's logical count:
                        // wrapped windows emit more rows than logical lines,
                        // and an approximate total can overshoot the real line
                        // count so a tail segment yields fewer rows than
                        // planned. Both drifts reseed the window on the next
                        // repaint. Rope-backed views must match exactly.
                        let span_rows = if rope.is_none() {
                            encoded_lines.len()
                        } else {
                            debug_assert_eq!(encoded_lines.len(), seg.n);
                            seg.n
                        };
                        ops.push(UpdateOp::insert(encoded_lines));
                        b.add_span(span_rows, seg.our_line_num, line_cache_shadow::ALL_VALID);
                    }
                }
            }
        }

        self.lc_shadow = b.build();
        for find in &mut self.find {
            find.set_hls_dirty(false)
        }

        let update =
            Update { ops, pristine, annotations, vlf_total_lines: vlf_total_lines_for(text) };
        client.update_view(self.view_id, &update);
    }

    /// Determines the current number of find results and search parameters to send them to
    /// the frontend. Rope-backed sources only; find is rope-bound in Stage A.
    pub fn find_status(&self, text: &dyn RenderSource, matches_only: bool) -> Vec<FindStatus> {
        let Some(rope) = text.as_rope() else {
            return Vec::new();
        };
        self.find
            .iter()
            .map(|find| find.find_status(self, rope, matches_only))
            .collect::<Vec<FindStatus>>()
    }

    /// Update front-end with any changes to view since the last time sent.
    /// The `pristine` argument indicates whether or not the buffer has
    /// unsaved changes.
    pub fn render_if_dirty(
        &mut self,
        text: &dyn RenderSource,
        client: &Client,
        pristine: bool,
        language_name: &str,
        syntax_enabled: bool,
    ) {
        let height = self.render_height(text);
        let plan = RenderPlan::create(height, self.first_line, self.height);
        self.send_update_for_plan(text, client, &plan, pristine, language_name, syntax_enabled);
        if let Some(new_scroll_pos) = self.scroll_to.take() {
            let (line, col) = match text.as_rope() {
                Some(rope) => self.offset_to_line_col(rope, new_scroll_pos),
                None => render_line_col(text, new_scroll_pos),
            };
            client.scroll_to(self.view_id, line, col);
        }
    }

    // Send the requested lines even if they're outside the current scroll region.
    pub fn request_lines(
        &mut self,
        text: &dyn RenderSource,
        client: &Client,
        first_line: usize,
        last_line: usize,
        pristine: bool,
        language_name: &str,
        syntax_enabled: bool,
    ) {
        let height = self.render_height(text);
        let mut plan = RenderPlan::create(height, self.first_line, self.height);
        plan.request_lines(first_line, last_line);
        self.send_update_for_plan(text, client, &plan, pristine, language_name, syntax_enabled);
    }

    /// Invalidates front-end's entire line cache, forcing a full render at the next
    /// update cycle. This should be a last resort, updates should generally cause
    /// finer grain invalidation.
    pub fn set_dirty(&mut self, text: &dyn RenderSource) {
        let height = self.render_height(text);
        let mut b = line_cache_shadow::Builder::new();
        b.add_span(height, 0, 0);
        b.set_dirty(true);
        self.lc_shadow = b.build();
    }

    /// Returns the byte range of the currently visible lines.
    pub(super) fn interval_of_visible_region(&self, text: &dyn RenderSource) -> Interval {
        let start = match text.as_rope() {
            Some(rope) => self.offset_of_line(rope, self.first_line),
            None => render_line_byte(text, self.first_line),
        };
        let end = match text.as_rope() {
            Some(rope) => self.offset_of_line(rope, self.first_line + self.height + 1),
            None => render_line_byte(text, self.first_line + self.height + 1),
        };
        Interval::new(start, end)
    }

    /// Generate line breaks, based on current settings. Currently batch-mode,
    /// and currently in a debugging state.
    pub(crate) fn rewrap(&mut self, text: &Rope, width_cache: &mut WidthCache, client: &Client) {
        let _t = tracing::trace_span!("View::rewrap", categories = "core").entered();
        let visible = self.first_line..self.first_line + self.height;
        let inval = self.lines.rewrap_chunk(text, width_cache, client, visible);
        if let Some(InvalLines { start_line, inval_count, new_count }) = inval {
            self.lc_shadow.edit(start_line, start_line + inval_count, new_count);
        }
    }

    /// Updates the view after the text has been modified by the given `delta`.
    /// This method is responsible for updating the cursors, and also for
    /// recomputing line wraps.
    pub fn after_edit(
        &mut self,
        text: &Rope,
        last_text: &Rope,
        delta: &RopeDelta,
        client: &Client,
        width_cache: &mut WidthCache,
        drift: InsertDrift,
    ) {
        let visible = self.first_line..self.first_line + self.height;
        match self.lines.after_edit(text, last_text, delta, width_cache, client, visible) {
            Some(InvalLines { start_line, inval_count, new_count }) => {
                self.lc_shadow.edit(start_line, start_line + inval_count, new_count);
            }
            None => self.set_dirty(&rope_render_source(text)),
        }

        // Any edit cancels a drag. This is good behavior for edits initiated through
        // the front-end, but perhaps not for async edits.
        self.drag_state = None;

        // all annotations that come after the edit need to be invalidated
        let (iv, _) = delta.summary();
        self.annotations.invalidate(iv);

        // update only find highlights affected by change
        for find in &mut self.find {
            find.update_highlights(text, delta);
            self.find_changed = FindStatusChange::All;
        }

        // Note: for committing plugin edits, we probably want to know the priority
        // of the delta so we can set the cursor before or after the edit, as needed.
        let new_sel = self.selection.apply_delta(delta, true, drift);
        self.set_selection_for_edit(text, new_sel);
    }

    // ── RenderSource helpers ────────────────────────────────────────────────

    /// Height for `RenderPlan` creation: wrap-aware visual height for
    /// rope-backed sources, logical line count otherwise (no wrapping in
    /// Stage A).
    fn render_height(&self, text: &dyn RenderSource) -> usize {
        match text.as_rope() {
            Some(rope) => self.line_of_offset(rope, rope.len()) + 1,
            None => match text.total_lines() {
                RenderLineCount::Exact(n) | RenderLineCount::Approximate(n) => {
                    n.min(usize::MAX as u64) as usize
                }
            },
        }
    }

    /// Iterates visual lines from `start_line`: the wrap-aware `Lines`
    /// iterator for rope-backed sources (byte-identical with the pre-facade
    /// path), and a 1:1 logical-line walk for non-rope sources.
    fn iter_render_lines<'a>(
        &'a self,
        text: &'a dyn RenderSource,
        start_line: usize,
    ) -> Box<dyn Iterator<Item = VisualLine> + 'a> {
        match text.as_rope() {
            Some(rope) => Box::new(self.lines.iter_lines(rope, start_line)),
            None => Box::new(facade_iter_lines(text, start_line)),
        }
    }

    /// Active byte-column wrap width for non-rope sources, when VLF wrapping
    /// is enabled (Phase 4). `None` keeps 1:1 rows. Frontend-measured width
    /// wrapping (`word_wrap`) stays unsupported for VLF: the windowed width
    /// pass measures monospace codepoints.
    fn facade_wrap_cols(&self) -> Option<usize> {
        if !self.vlf_wrap {
            return None;
        }
        match self.lines.wrap_width() {
            WrapWidth::Bytes(cols) if cols > 0 => Some(cols),
            _ => None,
        }
    }
}

/// Wrapped VLF rows for `count` logical lines starting at `start_line`
/// (Phase 4): a width pass over each logical line's decoded chunk, emitting
/// one `VisualLine` per visual segment. The paired `usize` is the row's
/// logical line number (1-based, matching `VisualLine::line_num` semantics).
///
/// Window-bounded: only the requested logical window is read, and a line that
/// cannot be decoded (index lag / oversized read) degrades to a single
/// placeholder row.
fn wrapped_facade_rows(
    text: &dyn RenderSource,
    start_line: usize,
    count: usize,
    cols: usize,
) -> Vec<(VisualLine, usize)> {
    let total = match text.total_lines() {
        RenderLineCount::Exact(n) | RenderLineCount::Approximate(n) => n as usize,
    };
    let mut rows = Vec::new();
    for (line, logical) in (start_line..(start_line + count).min(total)).zip(start_line + 1..) {
        let start = render_line_byte(text, line);
        let raw_end = render_line_byte(text, line + 1);
        let intervals: Vec<(usize, usize)> = match text.read_range(start, raw_end) {
            ReadResult::Ready(chunk) => wrap_row_intervals(&chunk, cols)
                .into_iter()
                .map(|(rel_start, rel_end)| (start + rel_start, start + rel_end))
                .collect(),
            _ => vec![(start, raw_end)],
        };
        for (index, (row_start, row_end)) in intervals.into_iter().enumerate() {
            let line_num = if index == 0 { Some(logical) } else { None };
            rows.push((
                VisualLine { interval: Interval::new(row_start, row_end), line_num },
                logical,
            ));
        }
    }
    rows
}

/// Greedy monospace width pass over one decoded line chunk, mirroring the
/// rope `WrapWidth::Bytes` path (`CodepointMono`: one unit per codepoint):
/// hard breaks always end a row (terminator included), soft breaks fall
/// through until the budget overflows, and a single word wider than the
/// budget is split at its end. Returns byte intervals within `chunk`.
pub(super) fn wrap_row_intervals(chunk: &str, cols: usize) -> Vec<(usize, usize)> {
    use xi_unicode::LineBreakLeafIter;

    if chunk.is_empty() {
        // Empty logical line: one empty row, like the rope path.
        return vec![(0, 0)];
    }

    let mut breaks: Vec<(usize, bool)> = Vec::new();
    {
        let mut lb = LineBreakLeafIter::new(chunk, 0);
        loop {
            let (next, hard) = lb.next(chunk);
            // The leaf iterator's terminal entry sits at (or past) the chunk
            // end; process it (it closes the final word) and stop.
            let terminal = next >= chunk.len();
            breaks.push((next.min(chunk.len()), hard));
            if terminal {
                break;
            }
        }
    }

    let max_width = cols as f64;
    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut row_start = 0usize;
    let mut row_width = 0.0f64;
    let mut pos = 0usize;
    for (next, hard) in breaks {
        let width = chunk[pos..next].chars().count() as f64;
        if hard {
            if row_width != 0.0 && width + row_width > max_width {
                // Overflow before a hard break: cut at the previous word start
                // (same policy as the rope rewrap task), then end the line at
                // the hard break.
                rows.push((row_start, pos));
                row_start = pos;
            }
            rows.push((row_start, next));
            row_start = next;
            row_width = 0.0;
        } else if row_width == 0.0 && width >= max_width {
            // A single word that alone fills (or exceeds) the budget.
            rows.push((row_start, next));
            row_start = next;
            row_width = 0.0;
        } else if row_width + width > max_width {
            rows.push((row_start, pos));
            row_start = pos;
            row_width = width;
        } else {
            row_width += width;
        }
        pos = next;
    }
    if row_start < chunk.len() {
        rows.push((row_start, chunk.len()));
    }
    rows
}

/// Byte offset of `line`, clamping to EOF on pending/out-of-range lookups
/// (mirrors the VLF viewport fallback semantics; the window renders empty at
/// the tail and the frontend retries on the next repaint).
fn render_line_byte(text: &dyn RenderSource, line: usize) -> usize {
    match text.line_to_byte(line as u64) {
        LineLookup::Exact(o) | LineLookup::Approximate(o) => o.0 as usize,
        _ => text.len_bytes(),
    }
}

/// `(line, col)` for a non-rope source (no wrapping): logical line plus the
/// byte column within it.
fn render_line_col(text: &dyn RenderSource, offset: usize) -> (usize, usize) {
    let line = text.byte_to_line(offset).unwrap_or(0) as usize;
    (line, offset.saturating_sub(render_line_byte(text, line)))
}

/// VLF-only total-line metadata for the `update` payload; rope-backed sources
/// omit it (`None` keeps the rope wire shape byte-identical).
fn vlf_total_lines_for(text: &dyn RenderSource) -> Option<crate::client::VlfTotalLines> {
    if text.as_rope().is_some() {
        return None;
    }
    let (count, exact) = match text.total_lines() {
        RenderLineCount::Exact(n) => (n, true),
        RenderLineCount::Approximate(n) => (n, false),
    };
    Some(crate::client::VlfTotalLines { count, exact, index_progress: text.index_progress() })
}

/// No-wrap visual-line iteration for non-rope sources: every visual line is
/// one logical line. Replicates `Lines::iter_lines` with `WrapWidth::None`
/// (intervals are the hard-break spans including the terminator, EOF clamps
/// to `len`, `line_num` is the 1-based logical line, and iteration stops
/// after `total_lines` items).
pub(super) fn facade_iter_lines<'a>(
    text: &'a dyn RenderSource,
    start_line: usize,
) -> impl Iterator<Item = VisualLine> + 'a {
    let total = match text.total_lines() {
        RenderLineCount::Exact(n) | RenderLineCount::Approximate(n) => n as usize,
    };
    let mut line = start_line;
    iter::from_fn(move || {
        if line >= total {
            return None;
        }
        let start = match text.line_to_byte(line as u64) {
            LineLookup::Exact(o) | LineLookup::Approximate(o) => {
                (o.0 as usize).min(text.len_bytes())
            }
            // Index lag (Pending/OutOfRange): emit an empty placeholder row so
            // the plan segment count stays consistent; content arrives on the
            // next repaint once the index catches up.
            _ => text.len_bytes(),
        };
        let next = line + 1;
        let end = match text.line_to_byte(next as u64) {
            LineLookup::Exact(o) | LineLookup::Approximate(o) => {
                (o.0 as usize).min(text.len_bytes())
            }
            _ => text.len_bytes(),
        };
        line = next;
        Some(VisualLine { interval: Interval::new(start, end), line_num: Some(line) })
    })
}
