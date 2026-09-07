//! Delta tests: operations and properties.
use super::*;

#[test]
fn simple() {
    let d = Delta::simple_edit(Interval::new(1, 9), Rope::from("era"), 11);
    assert_eq!("herald", d.apply_to_string("hello world"));
    assert_eq!(6, d.new_document_len());
}

#[test]
fn factor() {
    let d = Delta::simple_edit(Interval::new(1, 9), Rope::from("era"), 11);
    let (d1, ss) = d.factor();
    assert_eq!("heraello world", d1.apply_to_string("hello world"));
    assert_eq!("hld", ss.delete_from_string("hello world"));
}

#[test]
fn synthesize() {
    let d = Delta::simple_edit(Interval::new(1, 9), Rope::from("era"), 11);
    let (d1, del) = d.factor();
    let ins = d1.inserted_subset();
    let del = del.transform_expand(&ins);
    let union_str = d1.apply_to_string("hello world");
    let tombstones = ins.complement().delete_from_string(&union_str);
    let new_d = Delta::synthesize(&Rope::from(&tombstones), &ins, &del);
    assert_eq!("herald", new_d.apply_to_string("hello world"));
    let text = del.complement().delete_from_string(&union_str);
    let inv_d = Delta::synthesize(&Rope::from(&text), &del, &ins);
    assert_eq!("hello world", inv_d.apply_to_string("herald"));
}

#[test]
fn inserted_subset() {
    let d = Delta::simple_edit(Interval::new(1, 9), Rope::from("era"), 11);
    let (d1, _ss) = d.factor();
    assert_eq!("hello world", d1.inserted_subset().delete_from_string("heraello world"));
}

#[test]
fn transform_expand() {
    let str1 = "01259DGJKNQTUVWXYcdefghkmopqrstvwxy";
    let s1 = find_deletions(str1, TEST_STR);
    let d = Delta::simple_edit(Interval::new(10, 12), Rope::from("+"), str1.len());
    assert_eq!("01259DGJKN+UVWXYcdefghkmopqrstvwxy", d.apply_to_string(str1));
    let (d2, _ss) = d.factor();
    assert_eq!("01259DGJKN+QTUVWXYcdefghkmopqrstvwxy", d2.apply_to_string(str1));
    let d3 = d2.transform_expand(&s1, false);
    assert_eq!(
        "0123456789ABCDEFGHIJKLMN+OPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
        d3.apply_to_string(TEST_STR)
    );
    let d4 = d2.transform_expand(&s1, true);
    assert_eq!(
        "0123456789ABCDEFGHIJKLMNOP+QRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
        d4.apply_to_string(TEST_STR)
    );
}

#[test]
fn transformer_cursor_matches_fresh_transformer_for_mixed_query_order() {
    let mut builder = Builder::new(12);
    builder.replace(Interval::new(2, 4), Rope::from("ab"));
    builder.replace(Interval::new(7, 7), Rope::from("+"));
    builder.delete(Interval::new(9, 11));
    let delta = builder.build();
    let queries = [
        (0, false),
        (2, false),
        (2, true),
        (8, true),
        (3, false),
        (3, true),
        (12, false),
        (1, false),
    ];
    let mut cached = Transformer::new(&delta);

    for (coordinate, after) in queries {
        let expected = Transformer::new(&delta).transform(coordinate, after);
        assert_eq!(
            expected,
            cached.transform(coordinate, after),
            "coordinate={coordinate}, after={after}"
        );
    }
}

#[test]
fn transform_shrink() {
    let d = Delta::simple_edit(Interval::new(10, 12), Rope::from("+"), TEST_STR.len());
    let (d2, _ss) = d.factor();
    assert_eq!(
        "0123456789+ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
        d2.apply_to_string(TEST_STR)
    );

    let str1 = "0345678BCxyz";
    let s1 = find_deletions(str1, TEST_STR);
    let d3 = d2.transform_shrink(&s1);
    assert_eq!("0345678+BCxyz", d3.apply_to_string(str1));

    let str2 = "356789ABCx";
    let s2 = find_deletions(str2, TEST_STR);
    let d4 = d2.transform_shrink(&s2);
    assert_eq!("356789+ABCx", d4.apply_to_string(str2));
}

#[test]
fn iter_inserts() {
    let mut builder = Builder::new(10);
    builder.replace(Interval::new(2, 2), Rope::from("a"));
    builder.delete(Interval::new(3, 5));
    builder.replace(Interval::new(6, 8), Rope::from("b"));
    let delta = builder.build();

    assert_eq!("01a25b89", delta.apply_to_string("0123456789"));

    let mut iter = delta.iter_inserts();
    assert_eq!(Some(DeltaRegion::new(2, 2, 1)), iter.next());
    assert_eq!(Some(DeltaRegion::new(6, 5, 1)), iter.next());
    assert_eq!(None, iter.next());
}

#[test]
fn iter_deletions() {
    let mut builder = Builder::new(10);
    builder.delete(Interval::new(0, 2));
    builder.delete(Interval::new(4, 6));
    builder.delete(Interval::new(8, 10));
    let delta = builder.build();

    assert_eq!("2367", delta.apply_to_string("0123456789"));

    let mut iter = delta.iter_deletions();
    assert_eq!(Some(DeltaRegion::new(0, 0, 2)), iter.next());
    assert_eq!(Some(DeltaRegion::new(4, 2, 2)), iter.next());
    assert_eq!(Some(DeltaRegion::new(8, 4, 2)), iter.next());
    assert_eq!(None, iter.next());
}

