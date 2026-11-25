use super::*;

use super::parse::{QueryWarmup, runtime_language};

// ---------------------------------------------------------------------------
// VLF visible-range highlighting
// ---------------------------------------------------------------------------

/// Parse only the supplied visible text and return line-relative syntax spans.
///
/// This is intentionally bounded by bytes, wall time, match count, and capture
/// count so VLF highlighting cannot become whole-file work. The input must be a
/// viewport chunk, optionally with small overscan; callers must not pass full
/// VLF file contents. This path also avoids warming runtime query caches so
/// query inheritance and runtime reload only affect whole-buffer query
/// consumers, not viewport-bounded syntax work.
#[cfg(test)]
pub(crate) fn visible_syntax_spans(
    language_name: &str,
    visible_text: &str,
    limits: VisibleSyntaxLimits,
) -> Vec<Vec<VisibleSyntaxSpan>> {
    let line_starts = line_start_offsets(visible_text);
    let segments = line_segments(visible_text, &line_starts);
    chunk_syntax_spans_with_depth(
        language_name,
        visible_text,
        &segments,
        0..segments.len(),
        limits,
        0,
        Instant::now(),
    )
}

pub(super) fn chunk_syntax_spans_with_depth(
    language_name: &str,
    chunk_text: &str,
    segments: &[Range<usize>],
    requested: Range<usize>,
    limits: VisibleSyntaxLimits,
    injection_depth: usize,
    started_at: Instant,
) -> Vec<Vec<VisibleSyntaxSpan>> {
    let mut per_segment = vec![Vec::new(); segments.len().max(1)];
    if chunk_text.is_empty()
        || chunk_text.len() > limits.max_bytes
        || limits.timeout.is_zero()
        || limits.max_matches == 0
        || limits.max_captures == 0
    {
        return per_segment;
    }

    let Some(language) = runtime_language(Some(language_name), None, QueryWarmup::None) else {
        return per_segment;
    };

    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return per_segment;
    }
    let mut progress = |_: &tree_sitter::ParseState| {
        if started_at.elapsed() >= limits.timeout {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = chunk_text.as_bytes();
    let mut read = |offset: usize, _: Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: Some(&mut progress) };
    #[cfg(test)]
    let stage_started = Instant::now();
    let Some(tree) = parser.parse_with_options(&mut read, None, Some(options)) else {
        return per_segment;
    };
    if started_at.elapsed() >= limits.timeout {
        return per_segment;
    }
    #[cfg(test)]
    if injection_depth == 0 {
        crate::syntax_span_probe::record(
            crate::syntax_span_probe::SpanStage::Parse,
            stage_started.elapsed(),
        );
    }

    #[cfg(test)]
    let stage_started = Instant::now();
    let highlighted = apply_highlight_query_spans(
        language_name,
        chunk_text,
        segments,
        &requested,
        &tree,
        &mut per_segment,
        limits,
        started_at,
    );
    #[cfg(test)]
    if injection_depth == 0 {
        crate::syntax_span_probe::record(
            crate::syntax_span_probe::SpanStage::HighlightWalk,
            stage_started.elapsed(),
        );
    }
    if !highlighted && requested.start < requested.end {
        #[cfg(test)]
        let stage_started = Instant::now();
        let mut state = VisibleSyntaxWalk {
            text: chunk_text,
            segments,
            requested: &requested,
            per_segment: &mut per_segment,
            started: started_at,
            limits,
            matches: 0,
            captures: 0,
        };
        state.walk(tree.root_node());
        #[cfg(test)]
        if injection_depth == 0 {
            crate::syntax_span_probe::record(
                crate::syntax_span_probe::SpanStage::FallbackWalk,
                stage_started.elapsed(),
            );
        }
    }
    if injection_depth < MAX_VISIBLE_INJECTION_DEPTH {
        #[cfg(test)]
        let stage_started = Instant::now();
        apply_injection_spans(
            language_name,
            chunk_text,
            segments,
            &requested,
            &tree,
            &mut per_segment,
            limits,
            injection_depth,
            started_at,
        );
        #[cfg(test)]
        if injection_depth == 0 {
            crate::syntax_span_probe::record(
                crate::syntax_span_probe::SpanStage::Injection,
                stage_started.elapsed(),
            );
        }
    }
    #[cfg(test)]
    let stage_started = Instant::now();
    for spans in &mut per_segment {
        compact_visible_spans(spans);
    }
    #[cfg(test)]
    if injection_depth == 0 {
        crate::syntax_span_probe::record(
            crate::syntax_span_probe::SpanStage::Compact,
            stage_started.elapsed(),
        );
    }
    per_segment
}

