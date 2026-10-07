//! Indent-guide rendering for the buffer pane (vim-indent-guides / helix style).
//!
//! Guides are presentation-only: the backend ships row text, this module decides
//! which leading-whitespace cells to replace with a vertical rule.  That keeps
//! the feature frontend-owned and cheap — no syntax tree, no extra RPC.
//!
//! Two knobs come from the editor config: `indent_guides` (enabled) and
//! `indent_guides_max_lines`, the safety cap: a block (a function or scope) that
//! spans more lines than the cap draws no guides, so a huge function cannot fill
//! the viewport with rules; `0` disables the cap.
//!
//! Algorithm, once per buffer render:
//!   1. classify every row in a scan window that extends the cap past the
//!      viewport on both sides — indent level, empty rows, wrapped
//!      continuations, rows the client has not loaded yet;
//!   2. for each level, walk the maximal runs of rows indented deeper than that
//!      level.  A run draws its rule only when the block it belongs to (the run
//!      plus the rows that delimit it) fits under the cap.  A run that reaches
//!      the scan window without a delimiter is longer than the cap, because the
//!      window is cap rows wider than the viewport on each side;
//!   3. replace the whitespace cell at each allowed level column with the guide
//!      glyph.
//!
//! Rows are only ever written where the row already has a whitespace cell, so
//! empty lines stay empty (like vim-indent-guides) and no cell is invented.
use super::*;

/// Vertical rule drawn at each guide column.
pub(super) const GUIDE_GLYPH: char = '│';

/// Indent levels deeper than this never draw a guide; terminal width is rarely
/// enough for them and the block sweep is `O(levels * rows)`.
pub(super) const MAX_GUIDE_LEVELS: usize = 16;

/// Ceiling for the `indent_guides_max_lines` scan window: a cap above this is
/// treated as this value so a hostile or accidental config value cannot turn a
/// single frame into a whole-buffer scan.
pub(super) const MAX_BLOCK_SCAN_ROWS: usize = 4096;

/// Per-row indent facts the planner works from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RowFacts {
    /// Indent level of the row's own text, clamped to `MAX_GUIDE_LEVELS`.
    pub(super) level: u8,
    /// The row has no text at all; it joins its shallower neighbour's block.
    pub(super) empty: bool,
    /// Wrapped continuation or not-yet-loaded row: it stays inside the block
    /// started above it but draws no guides of its own.
    pub(super) no_guides: bool,
}

/// Guide bitmask per row for one render pass.
///
/// Bit `k` set on a row means "draw a guide at level `k`" (display column
/// `k * tab_width`), provided the row has a whitespace cell there.
pub(super) struct GuidePlan {
    start: usize,
    masks: Vec<u16>,
}

impl GuidePlan {
    /// Guide mask for the buffer row `row`; `0` when the row is outside the
    /// planned window or draws nothing.
    pub(super) fn mask(&self, row: usize) -> u16 {
        row.checked_sub(self.start).and_then(|i| self.masks.get(i)).copied().unwrap_or(0)
    }
}

/// Build the guide plan covering the rendered rows `first_row..end_row`.
///
/// Callers pass the fold-aware rendered row set, which can jump far past the
/// viewport height.  `max_lines` is the block-length cap in lines (`0` =
/// unlimited); the scan window is `min(max_lines, MAX_BLOCK_SCAN_ROWS)` rows
/// wide on each side.
pub(super) fn plan(
    buf: &BufState,
    first_row: usize,
    end_row: usize,
    tab_width: usize,
    max_lines: usize,
) -> GuidePlan {
    let tab_width = tab_width.max(1);
    let line_count = buf.line_count();
    let cap = (max_lines > 0).then(|| max_lines.min(MAX_BLOCK_SCAN_ROWS));
    let margin = cap.unwrap_or(0);

    let window_end = end_row.saturating_add(margin).min(line_count);
    let window_start = first_row.saturating_sub(margin).min(window_end);
    let facts: Vec<RowFacts> =
        (window_start..window_end).map(|row| facts_for_row(buf, row, tab_width)).collect();
    let masks = plan_masks(&facts, window_start, line_count, cap);
    GuidePlan { start: window_start, masks }
}

