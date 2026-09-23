//! `RenderSource` facade render tests: non-rope sources must produce the same
//! `UpdateOp` streams as the rope-backed path for the same content (Stage A
//! acceptance gate: byte-identical ops, rope-bound annotations gated).

use std::borrow::Cow;

use serde_json::Value;

use crate::text_store::ByteOffset;

use super::*;

/// A minimal non-rope `RenderSource` over a plain string, mirroring what the
/// VLF store will provide in Phase 2 (exact lookups, no wrap).
struct FakeRenderSource {
    text: String,
}

impl FakeRenderSource {
    /// Byte offset of the start of `line`; clamps to `len` past EOF.
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

impl RenderSource for FakeRenderSource {
    fn len_bytes(&self) -> usize {
        self.text.len()
    }

    fn total_lines(&self) -> RenderLineCount {
        let newlines = self.text.bytes().filter(|&b| b == b'\n').count();
        RenderLineCount::Exact((newlines + 1) as u64)
    }

    fn line_to_byte(&self, line: u64) -> LineLookup {
        let len = self.text.len();
        if line as usize > self.text.bytes().filter(|&b| b == b'\n').count() {
            return LineLookup::Exact(ByteOffset(len as u64));
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
        1.0
    }

    fn as_rope(&self) -> Option<&Rope> {
        None
    }
}

fn update_ops(peer: &RecordingPeer) -> Value {
    let notifications = peer.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "update")
        .expect("update notification")
        .clone();
    params["update"]["ops"].clone()
}

/// The acceptance gate from the Stage A plan: the same content rendered
/// through the rope store and through a non-rope source yields identical
/// `UpdateOp` streams (wrap disabled on both sides; non-rope sources have no
/// rope-bound annotations in Stage A).
#[test]
fn facade_render_matches_rope_render_op_stream() {
    for content in ["alpha\nbeta gamma\ndelta\n", "single", "", "a\nb\n", "é😀\nline\n"] {
        let rope = Rope::from(content);

        let mut view_rope = View::new(1.into(), BufferId::new(2));
        view_rope.height = 40;
        view_rope.debug_force_rewrap_cols(&rope, 0);
        let (client_rope, peer_rope) = recording_client();
        let store = RopeTextStore::new(rope.clone(), 0);
        view_rope.render_if_dirty(&store, &client_rope, true, "", false);

        let mut view_facade = View::new(2.into(), BufferId::new(3));
        view_facade.height = 40;
        let (client_facade, peer_facade) = recording_client();
        let fake = FakeRenderSource { text: content.to_string() };
        view_facade.render_if_dirty(&fake, &client_facade, true, "", false);

        assert_eq!(update_ops(&peer_facade), update_ops(&peer_rope), "content: {content:?}");
    }
}

/// The same view-transition sequence (fresh render → scroll → selection
/// change → out-of-viewport request) must produce identical op streams for
/// rope and non-rope sources. This exercises the `Preserve`/`copy`,
/// `update` (cursors invalid), and `request_lines` plan paths, not just the
/// initial full `insert`.
#[test]
fn facade_render_matches_rope_on_scroll_selection_request() {
    let content = "alpha\nbeta gamma\ndelta\n\nepsilon";
    let rope = Rope::from(content);

    let mut view_rope = View::new(1.into(), BufferId::new(2));
    view_rope.height = 40;
    view_rope.debug_force_rewrap_cols(&rope, 0);
    let (client_rope, peer_rope) = recording_client();
    let store = RopeTextStore::new(rope.clone(), 0);

    let mut view_facade = View::new(2.into(), BufferId::new(3));
    view_facade.height = 40;
    let (client_facade, peer_facade) = recording_client();
    let fake = FakeRenderSource { text: content.to_string() };

    // 1. initial full render (insert path).
    view_rope.render_if_dirty(&store, &client_rope, true, "", false);
    view_facade.render_if_dirty(&fake, &client_facade, true, "", false);
    assert_eq!(update_ops(&peer_facade), update_ops(&peer_rope), "initial render");

    // 2. scroll down: preserved segments emit copy/skip ops.
    view_rope.first_line = 2;
    view_facade.first_line = 2;
    view_rope.render_if_dirty(&store, &client_rope, true, "", false);
    view_facade.render_if_dirty(&fake, &client_facade, true, "", false);
    assert_eq!(update_ops(&peer_facade), update_ops(&peer_rope), "scrolled render");

    // 3. selection change invalidates CURSOR_VALID: update path with cursors.
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(3, 8));
    view_rope.set_selection(&rope, selection.clone());
    view_facade.set_selection(&rope, selection);
    view_rope.render_if_dirty(&store, &client_rope, true, "", false);
    view_facade.render_if_dirty(&fake, &client_facade, true, "", false);
    assert_eq!(update_ops(&peer_facade), update_ops(&peer_rope), "selection-change render");

