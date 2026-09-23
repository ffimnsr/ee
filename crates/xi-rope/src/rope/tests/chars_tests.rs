//! Rope tests: char counts, byte<->char conversions, and char-based edits.
use super::*;

#[test]
fn len_chars_counts_scalar_values() {
    let rope = Rope::from("Hello みんなさん!");
    assert_eq!(rope.len_chars(), "Hello みんなさん!".chars().count());
    assert_eq!(Rope::from("").len_chars(), 0);
    assert_eq!(Rope::from("aé🐸").len_chars(), 3);
}

#[test]
fn len_bytes_matches_len() {
    let rope = Rope::from("aé🐸");
    assert_eq!(rope.len_bytes(), rope.len());
    assert_eq!(rope.len_bytes(), 7);
}

#[test]
fn byte_char_roundtrip() {
    let text = "aé🐸\nhello みんな";
    let rope = Rope::from(text);
    for (byte_idx, _) in text.char_indices() {
        assert_eq!(rope.char_to_byte(rope.byte_to_char(byte_idx)), byte_idx);
    }
    assert_eq!(rope.char_to_byte(rope.len_chars()), rope.len());
    assert_eq!(rope.byte_to_char(rope.len()), rope.len_chars());
}

#[test]
fn byte_to_char_matches_model() {
    let text = "aé🐸\nhello みんな";
    let rope = Rope::from(text);
    for byte_idx in 0..=text.len() {
        assert_eq!(
            rope.byte_to_char(byte_idx),
            byte_to_char_idx_model(text, byte_idx),
            "byte_to_char({byte_idx})"
        );
    }
}

#[test]
fn len_utf16_cu_counts_surrogate_pairs() {
    assert_eq!(Rope::from("aé🐸").len_utf16_cu(), 4);
    assert_eq!(Rope::from("aé🐸").len_utf16_cu(), "aé🐸".encode_utf16().count());
    assert_eq!(Rope::from("").len_utf16_cu(), 0);
}

#[test]
fn char_at_fetches_scalar_values() {
    let rope = Rope::from("aé🐸");
    assert_eq!(rope.char_at(0), 'a');
    assert_eq!(rope.char_at(1), 'é');
    assert_eq!(rope.char_at(2), '🐸');
}

#[test]
fn get_char_across_leaf_boundary() {
    // 3-byte chars, 700 glyphs = 2100 bytes, spans multiple leaves.
    let filler = "あ".repeat(700);
    let text = format!("{filler}{filler}");
    let rope = Rope::from(text.as_str());
    assert_eq!(rope.get_char(699), Some('あ'));
    assert_eq!(rope.get_char(700), Some('あ'));
    assert_eq!(rope.get_char(1400), None);
}

#[test]
fn get_char_out_of_bounds() {
    let rope = Rope::from("hello");
    assert_eq!(rope.get_char(5), None);
    assert_eq!(rope.get_char(6), None);
    assert_eq!(Rope::from("").get_char(0), None);
}

#[test]
fn char_to_byte_matches_model() {
    let text = "aé🐸\nhello みんな";
    let rope = Rope::from(text);
    for char_idx in 0..=text.chars().count() {
        assert_eq!(
            rope.char_to_byte(char_idx),
            char_to_byte_idx_model(text, char_idx),
            "char_to_byte({char_idx})"
        );
    }
}

#[test]
fn char_to_byte_out_of_bounds() {
    let rope = Rope::from("hello");
    assert_eq!(
        rope.try_char_to_byte(6),
        Err(RopeError::CharOffsetOutOfBounds { offset: 6, len: 5 })
    );
}

#[test]
fn byte_to_char_out_of_bounds() {
    let rope = Rope::from("hello");
    assert_eq!(rope.try_byte_to_char(6), Err(RopeError::OffsetOutOfBounds { offset: 6, len: 5 }));
}

#[test]
fn insert_char_positions_multibyte() {
    let mut rope = Rope::from("helloworld");
    rope.insert(5, " みんな");
    assert_eq!(String::from(&rope), "hello みんなworld");
    rope.insert(0, "^");
    rope.insert(rope.len_chars(), "$");
    assert_eq!(String::from(&rope), "^hello みんなworld$");
}

#[test]
fn insert_into_empty_rope() {
    let mut rope = Rope::from("");
    rope.insert(0, "こんにちは");
    assert_eq!(String::from(&rope), "こんにちは");
    assert_eq!(rope.len_chars(), 5);
}

