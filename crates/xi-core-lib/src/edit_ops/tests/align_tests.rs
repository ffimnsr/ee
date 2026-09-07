//! Edit-ops tests: align, sort, reflow, and tab expansion.
use super::*;

#[test]
fn align_selections_pads_columns_across_lines() {
    let text: Rope = "a  b\nab".into();
    let regions =
        [SelRegion::new(0, 1), SelRegion::new(3, 4), SelRegion::new(5, 6), SelRegion::new(6, 7)];

    let delta = align_selections(&text, &regions, 4);

    assert_eq!(String::from(delta.apply(&text)), "a  b\na  b");
}

#[test]
fn align_it_expands_contiguous_matching_block_from_caret() {
    let text: Rope = "a=1\nbbb=22\nskip\nz=3".into();
    let regions = [SelRegion::new(0, 0)];

    let delta = align_it(&text, &regions, 4, "=", false, 1, false, "", None);

    assert_eq!(String::from(delta.apply(&text)), "a   = 1\nbbb = 22\nskip\nz=3");
}

#[test]
fn align_it_uses_selected_lines_and_skips_unmatched_lines() {
    let text: Rope = "a=1\nskip\nbb=2".into();
    let regions = [SelRegion::new(0, text.len())];

    let delta = align_it(&text, &regions, 4, "=", false, 1, false, "", None);

    assert_eq!(String::from(delta.apply(&text)), "a  = 1\nskip\nbb = 2");
}

#[test]
fn align_it_supports_regex_and_explicit_line_ranges() {
    let text: Rope = "apple=1\nbanana += 22\npear ||= 3\nend".into();
    let regions = [SelRegion::new(0, 0)];

    let delta = align_it(&text, &regions, 4, r"\|\|=|\+=|=", true, 1, false, "", Some((0, 2)));

    assert_eq!(String::from(delta.apply(&text)), "apple    = 1\nbanana  += 22\npear   ||= 3\nend");
}

#[test]
fn align_it_supports_nth_match_selection() {
    let text: Rope = "a = 1 => foo\nlong_name = 22 => bar".into();
    let regions = [SelRegion::new(0, 0)];

    let delta = align_it(&text, &regions, 4, r"=>|=", true, 2, false, "", None);

    assert_eq!(String::from(delta.apply(&text)), "a = 1          => foo\nlong_name = 22 => bar");
}

#[test]
fn align_it_supports_all_matches_with_tabular_format() {
    let text: Rope = "abc,def,ghi\na,b\na,b,c".into();
    let regions = [SelRegion::new(0, text.len())];

    let delta = align_it(&text, &regions, 4, ",", false, 1, true, "r1c1l0", None);

    assert_eq!(String::from(delta.apply(&text)), "abc , def, ghi\n  a , b\n  a , b  ,  c");
}

#[test]
fn align_it_supports_custom_spacing_format() {
    let text: Rope = "a=1\nbbb=22".into();
    let regions = [SelRegion::new(0, text.len())];

    let delta = align_it(&text, &regions, 4, "=", false, 1, false, "l0r0l0", None);

    assert_eq!(String::from(delta.apply(&text)), "a  =1\nbbb=22");
}

#[test]
fn sort_lines_uses_whole_buffer_when_only_caret_present() {
    let text: Rope = "z\nc\na\nb".into();
    let regions = [SelRegion::new(0, 0)];

    let delta = sort_lines(&text, &regions, false, None);

    assert_eq!(String::from(delta.apply(&text)), "a\nb\nc\nz");
}

#[test]
fn sort_lines_supports_explicit_reverse_range() {
    let text: Rope = "keep\naaa\nccc\nbbb\nstay".into();
    let regions = [SelRegion::new(0, 0)];

    let delta = sort_lines(&text, &regions, true, Some((1, 3)));

    assert_eq!(String::from(delta.apply(&text)), "keep\nccc\nbbb\naaa\nstay");
}

#[test]
fn reflow_lines_wraps_selected_or_explicit_lines() {
    let text: Rope = "alpha beta\ngamma delta\n\nkeep".into();
    let regions = [SelRegion::new(0, 0)];

    let delta = reflow_lines(&text, &regions, 10, 4, Some((0, 1)));

    assert_eq!(String::from(delta.apply(&text)), "alpha beta\ngamma\ndelta\n\nkeep");
}

#[test]
fn expand_tabs_in_lines_rewrites_selected_lines() {
    let text: Rope = "\talpha\nb\tcd".into();
    let regions = [SelRegion::new(0, text.len())];

    let delta = expand_tabs_in_lines(&text, &regions, 4, None);

    assert_eq!(String::from(delta.apply(&text)), "    alpha\nb   cd");
}