#[derive(Debug, Clone)]
struct InjectionRegion {
    language_name: String,
    range: Range<usize>,
}

#[derive(Debug, Clone)]
struct InjectionSegmentMapping {
    parent_index: usize,
    parent_segment_start: usize,
    parent_range: Range<usize>,
    child_segment: Range<usize>,
}

fn apply_highlight_query_spans(
    language_name: &str,
    chunk_text: &str,
    segments: &[Range<usize>],
    requested: &Range<usize>,
    tree: &Tree,
    per_segment: &mut [Vec<VisibleSyntaxSpan>],
    limits: VisibleSyntaxLimits,
    started_at: Instant,
) -> bool {
    if started_at.elapsed() >= limits.timeout {
        return false;
    }

    with_default_runtime_loader_mut(|loader| {
        let Ok(highlights) =
            loader.compile_query_kind_transient(language_name, RuntimeQueryKind::Highlights)
        else {
            return false;
        };
        let Some(highlights) = highlights else {
            return false;
        };

        let bytes = chunk_text.as_bytes();
        let capture_names = highlights.query.capture_names();
        let mut cursor = QueryCursor::new();
        // Only pattern roots intersecting the requested byte range are walked;
        // roots outside it cannot contribute captures that overlap requested
        // rows, so the emitted spans are identical while deep windows skip the
        // parse-context prefix.
        if let Some(range) = requested_segment_bytes(segments, requested) {
            cursor.set_byte_range(range);
        }
        let mut captures = cursor.captures(&highlights.query, tree.root_node(), bytes);
        let scan_started = Instant::now();
        let mut match_count = 0usize;
        let mut capture_count = 0usize;
        let mut emitted = false;

        while scan_started.elapsed() < limits.timeout
            && match_count < limits.max_matches
            && capture_count < limits.max_captures
        {
            captures.advance();
            let Some((query_match, capture_index)) = captures.get() else {
                break;
            };
            let capture = &query_match.captures[*capture_index];
            let Some(scope) = capture_names.get(capture.index as usize) else {
                continue;
            };
            if scope.starts_with("injection.") || scope.starts_with("local.") {
                continue;
            }
            let start = capture.node.start_byte().min(chunk_text.len());
            let end = capture.node.end_byte().min(chunk_text.len());
            if end <= start {
                continue;
            }
            if requested.start >= requested.end || requested.start >= segments.len() {
                break;
            }
            // Segments are line ranges in ascending order, so a capture can
            // only overlap the ones starting before `end`; visit just those.
            // Context rows outside `requested` are not window work: they must
            // not consume the match budget, or deep windows in
            // parse-from-document-start languages (YAML) starve before the
            // walk reaches them and render without spans.
            let first_overlapping = segments.partition_point(|segment| segment.end <= start);
            if first_overlapping >= segments.len() || segments[first_overlapping].start >= end {
                continue;
            }
            let mut emitted_by_capture = false;
            for (segment_idx, segment) in segments.iter().enumerate().skip(first_overlapping) {
                if segment.start >= end {
                    break;
                }
                if !requested.contains(&segment_idx) {
                    continue;
                }
                if capture_count >= limits.max_captures {
                    break;
                }
                let span_start = start.max(segment.start).saturating_sub(segment.start);
                let span_end = end.min(segment.end).saturating_sub(segment.start);
                if span_end > span_start {
                    per_segment[segment_idx].push(VisibleSyntaxSpan {
                        start_byte: span_start,
                        end_byte: span_end,
                        scope: scope.to_string(),
                    });
                    capture_count += 1;
                    emitted_by_capture = true;
                }
            }
            if emitted_by_capture {
                match_count += 1;
                emitted = true;
            }
        }

        emitted
    })
}