#[test]
fn try_insert_out_of_bounds() {
    let mut rope = Rope::from("hello");
    assert_eq!(
        rope.try_insert(6, "x"),
        Err(RopeError::CharOffsetOutOfBounds { offset: 6, len: 5 })
    );
    rope.insert(5, "!");
    assert_eq!(String::from(&rope), "hello!");
}

#[test]
fn insert_char_single() {
    let mut rope = Rope::from("");
    rope.insert_char(0, 'こ');
    rope.insert_char(1, 'ん');
    rope.insert_char(1, 'な');
    assert_eq!(String::from(&rope), "こなん");
}

#[test]
fn remove_ranges_ops() {
    let mut rope = Rope::from("Hello みんなさん");
    rope.remove(5..5);
    assert_eq!(String::from(&rope), "Hello みんなさん");

    let mut rope = Rope::from("Hello みんなさん");
    rope.remove(..5);
    assert_eq!(String::from(&rope), " みんなさん");

    let mut rope = Rope::from("Hello みんなさん");
    rope.remove(6..);
    assert_eq!(String::from(&rope), "Hello ");

    let mut rope = Rope::from("Hello みんなさん");
    rope.remove(..);
    assert_eq!(String::from(&rope), "");

    let mut rope = Rope::from("Hello みんなさん");
    rope.remove(1..=3);
    assert_eq!(String::from(&rope), "Ho みんなさん");
}

#[test]
fn remove_reversed_range_error() {
    let mut rope = Rope::from("hello");
    let start = 3;
    let end = 1;
    assert_eq!(
        rope.try_remove(start..end),
        Err(RopeError::CharReversedInterval { start: 3, end: 1 })
    );
}

#[test]
fn remove_out_of_bounds_error() {
    let mut rope = Rope::from("hello");
    assert_eq!(
        rope.try_remove(4..6),
        Err(RopeError::CharIntervalOutOfBounds { start: 4, end: 6, len: 5 })
    );
}

#[test]
fn replace_char_range_ops() {
    let mut rope = Rope::from("Hello みんなさん");
    rope.replace(6..11, "world");
    assert_eq!(String::from(&rope), "Hello world");

    let mut rope = Rope::from("Hello");
    rope.replace(.., "");
    assert_eq!(String::from(&rope), "");
}

#[test]
fn try_replace_out_of_bounds_error() {
    let mut rope = Rope::from("hello");
    assert_eq!(
        rope.try_replace(2..8, "x"),
        Err(RopeError::CharIntervalOutOfBounds { start: 2, end: 8, len: 5 })
    );
}

#[test]
fn remove_across_leaf_boundary() {
    let filler = "x".repeat(1000);
    let text = format!("{filler}🌊{filler}");
    let mut rope = Rope::from(text.clone());
    rope.remove(filler.len()..filler.len() + 1);
    let expected: String =
        text.chars().take(filler.len()).chain(text.chars().skip(filler.len() + 1)).collect();
    assert_eq!(String::from(&rope), expected);
}

#[test]
fn insert_across_leaf_boundary() {
    // 3-byte chars, 700 glyphs = 2100 bytes, spans multiple leaves.
    let filler = "あ".repeat(700);
    let mut rope = Rope::from(filler.clone());
    rope.insert(350, "X");
    let expected: String = filler
        .chars()
        .take(350)
        .chain(std::iter::once('X'))
        .chain(filler.chars().skip(350))
        .collect();
    assert_eq!(String::from(&rope), expected);
}

#[test]
fn char_edits_preserve_metrics() {
    let mut rope = Rope::from("Hello\n世界");
    rope.insert(6, "みんな");
    assert_eq!(String::from(&rope), "Hello\nみんな世界");
    rope.remove(0..6);
    assert_eq!(String::from(&rope), "みんな世界");
    assert_eq!(rope.slice(0..3), Rope::from("み"));
    assert_matches_str(&rope, "みんな世界");
}

#[test]
fn split_off_basic() {
    let mut rope = Rope::from("Hello みんなさん world");
    let right = rope.split_off(6);
    assert_eq!(String::from(&rope), "Hello ");
    assert_eq!(String::from(&right), "みんなさん world");
    assert_matches_str(&rope, "Hello ");
    assert_matches_str(&right, "みんなさん world");
}

#[test]
fn split_off_edges() {
    let text = "aé🐸";
    let mut rope = Rope::from(text);
    assert_eq!(String::from(&rope.split_off(0)), text);
    assert_eq!(String::from(&rope), "");
    let mut rope = Rope::from(text);
    assert_eq!(String::from(&rope.split_off(3)), "");
    assert_eq!(String::from(&rope), text);
}

