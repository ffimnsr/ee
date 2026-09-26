//! Out-of-range scroll handling on the render path.
//!
//! The frontend's VLF goto-end sentinel scrolls to line 9e9 on purpose so the
//! core clamps the window onto the tail. `RenderPlan::create` clamps its own
//! spans, but the stored view position also anchors every byte offset and the
//! source's read-ahead viewport, so a position left past EOF collapses that
//! window to `len..len` — demoting the pages being rendered out of viewport
//! priority and killing the pager read-ahead.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use crate::text_store::ByteOffset;

use super::render_source_tests::update_ops;
use super::*;

/// Non-rope source over a plain string that records `set_viewport` calls.
///
/// `pending_from` makes line lookups at or past a line answer `Pending`
/// (mirroring VLF index lag), and `reported_lines` overrides `total_lines`
/// (an approximate VLF total that overshoots the real line count).
struct RecordingSource {
    text: String,
    pending_from: Option<usize>,
    reported_lines: Option<u64>,
    viewports: Arc<Mutex<Vec<(usize, usize)>>>,
}

impl RecordingSource {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            pending_from: None,
            reported_lines: None,
            viewports: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_pending_from(mut self, line: usize) -> Self {
        self.pending_from = Some(line);
        self
    }

    fn with_reported_lines(mut self, lines: u64) -> Self {
        self.reported_lines = Some(lines);
        self
    }

    fn last_viewport(&self) -> (usize, usize) {
        *self
            .viewports
            .lock()
            .expect("viewport recorder poisoned")
            .last()
            .expect("render must drive the read-ahead viewport")
    }

    fn real_lines(&self) -> usize {
        self.text.bytes().filter(|&b| b == b'\n').count() + 1
    }

    /// Byte offset of the start of `line`, clamped to `len` past EOF.
    fn offset_of_line(&self, line: usize) -> usize {
        let mut current = 0;
        for (idx, b) in self.text.bytes().enumerate() {
            if current == line {
                return idx;
            }
            if b == b'\n' {
                current += 1;
            }
        }
        self.text.len()
    }
}

impl RenderSource for RecordingSource {
    fn len_bytes(&self) -> usize {
        self.text.len()
    }

    fn total_lines(&self) -> RenderLineCount {
        match self.reported_lines {
            Some(lines) => RenderLineCount::Approximate(lines),
            None => RenderLineCount::Exact(self.real_lines() as u64),
        }
    }

    fn line_to_byte(&self, line: u64) -> LineLookup {
        if self.pending_from.is_some_and(|from| line as usize >= from) {
            return LineLookup::Pending;
        }
        LineLookup::Exact(ByteOffset(self.offset_of_line(line as usize) as u64))
    }

    fn byte_to_line(&self, byte: usize) -> Option<u64> {
        let byte = byte.min(self.text.len());
        Some(self.text[..byte].bytes().filter(|&b| b == b'\n').count() as u64)
    }

    fn read_range(&self, start: usize, end: usize) -> ReadResult<'_> {
        let len = self.text.len();
        let start = start.min(len);
        let end = end.min(len).max(start);
        ReadResult::Ready(Cow::Borrowed(&self.text[start..end]))
    }

    fn index_progress(&self) -> f64 {
        0.5
    }

    fn set_viewport(&self, start: usize, end: usize) {
        self.viewports.lock().expect("viewport recorder poisoned").push((start, end));
    }

    fn as_rope(&self) -> Option<&Rope> {
        None
    }
}

/// `count` logical lines with no trailing terminator, so the last line index
/// is `count - 1` (a trailing newline would add an empty final line).
fn numbered_lines(count: usize) -> String {
    (0..count).map(|idx| format!("line {idx}")).collect::<Vec<_>>().join("\n")
}

/// The last rendered rows of an `update` payload, in op order.
fn inserted_lines(ops: &Value) -> Vec<u64> {
    ops.as_array()
        .expect("ops array")
        .iter()
        .filter(|op| op["op"] == json!("ins"))
        .flat_map(|op| op["lines"].as_array().cloned().unwrap_or_default())
        .map(|line| line["ln"].as_u64().expect("inserted rows carry absolute line numbers"))
        .collect()
}

#[test]
fn scroll_past_eof_settles_view_and_read_ahead_on_the_tail() {
    let content = numbered_lines(100);
    let mut view = View::new(1.into(), BufferId::new(2));
    view.height = 40;
    let (client, peer) = recording_client();
    let source = RecordingSource::new(&content);

    // The VLF goto-end sentinel: a line far past the end of the document.
    view.set_scroll(9_000_000_000, 9_000_000_000 + 40);
    view.render_if_dirty(&source, &client, true, "", false);

    assert_eq!(view.first_line, 99, "the view must settle on the last line");

    // The tail window is still what gets rendered.
    let ops = update_ops(&peer);
    let lines = inserted_lines(&ops);
    assert_eq!(lines, vec![97, 98, 99], "the sentinel must render the last rows: {ops}");

    // Read-ahead stays inside the document, anchored on the rendered tail.
    let (start, end) = source.last_viewport();
    assert!(start < end, "read-ahead window must not collapse: {start}..{end}");
    assert!(end <= content.len(), "window must not run past EOF: {start}..{end}");
    assert!(
        start >= content.len() - 64,
        "window must follow the rendered tail, got {start}..{end}"
    );
}

#[test]
fn scroll_past_eof_with_lagging_lookups_still_anchors_read_ahead_on_the_tail() {
    // Approximate VLF total overshooting the real line count, with the index
    // still lagging (lookups at/after line 90 answer Pending). Both offsets
    // then resolve to EOF, which used to collapse the window to `len..len`.
    let content = numbered_lines(100);
    let mut view = View::new(1.into(), BufferId::new(2));
    view.height = 40;
    let (client, _peer) = recording_client();
    let source = RecordingSource::new(&content).with_pending_from(90).with_reported_lines(120);

    view.set_scroll(9_000_000_000, 9_000_000_000 + 40);
    view.render_if_dirty(&source, &client, true, "", false);

    assert_eq!(view.first_line, 119, "clamped into the reported document");
    assert_eq!(
        source.last_viewport(),
        (content.len() - 1, content.len()),
        "a collapsed window must fall back to the document tail"
    );
}

#[test]
fn in_range_scroll_keeps_read_ahead_on_the_rendered_window() {
    let content = numbered_lines(100);
    let mut view = View::new(1.into(), BufferId::new(2));
    view.height = 40;
    let (client, _peer) = recording_client();
    let source = RecordingSource::new(&content);

    view.set_scroll(10, 50);
    view.render_if_dirty(&source, &client, true, "", false);

    assert_eq!(view.first_line, 10);
    let (start, end) = source.last_viewport();
    assert_eq!(start, source.offset_of_line(10));
    assert_eq!(end, source.offset_of_line(52));
    assert!(end < content.len(), "an in-range view must not be tail-anchored: {start}..{end}");
}
