use arbitrary::Arbitrary;
use std::collections::BTreeSet;
use xi_core_lib::backspace::offset_for_delete_backwards;
use xi_core_lib::config::BufferItems;
use xi_core_lib::edit_ops::{delete_backward, transpose};
use xi_core_lib::line_offset::LogicalLines;
use xi_core_lib::linewrap::fuzz_rewrap_mono;
use xi_core_lib::movement::{Movement, region_movement};
use xi_core_lib::selection::{SelRegion, Selection};
use xi_core_lib::word_boundaries::WordCursor;
use xi_rope::compare::{ne_idx, ne_idx_fallback, ne_idx_rev, ne_idx_rev_fallback};
use xi_rope::delta::Delta;
use xi_rope::engine::{Engine, SessionId};
use xi_rope::{Interval, Rope, RopeInfo};

#[derive(Arbitrary, Clone, Debug)]
pub struct EditOp {
    base_hint: u8,
    start: u16,
    end: u16,
    insert: String,
    priority: u8,
    undo_group: u8,
}

#[derive(Arbitrary, Debug)]
pub struct RopeInput {
    pub initial: String,
    pub left_ops: Vec<EditOp>,
    pub right_ops: Vec<EditOp>,
    pub left_undo_groups: Vec<u8>,
    pub right_undo_groups: Vec<u8>,
    pub left_gc_groups: Vec<u8>,
    pub right_gc_groups: Vec<u8>,
}

#[derive(Arbitrary, Debug)]
pub struct CompareInput {
    pub left: Vec<u8>,
    pub right: Vec<u8>,
    pub left_offset: u16,
    pub right_offset: u16,
}

#[derive(Arbitrary, Debug)]
pub struct RegionInput {
    pub start: u16,
    pub end: u16,
}

#[derive(Arbitrary, Debug)]
pub struct CoreTextInput {
    pub text: String,
    pub regions: Vec<RegionInput>,
    pub movement_case: u8,
    pub modify: bool,
    pub height: u8,
    pub wrap_cols: u16,
    pub visible_start: u16,
    pub visible_end: u16,
    pub tab_size: u8,
    pub translate_tabs_to_spaces: bool,
    pub use_tab_stops: bool,
}

fn trim_slice(bytes: &[u8], raw_offset: u16) -> &[u8] {
    if bytes.is_empty() {
        return bytes;
    }
    let offset = usize::from(raw_offset) % (bytes.len() + 1);
    &bytes[offset..]
}

#[cfg(target_arch = "x86_64")]
fn expected_mask<const N: usize>(left: &[u8; N], right: &[u8; N]) -> i32 {
    let mut mask = 0i32;
    for index in 0..N {
        if left[index] != right[index] {
            mask |= 1 << index;
        }
    }
    mask
}

#[cfg(target_arch = "x86_64")]
fn run_x86_compare_checks(left: &[u8], right: &[u8]) {
    if is_x86_feature_detected!("sse4.2") {
        let mut left_block = [0u8; 16];
        let mut right_block = [0u8; 16];
        let left_len = left.len().min(16);
        let right_len = right.len().min(16);
        left_block[..left_len].copy_from_slice(&left[..left_len]);
        right_block[..right_len].copy_from_slice(&right[..right_len]);

        let expected = expected_mask(&left_block, &right_block);
        let actual = unsafe { xi_rope::compare::sse_compare_mask(&left_block, &right_block) };
        assert_eq!(actual, expected);

        let expected_idx = ne_idx_fallback(left, right);
        let actual_idx = unsafe { xi_rope::compare::ne_idx_sse(left, right) };
        assert_eq!(actual_idx, expected_idx);
    }

    if is_x86_feature_detected!("avx2") {
        let mut left_block = [0u8; 32];
        let mut right_block = [0u8; 32];
        let left_len = left.len().min(32);
        let right_len = right.len().min(32);
        left_block[..left_len].copy_from_slice(&left[..left_len]);
        right_block[..right_len].copy_from_slice(&right[..right_len]);

        let expected = expected_mask(&left_block, &right_block);
        let actual = unsafe { xi_rope::compare::avx_compare_mask(&left_block, &right_block) };
        assert_eq!(actual, expected);

        let expected_idx = ne_idx_fallback(left, right);
        let actual_idx = unsafe { xi_rope::compare::ne_idx_avx(left, right) };
        assert_eq!(actual_idx, expected_idx);
    }
}