    // 4. out-of-viewport request: plan.request_lines re-inserts requested
    //    segments even though the visible plan is unchanged.
    view_rope.request_lines(&store, &client_rope, 0, 3, true, "", false);
    view_facade.request_lines(&fake, &client_facade, 0, 3, true, "", false);
    assert_eq!(update_ops(&peer_facade), update_ops(&peer_rope), "request_lines render");
}

/// Non-rope sources gate rope-bound annotations (Phase 2 enables cursor
/// markers behind a feature gate); rope sources keep emitting them.
#[test]
fn facade_render_gates_rope_bound_annotations() {
    let content = "first\nsecond\n";
    let rope = Rope::from(content);

    let mut view_rope = View::new(1.into(), BufferId::new(2));
    view_rope.height = 40;
    view_rope.debug_force_rewrap_cols(&rope, 0);
    let (client_rope, peer_rope) = recording_client();
    let store = RopeTextStore::new(rope.clone(), 0);
    view_rope.render_if_dirty(&store, &client_rope, true, "", false);

    let mut view_facade = View::new(2.into(), BufferId::new(3));
    view_facade.height = 40;
    let (client_facade, peer_facade) = recording_client();
    let fake = FakeRenderSource { text: content.to_string() };
    view_facade.render_if_dirty(&fake, &client_facade, true, "", false);

    let rope_annotations = rope_update_annotations(&peer_rope);
    assert!(
        rope_annotations.as_array().is_some_and(|a| !a.is_empty()),
        "caret in the visible range must produce a rope selection annotation"
    );
    let facade_annotations = rope_update_annotations(&peer_facade);
    assert!(
        facade_annotations.as_array().is_some_and(|a| a.is_empty()),
        "non-rope sources ship no rope-bound annotations in Stage A"
    );
}

fn rope_update_annotations(peer: &RecordingPeer) -> Value {
    let notifications = peer.take_notifications();
    let (_, params) = notifications
        .iter()
        .find(|(method, _)| method == "update")
        .expect("update notification")
        .clone();
    params["update"]["annotations"].clone()
}

/// The facade fallback line iterator must match `Lines::iter_lines` with no
/// wrap on the same content (exercised indirectly by the op-stream parity
/// test above; this asserts intervals directly).
#[test]
fn facade_lines_match_no_wrap_rope_lines() {
    for content in ["alpha\nbeta\ndelta\n", "single", "", "a\nb\n", "é😀\nline\n"] {
        let rope = Rope::from(content);
        let mut lines = Lines::default();
        lines.set_wrap_width(&rope, WrapWidth::None);
        let rope_intervals = lines
            .iter_lines(&rope, 0)
            .map(|l| (l.interval.start(), l.interval.end(), l.line_num))
            .collect::<Vec<_>>();

        let fake = FakeRenderSource { text: content.to_string() };
        let facade_intervals = facade_iter_lines_test(&fake, 0).collect::<Vec<_>>();

        assert_eq!(facade_intervals, rope_intervals, "content: {content:?}");
    }
}

/// Test-only wrapper around the private `facade_iter_lines` helper.
fn facade_iter_lines_test<'a>(
    text: &'a dyn RenderSource,
    start_line: usize,
) -> impl Iterator<Item = (usize, usize, Option<usize>)> + 'a {
    super::render::facade_iter_lines(text, start_line)
        .map(|l| (l.interval.start(), l.interval.end(), l.line_num))
}