#[test]
fn fancy_bounds() {
    let mut builder = Builder::new(10);
    builder.delete(..2);
    builder.delete(4..=5);
    builder.delete(8..);
    let delta = builder.build();
    assert_eq!("2367", delta.apply_to_string("0123456789"));
}

#[test]
fn is_simple_delete() {
    let d = Delta::simple_edit(10..12, Rope::from("+"), TEST_STR.len());
    assert!(!d.is_simple_delete());

    let d = Delta::simple_edit(Interval::new(0, 0), Rope::from(""), 0);
    assert!(!d.is_simple_delete());

    let d = Delta::simple_edit(Interval::new(10, 11), Rope::from(""), TEST_STR.len());
    assert!(d.is_simple_delete());

    let mut builder = Builder::<RopeInfo>::new(10);
    builder.delete(Interval::new(0, 2));
    builder.delete(Interval::new(4, 6));
    let d = builder.build();
    assert!(!d.is_simple_delete());

    let builder = Builder::<RopeInfo>::new(10);
    let d = builder.build();
    assert!(!d.is_simple_delete());

    let delta = Delta {
        els: vec![
            DeltaElement::Copy(0, 10),
            DeltaElement::Copy(12, 20),
            DeltaElement::Insert(Rope::from("hi")),
        ],
        base_len: 20,
    };

    assert!(!delta.is_simple_delete());
}

#[test]
fn is_identity() {
    let d = Delta::simple_edit(10..12, Rope::from("+"), TEST_STR.len());
    assert!(!d.is_identity());

    let d = Delta::simple_edit(0..0, Rope::from(""), TEST_STR.len());
    assert!(d.is_identity());

    let d = Delta::simple_edit(0..0, Rope::from(""), 0);
    assert!(d.is_identity());
}

#[test]
fn as_simple_insert() {
    let d = Delta::simple_edit(Interval::new(10, 11), Rope::from("+"), TEST_STR.len());
    assert_eq!(None, d.as_simple_insert());

    let d = Delta::simple_edit(Interval::new(10, 10), Rope::from("+"), TEST_STR.len());
    assert_eq!(Some(Rope::from("+")).as_ref(), d.as_simple_insert());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_simple_edit_apply_matches_manual_splice(
        base in string_regex("[a-z0-9]{0,24}").unwrap(),
        insert in string_regex("[a-z0-9]{0,12}").unwrap(),
        raw_start in any::<usize>(),
        raw_end in any::<usize>(),
    ) {
        let interval = normalized_interval(base.len(), raw_start, raw_end);
        let delta = Delta::simple_edit(interval, Rope::from(insert.as_str()), base.len());
        let expected = apply_simple_edit(&base, raw_start, raw_end, &insert);

        prop_assert_eq!(expected, delta.apply_to_string(&base));
    }

    #[test]
    fn prop_simple_edit_factor_and_synthesize_round_trip(
        base in string_regex("[a-z0-9]{0,24}").unwrap(),
        insert in string_regex("[a-z0-9]{0,12}").unwrap(),
        raw_start in any::<usize>(),
        raw_end in any::<usize>(),
    ) {
        let interval = normalized_interval(base.len(), raw_start, raw_end);
        let delta = Delta::simple_edit(interval, Rope::from(insert.as_str()), base.len());
        let expected = apply_simple_edit(&base, raw_start, raw_end, &insert);
        let (insert_delta, deletes) = delta.clone().factor();
        let inserts = insert_delta.inserted_subset();
        let expanded_deletes = deletes.transform_expand(&inserts);
        let union = insert_delta.apply(&Rope::from(base.as_str()));
        let tombstones = inserts.complement().delete_from(&union);
        let rebuilt = Delta::synthesize(&tombstones, &inserts, &expanded_deletes);

        prop_assert_eq!(expected.clone(), delta.apply_to_string(&base));
        prop_assert_eq!(expected, rebuilt.apply_to_string(&base));
    }

    #[test]
    fn prop_builder_delta_apply_matches_manual_script(
        base in string_regex("[a-z0-9]{0,24}").unwrap(),
        edits in proptest::collection::vec(
            (
                any::<usize>(),
                any::<usize>(),
                string_regex("[a-z0-9]{0,8}").unwrap(),
            ),
            0..8,
        ),
    ) {
        let (delta, expected) = build_script_delta(&base, &edits);

        prop_assert_eq!(expected, delta.apply_to_string(&base));
    }

    #[test]
    fn prop_builder_delta_factor_and_synthesize_round_trip(
        base in string_regex("[a-z0-9]{0,24}").unwrap(),
        edits in proptest::collection::vec(
            (
                any::<usize>(),
                any::<usize>(),
                string_regex("[a-z0-9]{0,8}").unwrap(),
            ),
            0..8,
        ),
    ) {
        let (delta, expected) = build_script_delta(&base, &edits);
        let (insert_delta, deletes) = delta.clone().factor();
        let inserts = insert_delta.inserted_subset();
        let expanded_deletes = deletes.transform_expand(&inserts);
        let union = insert_delta.apply(&Rope::from(base.as_str()));
        let tombstones = inserts.complement().delete_from(&union);
        let rebuilt = Delta::synthesize(&tombstones, &inserts, &expanded_deletes);

        prop_assert_eq!(expected.clone(), delta.apply_to_string(&base));
        prop_assert_eq!(expected, rebuilt.apply_to_string(&base));
    }
}