fn apply_injection_spans(
    language_name: &str,
    chunk_text: &str,
    segments: &[Range<usize>],
    requested: &Range<usize>,
    tree: &Tree,
    per_segment: &mut [Vec<VisibleSyntaxSpan>],
    limits: VisibleSyntaxLimits,
    injection_depth: usize,
    started_at: Instant,
) {
    if started_at.elapsed() >= limits.timeout {
        return;
    }

    let scan_range = requested_segment_bytes(segments, requested);
    let injections =
        injection_regions(language_name, chunk_text, scan_range.as_ref(), tree, limits);
    for injection in injections {
        let Some(child_text) = chunk_text.get(injection.range.clone()) else {
            continue;
        };
        // Child spans only matter for requested parent rows; rows outside the
        // range are parse context the caller discards.
        let mappings = injection_segment_mappings(segments, &injection.range)
            .into_iter()
            .filter(|mapping| requested.contains(&mapping.parent_index))
            .collect::<Vec<_>>();
        if mappings.is_empty() {
            continue;
        }
        let child_segments =
            mappings.iter().map(|mapping| mapping.child_segment.clone()).collect::<Vec<_>>();
        let child_spans = chunk_syntax_spans_with_depth(
            &injection.language_name,
            child_text,
            &child_segments,
            0..child_segments.len(),
            limits,
            injection_depth + 1,
            Instant::now(),
        );
        for (mapping, spans) in mappings.iter().zip(child_spans) {
            if spans.is_empty() {
                continue;
            }
            remove_overlapping_spans(&mut per_segment[mapping.parent_index], &mapping.parent_range);
            for span in spans {
                let absolute_start =
                    injection.range.start + mapping.child_segment.start + span.start_byte;
                let absolute_end =
                    injection.range.start + mapping.child_segment.start + span.end_byte;
                let start_byte = absolute_start.saturating_sub(mapping.parent_segment_start);
                let end_byte = absolute_end.saturating_sub(mapping.parent_segment_start);
                if end_byte > start_byte {
                    per_segment[mapping.parent_index].push(VisibleSyntaxSpan {
                        start_byte,
                        end_byte,
                        scope: span.scope,
                    });
                }
            }
        }
    }
}

fn injection_regions(
    language_name: &str,
    chunk_text: &str,
    requested_bytes: Option<&Range<usize>>,
    tree: &Tree,
    limits: VisibleSyntaxLimits,
) -> Vec<InjectionRegion> {
    with_default_runtime_loader_mut(|loader| {
        let Ok(injections) =
            loader.compile_query_kind_transient(language_name, RuntimeQueryKind::Injections)
        else {
            return Vec::new();
        };
        let Some(injections) = injections else {
            return Vec::new();
        };
        let content_capture = injections.query.capture_index_for_name("injection.content");
        let language_capture = injections.query.capture_index_for_name("injection.language");
        let Some(content_capture) = content_capture else {
            return Vec::new();
        };

        let bytes = chunk_text.as_bytes();
        let mut cursor = QueryCursor::new();
        // Same requested-range restriction as the highlight walk: injection
        // matches whose root cannot intersect the window cannot produce child
        // spans for requested rows either.
        if let Some(range) = requested_bytes {
            cursor.set_byte_range(range.clone());
        }
        let mut captures = cursor.captures(&injections.query, tree.root_node(), bytes);
        let mut regions = Vec::new();
        let scan_started = Instant::now();
        while scan_started.elapsed() < limits.timeout && regions.len() < limits.max_matches {
            captures.advance();
            let Some((query_match, capture_index)) = captures.get() else {
                break;
            };
            if query_match.captures[*capture_index].index != content_capture {
                continue;
            }
            let Some(resolved_language) = resolve_injection_language(
                loader,
                &injections.query,
                query_match,
                language_capture,
                bytes,
            ) else {
                continue;
            };
            let node = query_match.captures[*capture_index].node;
            let start = node.start_byte().min(chunk_text.len());
            let end = node.end_byte().min(chunk_text.len());
            if end > start {
                regions
                    .push(InjectionRegion { language_name: resolved_language, range: start..end });
            }
        }
        regions.sort_by(|left, right| {
            left.range
                .start
                .cmp(&right.range.start)
                .then_with(|| left.range.end.cmp(&right.range.end))
        });
        let mut deduped = Vec::with_capacity(regions.len());
        for region in regions {
            if deduped.iter().any(|existing: &InjectionRegion| {
                region.range.start < existing.range.end && existing.range.start < region.range.end
            }) {
                continue;
            }
            deduped.push(region);
        }
        deduped
    })
}

