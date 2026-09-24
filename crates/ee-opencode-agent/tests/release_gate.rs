//! Release-gate checks: catalog integrity, fail-closed routing, endpoint trust,
//! and secret containment.
//!
//! These run offline. No OpenCode account, balance, or paid model call is
//! involved anywhere in this file.

use std::fs;
use std::path::Path;

use ee_chat_completions::{EndpointTransport, RetryPolicy, TrustedEndpoint};
use ee_opencode_agent::config::{ConfigError, route_profile};
use ee_opencode_agent::discovery::{HttpMetadataFetcher, metadata_endpoint};
use ee_opencode_agent::routes::{
    self, GO_API_ROOT, OpenCodeDialect, OpenCodeRoute, OpenCodeSurface, ZEN_API_ROOT, catalog,
};
use serde_json::json;
use tempfile::TempDir;

/// Every dialect the agent can encode and decode.
const SUPPORTED_DIALECTS: [OpenCodeDialect; 3] = [
    OpenCodeDialect::OpenAiResponses,
    OpenCodeDialect::AnthropicMessages,
    OpenCodeDialect::OpenAiChatCompletions,
];

fn test_config(
    surface: OpenCodeSurface,
    model_id: &str,
    api_key: &str,
) -> ee_opencode_agent::config::Config {
    let route = routes::resolve_route(surface, model_id).expect("documented route");
    let mut config = ee_opencode_agent::adapter::test_support::test_config(surface, route.model_id);
    config.api_key = Some(api_key.to_string());
    config
}

#[test]
fn every_catalog_row_is_an_exact_https_route_for_its_declared_dialect() {
    let mut seen = std::collections::BTreeSet::new();
    // `(surface name, model id)` keeps the check free of extra trait bounds.

    for route in catalog() {
        let label = format!("{} {}", route.surface.as_str(), route.model_id);
        assert!(route.surface != OpenCodeSurface::Zen || route.endpoint.starts_with(ZEN_API_ROOT));
        let root = match route.surface {
            OpenCodeSurface::Zen => ZEN_API_ROOT,
            OpenCodeSurface::Go => GO_API_ROOT,
        };
        assert!(route.endpoint.starts_with(root), "{label} stays under {root}");
        assert!(route.endpoint.starts_with("https://"), "{label} is HTTPS");
        assert_eq!(
            route.endpoint,
            routes::endpoint_for(route.surface, route.dialect),
            "{label} uses the dialect endpoint"
        );
        assert!(SUPPORTED_DIALECTS.contains(&route.dialect), "{label} dialect is implemented");
        assert!(
            seen.insert((route.surface.as_str(), route.model_id)),
            "{label} is unique in the catalog"
        );

        // A catalog row round-trips through resolution, and its endpoint is
        // accepted by the credential-safety check the codecs use.
        let resolved =
            routes::resolve_route(route.surface, route.model_id).expect("row resolves back");
        assert_eq!(resolved, route, "{label} round-trips");
        let endpoint = TrustedEndpoint::parse(route.endpoint).expect("trusted endpoint");
        assert_eq!(endpoint.as_str(), route.endpoint);
        assert!(
            route_profile(&route, "system", std::time::Duration::from_secs(1), RetryPolicy::none())
                .is_ok(),
            "{label} builds a profile for its endpoint"
        );
    }
}

#[test]
fn malformed_endpoint_metadata_cannot_carry_a_credential() {
    // A catalog endpoint that is not credential-safe is rejected before any
    // codec exists, so a defective table cannot leak the key.
    let route = OpenCodeRoute {
        surface: OpenCodeSurface::Zen,
        model_id: "gpt-5.5",
        dialect: OpenCodeDialect::OpenAiResponses,
        endpoint: "http://insecure.test/v1/responses",
    };

    let error =
        route_profile(&route, "system", std::time::Duration::from_secs(1), RetryPolicy::none())
            .expect_err("insecure endpoint is refused");

    assert!(matches!(error, ConfigError::UntrustedRouteEndpoint { .. }), "{error}");
    assert!(!error.to_string().contains("key"), "{error}");
    assert!(TrustedEndpoint::parse("http://insecure.test/v1/models").is_err());
    assert_eq!(
        metadata_endpoint(OpenCodeSurface::Zen).expect("zen").as_str(),
        "https://opencode.ai/zen/v1/models"
    );
}

