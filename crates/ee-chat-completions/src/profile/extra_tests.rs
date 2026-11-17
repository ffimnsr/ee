//! Phase budgets, encodings, sampling gates, and query handling.

use std::time::Duration;

use ee_agent_orchestrator::{GenerationOptions, SamplingSupport};

use crate::endpoint::TrustedEndpoint;

use super::{EndpointProfile, RequestEncoding, Timeouts};

fn profile() -> EndpointProfile {
    EndpointProfile::new(
        "Example",
        "EXAMPLE_API_KEY",
        TrustedEndpoint::parse("https://example.test/v1/chat/completions").expect("https"),
        "example-model",
    )
}

#[test]
fn phase_defaults_match_the_reference_budgets() {
    let timeouts = Timeouts::new();
    assert_eq!(timeouts.headers, Duration::from_secs(300));
    assert_eq!(timeouts.chunk, Duration::from_secs(900));
    assert!(timeouts.total.is_none(), "the whole exchange is unbounded by default");
}

#[test]
fn with_timeout_only_sets_the_total_budget() {
    let profile = profile().with_timeout(Duration::from_secs(30));
    assert_eq!(profile.timeouts.total, Some(Duration::from_secs(30)));
    assert_eq!(profile.timeouts.headers, Timeouts::DEFAULT_HEADERS);
    assert_eq!(profile.timeouts.chunk, Timeouts::DEFAULT_CHUNK);
}

#[test]
fn encodings_parse_and_report_their_header() {
    assert_eq!(RequestEncoding::parse("none"), Ok(RequestEncoding::None));
    assert_eq!(RequestEncoding::parse("GZIP"), Ok(RequestEncoding::Gzip));
    assert_eq!(RequestEncoding::parse("zstd"), Ok(RequestEncoding::Zstd));
    assert!(RequestEncoding::parse("br").is_err());
    assert_eq!(RequestEncoding::None.content_encoding(), None);
    assert_eq!(RequestEncoding::Gzip.content_encoding(), Some("gzip"));
    assert_eq!(RequestEncoding::Zstd.content_encoding(), Some("zstd"));
}

#[test]
fn duplicate_query_keys_keep_the_first_value() {
    let profile = profile().with_query(vec![
        (String::from("a"), String::from("1")),
        (String::from("a"), String::from("2")),
        (String::from("b"), String::from("3")),
    ]);
    assert_eq!(
        profile.query,
        vec![(String::from("a"), String::from("1")), (String::from("b"), String::from("3"))]
    );
}

#[test]
fn unsupported_generation_names_the_first_knob() {
    let sampling = SamplingSupport { temperature: false, ..SamplingSupport::ALL };
    let profile = profile()
        .with_sampling(sampling)
        .with_generation(GenerationOptions { temperature: Some(0.4), ..GenerationOptions::none() });
    assert_eq!(profile.unsupported_generation(), Some("temperature"));

    let clean = profile.with_generation(GenerationOptions::none());
    assert_eq!(clean.unsupported_generation(), None, "an empty configuration is always allowed");
}

#[test]
fn per_request_output_limit_wins_and_stays_clamped() {
    let profile = profile()
        .with_limits(Some(ee_agent_orchestrator::ModelLimits::new(32_000, 8_000)))
        .with_max_output_tokens(Some(4_000));

    let fitted = profile.fitted_output_limit_with(1_000, Some(6_000));
    assert_eq!(fitted, Some(6_000), "the request value wins inside the ceiling");

    let clamped = profile.fitted_output_limit_with(1_000, Some(999_999));
    assert!(
        clamped.expect("fitted") <= 8_000,
        "the catalog ceiling still bounds the request value"
    );
}