fn resolve_injection_language(
    loader: &mut RuntimeLoader,
    query: &tree_sitter::Query,
    query_match: &tree_sitter::QueryMatch<'_, '_>,
    language_capture: Option<u32>,
    bytes: &[u8],
) -> Option<String> {
    let configured = query
        .property_settings(query_match.pattern_index)
        .iter()
        .find(|property| property.key.as_ref() == "injection.language")
        .and_then(|property| property.value.as_deref())
        .map(normalize_injection_language_identifier)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            language_capture
                .and_then(|capture| query_match.nodes_for_capture_index(capture).next())
                .and_then(|node| node.utf8_text(bytes).ok())
                .map(normalize_injection_language_identifier)
                .filter(|value| !value.is_empty())
        })?;

    loader
        .match_injection_language(&configured)
        .map(|matched| matched.canonical_id)
        .or_else(|| loader.canonical_language_name(&configured))
}

fn normalize_injection_language_identifier(value: &str) -> String {
    value.trim().trim_matches(|ch| matches!(ch, '"' | '\'' | '`')).trim().to_string()
}

/// Byte span covered by the `requested` segment indices, clamped to `segments`.
///
/// `None` when the range selects no segments (a fully past-EOF window), which
/// callers treat as "nothing to scan".
fn requested_segment_bytes(
    segments: &[Range<usize>],
    requested: &Range<usize>,
) -> Option<Range<usize>> {
    let start = requested.start.min(segments.len());
    let end = requested.end.min(segments.len()).max(start);
    (start < end).then(|| segments[start].start..segments[end - 1].end)
}

fn injection_segment_mappings(
    segments: &[Range<usize>],
    injection_range: &Range<usize>,
) -> Vec<InjectionSegmentMapping> {
    segments
        .iter()
        .enumerate()
        .filter_map(|(parent_index, segment)| {
            let start = segment.start.max(injection_range.start);
            let end = segment.end.min(injection_range.end);
            (end > start).then(|| InjectionSegmentMapping {
                parent_index,
                parent_segment_start: segment.start,
                parent_range: (start - segment.start)..(end - segment.start),
                child_segment: (start - injection_range.start)..(end - injection_range.start),
            })
        })
        .collect()
}

fn remove_overlapping_spans(spans: &mut Vec<VisibleSyntaxSpan>, range: &Range<usize>) {
    spans.retain(|span| span.end_byte <= range.start || span.start_byte >= range.end);
}

pub(super) struct VisibleSyntaxWalk<'a> {
    pub(super) text: &'a str,
    pub(super) segments: &'a [Range<usize>],
    pub(super) requested: &'a Range<usize>,
    pub(super) per_segment: &'a mut [Vec<VisibleSyntaxSpan>],
    pub(super) started: Instant,
    pub(super) limits: VisibleSyntaxLimits,
    pub(super) matches: usize,
    pub(super) captures: usize,
}

impl VisibleSyntaxWalk<'_> {
    fn exhausted(&self) -> bool {
        self.matches >= self.limits.max_matches
            || self.captures >= self.limits.max_captures
            || self.started.elapsed() >= self.limits.timeout
    }

    pub(super) fn walk(&mut self, node: Node<'_>) {
        if self.exhausted() {
            return;
        }

        if let Some(scope) = scope_for_node(node, self.text) {
            // Same window-scoped budget as the highlight walk: nodes that
            // cannot produce spans in a requested row are context, not work.
            if self.node_overlaps_requested(node) {
                self.matches += 1;
                self.push_node_spans(node, scope);
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child);
            if self.exhausted() {
                break;
            }
        }
    }

    fn node_overlaps_requested(&self, node: Node<'_>) -> bool {
        let start = node.start_byte().min(self.text.len());
        let end = node.end_byte().min(self.text.len());
        let first = self.segments.partition_point(|segment| segment.end <= start);
        self.segments
            .iter()
            .enumerate()
            .skip(first)
            .take_while(|(_, segment)| segment.start < end)
            .any(|(segment_idx, _)| self.requested.contains(&segment_idx))
    }

    fn push_node_spans(&mut self, node: Node<'_>, scope: &'static str) {
        let start = node.start_byte().min(self.text.len());
        let end = node.end_byte().min(self.text.len());
        if end <= start {
            return;
        }

        // Ordered line ranges: only the segments a node can overlap are
        // visited, and only requested ones count toward the budgets.
        let first = self.segments.partition_point(|segment| segment.end <= start);
        for (segment_idx, segment) in self.segments.iter().enumerate().skip(first) {
            if segment.start >= end {
                break;
            }
            if !self.requested.contains(&segment_idx) {
                continue;
            }
            if self.captures >= self.limits.max_captures {
                break;
            }
            let span_start = start.max(segment.start).saturating_sub(segment.start);
            let span_end = end.min(segment.end).saturating_sub(segment.start);
            if span_end > span_start {
                self.per_segment[segment_idx].push(VisibleSyntaxSpan {
                    start_byte: span_start,
                    end_byte: span_end,
                    scope: scope.to_string(),
                });
                self.captures += 1;
            }
        }
    }
}

