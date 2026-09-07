//! Engine tests: whiteboard merge property tests.
use super::*;

    #[derive(Clone, Debug)]
    enum MergeTestOp {
        Merge(usize, usize),
        Assert(usize, String),
        AssertAll(String),
        AssertMaxUndoSoFar(usize, usize),
        Edit { ei: usize, p: usize, u: usize, d: Delta<RopeInfo> },
    }

    #[derive(Debug)]
    struct MergeTestState {
        peers: Vec<Engine>,
    }

    impl MergeTestState {
        fn new(count: usize) -> MergeTestState {
            let mut peers = Vec::with_capacity(count);
            for i in 0..count {
                let mut peer = Engine::new(Rope::from(""));
                peer.set_session_id(((i*1000) as u64, 0));
                peers.push(peer);
            }
            MergeTestState { peers }
        }

        fn run_op(&mut self, op: &MergeTestOp) {
            match *op {
                MergeTestOp::Merge(ai, bi) => {
                    let (start, end) = self.peers.split_at_mut(ai);
                    let (a, rest) = end.split_first_mut().unwrap();
                    let b = if bi < ai {
                        &mut start[bi]
                    } else {
                        &mut rest[bi - ai - 1]
                    };
                    a.merge(b);
                },
                MergeTestOp::Assert(ei, ref correct) => {
                    let e = &mut self.peers[ei];
                    assert_eq!(correct, &String::from(e.get_head()), "for peer {}", ei);
                },
                MergeTestOp::AssertMaxUndoSoFar(ei, correct) => {
                    let e = &mut self.peers[ei];
                    assert_eq!(correct, e.max_undo_group_id(), "for peer {}", ei);
                },
                MergeTestOp::AssertAll(ref correct) => {
                    for (ei, e) in self.peers.iter().enumerate() {
                        assert_eq!(correct, &String::from(e.get_head()), "for peer {}", ei);
                    }
                },
                MergeTestOp::Edit { ei, p, u, d: ref delta } => {
                    let e = &mut self.peers[ei];
                    let head = e.get_head_rev_id().token();
                    e.edit_rev(p, u, head, delta.clone());
                },
            }
        }

        fn run_script(&mut self, script: &[MergeTestOp]) {
            for (i, op) in script.iter().enumerate() {
                println!("running {:?} at index {}", op, i);
                self.run_op(op);
            }
        }
    }

    /// Like the scanned whiteboard diagram I have, but without deleting 'a'
    #[test]
    fn merge_insert_only_whiteboard() {
        use self::MergeTestOp::*;
        let script = vec![
            Edit { ei: 2, p: 1, u: 1, d: parse_delta("ab") },
            Merge(0,2), Merge(1, 2),
            Assert(0, "ab".to_owned()),
            Assert(1, "ab".to_owned()),
            Assert(2, "ab".to_owned()),
            Edit { ei: 0, p: 3, u: 1, d: parse_delta("-c-") },
            Edit { ei: 0, p: 3, u: 1, d: parse_delta("---d") },
            Assert(0, "acbd".to_owned()),
            Edit { ei: 1, p: 5, u: 1, d: parse_delta("-p-") },
            Edit { ei: 1, p: 5, u: 1, d: parse_delta("---j") },
            Assert(1, "apbj".to_owned()),
            Edit { ei: 2, p: 1, u: 1, d: parse_delta("z--") },
            Merge(0,2), Merge(1, 2),
            Assert(0, "zacbd".to_owned()),
            Assert(1, "zapbj".to_owned()),
            Merge(0,1),
            Assert(0, "zacpbdj".to_owned()),
        ];
        MergeTestState::new(3).run_script(&script[..]);
    }

    /// Tests that priorities are used to break ties correctly
    #[test]
    fn merge_priorities() {
        use self::MergeTestOp::*;
        let script = vec![
            Edit { ei: 2, p: 1, u: 1, d: parse_delta("ab") },
            Merge(0,2), Merge(1, 2),
            Assert(0, "ab".to_owned()),
            Assert(1, "ab".to_owned()),
            Assert(2, "ab".to_owned()),
            Edit { ei: 0, p: 3, u: 1, d: parse_delta("-c-") },
            Edit { ei: 0, p: 3, u: 1, d: parse_delta("---d") },
            Assert(0, "acbd".to_owned()),
            Edit { ei: 1, p: 5, u: 1, d: parse_delta("-p-") },
            Assert(1, "apb".to_owned()),
            Edit { ei: 2, p: 4, u: 1, d: parse_delta("-r-") },
            Merge(0,2), Merge(1, 2),
            Assert(0, "acrbd".to_owned()),
            Assert(1, "arpb".to_owned()),
            Edit { ei: 1, p: 5, u: 1, d: parse_delta("----j") },
            Assert(1, "arpbj".to_owned()),
            Edit { ei: 2, p: 4, u: 1, d: parse_delta("---z") },
            Merge(0,2), Merge(1, 2),
            Assert(0, "acrbdz".to_owned()),
            Assert(1, "arpbzj".to_owned()),
            Merge(0,1),
            Assert(0, "acrpbdzj".to_owned()),
        ];
        MergeTestState::new(3).run_script(&script[..]);
    }

    /// Tests that merging again when there are no new revisions does nothing
    #[test]
    fn merge_idempotent() {
        use self::MergeTestOp::*;
        let script = vec![
            Edit { ei: 2, p: 1, u: 1, d: parse_delta("ab") },
            Merge(0,2), Merge(1, 2),
            Assert(0, "ab".to_owned()),
            Assert(1, "ab".to_owned()),
            Assert(2, "ab".to_owned()),
            Edit { ei: 0, p: 3, u: 1, d: parse_delta("-c-") },
            Edit { ei: 0, p: 3, u: 1, d: parse_delta("---d") },
            Assert(0, "acbd".to_owned()),
            Edit { ei: 1, p: 5, u: 1, d: parse_delta("-p-") },
            Edit { ei: 1, p: 5, u: 1, d: parse_delta("---j") },
            Merge(0,1),
            Assert(0, "acpbdj".to_owned()),
            Merge(0,1), Merge(1,0), Merge(0,1), Merge(1,0),
            Assert(0, "acpbdj".to_owned()),
            Assert(1, "acpbdj".to_owned()),
        ];
        MergeTestState::new(3).run_script(&script[..]);
    }

    #[test]
    fn merge_associative() {
        use self::MergeTestOp::*;
        let script = vec![
            Edit { ei: 2, p: 1, u: 1, d: parse_delta("ab") },
            Merge(0,2), Merge(1, 2),
            Edit { ei: 0, p: 3, u: 1, d: parse_delta("-c-") },
            Edit { ei: 1, p: 5, u: 1, d: parse_delta("-p-") },
            Edit { ei: 2, p: 2, u: 1, d: parse_delta("z--") },
            // copy the current state
            Merge(3, 0), Merge(4, 1), Merge(5, 2),
            // Do the merge one direction
            Merge(1,2),
            Merge(0,1),
            Assert(0, "zacpb".to_owned()),
            // Do it the other way on the copy
            Merge(4,3),
            Merge(5,4),
            Assert(5, "zacpb".to_owned()),
            // Go crazy
            Merge(0,5), Merge(2,5), Merge(4,5), Merge(1,4),
            Merge(3,1), Merge(5,3),
            AssertAll("zacpb".to_owned()),
        ];
        MergeTestState::new(6).run_script(&script[..]);
    }

    #[test]
    fn merge_simple_delete_1() {
        use self::MergeTestOp::*;
        let script = vec![
            Edit { ei: 0, p: 1, u: 1, d: parse_delta("abc") },
            Merge(1,0),
            Assert(0, "abc".to_owned()),
            Assert(1, "abc".to_owned()),
            Edit { ei: 0, p: 1, u: 1, d: parse_delta("!-d-") },
            Assert(0, "bdc".to_owned()),
            Edit { ei: 1, p: 3, u: 1, d: parse_delta("--efg!") },
            Assert(1, "abefg".to_owned()),
            Merge(1,0),
            Assert(1, "bdefg".to_owned()),
        ];
        MergeTestState::new(2).run_script(&script[..]);
    }

    #[test]
    fn merge_simple_delete_2() {
        use self::MergeTestOp::*;
        let script = vec![
            Edit { ei: 0, p: 1, u: 1, d: parse_delta("ab") },
            Merge(1,0),
            Assert(0, "ab".to_owned()),
            Assert(1, "ab".to_owned()),
            Edit { ei: 0, p: 1, u: 1, d: parse_delta("!-") },
            Assert(0, "b".to_owned()),
            Edit { ei: 1, p: 3, u: 1, d: parse_delta("-c-") },
            Assert(1, "acb".to_owned()),
            Merge(1,0),
            Assert(1, "cb".to_owned()),
        ];
        MergeTestState::new(2).run_script(&script[..]);
    }

    /// I have a scanned whiteboard diagram of doing this merge by hand, good for reference
    #[test]
    fn merge_whiteboard() {
        use self::MergeTestOp::*;
        let script = vec![
            Edit { ei: 2, p: 1, u: 1, d: parse_delta("ab") },
            Merge(0,2), Merge(1, 2), Merge(3, 2),
            Assert(0, "ab".to_owned()),
            Assert(1, "ab".to_owned()),
            Assert(2, "ab".to_owned()),
            Assert(3, "ab".to_owned()),
            Edit { ei: 2, p: 1, u: 1, d: parse_delta("!-") },
            Assert(2, "b".to_owned()),
            Edit { ei: 0, p: 3, u: 1, d: parse_delta("-c-") },
            Edit { ei: 0, p: 3, u: 1, d: parse_delta("---d") },
            Assert(0, "acbd".to_owned()),
            Merge(0,2),
            Assert(0, "cbd".to_owned()),
            Edit { ei: 1, p: 5, u: 1, d: parse_delta("-p-") },
            Merge(1,2),
            Assert(1, "pb".to_owned()),
            Edit { ei: 1, p: 5, u: 1, d: parse_delta("--j") },
            Assert(1, "pbj".to_owned()),
            // to replicate whiteboard, z must be before a tombstone
            // which we can do with another peer that inserts before a and merges.
            Edit { ei: 3, p: 7, u: 1, d: parse_delta("z--") },
            Merge(2,3),
            Merge(0,2), Merge(1, 2),
            Assert(0, "zcbd".to_owned()),
            Assert(1, "zpbj".to_owned()),
            Merge(0,1), // the merge from the whiteboard scan
            Assert(0, "zcpbdj".to_owned()),
        ];
        MergeTestState::new(4).run_script(&script[..]);
    }

    #[test]
    fn merge_max_undo_so_far() {
        use self::MergeTestOp::*;
        let script = vec![
            Edit { ei: 0, p: 1, u: 1, d: parse_delta("ab") },
            Merge(1,0), Merge(2,0),
            AssertMaxUndoSoFar(1,1),
            Edit { ei: 0, p: 1, u: 2, d: parse_delta("!-") },
            Edit { ei: 1, p: 3, u: 3, d: parse_delta("-!") },
            Merge(1,0),
            AssertMaxUndoSoFar(1,3),
            AssertMaxUndoSoFar(0,2),
            Merge(0,1),
            AssertMaxUndoSoFar(0,3),
            Edit { ei: 2, p: 1, u: 1, d: parse_delta("!!") },
            Merge(1,2),
            AssertMaxUndoSoFar(1,3),
        ];
        MergeTestState::new(3).run_script(&script[..]);
    }

    /// This is a regression test to ensure that session IDs are used to break
    /// ties in edit priorities. Otherwise the results may be inconsistent.
    #[test]
    fn merge_session_priorities() {
        use self::MergeTestOp::*;
        let script = vec![
            Edit { ei: 0, p: 1, u: 1, d: parse_delta("ac") },
            Merge(1,0),
            Merge(2,0),
            AssertAll("ac".to_owned()),
            Edit { ei: 0, p: 1, u: 1, d: parse_delta("-d-") },
            Assert(0, "adc".to_owned()),
            Edit { ei: 1, p: 1, u: 1, d: parse_delta("-f-") },
            Merge(2,1),
            Assert(1, "afc".to_owned()),
            Assert(2, "afc".to_owned()),
            Merge(2,0),
            Merge(0,1),
            // These two will be different without using session IDs
            Assert(2, "adfc".to_owned()),
            Assert(0, "adfc".to_owned()),
        ];
        MergeTestState::new(3).run_script(&script[..]);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn prop_two_peer_merge_commutes_and_is_idempotent(
            base in string_regex("[a-z0-9]{0,24}").unwrap(),
            left_insert in string_regex("[a-z0-9]{0,10}").unwrap(),
            right_insert in string_regex("[a-z0-9]{0,10}").unwrap(),
            left_start in any::<usize>(),
            left_end in any::<usize>(),
            right_start in any::<usize>(),
            right_end in any::<usize>(),
            left_priority in 0usize..8,
            right_priority in 0usize..8,
        ) {
            let base_len = base.len();

            let mut left_first = seeded_engine(11, &base);
            let mut right_first = seeded_engine(29, &base);

            let common_base = left_first.get_head_rev_id().token();
            let left_delta = build_simple_delta(base_len, left_start, left_end, &left_insert);
            let right_delta = build_simple_delta(base_len, right_start, right_end, &right_insert);
            left_first.edit_rev(left_priority, 1, common_base, left_delta.clone());
            right_first.edit_rev(right_priority, 2, common_base, right_delta.clone());

            let mut left_second = seeded_engine(11, &base);
            let mut right_second = seeded_engine(29, &base);

            let common_base_2 = left_second.get_head_rev_id().token();
            left_second.edit_rev(left_priority, 1, common_base_2, left_delta);
            right_second.edit_rev(right_priority, 2, common_base_2, right_delta);

            let mut order_a = vec![left_first, right_first];
            merge_one(&mut order_a, 0, 1);
            merge_one(&mut order_a, 1, 0);
            synchronize(&mut order_a);

            let mut order_b = vec![left_second, right_second];
            merge_one(&mut order_b, 1, 0);
            merge_one(&mut order_b, 0, 1);
            synchronize(&mut order_b);

            let head_a = assert_converged(&order_a);
            let head_b = assert_converged(&order_b);
            prop_assert_eq!(head_a, head_b.clone());

            synchronize(&mut order_a);
            let after_repeat = assert_converged(&order_a);
            prop_assert_eq!(head_b, after_repeat);
        }

        #[test]
        fn prop_three_peer_merge_is_associative_and_convergent(
            base in string_regex("[a-z0-9]{0,24}").unwrap(),
            insert_a in string_regex("[a-z0-9]{0,8}").unwrap(),
            insert_b in string_regex("[a-z0-9]{0,8}").unwrap(),
            insert_c in string_regex("[a-z0-9]{0,8}").unwrap(),
            start_a in any::<usize>(),
            end_a in any::<usize>(),
            start_b in any::<usize>(),
            end_b in any::<usize>(),
            start_c in any::<usize>(),
            end_c in any::<usize>(),
            priority_a in 0usize..8,
            priority_b in 0usize..8,
            priority_c in 0usize..8,
        ) {
            let base_len = base.len();
            let deltas = [
                build_simple_delta(base_len, start_a, end_a, &insert_a),
                build_simple_delta(base_len, start_b, end_b, &insert_b),
                build_simple_delta(base_len, start_c, end_c, &insert_c),
            ];
            let priorities = [priority_a, priority_b, priority_c];
            let sessions = [11u64, 29, 47, 111, 129, 147];

            let mut peers: Vec<Engine> = sessions
                .iter()
                .map(|session| seeded_engine(*session, &base))
                .collect();

            for group_start in [0usize, 3] {
                let group_base = peers[group_start].get_head_rev_id().token();
                for offset in 0..3 {
                    peers[group_start + offset].edit_rev(
                        priorities[offset],
                        offset + 1,
                        group_base,
                        deltas[offset].clone(),
                    );
                }
            }

            merge_one(&mut peers, 0, 1);
            merge_one(&mut peers, 0, 2);
            synchronize(&mut peers[0..3]);

            merge_one(&mut peers, 4, 5);
            merge_one(&mut peers, 3, 4);
            synchronize(&mut peers[3..6]);

            let head_left = assert_converged(&peers[0..3]);
            let head_right = assert_converged(&peers[3..6]);
            prop_assert_eq!(head_left, head_right);
        }
    }
