//! Family table tests: completeness against the catalog, no stale rows, and the
//! contrast property the rubber-duck registry relies on.

use std::collections::BTreeSet;

use super::*;
use crate::routes::catalog;

#[test]
fn every_catalog_route_declares_a_family() {
    for route in catalog() {
        assert!(
            route_family(&route).is_some(),
            "{} {} has no declared vendor family",
            route.surface.as_str(),
            route.model_id
        );
        assert_ne!(family_label(&route), "unknown", "{} is labelled", route.model_id);
    }
}

#[test]
fn family_table_declares_no_model_outside_the_catalog() {
    let routable: BTreeSet<&str> = catalog().map(|route| route.model_id).collect();

    for id in declared_ids() {
        assert!(routable.contains(id), "declared id `{id}` is not a routable catalog row");
    }
}

#[test]
fn family_table_declares_each_id_exactly_once() {
    let mut seen = BTreeSet::new();

    for id in declared_ids() {
        assert!(seen.insert(id), "`{id}` is declared twice");
    }
}

#[test]
fn shared_ids_keep_one_family_across_surfaces() {
    let mut per_id = std::collections::BTreeMap::new();

    for route in catalog() {
        let family = route_family(&route).expect("declared family");
        if let Some(previous) = per_id.insert(route.model_id, family.clone()) {
            assert_eq!(previous, family, "`{}` changed family between surfaces", route.model_id);
        }
    }
}

#[test]
fn documented_vendors_keep_distinct_families() {
    // The pairs the critic rule must reject and accept.
    let openai = family_for("gpt-5.5").expect("openai family");
    let grok = family_for("grok-4.5").expect("xai family");
    let claude = family_for("claude-opus-4-5").expect("anthropic family");
    let qwen = family_for("qwen3.7-max").expect("qwen family");
    let deepseek = family_for("deepseek-v4-pro").expect("deepseek family");
    let kimi = family_for("kimi-k3").expect("moonshot family");

    for (left, right) in [
        (&openai, &grok),
        (&openai, &claude),
        (&grok, &claude),
        (&qwen, &deepseek),
        (&claude, &kimi),
        (&deepseek, &kimi),
    ] {
        assert_ne!(left, right, "declared families must stay distinct");
    }
    assert_eq!(family_for("gpt-6-luna"), Some(openai));
    assert_eq!(family_for("grok-4.7"), Some(grok));
}

#[test]
fn unknown_ids_have_no_family_and_are_never_guessed() {
    for unknown in ["not-a-model", "gpt-5.5-alias", "opencode/gpt-5.5", ""] {
        assert_eq!(family_for(unknown), None, "`{unknown}` must not resolve a family");
    }
}