/// The wrapped row pass must segment exactly like the rope `Bytes` rewrap
/// (both mirror `CodepointMono` word breaks).
#[test]
fn wrap_row_intervals_match_rope_bytes_rewrap() {
    let samples = [
        "every wordthing should getits own",
        "create abreak between THESE TWO\nwords andbreakcorrectlyhere\nplz",
        "so\nevery wordthing should getits own",
        "",
        "\n\n",
        "hello\n",
        "a very long singlewordthatcannotfit",
        "héllo wörld 😀 emoji\nsecond line\n",
        "alpha\r\nbeta\n",
    ];
    for (idx, sample) in samples.iter().enumerate() {
        for cols in [4usize, 8, 20, 200] {
            let rope = Rope::from(*sample);
            let mut view = View::new(1.into(), BufferId::new(2));
            view.debug_force_rewrap_cols(&rope, cols);
            let rope_rows: Vec<(usize, usize, Option<usize>)> = view
                .get_lines()
                .iter_lines(&rope, 0)
                .map(|l| (l.interval.start(), l.interval.end(), l.line_num))
                .collect();
            // `wrap_row_intervals` operates on one logical line; wrap each
            // line of the sample separately (rope slice keeps terminators and
            // the empty final line) and concatenate with absolute offsets.
            let line_count = rope.measure::<LinesMetric>() + 1;
            let mut ours: Vec<(usize, usize)> = Vec::new();
            for line in 0..line_count {
                let start = rope.offset_of_line(line);
                let end = rope.offset_of_line(line + 1);
                let chunk = rope.slice_to_cow(start..end);
                for (a, b) in super::render::wrap_row_intervals(&chunk, cols) {
                    ours.push((start + a, start + b));
                }
            }
            assert_eq!(ours.len(), rope_rows.len(), "sample {idx} cols {cols}");
            for (ours_row, rope_row) in ours.iter().zip(rope_rows.iter()) {
                assert_eq!(ours_row, &(rope_row.0, rope_row.1), "sample {idx} cols {cols}");
            }
        }
    }
}

/// Wrapped VLF render: rows split at the width budget, `ln` only on the first
/// row of each logical line, matched against the rope wrapped render of the
/// same content.
#[test]
fn facade_wrapped_render_matches_rope_wrapped_lines() {
    let content = "one two three four five six seven\neight nine ten eleven twelve\nwideeeeeeeeee\nhello world\n";
    let rope = Rope::from(content);
    let fake = FakeRenderSource { text: content.to_string() };
    let cols = 10;

    let mut view_rope = View::new(1.into(), BufferId::new(2));
    view_rope.height = 40;
    view_rope.debug_force_rewrap_cols(&rope, cols);
    let (client_rope, peer_rope) = recording_client();
    view_rope.render_if_dirty(&RopeTextStore::new(rope.clone(), 0), &client_rope, true, "", false);

    let mut view_facade = View::new(2.into(), BufferId::new(3));
    view_facade.height = 40;
    view_facade.vlf_wrap = true;
    view_facade.update_wrap_settings(&rope, cols, false);
    let (client_facade, peer_facade) = recording_client();
    view_facade.render_if_dirty(&fake, &client_facade, true, "", false);

    let rows_of = |peer: &RecordingPeer| -> Vec<(String, Option<usize>)> {
        let notifications = peer.take_notifications();
        let (_, params) =
            notifications.iter().find(|(m, _)| m == "update").expect("update notification").clone();
        params["update"]["ops"]
            .as_array()
            .expect("ops")
            .iter()
            .flat_map(|op| op["lines"].as_array().cloned().unwrap_or_default())
            .map(|line| {
                let text = line["text"].as_str().unwrap_or("").to_owned();
                let ln = line["ln"].as_u64().map(|n| n as usize);
                (text, ln)
            })
            .collect::<Vec<_>>()
    };

    let rope_rows = rows_of(&peer_rope);
    let facade_rows = rows_of(&peer_facade);

    assert!(
        rope_rows.len() > 6,
        "wrapped rope render must emit wrapped rows (got {})",
        rope_rows.len()
    );
    assert_eq!(facade_rows, rope_rows, "wrapped row texts and ln must match");
}
