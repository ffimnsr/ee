//! Engine tests: merge across histories.
use super::*;

    #[test]
    fn merge_after_empty_gc_keeps_common_base() {
        let mut left = Engine::new(Rope::from(""));
        left.set_session_id((11, 0));
        let mut right = Engine::new(Rope::from(""));
        right.set_session_id((29, 0));

        left.undo(std::iter::empty::<usize>());
        right.undo(std::iter::empty::<usize>());

        left.gc(std::iter::empty::<usize>());
        right.gc(std::iter::empty::<usize>());

        left.merge(&right);
        right.merge(&left);

        assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
        assert_eq!("", String::from(left.get_head()));
    }

    #[test]
    fn merge_ignores_noop_edit_revision() {
        let mut left = Engine::new(Rope::from(""));
        left.set_session_id((11, 0));
        let mut right = Engine::new(Rope::from(""));
        right.set_session_id((29, 0));

        let base_rev = left.get_head_rev_id().token();
        let no_op = Delta::simple_edit(Interval::new(0, 0), Rope::from(""), 0);
        left.edit_rev(0, 0, base_rev, no_op);

        left.merge(&right);
        right.merge(&left);

        assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
        assert_eq!(left.revs.len(), right.revs.len());
        assert_eq!(1, left.revs.len());
    }

    #[test]
    fn merge_ignores_unknown_undo_group() {
        let mut left = Engine::new(Rope::from(""));
        left.set_session_id((11, 0));
        let mut right = Engine::new(Rope::from(""));
        right.set_session_id((29, 0));

        left.undo([0usize]);

        left.merge(&right);
        right.merge(&left);

        assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
        assert_eq!(left.revs.len(), right.revs.len());
        assert_eq!(1, left.revs.len());
    }

    #[test]
    fn merge_replays_undo_of_initial_contents() {
        let mut left = Engine::empty();
        left.set_session_id((11, 0));
        left.merge(&Engine::new(Rope::from("\u{1}")));
        let mut right = Engine::empty();
        right.set_session_id((29, 0));
        right.merge(&Engine::new(Rope::from("\u{1}")));

        left.undo([0usize]);

        left.merge(&right);
        right.merge(&left);
        left.merge(&right);
        right.merge(&left);

        assert_eq!("", String::from(left.get_head()));
        assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
        assert_eq!(left.revs.len(), right.revs.len());
        assert_eq!(3, left.revs.len());
        assert_eq!(left.undone_groups, right.undone_groups);
    }

    #[test]
    fn merge_handles_histories_without_shared_prefix() {
        let mut left = Engine::new(Rope::from(""));
        left.set_session_id((11, 0));
        let mut right = Engine::new(Rope::from(""));
        right.set_session_id((29, 0));

        let right_base = right.get_head_rev_id().token();
        right.edit_rev(
            0,
            0,
            right_base,
            Delta::simple_edit(Interval::new(0, 0), Rope::from("\u{1}"), 0),
        );

        left.merge(&right);
        right.merge(&left);

        assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
        assert_eq!("\u{1}", String::from(left.get_head()));
    }

    #[test]
    fn merge_concurrent_insert_in_undone_group_stays_undone() {
        let mut left = Engine::empty();
        left.set_session_id((11, 0));
        left.merge(&Engine::new(Rope::from("A")));
        let mut right = Engine::empty();
        right.set_session_id((29, 0));
        right.merge(&Engine::new(Rope::from("A")));

        left.undo([0usize]);

        let right_base = right.get_head_rev_id().token();
        right.edit_rev(
            255,
            0,
            right_base,
            Delta::simple_edit(Interval::new(1, 1), Rope::from("\0"), 1),
        );

        left.merge(&right);
        right.merge(&left);

        assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
        assert_eq!("", String::from(left.get_head()));
    }

    #[test]
    fn merge_duplicate_undo_toggles_converge_after_gc() {
        let mut left = Engine::empty();
        left.set_session_id((11, 0));
        left.merge(&Engine::new(Rope::from("\0")));
        let mut right = Engine::empty();
        right.set_session_id((29, 0));
        right.merge(&Engine::new(Rope::from("\0")));

        left.undo([0usize, 255]);
        right.undo([255usize, 0]);
        left.gc([208usize, 0]);

        left.merge(&right);
        right.merge(&left);

        assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
    }

    #[test]
    fn gc_group_zero_after_delete_from_initial_revision() {
        let mut engine = Engine::new(Rope::from("\0\n"));
        let base = engine.get_head_rev_id().token();
        engine.edit_rev(0, 0, base, Delta::simple_edit(Interval::new(0, 2), Rope::from(""), 2));
        engine.gc([0usize]);

        assert_eq!("", String::from(engine.get_head()));
    }

    #[test]
    fn merge_after_gc_of_group_zero_delete_from_initial_revision() {
        let mut left = Engine::empty();
        left.set_session_id((11, 0));
        left.merge(&Engine::new(Rope::from("\0\n")));
        let base = left.get_head_rev_id().token();
        left.edit_rev(0, 0, base, Delta::simple_edit(Interval::new(0, 2), Rope::from(""), 2));
        left.gc([0usize]);

        let mut right = Engine::empty();
        right.set_session_id((29, 0));
        right.merge(&Engine::new(Rope::from("\0\n")));

        left.merge(&right);
        right.merge(&left);

        assert_eq!(String::from(left.get_head()), String::from(right.get_head()));
    }