/// Classify one buffer row for the planner.
fn facts_for_row(buf: &BufState, row: usize, tab_width: usize) -> RowFacts {
    // `None`/`Invalid` covers both "VLF window does not hold this row yet" and
    // "the slot was never reported"; either way there is no text to guide.
    let Some(LineSlot::Known(line)) = buf.line_slot(row) else {
        return RowFacts { level: 0, empty: false, no_guides: true };
    };
    // Rows without a logical line are wrapped continuations: the text is a
    // slice of the logical line, so its leading whitespace is not indentation.
    // (Rows rewritten by a VLF optimistic edit also arrive without one and
    // simply draw no guides until the payload re-reports them.)
    if line.logical_line.is_none() {
        return RowFacts { level: 0, empty: false, no_guides: true };
    }
    RowFacts {
        level: indent_level(&line.text, tab_width),
        empty: line.text.is_empty(),
        no_guides: false,
    }
}

/// Indent level of `text`: leading whitespace width divided by `tab_width`,
/// with tabs advancing to the next tab stop, clamped to `MAX_GUIDE_LEVELS`.
fn indent_level(text: &str, tab_width: usize) -> u8 {
    let limit = MAX_GUIDE_LEVELS * tab_width;
    let mut cols = 0usize;
    for ch in text.chars() {
        match ch {
            ' ' => cols += 1,
            '\t' => cols += tab_width - (cols % tab_width),
            _ => break,
        }
        if cols >= limit {
            break;
        }
    }
    (cols / tab_width).min(MAX_GUIDE_LEVELS) as u8
}

/// Bits for every guide level below `level` (`level` itself starts the content).
fn level_mask(level: u8) -> u16 {
    let level = u32::from(level.min(MAX_GUIDE_LEVELS as u8));
    if level >= 16 { u16::MAX } else { ((1u32 << level) - 1) as u16 }
}

/// Compute the per-row guide masks.
///
/// `window_start` is the buffer row the first entry of `facts` describes and
/// `line_count` the buffer's row count, so the planner can tell a block edge
/// (a delimiting row) from the scan window's edge (a block that runs on).
pub(super) fn plan_masks(
    facts: &[RowFacts],
    window_start: usize,
    line_count: usize,
    cap: Option<usize>,
) -> Vec<u16> {
    let rows = facts.len();
    let mut masks = vec![0u16; rows];
    if rows == 0 {
        return masks;
    }

    // Structural level per row.  Continuation and unloaded rows inherit the
    // level above them so they neither break nor delimit a block.
    let mut levels = vec![0u8; rows];
    let mut carried = 0u8;
    for (i, fact) in facts.iter().enumerate() {
        if fact.no_guides {
            levels[i] = carried;
        } else {
            levels[i] = fact.level;
            carried = fact.level;
        }
    }
    // Empty rows join the shallower neighbour: a blank line inside a block
    // keeps the block alive, while a blank line after the closing `}` does not
    // inherit the block's depth.
    let mut prev_level = vec![None; rows];
    let mut last = None;
    for i in 0..rows {
        if !facts[i].empty && !facts[i].no_guides {
            last = Some(levels[i]);
        }
        prev_level[i] = last;
    }
    let mut next_level = vec![None; rows];
    last = None;
    for i in (0..rows).rev() {
        if !facts[i].empty && !facts[i].no_guides {
            last = Some(levels[i]);
        }
        next_level[i] = last;
    }
    for i in 0..rows {
        if facts[i].empty && !facts[i].no_guides {
            levels[i] = match (prev_level[i], next_level[i]) {
                (Some(prev), Some(next)) => prev.min(next),
                (Some(prev), None) => prev,
                (None, Some(next)) => next,
                (None, None) => 0,
            };
        }
    }

    match cap {
        // No cap: every row draws a guide at each level below its own.
        None => {
            for i in 0..rows {
                if !facts[i].no_guides {
                    masks[i] = level_mask(levels[i]);
                }
            }
        }
        Some(cap) => {
            let max_level = levels.iter().copied().max().unwrap_or(0) as usize;
            for level in 0..max_level {
                let bit = 1u16 << level;
                let mut run_start: Option<usize> = None;
                for i in 0..=rows {
                    if i < rows && levels[i] as usize > level {
                        run_start.get_or_insert(i);
                        continue;
                    }
                    if let Some(start) = run_start.take()
                        && block_fits(start, i, rows, window_start, line_count, cap)
                    {
                        for mask in &mut masks[start..i] {
                            *mask |= bit;
                        }
                    }
                }
            }
            // Continuations and unloaded rows never draw.
            for i in 0..rows {
                if facts[i].no_guides {
                    masks[i] = 0;
                }
            }
        }
    }
    masks
}

