//! Backend syntax span cache tests: hits, invalidation, and bounds.
use super::*;
use crate::view::syntax_cache::SyntaxSpanCache;
use xi_rope::{DeltaBuilder, Interval};

/// Renders `view` for a window and returns the span rows of the payload.
fn render_span_values(
    view: &mut View,
    store: &dyn RenderSource,
    client: &Client,
    peer: &RecordingPeer,
    first: usize,
    last: usize,
) -> Vec<Value> {
    view.request_lines(store, client, first, last, true, "rust", true);
    span_values(&peer.take_notifications())
}

#[test]
fn scroll_back_is_served_from_the_span_cache() {
    // The trigger case: the plan discards the rows of the window we scroll away
    // from, so returning to it re-renders them. That re-render must not re-walk the
    // syntax.
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust"]);
    let text =
        Rope::from((0..200).map(|index| format!("let value_{index} = 42;\n")).collect::<String>());
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(&text, 80);
    let (client, peer) = recording_client();
    let store = RopeTextStore::new(text.clone(), 0);

    let cold = render_span_values(&mut view, &store, &client, &peer, 0, 199);
    assert!(!cold.is_empty(), "the cold window should carry spans");

    // Jump away far enough that the plan discards the window's rows.
    let _ = render_span_values(&mut view, &store, &client, &peer, 400, 599);

    let back = render_span_values(&mut view, &store, &client, &peer, 0, 199);
    let (hits, _misses, cached, _rows) = view.syntax_cache_stats();

    assert!(!back.is_empty(), "returning to a discarded window must re-render its rows");
    // The plan may re-render a few more or fewer rows than the cold pass; what the
    // cache guarantees is that every row it serves carries the spans that walk
    // produced, never something re-derived.
    for (row, spans) in back.iter().enumerate() {
        assert!(
            cold.contains(spans),
            "row {row} of the cached render is not what the walk produced: {spans:?} not in {cold:?}"
        );
    }
    assert!(hits >= 1, "the re-render should be served from the cache, hits={hits}");
    assert!(cached <= SyntaxSpanCache::MAX_ENTRIES);
}

#[test]
fn edits_drop_cached_windows_and_recompute() {
    // The stale-serve guard: after an edit the next render must re-walk, and its
    // spans must equal what an independent view over the edited text produces.
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust"]);
    let text = Rope::from("let value = compute();\nlet other = 2;\n");
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(&text, 80);
    let (client, peer) = recording_client();
    let store = RopeTextStore::new(text.clone(), 0);

    let before = render_span_values(&mut view, &store, &client, &peer, 0, 1);
    assert!(!before.is_empty(), "the window should carry spans");
    let (_, misses_before, ..) = view.syntax_cache_stats();

    // Turn the first line into a comment, which changes its scopes.
    let mut edited = text.clone();
    let mut builder = DeltaBuilder::new(text.len());
    builder.replace(Interval::new(0, 0), "// ".into());
    let delta = builder.build();
    edited.try_edit(0..0, "// ").expect("insertion at the document start is in bounds");
    let mut width_cache = WidthCache::new();
    view.after_edit(&edited, &text, &delta, &client, &mut width_cache, InsertDrift::Default);

    // A fresh view over the edited text is the reference the cache must match.
    let mut reference_view = View::new(1.into(), BufferId::new(2));
    reference_view.debug_force_rewrap_cols(&edited, 80);
    let (reference_client, reference_peer) = recording_client();
    let reference_store = RopeTextStore::new(edited.clone(), 0);
    let reference = render_span_values(
        &mut reference_view,
        &reference_store,
        &reference_client,
        &reference_peer,
        0,
        1,
    );

    let after =
        render_span_values(&mut view, &RopeTextStore::new(edited.clone(), 0), &client, &peer, 0, 1);
    let (_, misses_after, ..) = view.syntax_cache_stats();

    assert!(misses_after > misses_before, "an edit must invalidate the window");
    assert!(!after.is_empty(), "the edited window should carry spans");
    assert_ne!(after[0], before[0], "the edit must change the row's spans (comment scope)");
    assert_eq!(after[0], reference[0], "the re-walk must match an independent view");
}

