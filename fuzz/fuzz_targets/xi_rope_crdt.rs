#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeSet;
use xi_rope::delta::Delta;
use xi_rope::engine::{Engine, SessionId};
use xi_rope::{Interval, Rope, RopeInfo};

#[derive(Arbitrary, Clone, Debug)]
struct EditOp {
    base_hint: u8,
    start: u16,
    end: u16,
    insert: String,
    priority: u8,
    undo_group: u8,
}

#[derive(Arbitrary, Debug)]
struct RopeInput {
    initial: String,
    left_ops: Vec<EditOp>,
    right_ops: Vec<EditOp>,
    left_undo_groups: Vec<u8>,
    right_undo_groups: Vec<u8>,
    left_gc_groups: Vec<u8>,
    right_gc_groups: Vec<u8>,
}

fn clamp_interval(len: usize, start: u16, end: u16) -> (usize, usize) {
    let start = usize::from(start) % (len + 1);
    let end = usize::from(end) % (len + 1);
    if start <= end { (start, end) } else { (end, start) }
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
        let (start, end) = clamp_interval(base_text.len(), op.start, op.end);
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
    let mut engine = Engine::new(Rope::from(initial));
    engine.set_session_id(session);
    engine
}

fuzz_target!(|input: RopeInput| {
    let mut left = new_engine(&input.initial, (11, 0));
    let mut right = new_engine(&input.initial, (29, 0));

    apply_ops(&mut left, &input.left_ops);
    apply_ops(&mut right, &input.right_ops);

    left.undo(to_group_set(&input.left_undo_groups));
    right.undo(to_group_set(&input.right_undo_groups));

    left.gc(to_group_set(&input.left_gc_groups).iter());
    right.gc(to_group_set(&input.right_gc_groups).iter());

    left.merge(&right);
    right.merge(&left);

    assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
});
