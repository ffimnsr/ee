//! Edit-ops tests: case-preserving insert and delete-backward.
use super::*;

#[test]
fn insert_preserving_case_handles_common_case_patterns() {
    let text: Rope = "lower Title UPPER mIxEd 123".into();
    let regions = [
        SelRegion::new(0, 5),
        SelRegion::new(6, 11),
        SelRegion::new(12, 17),
        SelRegion::new(18, 23),
        SelRegion::new(24, 27),
    ];

    let delta = insert_preserving_case(&text, &regions, "rEpLaCe");

    assert_eq!(String::from(delta.apply(&text)), "replace Replace REPLACE rEpLaCe rEpLaCe");
}

#[test]
fn delete_backward_merges_overlapping_regions() {
    let text: Rope = "abcd".into();
    let mut config = test_config();
    config.auto_indent = false;
    let regions = [SelRegion::new(2, 2), SelRegion::new(3, 3)];

    let delta = delete_backward(&text, &regions, &config);

    assert_eq!(String::from(delta.apply(&text)), "ad");
}
