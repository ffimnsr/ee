//! Edit-ops tests: transpose, rotate, and reverse selections.
use super::*;

#[test]
fn transpose_skips_overlapping_mixed_regions() {
    let text: Rope = "abcd".into();
    let regions = [SelRegion::new(1, 3), SelRegion::new(2, 2)];

    let delta = transpose(&text, &regions);

    assert_eq!(String::from(delta.apply(&text)), "abcd");
}

#[test]
fn transpose_skips_eol_overlap_after_adjustment() {
    let text: Rope = "ab\n".into();
    let regions = [SelRegion::new(0, 0), SelRegion::new(2, 2)];

    let delta = transpose(&text, &regions);

    assert_eq!(String::from(delta.apply(&text)), "a\nb");
}

#[test]
fn transpose_handles_multibyte_eol_grapheme() {
    let text: Rope = "1ё\n".into();
    let regions = [SelRegion::new(3, 3)];

    let delta = transpose(&text, &regions);

    assert_eq!(String::from(delta.apply(&text)), "1\nё");
}
#[test]
fn rotate_selection_contents_forward_wraps_last_into_first() {
    let text: Rope = "aa bb cc".into();
    let regions = [SelRegion::new(0, 2), SelRegion::new(3, 5), SelRegion::new(6, 8)];

    let delta = rotate_selection_contents(&text, &regions, true);

    assert_eq!(String::from(delta.apply(&text)), "cc aa bb");
}

#[test]
fn rotate_selection_contents_backward_wraps_first_into_last() {
    let text: Rope = "aa bb cc".into();
    let regions = [SelRegion::new(0, 2), SelRegion::new(3, 5), SelRegion::new(6, 8)];

    let delta = rotate_selection_contents(&text, &regions, false);

    assert_eq!(String::from(delta.apply(&text)), "bb cc aa");
}

#[test]
fn reverse_selection_contents_reverses_each_selection() {
    let text: Rope = "ab cde z".into();
    let regions = [SelRegion::new(0, 2), SelRegion::new(3, 6)];

    let delta = reverse_selection_contents(&text, &regions);

    assert_eq!(String::from(delta.apply(&text)), "ba edc z");
}

#[test]
fn reverse_selection_contents_preserves_utf8_codepoints() {
    let text: Rope = "aéß".into();
    let regions = [SelRegion::new(0, text.len())];

    let delta = reverse_selection_contents(&text, &regions);

    assert_eq!(String::from(delta.apply(&text)), "ßéa");
}
