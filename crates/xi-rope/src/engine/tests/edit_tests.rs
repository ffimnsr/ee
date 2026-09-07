//! Engine tests: edits and try-delta paths.
use super::*;

    #[test]
    fn edit_rev_simple() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let first_rev = engine.get_head_rev_id().token();
        engine.edit_rev(0, 1, first_rev, build_delta_1());
        assert_eq!("0123456789abcDEEFghijklmnopqr999stuvz", String::from(engine.get_head()));
    }

    #[test]
    fn edit_rev_empty() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let first_rev = engine.get_head_rev_id().token();
        let delta = Delta {
            base_len: TEST_STR.len(),
            els: vec![DeltaElement::Copy(0, TEST_STR.len())],
        };
        engine.edit_rev(0, 1, first_rev, delta.clone());
        assert_eq!(TEST_STR, String::from(engine.get_head()));
        engine.edit_rev(0, 1, first_rev, delta);
        assert_eq!(TEST_STR, String::from(engine.get_head()));
    }

    #[test]
    fn edit_rev_concurrent() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let first_rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, first_rev, build_delta_1());
        engine.edit_rev(0, 2, first_rev, build_delta_2());
        assert_eq!("0!3456789abcDEEFGIjklmnopqr888999stuvHIz", String::from(engine.get_head()));
    }

    #[test]
    #[should_panic(expected = "Delta base_len 5 does not match revision length 6")]
    fn edit_rev_bad_delta_len() {
        let test_str = "hello";
        let mut engine = Engine::new(Rope::from(test_str));
        let iv = Interval::new(1, 1);

        let mut builder = Builder::new(test_str.len());
        builder.replace(iv, "1".into());
        let delta1 = builder.build();

        let mut builder = Builder::new(test_str.len());
        builder.replace(iv, "2".into());
        let delta2 = builder.build();

        let rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, rev, delta1);

        // this second delta now has an incorrect length for the engine
        let rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 2, rev, delta2);
    }
    #[test]
    fn edit_rev_undo() {
        undo_test(true, [1usize,2].iter().copied().collect(), TEST_STR);
    }

    #[test]
    fn edit_rev_undo_2() {
        undo_test(true, [2usize].iter().copied().collect(), "0123456789abcDEEFghijklmnopqr999stuvz");
    }

    #[test]
    fn edit_rev_undo_3() {
        undo_test(true, [1usize].iter().copied().collect(), "0!3456789abcdefGIjklmnopqr888stuvwHIyz");
    }
    #[test]
    fn try_delta_rev_head() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let first_rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, first_rev, build_delta_1());
        let d = engine.try_delta_rev_head(first_rev).unwrap();
        assert_eq!(String::from(engine.get_head()), d.apply_to_string(TEST_STR));
    }

    #[test]
    fn try_delta_rev_head_2() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let first_rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, first_rev, build_delta_1());
        engine.edit_rev(0, 2, first_rev, build_delta_2());
        let d = engine.try_delta_rev_head(first_rev).unwrap();
        assert_eq!(String::from(engine.get_head()), d.apply_to_string(TEST_STR));
    }

    #[test]
    fn try_delta_rev_head_3() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let first_rev = engine.get_head_rev_id().token();
        engine.edit_rev(1, 1, first_rev, build_delta_1());
        let after_first_edit = engine.get_head_rev_id().token();
        engine.edit_rev(0, 2, first_rev, build_delta_2());
        let d = engine.try_delta_rev_head(after_first_edit).unwrap();
        assert_eq!(String::from(engine.get_head()), d.apply_to_string("0123456789abcDEEFghijklmnopqr999stuvz"));
    }

    #[test]
    fn try_delta_rev_head_missing_token() {
        let mut engine = Engine::new(Rope::from(TEST_STR));
        let first_rev = engine.get_head_rev_id().token();
        let bad_rev = RevToken::default();
        engine.edit_rev(1, 1, first_rev, build_delta_1());
        let d = engine.try_delta_rev_head(bad_rev);
        assert!(d.is_err());
    }
