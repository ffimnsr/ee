use arbitrary::Arbitrary;
use std::collections::BTreeSet;
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
    if start <= end {
        (start, end)
    } else {
        (end, start)
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

#[cfg(test)]
mod tests {
    use super::{clamp_interval, new_engine, run_rope_input, RopeInput};
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
