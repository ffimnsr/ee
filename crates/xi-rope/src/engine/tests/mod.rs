//! Engine tests: shared harness and fixtures.
    use crate::engine::*;
    use crate::rope::{Rope, RopeInfo};
    use crate::delta::{Builder, Delta, DeltaElement};
    use crate::multiset::Subset;
    use crate::interval::Interval;
    use proptest::prelude::*;
    use proptest::string::string_regex;
    use std::collections::BTreeSet;
    use crate::test_helpers::{parse_subset_list, parse_subset, parse_delta, debug_subsets};

    const TEST_STR: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

    fn normalized_interval(base_len: usize, raw_start: usize, raw_end: usize) -> Interval {
        let start = if base_len == 0 { 0 } else { raw_start % (base_len + 1) };
        let end = start + (raw_end % (base_len + 1 - start));
        Interval::new(start, end)
    }

    fn build_simple_delta(base_len: usize, raw_start: usize, raw_end: usize, insert: &str) -> Delta<RopeInfo> {
        Delta::simple_edit(
            normalized_interval(base_len, raw_start, raw_end),
            Rope::from(insert),
            base_len,
        )
    }

    fn seeded_engine(session: u64, initial: &str) -> Engine {
        let mut engine = Engine::empty();
        engine.set_session_id((session, 0));
        engine.merge(&Engine::new(Rope::from(initial)));
        engine
    }

    fn merge_one(peers: &mut [Engine], target: usize, source: usize) {
        let (start, end) = peers.split_at_mut(target);
        let (dst, rest) = end.split_first_mut().unwrap();
        let src = if source < target { &mut start[source] } else { &mut rest[source - target - 1] };
        dst.merge(src);
    }

    fn synchronize(peers: &mut [Engine]) {
        let count = peers.len();
        for _ in 0..count {
            for target in 0..count {
                for source in 0..count {
                    if target != source {
                        merge_one(peers, target, source);
                    }
                }
            }
        }
    }

    fn assert_converged(peers: &[Engine]) -> String {
        let expected = String::from(peers[0].get_head());
        for (index, peer) in peers.iter().enumerate().skip(1) {
            assert_eq!(expected, String::from(peer.get_head()), "peer {} diverged", index);
        }
        expected
    }
    fn build_delta_1() -> Delta<RopeInfo> {
        let mut d_builder = Builder::new(TEST_STR.len());
        d_builder.delete(Interval::new(10, 36));
        d_builder.replace(Interval::new(39, 42), Rope::from("DEEF"));
        d_builder.replace(Interval::new(54, 54), Rope::from("999"));
        d_builder.delete(Interval::new(58, 61));
        d_builder.build()
    }

    fn build_delta_2() -> Delta<RopeInfo> {
        let mut d_builder = Builder::new(TEST_STR.len());
        d_builder.replace(Interval::new(1, 3), Rope::from("!"));
        d_builder.delete(Interval::new(10, 36));
        d_builder.replace(Interval::new(42, 45), Rope::from("GI"));
        d_builder.replace(Interval::new(54, 54), Rope::from("888"));
        d_builder.replace(Interval::new(59, 60), Rope::from("HI"));
        d_builder.build()
    }
    fn undo_test(before: bool, undos : OrdSet<usize>, output: &str) {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let first_rev = engine.get_head_rev_id().token();
        if before {
            engine.undo(undos.clone());
        }
        engine.edit_rev(1, 1, first_rev, build_delta_1());
        engine.edit_rev(0, 2, first_rev, build_delta_2());
        if !before {
            engine.undo(undos);
        }
        assert_eq!(output, String::from(engine.get_head()));
    }

mod edit_tests;
mod gc_tests;
mod merge_internals_tests;
mod merge_property_tests;
mod merge_tests;
mod undo_tests;