#[test]
fn split_off_roundtrip_restores_original() {
    let text = "あ".repeat(700) + "xé🐸\n" + &"あ".repeat(700);
    let mut rope = Rope::from(text.as_str());
    let right = rope.split_off(350);
    // First 350 chars are all 'あ' (3 bytes each), so byte slicing is aligned.
    assert_matches_str(&rope, &text[..1050]);
    assert_matches_str(&right, &text[1050..]);
    rope.append(right);
    assert_matches_str(&rope, &text);
}

#[test]
fn split_off_then_independent_edits() {
    let mut rope = Rope::from("Hello みんな");
    let mut right = rope.split_off(5);
    rope.insert(0, "X");
    right.remove(0..2);
    assert_eq!(String::from(&rope), "XHello");
    assert_eq!(String::from(&right), "んな");
    assert_matches_str(&rope, "XHello");
    assert_matches_str(&right, "んな");
}

#[test]
fn try_split_off_out_of_bounds() {
    let mut rope = Rope::from("hello");
    assert!(matches!(
        rope.try_split_off(6),
        Err(RopeError::CharOffsetOutOfBounds { offset: 6, len: 5 })
    ));
    assert_eq!(String::from(&rope), "hello");
}

#[test]
#[should_panic]
fn split_off_panics_out_of_bounds() {
    let mut rope = Rope::from("hello");
    rope.split_off(6);
}

#[test]
fn append_basic() {
    let mut rope = Rope::from("Hello みんな");
    rope.append(Rope::from("さん"));
    assert_matches_str(&rope, "Hello みんなさん");
    let mut empty = Rope::from("");
    empty.append(Rope::from("X"));
    assert_matches_str(&empty, "X");
    let mut rope2 = Rope::from("Y");
    rope2.append(Rope::from(""));
    assert_matches_str(&rope2, "Y");
}

#[test]
fn append_multileaf_inner_boundary() {
    let a = "あ".repeat(700);
    let b = "xé🐸\n".to_owned() + &"あ".repeat(700);
    let mut rope = Rope::from(a.clone());
    rope.append(Rope::from(b.as_str()));
    assert_matches_str(&rope, &(a + &b));
}

#[test]
fn split_off_shares_subtrees() {
    let text = "あ".repeat(700) + "xé🐸\n" + &"あ".repeat(700);
    let mut rope = Rope::from(text.as_str());
    let right = rope.split_off(350);
    // The two halves share storage where they do not overlap; edits must not
    // bleed across the boundary.
    let mut clone = rope.clone();
    clone.insert(0, "ZZ");
    assert_eq!(String::from(&rope), &text[..1050]);
    assert_eq!(String::from(&clone), "ZZ".to_string() + &text[..1050]);
    assert_eq!(String::from(&right), &text[1050..]);
}

#[test]
fn split_off_at_leaf_boundary() {
    // 3-byte chars: 700 glyphs = 2100 bytes, but the first leaf is clamped to
    // 1023 bytes (341 chars). Splitting exactly at that leaf end exercises
    // the cross-leaf share path.
    let filler = "あ".repeat(700);
    let mut rope = Rope::from(filler.as_str());
    let right = rope.split_off(341);
    assert_matches_str(&rope, &filler[..1023]);
    assert_matches_str(&right, &filler[1023..]);
}

#[test]
fn append_many_small() {
    let mut rope = Rope::from("");
    let mut model = String::new();
    for ch in "Hello みんなさん world 🐸".chars() {
        rope.append(Rope::from(ch.to_string()));
        model.push(ch);
    }
    assert_matches_str(&rope, &model);
    // The repeated-appends tree must still behave like a normal rope.
    assert_eq!(rope.char_at(6), 'み');
    assert_eq!(rope.byte_to_char(0), 0);
    rope.insert(0, "X");
    rope.remove(1..2);
    assert_matches_str(&rope, &format!("X{}", &model[1..]));
}

#[test]
fn append_self_clone() {
    let mut rope = Rope::from("abc");
    let clone = rope.clone();
    rope.append(clone);
    assert_matches_str(&rope, "abcabc");
}