pub(super) fn line_segments(text: &str, line_starts: &[usize]) -> Vec<Range<usize>> {
    line_starts
        .iter()
        .enumerate()
        .map(|(line, &line_start)| {
            let line_end = line_starts
                .get(line + 1)
                .copied()
                .unwrap_or(text.len())
                .saturating_sub(usize::from(line + 1 < line_starts.len()));
            line_start..line_end
        })
        .collect()
}

pub(super) fn line_start_offsets(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(idx, _)| idx + 1));
    starts
}

fn scope_for_node(node: Node<'_>, text: &str) -> Option<&'static str> {
    match node.kind() {
        "line_comment" | "block_comment" | "comment" => Some("comment.line"),
        "string_literal" | "raw_string_literal" | "interpreted_string_literal" | "string" => {
            Some("string.quoted")
        }
        "integer_literal" | "float_literal" => Some("constant.numeric.decimal"),
        "char_literal" | "boolean_literal" => Some("constant.language"),
        "primitive_type" | "predefined_type" | "type_identifier" => Some("entity.name.type"),
        kind if is_keyword_node(kind, node, text) => Some("keyword.control"),
        _ => None,
    }
}

fn compact_visible_spans(spans: &mut Vec<VisibleSyntaxSpan>) {
    if spans.is_empty() {
        return;
    }

    let source = spans.clone();
    let mut boundaries =
        source.iter().flat_map(|span| [span.start_byte, span.end_byte]).collect::<Vec<_>>();
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut compacted: Vec<VisibleSyntaxSpan> = Vec::new();
    for window in boundaries.windows(2) {
        let [start, end] = [window[0], window[1]];
        if end <= start {
            continue;
        }

        let best = source
            .iter()
            .filter(|span| span.start_byte <= start && span.end_byte >= end)
            .max_by_key(|span| (scope_priority(&span.scope), span.end_byte - span.start_byte));
        let Some(best) = best else {
            continue;
        };

        if let Some(last) = compacted.last_mut()
            && last.end_byte == start
            && last.scope == best.scope
        {
            last.end_byte = end;
            continue;
        }

        compacted.push(VisibleSyntaxSpan {
            start_byte: start,
            end_byte: end,
            scope: best.scope.clone(),
        });
    }

    *spans = compacted;
}

fn scope_priority(scope: &str) -> u8 {
    if scope.starts_with("punctuation") {
        7
    } else if scope.starts_with("variable") || scope.starts_with("property") {
        6
    } else if scope.starts_with("function")
        || scope.starts_with("type")
        || scope.starts_with("keyword")
        || scope.starts_with("constant")
        || scope.starts_with("number")
        || scope.starts_with("boolean")
        || scope.starts_with("attribute")
        || scope.starts_with("label")
    {
        5
    } else if scope.starts_with("string") {
        4
    } else if scope.starts_with("comment") {
        3
    } else {
        1
    }
}

fn is_keyword_node(kind: &str, node: Node<'_>, text: &str) -> bool {
    if node.is_named() {
        return false;
    }
    matches!(
        kind,
        "fn" | "let"
            | "mut"
            | "pub"
            | "impl"
            | "struct"
            | "enum"
            | "trait"
            | "mod"
            | "use"
            | "match"
            | "if"
            | "else"
            | "for"
            | "while"
            | "loop"
            | "return"
            | "async"
            | "await"
            | "move"
            | "const"
            | "static"
            | "where"
            | "in"
            | "crate"
            | "self"
            | "super"
            | "def"
            | "class"
            | "import"
            | "from"
            | "elif"
            | "try"
            | "except"
            | "finally"
            | "with"
            | "as"
            | "pass"
            | "yield"
    ) || node
        .utf8_text(text.as_bytes())
        .is_ok_and(|token| matches!(token, "True" | "False" | "None"))
}
