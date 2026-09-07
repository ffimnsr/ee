//! UI: annotations rendering.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AnnotationVisual {
    pub(super) bg: Color,
    pub(super) fg: Option<Color>,
    pub(super) modifier: Modifier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LineAnnotationSegment {
    pub(super) start_display: usize,
    pub(super) end_display: usize,
    pub(super) visual: AnnotationVisual,
    pub(super) priority: u8,
}

pub(super) fn annotation_visual(kind: &str) -> AnnotationVisual {
    match kind {
        "selection" => AnnotationVisual {
            bg: theme::BG_SELECTION,
            fg: Some(theme::FG_TEXT),
            modifier: Modifier::empty(),
        },
        "find" => AnnotationVisual {
            bg: theme::BG_FIND,
            fg: Some(theme::BG_APP),
            modifier: Modifier::BOLD,
        },
        _ => {
            AnnotationVisual { bg: theme::BG_ANNOTATION, fg: None, modifier: Modifier::UNDERLINED }
        }
    }
}

pub(super) fn annotation_priority(kind: &str) -> u8 {
    match kind {
        "selection" => 0,
        "find" => 2,
        _ => 1,
    }
}

pub(super) fn annotation_marker_color(kind: &str) -> Color {
    match kind {
        "find" => theme::FG_WARNING,
        "selection" => theme::FG_INFO,
        _ => theme::FG_MARKER_HINT,
    }
}

pub(super) fn payload_marker_for_value(value: &serde_json::Value) -> Option<char> {
    match value {
        serde_json::Value::String(text) => text
            .chars()
            .find(|ch| !ch.is_whitespace())
            .map(|ch| if ch.is_alphanumeric() { ch.to_ascii_uppercase() } else { '•' }),
        serde_json::Value::Object(map) => ["label", "kind", "name", "message"]
            .iter()
            .find_map(|key| map.get(*key).and_then(payload_marker_for_value))
            .or(Some('•')),
        serde_json::Value::Array(values) => values.iter().find_map(payload_marker_for_value),
        serde_json::Value::Null => None,
        _ => Some('•'),
    }
}

pub(super) fn annotation_marker_for_line(
    buf: &BufState,
    line_index: usize,
) -> Option<(char, Color)> {
    let mut best: Option<(u8, char, Color)> = None;

    if buf.is_vlf && buf.vlf_search_ranges.iter().any(|range| range.line as usize == line_index) {
        best = Some((annotation_priority("find"), '•', annotation_marker_color("find")));
    }

    for annotation in &buf.annotations {
        if matches!(annotation.annotation_type.as_str(), "selection" | "find") {
            continue;
        }
        let Some(payloads) = annotation.payloads.as_ref() else { continue };
        for (idx, range) in annotation.ranges.iter().enumerate() {
            let Some(payload) = payloads.get(idx) else { continue };
            let [start_line, _, end_line, _] = *range;
            if line_index < start_line || line_index > end_line {
                continue;
            }
            let Some(marker) = payload_marker_for_value(payload) else { continue };
            let priority = annotation_priority(&annotation.annotation_type);
            let color = annotation_marker_color(&annotation.annotation_type);
            if best.as_ref().is_none_or(|(best_priority, _, _)| priority >= *best_priority) {
                best = Some((priority, marker, color));
            }
        }
    }

    best.map(|(_, marker, color)| (marker, color))
}

pub(super) fn annotation_style(base: Style, visual: AnnotationVisual) -> Style {
    let mut style = base.bg(visual.bg);
    if let Some(fg) = visual.fg {
        style = style.fg(fg);
    }
    if visual.modifier != Modifier::empty() {
        style = style.add_modifier(visual.modifier);
    }
    style
}

pub(super) fn apply_annotation_overlay(
    spans: Vec<Span<'static>>,
    col_start: usize,
    col_end: usize,
    left: usize,
    visual: AnnotationVisual,
) -> Vec<Span<'static>> {
    let sel_start = col_start.saturating_sub(left);
    let mut sel_end = col_end.saturating_sub(left);
    if sel_end <= sel_start {
        sel_end = sel_start + 1;
    }

    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    let mut painted = false;

    for sp in spans {
        let content = sp.content.into_owned();
        let style = sp.style;
        let span_cols = byte_col_to_display_col(&content, content.len());
        let sp_end = col + span_cols;

        if sp_end <= sel_start || col >= sel_end {
            out.push(Span::styled(content, style));
            col = sp_end;
            continue;
        }

        let local_start = sel_start.saturating_sub(col).min(span_cols);
        let local_end = sel_end.saturating_sub(col).min(span_cols);
        let start_byte = display_col_to_byte(&content, local_start);
        let end_byte = display_col_to_byte(&content, local_end);

        if start_byte > 0 {
            out.push(Span::styled(content[..start_byte].to_owned(), style));
        }

        if end_byte > start_byte {
            out.push(Span::styled(
                content[start_byte..end_byte].to_owned(),
                annotation_style(style, visual),
            ));
            painted = true;
        }

        if end_byte < content.len() {
            out.push(Span::styled(content[end_byte..].to_owned(), style));
        }

        col = sp_end;
    }

    if !painted && col <= sel_start {
        let pad = sel_start - col;
        if pad > 0 {
            out.push(Span::raw(" ".repeat(pad)));
        }
        out.push(Span::styled(" ", annotation_style(Style::default(), visual)));
    } else if painted && sel_end > col {
        out.push(Span::styled(" ", annotation_style(Style::default(), visual)));
    }

    out
}