#[test]
fn partially_covered_requests_walk() {
    // Coverage, not intersection: a request that overlaps a cached window but is not
    // covered by it must walk the whole window, exactly like the uncached path.
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust"]);
    let text = Rope::from(
        (0..40).map(|index| format!("let value_{index} = compute({index});\n")).collect::<String>(),
    );
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(&text, 80);
    let store = RopeTextStore::new(text.clone(), 0);

    let first = view.cached_syntax_spans_for_segment(&store, 0, 20, "rust", true);
    assert_eq!(first.len(), 20, "the first window should be walked for every row");
    let (hits_0, misses_0, ..) = view.syntax_cache_stats();

    // Rows 10..20 are cached, 20..30 are not.
    let partial = view.cached_syntax_spans_for_segment(&store, 10, 20, "rust", true);
    let (hits_1, misses_1, ..) = view.syntax_cache_stats();
    assert_eq!(misses_1, misses_0 + 1, "a partly covered request must walk");
    assert_eq!(hits_1, hits_0, "a partly covered request must not count as a hit");

    let mut reference_view = View::new(1.into(), BufferId::new(2));
    reference_view.debug_force_rewrap_cols(&text, 80);
    let reference = reference_view.cached_syntax_spans_for_segment(&store, 10, 20, "rust", true);
    assert_eq!(partial, reference, "the walked window must match the uncached path");

    // The walk recorded 10..30, so the same request is now covered end to end.
    let again = view.cached_syntax_spans_for_segment(&store, 10, 20, "rust", true);
    let (hits_2, misses_2, ..) = view.syntax_cache_stats();
    assert_eq!(again, partial, "a covered repeat must reproduce the walked spans");
    assert_eq!(misses_2, misses_1, "a covered repeat must not walk");
    assert!(hits_2 > hits_1, "a covered repeat must hit");
}

#[test]
fn rewrap_drops_cached_windows() {
    // Wrapping decides what a visual line is, so entries keyed by line numbers stop
    // describing the rows they were walked for.
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust"]);
    let text = Rope::from("let value = compute();\nlet other = 2;\n");
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(&text, 80);
    let (client, peer) = recording_client();
    let store = RopeTextStore::new(text.clone(), 0);

    let _ = render_span_values(&mut view, &store, &client, &peer, 0, 1);
    let (_, misses_before, cached, _) = view.syntax_cache_stats();
    assert!(cached > 0, "the first render should fill the cache");

    view.debug_force_rewrap_cols(&text, 20);
    assert_eq!(view.syntax_cache_stats().2, 0, "rewrap must drop cached windows");

    let _ = render_span_values(&mut view, &store, &client, &peer, 0, 1);
    let (_, misses_after, ..) = view.syntax_cache_stats();
    assert!(misses_after > misses_before, "the re-render must re-walk");
}

#[test]
fn language_changes_miss_the_cache() {
    // The language is part of the key, so a switch cannot serve the old grammar's
    // spans, and the same language after a shadow reset still hits.
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust", "python"]);
    let text = Rope::from("let value = compute();\n");
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(&text, 80);
    let (client, peer) = recording_client();
    let store = RopeTextStore::new(text.clone(), 0);

    view.request_lines(&store, &client, 0, 0, true, "rust", true);
    let rust = span_values(&peer.take_notifications());
    let (hits_0, misses_0, ..) = view.syntax_cache_stats();
    assert!(!rust.is_empty());

    // Same language, dirty shadow: the walk is skipped, and the spans are identical.
    view.set_dirty(&store);
    view.request_lines(&store, &client, 0, 0, true, "rust", true);
    let rust_again = span_values(&peer.take_notifications());
    let (hits_1, misses_1, ..) = view.syntax_cache_stats();
    assert_eq!(rust_again, rust, "a cache hit must reproduce the walked spans");
    assert!(hits_1 > hits_0 && misses_1 == misses_0, "same key must hit");

    // Different language: a miss, and a walk that produces the new grammar's spans.
    view.set_dirty(&store);
    view.request_lines(&store, &client, 0, 0, true, "python", true);
    let python = span_values(&peer.take_notifications());
    let (hits_2, misses_2, ..) = view.syntax_cache_stats();
    assert!(misses_2 > misses_1, "a different language must walk");
    assert_eq!(hits_2, hits_1, "a different language must not hit");
    assert_ne!(python, rust, "different grammars should produce different spans");
}

#[test]
fn cache_keeps_a_bounded_number_of_windows() {
    let _guard = crate::runtime_loader::runtime_loader_test_guard();
    warm_syntax_queries(&["rust"]);
    let text = Rope::from(
        (0..1_200)
            .map(|index| format!("let value_{index} = compute({index});\n"))
            .collect::<String>(),
    );
    let mut view = View::new(1.into(), BufferId::new(2));
    view.debug_force_rewrap_cols(&text, 80);
    let store = RopeTextStore::new(text.clone(), 0);

    // Walk more windows than the cache keeps, through the cache entry point used by
    // the renderer.
    for start in (0..1_200).step_by(20) {
        let spans = view.cached_syntax_spans_for_segment(&store, start, 20, "rust", true);
        assert_eq!(spans.len(), 20, "every window should be answered for {start}");
    }

    let (_, misses, cached, rows) = view.syntax_cache_stats();
    assert_eq!(cached, SyntaxSpanCache::MAX_ENTRIES, "cache should be full");
    assert!(rows <= SyntaxSpanCache::MAX_ROWS, "rows stay bounded: {rows}");
    assert_eq!(misses, (1_200 / 20) as u64, "every distinct window is a walk");
}
