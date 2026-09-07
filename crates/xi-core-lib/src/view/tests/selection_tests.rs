//! Selection, motion, and incremental-find tests.
use super::*;

#[test]
fn upstream_caret_invalidates_previous_visual_line() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("a\nb\nc");
    let mut shadow = line_cache_shadow::Builder::new();
    shadow.add_span(3, 0, line_cache_shadow::ALL_VALID);
    view.lc_shadow = shadow.build();
    view.selection = SelRegion::caret(2).with_affinity(Affinity::Upstream).into();

    view.invalidate_selection(&text);

    let plan =
        line_cache_shadow::RenderPlan { spans: vec![(3, line_cache_shadow::RenderTactic::Render)] };
    let segments = view
        .lc_shadow
        .iter_with_plan(&plan)
        .map(|segment| (segment.our_line_num, segment.n, segment.validity))
        .collect::<Vec<_>>();
    assert_eq!(
        segments,
        vec![
            (0, 2, line_cache_shadow::TEXT_VALID | line_cache_shadow::SYNTAX_VALID),
            (2, 1, line_cache_shadow::ALL_VALID),
        ]
    );
}

#[test]
fn extending_backward_preserves_direction_when_regions_merge() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("abcdefghi");
    let mut selection = Selection::new();
    selection.add_region(SelRegion::caret(2));
    selection.add_region(SelRegion::new(4, 8));
    view.selection = selection;

    view.extend_selection(&text, 1, SelectionGranularity::Point);

    assert_eq!(view.sel_regions(), &[SelRegion::new(4, 1)]);
}

#[test]
fn incremental_find_update() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let mut s = String::new();
    for _ in 0..(FIND_BATCH_SIZE - 2) {
        s += "x";
    }
    s += "aaaaaa";
    for _ in 0..(FIND_BATCH_SIZE) {
        s += "x";
    }
    s += "aaaaaa";
    assert!(!view.find_in_progress());

    let text = Rope::from(&s);
    view.do_edit(
        &text,
        ViewEvent::Find {
            chars: "aaaaaa".to_string(),
            case_sensitive: false,
            regex: false,
            whole_words: false,
        },
    );
    view.do_find(&text);
    assert!(view.find_in_progress());
    view.do_find_all(&text);
    assert_eq!(view.sel_regions().len(), 1);
    assert_eq!(
        view.sel_regions().first(),
        Some(&SelRegion::new(FIND_BATCH_SIZE - 2, FIND_BATCH_SIZE + 6 - 2))
    );
    view.do_find(&text);
    assert!(view.find_in_progress());
    view.do_find_all(&text);
    assert_eq!(view.sel_regions().len(), 2);
}

#[test]
fn incremental_find_codepoint_boundary() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let mut s = String::new();
    for _ in 0..(FIND_BATCH_SIZE + 2) {
        s += "£€äßß";
    }

    assert!(!view.find_in_progress());

    let text = Rope::from(&s);
    view.do_edit(
        &text,
        ViewEvent::Find {
            chars: "a".to_string(),
            case_sensitive: false,
            regex: false,
            whole_words: false,
        },
    );
    view.do_find(&text);
    assert!(view.find_in_progress());
    view.do_find_all(&text);
    assert_eq!(view.sel_regions().len(), 1); // cursor
}

#[test]
fn selection_for_find() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("hello hello world\n");
    view.set_selection(&text, SelRegion::new(6, 11));
    view.do_edit(&text, ViewEvent::SelectionForFind { case_sensitive: false });
    view.do_find(&text);
    view.do_find_all(&text);
    assert_eq!(view.sel_regions().len(), 2);
}

#[test]
fn set_scroll_clamps_inverted_ranges_without_underflow() {
    let mut view = View::new(1.into(), BufferId::new(2));
    view.set_scroll(10, 3);
    assert_eq!(view.first_line, 10);
    assert_eq!(view.scroll_height(), 0);
}

#[test]
fn zero_height_scroll_does_not_underflow_when_selection_moves_cursor() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("alpha\nbeta\ngamma\n");
    view.set_scroll(0, 0);
    view.set_selection(&text, SelRegion::caret(text.len()));
    assert_eq!(view.scroll_height(), 0);
    assert_eq!(view.first_line, view.line_of_offset(&text, text.len()));
}

