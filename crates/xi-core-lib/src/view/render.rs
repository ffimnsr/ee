//! `impl View` methods: render.
use super::*;

impl View {
    pub(super) fn encode_line(
        &self,
        line: VisualLine,
        text: Option<&Rope>,
        syntax_spans: &[VisibleSyntaxSpan],
        last_pos: usize,
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
            result["text"] = json!(text.slice_to_cow(start_pos..pos));
        }
        let encoded_syntax_spans = self.encode_syntax_spans(syntax_spans);
        if !encoded_syntax_spans.is_empty() {
            result["syntax_spans"] = Value::Array(encoded_syntax_spans);
        }
        if !cursors.is_empty() {
            result["cursor"] = json!(cursors);
        }
        if let Some(line_num) = line.line_num {
            result["ln"] = json!(line_num);
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
        text: &Rope,
        start_line: usize,
        line_count: usize,
        language_name: &str,
        syntax_enabled: bool,
    ) -> Vec<Vec<VisibleSyntaxSpan>> {
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
            self.lines.iter_lines(text, context_start_line).collect::<Vec<VisualLine>>()
        } else {
            let context_line_count = line_count
                + start_line.saturating_sub(context_start_line)
                + BACKEND_SYNTAX_CONTEXT_LINES;
            self.lines
                .iter_lines(text, context_start_line)
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

        let chunk_text = text.slice_to_cow(chunk_start..chunk_end).into_owned();
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
        text: &Rope,
        client: &Client,
        plan: &RenderPlan,
        pristine: bool,
        language_name: &str,
        syntax_enabled: bool,
    ) {
        // every time current visible range changes, annotations are sent to frontend
        let start_off = self.offset_of_line(text, self.first_line);
        let end_off = self.offset_of_line(text, self.first_line + self.height + 2);
        let visible_range = Interval::new(start_off, end_off);
        let selection_annotations =
            self.selection.get_annotations(visible_range, self, text).to_json();
        let find_annotations =
            self.find.iter().map(|f| f.get_annotations(visible_range, self, text).to_json());
        let plugin_annotations =
            self.annotations.iter_range(self, text, visible_range).map(|a| a.to_json());

        let annotations = iter::once(selection_annotations)
            .chain(find_annotations)
            .chain(plugin_annotations)
            .collect::<Vec<_>>();

        if !self.lc_shadow.needs_render(plan) {
            let total_lines = self.line_of_offset(text, text.len()) + 1;
            let update =
                Update { ops: vec![UpdateOp::copy(total_lines, 1)], pristine, annotations };
            client.update_view(self.view_id, &update);
            return;
        }

        // send updated find status only if there have been changes
        if self.find_changed != FindStatusChange::None {
            let matches_only = self.find_changed == FindStatusChange::Matches;
            client.find_status(self.view_id, &json!(self.find_status(text, matches_only)));
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
                        let line_offset = self.offset_of_line(text, seg.our_line_num);
                        let logical_line = text.line_of_offset(line_offset);
                        if (seg.validity & line_cache_shadow::CURSOR_VALID) != 0 {
                            // ALL_VALID; copy lines as-is
                            ops.push(UpdateOp::copy(seg.n, logical_line + 1));
                        } else {
                            // !CURSOR_VALID; update cursors
                            let start_line = seg.our_line_num;

                            let encoded_lines = self
                                .lines
                                .iter_lines(text, start_line)
                                .take(seg.n)
                                .map(|l| {
                                    self.encode_line(l, /* text = */ None, &[], text.len())
                                })
                                .collect::<Vec<_>>();

                            let logical_line_opt =
                                if logical_line == 0 { None } else { Some(logical_line + 1) };
                            ops.push(UpdateOp::update(encoded_lines, logical_line_opt));
                        }
                        b.add_span(seg.n, seg.our_line_num, seg.validity);
                        line_num = seg.their_line_num + seg.n;
                    } else if seg.tactic == RenderTactic::Preserve {
                        ops.push(UpdateOp::invalidate(seg.n));
                        b.add_span(seg.n, 0, 0);
                    } else if seg.tactic == RenderTactic::Render {
                        let start_line = seg.our_line_num;
                        let syntax_spans = self.backend_syntax_spans_for_segment(
                            text,
                            start_line,
                            seg.n,
                            language_name,
                            syntax_enabled,
                        );
                        let encoded_lines = self
                            .lines
                            .iter_lines(text, start_line)
                            .take(seg.n)
                            .zip(syntax_spans)
                            .map(|(line, syntax)| {
                                self.encode_line(line, Some(text), &syntax, text.len())
                            })
                            .collect::<Vec<_>>();
                        debug_assert_eq!(encoded_lines.len(), seg.n);
                        ops.push(UpdateOp::insert(encoded_lines));
                        b.add_span(seg.n, seg.our_line_num, line_cache_shadow::ALL_VALID);
                    }
                }
            }
        }

        self.lc_shadow = b.build();
        for find in &mut self.find {
            find.set_hls_dirty(false)
        }

        let update = Update { ops, pristine, annotations };
        client.update_view(self.view_id, &update);
    }

    /// Determines the current number of find results and search parameters to send them to
    /// the frontend.
    pub fn find_status(&self, text: &Rope, matches_only: bool) -> Vec<FindStatus> {
        self.find
            .iter()
            .map(|find| find.find_status(self, text, matches_only))
            .collect::<Vec<FindStatus>>()
    }

    /// Update front-end with any changes to view since the last time sent.
    /// The `pristine` argument indicates whether or not the buffer has
    /// unsaved changes.
    pub fn render_if_dirty(
        &mut self,
        text: &Rope,
        client: &Client,
        pristine: bool,
        language_name: &str,
        syntax_enabled: bool,
    ) {
        let height = self.line_of_offset(text, text.len()) + 1;
        let plan = RenderPlan::create(height, self.first_line, self.height);
        self.send_update_for_plan(text, client, &plan, pristine, language_name, syntax_enabled);
        if let Some(new_scroll_pos) = self.scroll_to.take() {
            let (line, col) = self.offset_to_line_col(text, new_scroll_pos);
            client.scroll_to(self.view_id, line, col);
        }
    }

    // Send the requested lines even if they're outside the current scroll region.
    pub fn request_lines(
        &mut self,
        text: &Rope,
        client: &Client,
        first_line: usize,
        last_line: usize,
        pristine: bool,
        language_name: &str,
        syntax_enabled: bool,
    ) {
        let height = self.line_of_offset(text, text.len()) + 1;
        let mut plan = RenderPlan::create(height, self.first_line, self.height);
        plan.request_lines(first_line, last_line);
        self.send_update_for_plan(text, client, &plan, pristine, language_name, syntax_enabled);
    }

    /// Invalidates front-end's entire line cache, forcing a full render at the next
    /// update cycle. This should be a last resort, updates should generally cause
    /// finer grain invalidation.
    pub fn set_dirty(&mut self, text: &Rope) {
        let height = self.line_of_offset(text, text.len()) + 1;
        let mut b = line_cache_shadow::Builder::new();
        b.add_span(height, 0, 0);
        b.set_dirty(true);
        self.lc_shadow = b.build();
    }

    /// Returns the byte range of the currently visible lines.
    pub(super) fn interval_of_visible_region(&self, text: &Rope) -> Interval {
        let start = self.offset_of_line(text, self.first_line);
        let end = self.offset_of_line(text, self.first_line + self.height + 1);
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
            None => self.set_dirty(text),
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
}
