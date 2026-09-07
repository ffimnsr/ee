//! Engine tests: undo.
use super::*;

    #[test]
    fn undo() {
        undo_test(false, [1usize,2].iter().copied().collect(), TEST_STR);
    }

    #[test]
    fn undo_2() {
        undo_test(false, [2usize].iter().copied().collect(), "0123456789abcDEEFghijklmnopqr999stuvz");
    }

    #[test]
    fn undo_3() {
        undo_test(false, [1usize].iter().copied().collect(), "0!3456789abcdefGIjklmnopqr888stuvwHIyz");
    }

    #[test]
    fn undo_4() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let d1 = Delta::simple_edit(Interval::new(0,0), Rope::from("a"), TEST_STR.len());
        let first_rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, first_rev, d1);
        let new_head = engine.get_head_rev_id().token();
        engine.undo([1usize]);
        let d2 = Delta::simple_edit(Interval::new(0,0), Rope::from("a"), TEST_STR.len()+1);
        engine.edit_rev(1, 2, new_head, d2); // note this is based on d1 before, not the undo
        let new_head_2 = engine.get_head_rev_id().token();
        let d3 = Delta::simple_edit(Interval::new(0,0), Rope::from("b"), TEST_STR.len()+1);
        engine.edit_rev(1, 3, new_head_2, d3);
        engine.undo([1usize, 3]);
        assert_eq!("a0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", String::from(engine.get_head()));
    }

    #[test]
    fn undo_5() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let d1 = Delta::simple_edit(Interval::new(0,10), Rope::from(""), TEST_STR.len());
        let first_rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, first_rev, d1.clone());
        engine.edit_rev(1, 2, first_rev, d1);
        engine.undo([1usize]);
        assert_eq!("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", String::from(engine.get_head()));
        engine.undo([1usize, 2]);
        assert_eq!(TEST_STR, String::from(engine.get_head()));
        engine.undo(std::iter::empty::<usize>());
        assert_eq!("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", String::from(engine.get_head()));
    }