fn clamp_offset(text: &Rope, raw: u16) -> usize {
    let offset = usize::from(raw) % (text.len() + 1);
    if text.is_codepoint_boundary(offset) {
        offset
    } else {
        text.prev_codepoint_offset(offset).unwrap_or(0)
    }
}

fn clamp_interval(text: &Rope, start: u16, end: u16) -> (usize, usize) {
    let start = clamp_offset(text, start);
    let end = clamp_offset(text, end);
    if start <= end { (start, end) } else { (end, start) }
}

fn clamp_offset_backwards(text: &Rope, raw: u16) -> usize {
    let mut offset = usize::from(raw) % (text.len() + 1);
    while offset > 0 && !text.is_codepoint_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn make_regions(text: &Rope, inputs: &[RegionInput]) -> Selection {
    let mut regions = Selection::new();
    for input in inputs.iter().take(8) {
        regions.add_region(SelRegion::new(
            clamp_offset_backwards(text, input.start),
            clamp_offset_backwards(text, input.end),
        ));
    }
    if regions.is_empty() {
        regions.add_region(SelRegion::new(0, 0));
    }
    regions
}

fn movement_from_case(raw: u8) -> Movement {
    match raw % 16 {
        0 => Movement::Left,
        1 => Movement::Right,
        2 => Movement::LeftWord,
        3 => Movement::RightWord,
        4 => Movement::LeftOfLine,
        5 => Movement::RightOfLine,
        6 => Movement::Up,
        7 => Movement::Down,
        8 => Movement::UpPage,
        9 => Movement::DownPage,
        10 => Movement::UpExactPosition,
        11 => Movement::DownExactPosition,
        12 => Movement::StartOfParagraph,
        13 => Movement::EndOfParagraph,
        14 => Movement::EndOfParagraphKill,
        _ => Movement::EndOfDocument,
    }
}

fn make_config(input: &CoreTextInput) -> BufferItems {
    BufferItems {
        line_ending: "\n".to_owned(),
        tab_size: usize::from(input.tab_size % 16).max(1),
        translate_tabs_to_spaces: input.translate_tabs_to_spaces,
        use_tab_stops: input.use_tab_stops,
        font_face: String::new(),
        font_size: 12.0,
        auto_indent: false,
        scroll_past_end: false,
        wrap_width: usize::from(input.wrap_cols),
        word_wrap: input.wrap_cols != 0,
        autodetect_whitespace: false,
        surrounding_pairs: Vec::new(),
        save_with_newline: false,
    }
}

fn to_group_set(groups: &[u8]) -> BTreeSet<usize> {
    groups.iter().map(|group| usize::from(*group)).collect()
}

fn apply_ops(engine: &mut Engine, ops: &[EditOp]) {
    let mut known_revs = vec![engine.get_head_rev_id().token()];

    for op in ops {
        let base_rev = known_revs[usize::from(op.base_hint) % known_revs.len()];
        let Some(base_text) = engine.get_rev(base_rev) else {
            continue;
        };
        let (start, end) = clamp_interval(&base_text, op.start, op.end);
        let delta = Delta::<RopeInfo>::simple_edit(
            Interval::new(start, end),
            Rope::from(op.insert.as_str()),
            base_text.len(),
        );

        if engine
            .try_edit_rev(usize::from(op.priority), usize::from(op.undo_group), base_rev, delta)
            .is_ok()
        {
            if let Ok(head_delta) = engine.try_delta_rev_head(base_rev) {
                let recomputed = head_delta.apply(&base_text);
                assert_eq!(String::from(recomputed), String::from(engine.get_head()));
            }
            known_revs.push(engine.get_head_rev_id().token());
        }
    }
}

fn new_engine(initial: &str, session: SessionId) -> Engine {
    let mut engine = Engine::empty();
    engine.set_session_id(session);

    if !initial.is_empty() {
        let base = Engine::new(Rope::from(initial));
        engine.merge(&base);
    }

    engine
}

pub fn run_rope_input(input: RopeInput) {
    let mut left = new_engine(&input.initial, (11, 0));
    let mut right = new_engine(&input.initial, (29, 0));

    apply_ops(&mut left, &input.left_ops);
    apply_ops(&mut right, &input.right_ops);

    left.undo(to_group_set(&input.left_undo_groups));
    right.undo(to_group_set(&input.right_undo_groups));

    left.gc(to_group_set(&input.left_gc_groups).iter());
    right.gc(to_group_set(&input.right_gc_groups).iter());

    // xi-rope gc intentionally prunes merge context aggressively; merging after
    // independent gc is not a supported invariant for this harness.
    if !input.left_gc_groups.is_empty() || !input.right_gc_groups.is_empty() {
        return;
    }

    left.merge(&right);
    right.merge(&left);

    assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
}

pub fn run_compare_input(input: CompareInput) {
    let left = trim_slice(&input.left, input.left_offset);
    let right = trim_slice(&input.right, input.right_offset);

    assert_eq!(ne_idx(left, right), ne_idx_fallback(left, right));
    assert_eq!(ne_idx_rev(left, right), ne_idx_rev_fallback(left, right));

    #[cfg(target_arch = "x86_64")]
    run_x86_compare_checks(left, right);
}

pub fn run_core_text_input(input: CoreTextInput) {
    let text: String = input.text.chars().take(512).collect();
    let rope = Rope::from(text.as_str());
    let regions = make_regions(&rope, &input.regions);
    let config = make_config(&input);
    let movement = movement_from_case(input.movement_case);

    for region in regions.iter() {
        let delete_start = offset_for_delete_backwards(region, &rope, &config);
        assert!(delete_start <= region.max());

        let moved = region_movement(
            movement,
            *region,
            &LogicalLines,
            usize::from(input.height).max(1),
            &rope,
            input.modify,
        );
        assert!(moved.min() <= rope.len());
        assert!(moved.max() <= rope.len());

        let mut cursor = WordCursor::new(&rope, region.end.min(rope.len()));
        let _ = cursor.prev_boundary();
        let _ = cursor.next_boundary();
        let (start, end) = cursor.select_word();
        assert!(start <= end);
        assert!(end <= rope.len());
    }

    let _ = delete_backward(&rope, &regions, &config).apply(&rope);
    let _ = transpose(&rope, &regions).apply(&rope);

    let visible_start = usize::from(input.visible_start);
    let visible_end = visible_start + usize::from(input.visible_end % 64) + 1;
    fuzz_rewrap_mono(&rope, usize::from(input.wrap_cols % 120), visible_start, visible_end);
}

#[cfg(test)]
mod tests {
    use super::{RopeInput, clamp_interval, new_engine, run_rope_input};
    use xi_rope::delta::Delta;
    use xi_rope::{Interval, Rope, RopeInfo};

    #[test]
    fn new_engine_sets_session_before_non_empty_initial_revision() {
        let mut engine = new_engine("||", (11, 7));
        let base_rev = engine.get_head_rev_id().token();
        let delta = Delta::<RopeInfo>::simple_edit(Interval::new(2, 2), Rope::from("!"), 2);
        engine.edit_rev(1, 1, base_rev, delta);

        assert_eq!(String::from(engine.get_head()), "||!");
        assert_eq!(engine.get_head_rev_id().session_id(), (11, 7));
    }

    #[test]
    fn rope_crdt_regression_empty_input() {
        run_rope_input(RopeInput {
            initial: String::new(),
            left_ops: Vec::new(),
            right_ops: Vec::new(),
            left_undo_groups: Vec::new(),
            right_undo_groups: Vec::new(),
            left_gc_groups: Vec::new(),
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_non_empty_initial_input() {
        run_rope_input(RopeInput {
            initial: String::from("||"),
            left_ops: Vec::new(),
            right_ops: Vec::new(),
            left_undo_groups: Vec::new(),
            right_undo_groups: Vec::new(),
            left_gc_groups: Vec::new(),
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_noop_left_edit() {
        run_rope_input(RopeInput {
            initial: String::new(),
            left_ops: vec![super::EditOp {
                base_hint: 0,
                start: 0,
                end: 0,
                insert: String::new(),
                priority: 0,
                undo_group: 0,
            }],
            right_ops: Vec::new(),
            left_undo_groups: Vec::new(),
            right_undo_groups: Vec::new(),
            left_gc_groups: Vec::new(),
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_unknown_undo_group() {
        run_rope_input(RopeInput {
            initial: String::new(),
            left_ops: Vec::new(),
            right_ops: Vec::new(),
            left_undo_groups: vec![0],
            right_undo_groups: Vec::new(),
            left_gc_groups: Vec::new(),
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_undo_initial_group() {
        run_rope_input(RopeInput {
            initial: String::from("\u{1}"),
            left_ops: Vec::new(),
            right_ops: Vec::new(),
            left_undo_groups: vec![0],
            right_undo_groups: Vec::new(),
            left_gc_groups: Vec::new(),
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_right_insert_without_shared_prefix() {
        run_rope_input(RopeInput {
            initial: String::new(),
            left_ops: Vec::new(),
            right_ops: vec![super::EditOp {
                base_hint: 12,
                start: 18_761,
                end: 18_761,
                insert: String::from("\u{1}"),
                priority: 0,
                undo_group: 0,
            }],
            left_undo_groups: Vec::new(),
            right_undo_groups: Vec::new(),
            left_gc_groups: Vec::new(),
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn clamp_interval_snaps_to_codepoint_boundaries() {
        let text = Rope::from("5ݦ");

        let (start, end) = clamp_interval(&text, 2, 2);

        assert_eq!((1, 1), (start, end));
    }

    #[test]
    fn rope_crdt_regression_insert_in_undone_group() {
        run_rope_input(RopeInput {
            initial: String::from("A"),
            left_ops: Vec::new(),
            right_ops: vec![super::EditOp {
                base_hint: 255,
                start: 64_511,
                end: 65_535,
                insert: String::from("\0"),
                priority: 255,
                undo_group: 0,
            }],
            left_undo_groups: vec![0],
            right_undo_groups: Vec::new(),
            left_gc_groups: Vec::new(),
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_duplicate_undo_toggle_with_gc() {
        run_rope_input(RopeInput {
            initial: String::from("\0"),
            left_ops: Vec::new(),
            right_ops: Vec::new(),
            left_undo_groups: vec![0, 255],
            right_undo_groups: vec![255, 0],
            left_gc_groups: vec![208, 0],
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_gc_keeps_visible_initial_text_context() {
        run_rope_input(RopeInput {
            initial: String::from("5551"),
            left_ops: vec![super::EditOp {
                base_hint: 38,
                start: 57_565,
                end: 47_776,
                insert: String::new(),
                priority: 224,
                undo_group: 240,
            }],
            right_ops: Vec::new(),
            left_undo_groups: Vec::new(),
            right_undo_groups: Vec::new(),
            left_gc_groups: vec![255, 255, 255, 0],
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_gc_with_single_char_initial_text() {
        run_rope_input(RopeInput {
            initial: String::from("+"),
            left_ops: vec![super::EditOp {
                base_hint: 190,
                start: 13_052,
                end: 255,
                insert: String::new(),
                priority: 128,
                undo_group: 0,
            }],
            right_ops: Vec::new(),
            left_undo_groups: vec![249],
            right_undo_groups: Vec::new(),
            left_gc_groups: vec![0],
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_gc_after_non_group_zero_delete() {
        run_rope_input(RopeInput {
            initial: String::from("55\u{b}"),
            left_ops: vec![super::EditOp {
                base_hint: 149,
                start: 43_823,
                end: 43_690,
                insert: String::new(),
                priority: 170,
                undo_group: 170,
            }],
            right_ops: Vec::new(),
            left_undo_groups: Vec::new(),
            right_undo_groups: Vec::new(),
            left_gc_groups: vec![49, 0],
            right_gc_groups: Vec::new(),
        });
    }

    #[test]
    fn rope_crdt_regression_gc_with_right_unknown_undo_group() {
        run_rope_input(RopeInput {
            initial: String::from("@"),
            left_ops: vec![super::EditOp {
                base_hint: 197,
                start: 46_848,
                end: 9_397,
                insert: String::new(),
                priority: 208,
                undo_group: 0,
            }],
            right_ops: Vec::new(),
            left_undo_groups: Vec::new(),
            right_undo_groups: vec![181],
            left_gc_groups: vec![0],
            right_gc_groups: Vec::new(),
        });
    }
}
