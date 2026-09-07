//! Engine tests: rearrange, find_common, compute_deltas, transforms, rebase.
use super::*;
use crate::engine::merge::{compute_deltas, compute_transforms, find_base_index, find_common, rearrange, rebase};

    fn basic_rev(i: usize) -> RevId {
        RevId { session1: 1, session2: 0, num: i as u32 }
    }

    fn basic_insert_ops(inserts: Vec<Subset>, priority: usize) -> Vec<Revision> {
        inserts.into_iter().enumerate().map(|(i, inserts)| {
            let deletes = Subset::new(inserts.len());
            Revision {
                rev_id: basic_rev(i+1),
                max_undo_so_far: i+1,
                edit: Contents::Edit {
                    priority, inserts, deletes,
                    undo_group: i+1,
                }
            }
        }).collect()
    }

    #[test]
    fn rearrange_1() {
        let inserts = parse_subset_list("
        ##
        -#-
        #---
        ---#-
        -----#
        #------
        ");
        let revs = basic_insert_ops(inserts, 1);
        let base: BTreeSet<RevId> = [3,5].iter().cloned().map(basic_rev).collect();

        let rearranged = rearrange(&revs, &base, 7);
        let rearranged_inserts: Vec<Subset> = rearranged.into_iter().map(|c| {
            match c.edit {
                Contents::Edit {inserts, ..} => inserts,
                Contents::Undo { .. } => panic!(),
            }
        }).collect();

        debug_subsets(&rearranged_inserts);
        let correct = parse_subset_list("
        -##-
        --#--
        ---#--
        #------
        ");
        assert_eq!(correct, rearranged_inserts);
    }

    fn ids_to_fake_revs(ids: &[usize]) -> Vec<Revision> {
        let contents = Contents::Edit {
            priority: 0,
            undo_group: 0,
            inserts: Subset::new(0),
            deletes: Subset::new(0),
        };

        ids.iter().cloned().map(|i| {
            Revision {
                rev_id: basic_rev(i),
                max_undo_so_far: i,
                edit: contents.clone()
            }
        }).collect()
    }

    #[test]
    fn find_common_1() {
        let a: Vec<Revision> = ids_to_fake_revs(&[0,2,4,6,8,10,12]);
        let b: Vec<Revision> = ids_to_fake_revs(&[0,1,2,4,5,8,9]);
        let res = find_common(&a, &b);

        let correct: BTreeSet<RevId> = [0,2,4,8].iter().cloned().map(basic_rev).collect();
        assert_eq!(correct, res);
    }


    #[test]
    fn find_base_1() {
        let a: Vec<Revision> = ids_to_fake_revs(&[0,2,4,6,8,10,12]);
        let b: Vec<Revision> = ids_to_fake_revs(&[0,1,2,4,5,8,9]);
        let res = find_base_index(&a, &b);

        assert_eq!(1, res);
    }

    #[test]
    fn find_base_2() {
        let a: Vec<Revision> = ids_to_fake_revs(&[0,1,2,6,8]);
        let b: Vec<Revision> = ids_to_fake_revs(&[0,1,2,7,9]);
        let res = find_base_index(&a, &b);

        assert_eq!(3, res);
    }

    #[test]
    fn compute_deltas_1() {
        let inserts = parse_subset_list("
        -##-
        --#--
        ---#--
        #------
        ");
        let revs = basic_insert_ops(inserts, 1);

        let text = Rope::from("13456");
        let tombstones = Rope::from("27");
        let deletes_from_union = parse_subset("-#----#");
        let delta_ops = compute_deltas(&revs, &text, &tombstones, &deletes_from_union);

        println!("{:#?}", delta_ops);

        let mut r = Rope::from("27");
        for op in &delta_ops {
            r = op.inserts.apply(&r);
        }
        assert_eq!("1234567", String::from(r));
    }

    #[test]
    fn compute_transforms_1() {
        let inserts = parse_subset_list("
        -##-
        --#--
        ---#--
        #------
        ");
        let revs = basic_insert_ops(inserts, 1);

        let expand_by = compute_transforms(revs);
        assert_eq!(1, expand_by.len());
        assert_eq!(1, expand_by[0].0.priority);
        let subset_str = format!("{:#?}", expand_by[0].1);
        assert_eq!("#-####-", &subset_str);
    }

    #[test]
    fn compute_transforms_2() {
        let inserts_1 = parse_subset_list("
        -##-
        --#--
        ");
        let mut revs = basic_insert_ops(inserts_1, 1);
        let inserts_2 = parse_subset_list("
        ----
        ");
        let mut revs_2 = basic_insert_ops(inserts_2, 4);
        revs.append(&mut revs_2);
        let inserts_3 = parse_subset_list("
        ---#--
        #------
        ");
        let mut revs_3 = basic_insert_ops(inserts_3, 2);
        revs.append(&mut revs_3);

        let expand_by = compute_transforms(revs);
        assert_eq!(2, expand_by.len());
        assert_eq!(1, expand_by[0].0.priority);
        assert_eq!(2, expand_by[1].0.priority);

        let subset_str = format!("{:#?}", expand_by[0].1);
        assert_eq!("-###-", &subset_str);
        let subset_str = format!("{:#?}", expand_by[1].1);
        assert_eq!("#---#--", &subset_str);
    }

    #[test]
    fn rebase_1() {
        let inserts = parse_subset_list("
        --#-
        ----#
        ");
        let a_revs = basic_insert_ops(inserts.clone(), 1);
        let b_revs = basic_insert_ops(inserts, 2);

        let text_b = Rope::from("zpbj");
        let tombstones_b = Rope::from("a");
        let deletes_from_union_b = parse_subset("-#---");
        let b_delta_ops = compute_deltas(&b_revs, &text_b, &tombstones_b, &deletes_from_union_b);

        println!("{:#?}", b_delta_ops);

        let text_a = Rope::from("zcbd");
        let tombstones_a = Rope::from("a");
        let deletes_from_union_a = parse_subset("-#---");
        let expand_by = compute_transforms(a_revs);

        let (revs, text_2, tombstones_2, deletes_from_union_2) =
            rebase(expand_by, b_delta_ops, text_a, tombstones_a, deletes_from_union_a, 0);

        let rebased_inserts: Vec<Subset> = revs.into_iter().map(|c| {
            match c.edit {
                Contents::Edit {inserts, ..} => inserts,
                Contents::Undo { .. } => panic!(),
            }
        }).collect();

        debug_subsets(&rebased_inserts);
        let correct = parse_subset_list("
        ---#--
        ------#
        ");
        assert_eq!(correct, rebased_inserts);


        assert_eq!("zcpbdj", String::from(&text_2));
        assert_eq!("a", String::from(&tombstones_2));
        assert_eq!("-#-----", format!("{:#?}", deletes_from_union_2));
    }