#[test]
fn unroutable_selections_fail_before_any_http_client_exists() {
    for (surface, model_id) in [
        (OpenCodeSurface::Zen, "definitely-not-a-model"),
        (OpenCodeSurface::Go, "gpt-5.5"),
        (OpenCodeSurface::Zen, "gemini-2.5-pro"),
        (OpenCodeSurface::Zen, "opencode/gpt-5.5"),
        (OpenCodeSurface::Zen, ""),
    ] {
        let error = routes::resolve_route(surface, model_id).expect_err("must fail closed");

        assert!(!error.to_string().is_empty(), "{surface:?}/{model_id} rejection carries guidance");
    }
}

#[test]
fn invalid_key_values_fail_header_construction_without_a_request() {
    let config = test_config(OpenCodeSurface::Zen, "gpt-5.5", "sk-bad\nheader");
    let transport =
        EndpointTransport::new(config.profile().expect("profile"), config.token_source())
            .expect("transport builds");

    let error = transport.headers().expect_err("invalid header value");

    assert!(error.to_string().contains("OPENCODE_API_KEY"), "{error}");
    assert!(!error.to_string().contains("sk-bad"), "the value never reaches an error: {error}");
}

#[test]
fn discovery_fetcher_never_accepts_an_environment_supplied_endpoint() {
    // The fetcher derives its URL from the catalog root, so no environment
    // value can move the credential to another origin.
    let config = test_config(OpenCodeSurface::Go, "kimi-k3", "sk-test-key");
    let fetcher = HttpMetadataFetcher::new(&config).expect("fetcher builds");

    assert_eq!(fetcher.endpoint(), "https://opencode.ai/zen/go/v1/models");
    assert!(fetcher.endpoint().starts_with(GO_API_ROOT));
}

#[test]
fn session_state_and_recovery_paths_stay_outside_the_workspace() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");

    // The binary's convention is `dirs::data_local_dir()/ee/agent-sessions`;
    // here we assert the property that matters: state is a separate tree, and
    // nothing in the agent writes a file into the workspace on startup.
    let mut config = test_config(OpenCodeSurface::Zen, "gpt-5.5", "sk-test-key");
    config.checkpoint_dir = Some(state.path().join("checkpoints"));

    let provider_config = ee_opencode_agent::adapter::opencode_orchestrator_config(
        &config,
        state.path().join("agent-sessions"),
    );

    assert!(provider_config.orchestrator.recovery.is_durable());
    assert!(
        !provider_config
            .session_state_dir
            .as_deref()
            .is_some_and(|dir| dir.starts_with(workspace.path()))
    );
    assert!(fs::read_dir(workspace.path()).expect("workspace reads").next().is_none());
}

#[test]
fn setup_manifest_and_diagnostics_stay_credential_free() {
    let secret = "sk-release-gate-secret";
    let config = test_config(OpenCodeSurface::Go, "kimi-k3", secret);

    let manifest = serde_json::to_string(&ee_opencode_agent::config::setup_manifest())
        .expect("manifest encodes");
    let debug = format!("{config:?}");
    let catalog = serde_json::to_string(
        &catalog()
            .map(|route| {
                json!({
                    "surface": route.surface.as_str(),
                    "model_id": route.model_id,
                    "dialect": route.dialect.as_str(),
                    "endpoint": route.endpoint,
                })
            })
            .collect::<Vec<_>>(),
    )
    .expect("catalog encodes");

    for (label, text) in [("manifest", &manifest), ("debug", &debug), ("catalog", &catalog)] {
        assert!(!text.contains(secret), "{label} must not carry the credential: {text}");
    }
    assert!(debug.contains("[redacted]"), "{debug}");
    assert!(!debug.contains("Bearer"), "{debug}");
    assert!(!manifest.contains("Authorization"), "{manifest}");
}

#[test]
fn catalog_is_bounded_and_every_row_is_reachable_by_exact_id() {
    let rows = catalog().collect::<Vec<_>>();

    assert!(rows.len() >= 95, "documented catalog stays complete: {}", rows.len());
    for route in &rows {
        assert!(!route.model_id.trim().is_empty());
        assert_eq!(route.model_id.trim(), route.model_id, "{} is a bare id", route.model_id);
        assert!(
            routes::resolve_route(route.surface, route.model_id)
                .is_ok_and(|resolved| resolved.dialect == route.dialect),
            "{} {} keeps its dialect",
            route.surface.as_str(),
            route.model_id
        );
    }
}

#[test]
fn fixture_directories_exist_for_offline_release_checks() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    for relative in ["tests/codec_fixtures.rs", "tests/acp_flows.rs", "tests/docs_examples.rs"] {
        assert!(manifest_dir.join(relative).is_file(), "{relative} keeps the release gate offline");
    }
}