/// Whether the block holding rows `start..end` (all deeper than one level) is
/// short enough to draw.
///
/// A run touching the scan window without a delimiting row belongs to a block
/// longer than the cap: the window is cap rows wider than the rendered rows on
/// each side, so the missing edge is at least cap rows away.
fn block_fits(
    start: usize,
    end: usize,
    rows: usize,
    window_start: usize,
    line_count: usize,
    cap: usize,
) -> bool {
    // The buffer's own start/end counts as a delimiter, the window's edge does not.
    let delimited_above = start > 0 || window_start == 0;
    let delimited_below = end < rows || window_start + rows >= line_count;
    if !delimited_above || !delimited_below {
        return false;
    }
    let block_lines = (end - start) + usize::from(start > 0) + usize::from(end < rows);
    block_lines <= cap
}

/// Replace the whitespace cell at each allowed guide level with `GUIDE_GLYPH`.
///
/// `spans` start at display column `left` (the horizontal scroll offset) and
/// cover the leading whitespace run plus the row content.  Only existing
/// whitespace cells are written; the visible-whitespace markers that stand in
/// for leading whitespace (`LEAD_GLYPH`) or a space (a `SPACE_GLYPH` tab pad)
/// count as whitespace only when `visible_whitespace` rendered them (a literal
/// `·`/`→`/`╎` opening a line is content and ends the indent run), and tabs
/// themselves never reach here because the caller already expanded them.
pub(super) fn overlay(
    spans: Vec<Span<'static>>,
    mask: u16,
    tab_width: usize,
    left: usize,
    visible_whitespace: bool,
) -> Vec<Span<'static>> {
    if mask == 0 {
        return spans;
    }
    let tab_width = tab_width.max(1);
    let guide_color = theme::FG_INDENT_GUIDE;
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut col = left;
    let mut in_indent = true;
    for span in spans {
        let base = span.style;
        if !in_indent {
            out.push(span);
            continue;
        }
        let mut plain = String::new();
        let mut guides = String::new();
        for ch in span.content.chars() {
            let is_ws = matches!(ch, ' ' | '\t')
                || (visible_whitespace && matches!(ch, LEAD_GLYPH | SPACE_GLYPH | TAB_GLYPH));
            if !is_ws {
                in_indent = false;
            }
            let level = col / tab_width;
            let is_guide = in_indent
                && ch != '\t'
                && col.is_multiple_of(tab_width)
                && level < MAX_GUIDE_LEVELS
                && mask & (1u16 << level) != 0;
            if is_guide {
                if !plain.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut plain), base));
                }
                guides.push(GUIDE_GLYPH);
            } else {
                if !guides.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut guides), base.fg(guide_color)));
                }
                plain.push(ch);
            }
            col += 1;
        }
        if !plain.is_empty() {
            out.push(Span::styled(plain, base));
        }
        if !guides.is_empty() {
            out.push(Span::styled(guides, base.fg(guide_color)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(level: u8) -> RowFacts {
        RowFacts { level, empty: false, no_guides: false }
    }

    fn empty() -> RowFacts {
        RowFacts { level: 0, empty: true, no_guides: false }
    }

    fn unloaded() -> RowFacts {
        RowFacts { level: 0, empty: false, no_guides: true }
    }

    fn all_masks(facts: &[RowFacts], cap: Option<usize>) -> Vec<u16> {
        plan_masks(facts, 0, facts.len(), cap)
    }

    #[test]
    fn indent_level_counts_tabs_to_tab_stops_and_clamps() {
        assert_eq!(indent_level("", 4), 0);
        assert_eq!(indent_level("x", 4), 0);
        assert_eq!(indent_level("    x", 4), 1);
        assert_eq!(indent_level("\tx", 4), 1);
        assert_eq!(indent_level("\t  x", 4), 1);
        assert_eq!(indent_level("  \tx", 4), 1);
        assert_eq!(indent_level("        x", 4), 2);
        assert_eq!(indent_level(&" ".repeat(400), 4), MAX_GUIDE_LEVELS as u8);
    }

    #[test]
    fn unlimited_plan_marks_every_level_below_the_row() {
        let facts = [row(0), row(1), row(2), row(0)];
        assert_eq!(all_masks(&facts, None), vec![0b0, 0b1, 0b11, 0b0]);
    }

    #[test]
    fn capped_plan_skips_blocks_longer_than_the_cap() {
        // A ten-row body between two level-0 rows is a 12-line block.
        let mut facts = vec![row(0)];
        facts.extend((0..10).map(|_| row(1)));
        facts.push(row(0));

        // Under the cap the body keeps its guide, over it the body loses it.
        assert_eq!(all_masks(&facts, Some(12))[5] & 1, 1);
        assert_eq!(all_masks(&facts, Some(11))[5] & 1, 0);
    }

    #[test]
    fn capped_plan_keeps_shorter_nested_block_inside_a_long_one() {
        // A deeply nested 6-line block inside a 13-line function inside an
        // 8-line level-1 block.
        let facts = [
            row(0),
            row(1),
            row(2),
            row(3),
            row(3),
            row(3),
            row(3),
            row(2),
            row(1),
            row(1),
            row(1),
            row(1),
            row(0),
        ];

        let masks = all_masks(&facts, Some(6));
        // Row 3 sits in the 6-line level-2 block, which is exactly at the cap;
        // the 8-line level-1 block and the 13-line level-0 block above it are
        // both over it, so only the level-2 guide survives.
        assert_eq!(masks[3], 0b100);
        // Row 1 only has the 13-line level-0 block above it: nothing fits.
        assert_eq!(masks[1], 0);
    }

    #[test]
    fn capped_plan_drops_blocks_running_past_the_scan_window() {
        // The window starts mid-buffer and the run reaches its top edge, so the
        // block is known to be longer than the cap however short the run looks.
        let facts = [row(2), row(2), row(0)];
        assert_eq!(plan_masks(&facts, 100, 500, Some(32)), vec![0, 0, 0]);

        // The buffer's own top edge counts as a delimiter.
        assert_eq!(plan_masks(&facts, 0, 100, Some(32)), vec![0b11, 0b11, 0]);

        // A run reaching the window's bottom edge is unbounded below …
        let facts = [row(0), row(2), row(2)];
        assert_eq!(plan_masks(&facts, 100, 500, Some(32))[1], 0);

        // … unless the window ends at the buffer's last row.
        assert_eq!(plan_masks(&facts, 97, 100, Some(32))[1], 0b11);
    }

    #[test]
    fn empty_rows_join_the_block_they_sit_in() {
        // The blank line counts into the block, so a five-line cap drops it and
        // a six-line cap keeps it.
        let facts = [row(0), row(2), row(2), empty(), row(2), row(0)];
        assert_eq!(all_masks(&facts, Some(6))[1], 0b11);
        assert_eq!(all_masks(&facts, Some(5))[1], 0);
    }

    #[test]
    fn empty_rows_after_a_block_do_not_inherit_its_depth() {
        // The blank line joins the shallower side, so the level-2 block above it
        // is four lines long (it would be five if the blank inherited level 2).
        let facts = [row(0), row(2), row(2), empty(), row(0)];
        assert_eq!(all_masks(&facts, Some(4))[1], 0b11);
    }

    #[test]
    fn continuation_rows_draw_nothing_but_keep_the_block_alive() {
        let facts = [row(0), row(1), unloaded(), row(1), row(0)];
        let masks = all_masks(&facts, Some(64));
        assert_eq!(masks[2], 0);
        // The block counts five lines, so a five-line cap keeps the guides.
        assert_eq!(all_masks(&facts, Some(5))[1], 0b1);
        assert_eq!(all_masks(&facts, Some(4))[1], 0);
    }

    #[test]
    fn overlay_writes_only_existing_whitespace_cells() {
        let spans = vec![Span::styled("      let x = 1;".to_owned(), Style::default())];
        let out = overlay(spans, 0b11, 4, 0, false);
        let text: String = out.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "│   │ let x = 1;");
    }

    #[test]
    fn overlay_respects_horizontal_scroll_and_stops_at_content() {
        // Scrolled four columns right: level 1 lands on screen column 0, level
        // 0 is behind the scroll offset, and the content keeps its own cells
        // (the cell at the level-3 column is already `b`).
        let spans = vec![Span::styled("            body".to_owned(), Style::default())];
        let out = overlay(spans, 0b111, 4, 4, false);
        let text: String = out.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "│   │       body");
        assert_eq!(text.matches(GUIDE_GLYPH).count(), 2);
    }

    #[test]
    fn overlay_replaces_list_mode_whitespace_glyphs() {
        let spans = vec![Span::styled("····x".to_owned(), Style::default())];
        let out = overlay(spans, 0b1, 4, 0, true);
        let text: String = out.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "│···x");
    }

    #[test]
    fn overlay_leaves_literal_whitespace_glyphs_alone_without_list_mode() {
        // `·`/`→` opening a line are content unless `list` mode rendered them.
        for text in ["····x", "→→x", "· "] {
            let spans = vec![Span::styled(text.to_owned(), Style::default())];
            let out = overlay(spans, u16::MAX, 4, 0, false);
            let joined: String = out.iter().map(|span| span.content.as_ref()).collect();
            assert_eq!(joined, text);
        }
    }

    #[test]
    fn overlay_keeps_deeply_indented_lines_untouched_beyond_level_cap() {
        let spans = vec![Span::styled(" ".repeat(80), Style::default())];
        let out = overlay(spans, u16::MAX, 4, 0, false);
        let text: String = out.iter().map(|span| span.content.as_ref()).collect();
        let guides = text.chars().filter(|&ch| ch == GUIDE_GLYPH).count();
        assert_eq!(guides, MAX_GUIDE_LEVELS);
    }

    /// A row that ends up with several spans (the highlighter fills the gaps
    /// between syntax spans) must keep the guide columns of a single span.
    #[test]
    fn overlay_counts_columns_across_multiple_spans() {
        let spans = vec![
            Span::styled("    ".to_owned(), Style::default()),
            Span::styled("let".to_owned(), Style::default()),
            Span::styled(" x = ".to_owned(), Style::default()),
            Span::styled("42".to_owned(), Style::default()),
            Span::styled(";".to_owned(), Style::default()),
        ];
        let out = overlay(spans, 0b1, 4, 0, false);
        let text: String = out.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "│   let x = 42;");
    }
}