#[test]
fn randomized_split_append_roundtrip() {
    let mut rng = Lcg(0x57e17);
    let mut rope = Rope::from("");
    let mut model = String::new();
    for _ in 0..150 {
        let n = rng.below(3) + 1;
        let text: String = std::iter::repeat_with(|| match rng.below(4) {
            0 => 'a',
            1 => 'é',
            2 => '🐸',
            _ => '\n',
        })
        .take(n as usize)
        .collect();
        let len = rope.len_chars();
        let pos = rng.below(len as u64 + 1) as usize;
        match rng.below(3) {
            0 => {
                rope.insert(pos, &text);
                insert_at_char(&mut model, pos, &text);
            }
            1 => {
                let end = (pos + rng.below(len as u64 - pos as u64 + 1) as usize).min(len);
                rope.remove(pos..end);
                replace_char_range(&mut model, pos, end, "");
            }
            _ => {
                let end = (pos + rng.below(len as u64 - pos as u64 + 1) as usize).min(len);
                rope.replace(pos..end, &text);
                replace_char_range(&mut model, pos, end, &text);
            }
        }
        assert_matches_str(&rope, &model);

        if rng.below(4) == 0 && !model.is_empty() {
            let k = rng.below(rope.len_chars() as u64 + 1) as usize;
            let mut right = rope.split_off(k);
            rope.assert_integrity();
            right.assert_integrity();
            let k_byte = char_to_byte_idx_model(&model, k);
            let mut right_model = model[k_byte..].to_string();
            model.truncate(k_byte);
            assert_matches_str(&rope, &model);
            assert_matches_str(&right, &right_model);

            if rng.below(2) == 0 {
                rope.insert(0, "«");
                insert_at_char(&mut model, 0, "«");
            }
            if rng.below(2) == 0 && !right_model.is_empty() {
                right.remove(0..1);
                replace_char_range(&mut right_model, 0, 1, "");
            }
            rope.append(right);
            model.push_str(&right_model);
            assert_matches_str(&rope, &model);
            rope.assert_integrity();
        }
    }
}

#[test]
fn assert_integrity_passes_on_common_shapes() {
    for text in ["", "x", "Hello みんなさん 🐸\n", &("あ".repeat(700) + "xé🐸\n")] {
        let rope = Rope::from(text);
        rope.assert_integrity();
        rope.assert_invariants();
    }
    // After edit chains.
    let mut rope = Rope::from("Hello みんなさん");
    for _ in 0..20 {
        rope.insert(rope.len_chars() / 2, "x🐸");
        rope.assert_integrity();
    }
    for _ in 0..20 {
        rope.remove(0..1);
        rope.assert_integrity();
    }
    // After split/append, on both halves.
    let text = "あ".repeat(700) + "xé🐸\n" + &"あ".repeat(700);
    let mut rope = Rope::from(text.as_str());
    let right = rope.split_off(350);
    rope.assert_integrity();
    right.assert_integrity();
    rope.append(right);
    rope.assert_integrity();
}

#[test]
fn randomized_char_edits_match_string() {
    let mut rng = Lcg(0x5eed);
    let mut rope = Rope::from("");
    let mut model = String::new();
    for _ in 0..500 {
        let n = rng.below(5) + 1;
        let text: String = std::iter::repeat_with(|| match rng.below(4) {
            0 => 'a',
            1 => 'é',
            2 => '🐸',
            _ => '\n',
        })
        .take(n as usize)
        .collect();
        let len = rope.len_chars();
        let pos = rng.below(len as u64 + 1) as usize;
        match rng.below(3) {
            0 => {
                rope.insert(pos, &text);
                insert_at_char(&mut model, pos, &text);
            }
            1 => {
                let end = (pos + rng.below(len as u64 - pos as u64 + 1) as usize).min(len);
                rope.remove(pos..end);
                replace_char_range(&mut model, pos, end, "");
            }
            _ => {
                let end = (pos + rng.below(len as u64 - pos as u64 + 1) as usize).min(len);
                rope.replace(pos..end, &text);
                replace_char_range(&mut model, pos, end, &text);
            }
        }
        assert_matches_str(&rope, &model);
        rope.assert_integrity();
        // Spot-check conversions against the model on the current tree shape;
        // random edits build varied multi-leaf trees, exercising the metric
        // traversal. The model may be empty, hence +1.
        let probe_byte = rng.below(model.len() as u64 + 1) as usize;
        assert_eq!(
            rope.byte_to_char(probe_byte),
            byte_to_char_idx_model(&model, probe_byte),
            "byte_to_char({probe_byte})"
        );
        let probe_char = rng.below(rope.len_chars() as u64 + 1) as usize;
        assert_eq!(
            rope.char_to_byte(probe_char),
            char_to_byte_idx_model(&model, probe_char),
            "char_to_byte({probe_char})"
        );
    }
}
