//! Engine tests: garbage collection.
use super::*;

    #[test]
    fn gc() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let d1 = Delta::simple_edit(Interval::new(0,0), Rope::from("c"), TEST_STR.len());
        let first_rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, first_rev, d1);
        let new_head = engine.get_head_rev_id().token();
        engine.undo([1usize]);
        let d2 = Delta::simple_edit(Interval::new(0,0), Rope::from("a"), TEST_STR.len()+1);
        engine.edit_rev(1, 2, new_head, d2);
        let gc : OrdSet<usize> = [1usize].iter().copied().collect();
        engine.gc(&gc);
        let d3 = Delta::simple_edit(Interval::new(0,0), Rope::from("b"), TEST_STR.len()+1);
        let new_head_2 = engine.get_head_rev_id().token();
        engine.edit_rev(1, 3, new_head_2, d3);
        engine.undo([3usize]);
        assert_eq!("a0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", String::from(engine.get_head()));
    }

    /// This case is a regression test reproducing a panic I found while using the UI.
    /// It does undos and gcs in a pattern that can actually happen when using the editor.
    fn gc_scenario(edits: usize, max_undos: usize) {
        let mut engine = Engine::new(Rope::from(""));

        // insert `edits` letter "b"s in separate undo groups
        for i in 0..edits {
            let d = Delta::simple_edit(Interval::new(0,0), Rope::from("b"), i);
            let head = engine.get_head_rev_id().token();
            engine.edit_rev(1, i+1, head, d);
            if i >= max_undos {
                let to_gc : OrdSet<usize> = [i-max_undos].iter().copied().collect();
                engine.gc(&to_gc)
            }
        }

        // spam cmd+z until the available undo history is exhausted
        let mut to_undo = OrdSet::new();
        for i in ((edits-max_undos)..edits).rev() {
            to_undo.insert(i+1);
            engine.undo(to_undo.clone());
        }

        // insert a character at the beginning
        let d1 = Delta::simple_edit(Interval::new(0,0), Rope::from("h"), engine.get_head().len());
        let head = engine.get_head_rev_id().token();
        engine.edit_rev(1, edits+1, head, d1);

        // since character was inserted after gc, editor gcs all undone things
        engine.gc(&to_undo);

        // insert character at end, when this test was added, it panic'd here
        let chars_left = (edits-max_undos)+1;
        let d2 = Delta::simple_edit(Interval::new(chars_left, chars_left), Rope::from("f"), engine.get_head().len());
        let head2 = engine.get_head_rev_id().token();
        engine.edit_rev(1, edits+1, head2, d2);

        let mut soln = String::from("h");
        for _ in 0..(edits-max_undos) {
            soln.push('b');
        }
        soln.push('f');
        assert_eq!(soln, String::from(engine.get_head()));
    }

    #[test]
    fn gc_2() {
        // the smallest values with which it still fails:
        gc_scenario(4,3);
    }

    #[test]
    fn gc_3() {
        // original values this test was created/found with in the UI:
        gc_scenario(35,20);
    }

    #[test]
    fn gc_4() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let d1 = Delta::simple_edit(Interval::new(0,10), Rope::from(""), TEST_STR.len());
        let first_rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, first_rev, d1.clone());
        engine.edit_rev(1, 2, first_rev, d1);
        let gc : OrdSet<usize> = [1usize].iter().copied().collect();
        engine.gc(&gc);
        // shouldn't do anything since it was double-deleted and one was GC'd
        engine.undo([1usize, 2]);
        assert_eq!("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", String::from(engine.get_head()));
    }

    #[test]
    fn gc_5() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let d1 = Delta::simple_edit(Interval::new(0,10), Rope::from(""), TEST_STR.len());
        let initial_rev = engine.get_head_rev_id().token();
        engine.undo([1usize]);
        engine.edit_rev(1, 1, initial_rev, d1.clone());
        engine.edit_rev(1, 2, initial_rev, d1);
        let gc : OrdSet<usize> = [1usize].iter().copied().collect();
        engine.gc(&gc);
        // only one of the deletes was gc'd, the other should still be in effect
        assert_eq!("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", String::from(engine.get_head()));
        // since one of the two deletes was gc'd this should undo the one that wasn't
        engine.undo([2usize]);
        assert_eq!(TEST_STR, String::from(engine.get_head()));
    }

    #[test]
    fn gc_6() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let d1 = Delta::simple_edit(Interval::new(0,10), Rope::from(""), TEST_STR.len());
        let initial_rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, initial_rev, d1.clone());
        engine.undo([1usize, 2]);
        engine.edit_rev(1, 2, initial_rev, d1);
        let gc : OrdSet<usize> = [1usize].iter().copied().collect();
        engine.gc(&gc);
        assert_eq!(TEST_STR, String::from(engine.get_head()));
        // since one of the two deletes was gc'd this should re-do the one that wasn't
        engine.undo(std::iter::empty::<usize>());
        assert_eq!("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz", String::from(engine.get_head()));
    }