pub(super) fn replace_display_column(
    spans: Vec<Span<'static>>,
    display_col: usize,
    replacement: char,
    replacement_style: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut col = 0usize;
    let mut replaced = false;

    for sp in spans {
        let content = sp.content.into_owned();
        let style = sp.style;
        let span_cols = UnicodeWidthStr::width(content.as_str());
        let span_end = col + span_cols;

        if replaced || display_col < col || display_col >= span_end {
            out.push(Span::styled(content, style));
            col = span_end;
            continue;
        }

        let local_start = display_col - col;
        let start_byte = display_col_to_byte(&content, local_start);
        let end_byte = display_col_to_byte(&content, local_start + 1);

        if start_byte > 0 {
            out.push(Span::styled(content[..start_byte].to_owned(), style));
        }
        out.push(Span::styled(replacement.to_string(), replacement_style));
        if end_byte < content.len() {
            out.push(Span::styled(content[end_byte..].to_owned(), style));
        }

        replaced = true;
        col = span_end;
    }

    out
}

pub(super) fn apply_swift_motion_targets(
    mut spans: Vec<Span<'static>>,
    targets: &[SwiftMotionTarget],
    left: usize,
) -> Vec<Span<'static>> {
    let visual =
        AnnotationVisual { bg: theme::FG_KEY, fg: Some(theme::BG_APP), modifier: Modifier::BOLD };
    let label_style =
        Style::default().fg(theme::BG_APP).bg(theme::BG_SWIFT_LABEL).add_modifier(Modifier::BOLD);

    for target in targets {
        if target.end_display_col <= left || target.display_col < left {
            continue;
        }
        spans = apply_annotation_overlay(
            spans,
            target.display_col,
            target.end_display_col,
            left,
            visual,
        );
    }

    for target in targets {
        if target.display_col < left {
            continue;
        }
        spans = replace_display_column(spans, target.display_col - left, target.label, label_style);
    }

    spans
}

pub(super) fn collect_line_annotation_segments(
    line: &str,
    log_idx: usize,
    annotations: &[CoreAnnotation],
) -> Vec<LineAnnotationSegment> {
    let mut segments = Vec::new();

    for annotation in annotations {
        let visual = annotation_visual(&annotation.annotation_type);
        let priority = annotation_priority(&annotation.annotation_type);
        for range in &annotation.ranges {
            let [start_line, start_col, end_line, end_col] = *range;
            if log_idx < start_line || log_idx > end_line {
                continue;
            }

            let start_byte = if log_idx == start_line { start_col.min(line.len()) } else { 0 };
            let end_byte = if log_idx == end_line { end_col.min(line.len()) } else { line.len() };
            let start_display = byte_col_to_display_col(line, start_byte);
            let mut end_display = byte_col_to_display_col(line, end_byte);
            if end_display <= start_display {
                end_display = start_display + 1;
            }

            segments.push(LineAnnotationSegment { start_display, end_display, visual, priority });
        }
    }

    segments.sort_by_key(|segment| (segment.priority, segment.start_display, segment.end_display));

    let mut merged: Vec<LineAnnotationSegment> = Vec::new();
    for segment in segments {
        if let Some(last) = merged.last_mut()
            && last.priority == segment.priority
            && last.visual == segment.visual
            && segment.start_display <= last.end_display
        {
            last.end_display = last.end_display.max(segment.end_display);
            continue;
        }
        merged.push(segment);
    }

    merged
}

pub(super) fn apply_core_annotations(
    mut spans: Vec<Span<'static>>,
    line: &str,
    log_idx: usize,
    annotations: &[CoreAnnotation],
    left: usize,
) -> Vec<Span<'static>> {
    for segment in collect_line_annotation_segments(line, log_idx, annotations) {
        spans = apply_annotation_overlay(
            spans,
            segment.start_display,
            segment.end_display,
            left,
            segment.visual,
        );
    }
    spans
}

pub(super) fn apply_vlf_search_ranges(
    mut spans: Vec<Span<'static>>,
    log_idx: usize,
    ranges: &[VlfSearchRange],
    left: usize,
) -> Vec<Span<'static>> {
    let visual = annotation_visual("find");
    for range in ranges.iter().filter(|range| range.line as usize == log_idx) {
        spans = apply_annotation_overlay(spans, range.start_col, range.end_col, left, visual);
    }
    spans
}