#[test]
fn find_next() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("hello hello world\n");
    view.do_edit(
        &text,
        ViewEvent::Find {
            chars: "foo".to_string(),
            case_sensitive: false,
            regex: false,
            whole_words: false,
        },
    );
    view.do_find(&text);
    view.do_find_next(&text, false, true, false, &SelectionModifier::Set);
    assert_eq!(view.sel_regions().len(), 1);
    assert_eq!(view.sel_regions().first(), Some(&SelRegion::new(0, 0))); // caret

    view.do_edit(
        &text,
        ViewEvent::Find {
            chars: "hello".to_string(),
            case_sensitive: false,
            regex: false,
            whole_words: false,
        },
    );
    view.do_find(&text);
    assert_eq!(view.sel_regions().len(), 1);
    view.do_find_next(&text, false, true, false, &SelectionModifier::Set);
    assert_eq!(view.sel_regions().first(), Some(&SelRegion::new(0, 5)));
    view.do_find_next(&text, false, true, false, &SelectionModifier::Set);
    assert_eq!(view.sel_regions().first(), Some(&SelRegion::new(6, 11)));
    view.do_find_next(&text, false, true, false, &SelectionModifier::Set);
    assert_eq!(view.sel_regions().first(), Some(&SelRegion::new(0, 5)));
    view.do_find_next(&text, true, true, false, &SelectionModifier::Set);
    assert_eq!(view.sel_regions().first(), Some(&SelRegion::new(6, 11)));

    view.do_find_next(&text, true, true, false, &SelectionModifier::Add);
    assert_eq!(view.sel_regions().len(), 2);
    view.do_find_next(&text, true, true, false, &SelectionModifier::AddRemovingCurrent);
    assert_eq!(view.sel_regions().len(), 1);
    view.do_find_next(&text, true, true, false, &SelectionModifier::None);
    assert_eq!(view.sel_regions().len(), 1);
}

#[test]
fn find_all() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("hello hello world\n hello!");
    view.do_edit(
        &text,
        ViewEvent::Find {
            chars: "foo".to_string(),
            case_sensitive: false,
            regex: false,
            whole_words: false,
        },
    );
    view.do_find(&text);
    view.do_find_all(&text);
    assert_eq!(view.sel_regions().len(), 1); // caret

    view.do_edit(
        &text,
        ViewEvent::Find {
            chars: "hello".to_string(),
            case_sensitive: false,
            regex: false,
            whole_words: false,
        },
    );
    view.do_find(&text);
    view.do_find_all(&text);
    assert_eq!(view.sel_regions().len(), 3);

    view.do_edit(
        &text,
        ViewEvent::Find {
            chars: "foo".to_string(),
            case_sensitive: false,
            regex: false,
            whole_words: false,
        },
    );
    view.do_find(&text);
    view.do_find_all(&text);
    assert_eq!(view.sel_regions().len(), 3);
}

#[test]
fn merge_consecutive_selections_only_joins_touching_regions() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("abcdef");
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(0, 1));
    selection.add_region(SelRegion::new(1, 3));
    selection.add_region(SelRegion::new(4, 6));
    view.set_selection(&text, selection);

    view.do_edit(&text, ViewEvent::MergeConsecutiveSelections);

    assert_eq!(view.sel_regions(), &[SelRegion::new(0, 3), SelRegion::new(4, 6)]);
}

#[test]
fn trim_selections_removes_edge_whitespace() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("  alpha  ");
    view.set_selection(&text, SelRegion::new(0, text.len()));

    view.do_edit(&text, ViewEvent::TrimSelections);

    assert_eq!(view.sel_regions(), &[SelRegion::new(2, 7)]);
}

#[test]
fn flip_and_ensure_forward_adjust_region_direction() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("abcdef");
    view.set_selection(&text, SelRegion::new(1, 4));

    view.do_edit(&text, ViewEvent::FlipSelections);
    assert_eq!(view.sel_regions(), &[SelRegion::new(4, 1)]);

    view.do_edit(&text, ViewEvent::EnsureSelectionsForward);
    assert_eq!(view.sel_regions(), &[SelRegion::new(1, 4)]);
}

#[test]
fn rotate_selections_changes_primary_region_without_reordering_ranges() {
    let mut view = View::new(1.into(), BufferId::new(2));
    let text = Rope::from("abcdef");
    let mut selection = Selection::new();
    selection.add_region(SelRegion::new(0, 1));
    selection.add_region(SelRegion::new(2, 3));
    selection.add_region(SelRegion::new(4, 5));
    view.set_selection(&text, selection);

    assert_eq!(view.primary_sel_region(), Some(SelRegion::new(4, 5)));

    view.do_edit(&text, ViewEvent::RotateSelectionsBackward);
    assert_eq!(view.primary_sel_region(), Some(SelRegion::new(2, 3)));

    view.do_edit(&text, ViewEvent::RotateSelectionsForward);
    assert_eq!(view.primary_sel_region(), Some(SelRegion::new(4, 5)));

    view.do_edit(&text, ViewEvent::RotateSelectionsForward);
    assert_eq!(view.primary_sel_region(), Some(SelRegion::new(0, 1)));
    assert_eq!(
        view.sel_regions(),
        &[SelRegion::new(0, 1), SelRegion::new(2, 3), SelRegion::new(4, 5)]
    );
}
