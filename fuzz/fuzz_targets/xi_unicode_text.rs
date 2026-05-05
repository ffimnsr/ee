#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use xi_unicode::{
    EmojiExt, LineBreakIterator, LineBreakLeafIter, is_keycap_base, is_variation_selector,
    linebreak_property, linebreak_property_str,
};

#[derive(Arbitrary, Debug)]
struct UnicodeInput {
    bytes: Vec<u8>,
    start_hint: u16,
}

fn snap_to_boundary(text: &str, raw: usize) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut offset = raw % (text.len() + 1);
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn exercise_text(text: &str, start_hint: u16) {
    let mut iter = LineBreakIterator::new(text);
    let mut prev = 0;
    let mut seen_end = text.is_empty();

    for _ in 0..=text.len().saturating_add(1) {
        let Some((offset, _hard)) = iter.next() else {
            break;
        };
        assert!(offset >= prev);
        assert!(offset <= text.len());
        assert!(text.is_char_boundary(offset));
        prev = offset;
        if offset == text.len() {
            seen_end = true;
            break;
        }
    }

    assert!(seen_end);

    for (offset, ch) in text.char_indices() {
        let (_property, width) = linebreak_property_str(text, offset);
        assert!(width > 0);
        let _ = linebreak_property(ch);
        let _ = is_variation_selector(ch);
        let _ = is_keycap_base(ch);
        let _ = ch.is_regional_indicator_symbol();
        let _ = ch.is_emoji_modifier();
        let _ = ch.is_emoji_combining_enclosing_keycap();
        let _ = ch.is_emoji();
        let _ = ch.is_emoji_modifier_base();
        let _ = ch.is_tag_spec_char();
        let _ = ch.is_emoji_cancel_tag();
        let _ = ch.is_zwj();
    }

    let start = snap_to_boundary(text, usize::from(start_hint));
    let mut leaf_iter = LineBreakLeafIter::new(text, start);
    for _ in 0..=text.len().saturating_add(1) {
        let (offset, _hard) = leaf_iter.next(text);
        assert!(offset <= text.len());
        assert!(text.is_char_boundary(offset));
        if offset == text.len() {
            break;
        }
    }
}

fuzz_target!(|input: UnicodeInput| {
    let bytes = if input.bytes.len() > 2048 { &input.bytes[..2048] } else { &input.bytes };

    match std::str::from_utf8(bytes) {
        Ok(text) => exercise_text(text, input.start_hint),
        Err(_) => {
            let lossy = String::from_utf8_lossy(bytes);
            exercise_text(&lossy, input.start_hint);
        }
    }
});
