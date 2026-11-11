//! Discovery tests: bounded parsing, display-only output, and the guarantee
//! that fetched metadata never changes routing or leaks a credential.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::adapter::test_support::{TEST_API_KEY, test_config};
use crate::config::{Config, MISSING_API_KEY};
use crate::routes::catalog;

/// Fetcher replaying one scripted document or error.
struct ScriptedFetcher {
    result: Result<Value, String>,
    calls: Mutex<Vec<OpenCodeSurface>>,
}

impl ScriptedFetcher {
    fn new(result: Result<Value, String>) -> Arc<Self> {
        Arc::new(Self { result, calls: Mutex::new(Vec::new()) })
    }

    fn calls(&self) -> Vec<OpenCodeSurface> {
        self.calls.lock().expect("scripted calls poisoned").clone()
    }
}

impl MetadataFetcher for ScriptedFetcher {
    fn fetch<'a>(&'a self, surface: OpenCodeSurface) -> FetchFuture<'a> {
        self.calls.lock().expect("scripted calls poisoned").push(surface);
        let result = self.result.clone();
        Box::pin(async move { result })
    }
}

fn document(entries: Value) -> Value {
    json!({ "object": "list", "data": entries })
}

fn fetcher_config(surface: OpenCodeSurface, model_id: &str) -> Config {
    test_config(surface, model_id)
}

#[test]
fn metadata_endpoint_is_the_surface_root_plus_models() {
    assert_eq!(
        metadata_endpoint(OpenCodeSurface::Zen).expect("zen endpoint").as_str(),
        "https://opencode.ai/zen/v1/models"
    );
    assert_eq!(
        metadata_endpoint(OpenCodeSurface::Go).expect("go endpoint").as_str(),
        "https://opencode.ai/zen/go/v1/models"
    );
}

#[test]
fn metadata_endpoint_is_built_only_from_the_catalog_root() {
    for surface in [OpenCodeSurface::Zen, OpenCodeSurface::Go] {
        let endpoint = metadata_endpoint(surface).expect("endpoint");

        assert!(endpoint.as_str().starts_with(surface.api_root()), "{}", endpoint.as_str());
        assert!(endpoint.as_str().starts_with("https://"), "a credential needs HTTPS");
        assert!(endpoint.as_str().ends_with(MODELS_PATH));
    }
}

#[test]
fn annotate_marks_only_static_catalog_ids_as_routed() {
    let documented = catalog().next().expect("catalog has routes");
    let document = document(json!([
        { "id": documented.model_id },
        { "id": "not-a-documented-model" },
        { "id": "gemini-3.0-pro" },
        { "id": "opencode/not-a-tui-alias" },
    ]));

    let models = annotate(documented.surface, &document).expect("annotates");

    assert_eq!(models.len(), 4);
    assert!(models[0].routed, "a catalog id stays routed");
    assert!(!models[1].routed);
    assert!(!models[2].routed, "unsupported Google-dialect ids are never routable");
    assert!(!models[3].routed, "a TUI alias is never routable");
}

#[test]
fn every_catalog_id_is_reported_as_routed() {
    for surface in [OpenCodeSurface::Zen, OpenCodeSurface::Go] {
        let ids = catalog()
            .filter(|route| route.surface == surface)
            .map(|route| json!({ "id": route.model_id }))
            .collect::<Vec<_>>();
        let document = document(Value::Array(ids));

        let models = annotate(surface, &document).expect("annotates");

        assert!(!models.is_empty());
        assert!(models.iter().all(|model| model.routed), "static routes stay routed");
    }
}

#[test]
fn annotate_reads_only_the_model_id_and_drops_provider_fields() {
    let document = document(json!([{
        "id": "gpt-5.5",
        "object": "model",
        "owned_by": "opencode",
        "description": "provider marketing text",
        "context_length": 400_000,
        "pricing": { "input": 0.5 },
    }]));

    let models = annotate(OpenCodeSurface::Zen, &document).expect("annotates");
    let printed = DiscoveryReport { surface: OpenCodeSurface::Zen, models }.to_json().to_string();

    assert!(printed.contains("gpt-5.5"));
    for dropped in ["owned_by", "description", "context_length", "pricing", "opencode"] {
        assert!(!printed.contains(dropped), "`{dropped}` must not be printed: {printed}");
    }
}

#[test]
fn report_states_display_only_routing_and_has_no_endpoint_or_dialect() {
    let document = document(json!([{ "id": "gpt-5.5" }]));
    let models = annotate(OpenCodeSurface::Zen, &document).expect("annotates");
    let value = DiscoveryReport { surface: OpenCodeSurface::Zen, models }.to_json();

    assert_eq!(value["surface"], json!("zen"));
    assert_eq!(value["routing"], json!(ROUTING_NOTE));
    assert_eq!(value["models"][0]["id"], json!("gpt-5.5"));
    assert_eq!(value["models"][0]["routed"], json!(true));
    assert_eq!(value.as_object().map(serde_json::Map::len), Some(3));
    assert_eq!(value["models"][0].as_object().map(serde_json::Map::len), Some(2));
    let printed = value.to_string();
    assert!(!printed.contains("endpoint"), "{printed}");
    assert!(!printed.contains("dialect"), "{printed}");
}

#[test]
fn annotate_rejects_a_document_without_a_data_array() {
    let error = annotate(OpenCodeSurface::Go, &json!({ "models": [] })).expect_err("no data array");

    assert!(error.contains("did not carry a `data` array"), "{error}");
}

#[test]
fn annotate_skips_unusable_ids_and_collapses_duplicates() {
    let long_id = "m".repeat(MAX_MODEL_ID_BYTES + 1);
    let document = document(json!([
        { "id": "  " },
        { "name": "no id" },
        { "id": 7 },
        { "id": long_id },
        { "id": "kimi-k3" },
        { "id": "kimi-k3" },
    ]));

    let models = annotate(OpenCodeSurface::Go, &document).expect("annotates");

    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "kimi-k3");
    assert!(models[0].routed);
}

#[test]
fn annotate_bounds_the_entry_count() {
    let entries = (0..MAX_METADATA_ENTRIES + 25)
        .map(|index| json!({ "id": format!("advertised-{index}") }))
        .collect::<Vec<_>>();
    let document = document(Value::Array(entries));

    let models = annotate(OpenCodeSurface::Zen, &document).expect("annotates");

    assert_eq!(models.len(), MAX_METADATA_ENTRIES);
}

#[tokio::test]
async fn discovery_is_one_attempt_against_the_configured_surface() {
    let document = document(json!([{ "id": "kimi-k3" }]));
    let fetcher = ScriptedFetcher::new(Ok(document));

    let report = discover_models(fetcher.as_ref(), OpenCodeSurface::Go).await.expect("report");

    assert_eq!(report.surface, OpenCodeSurface::Go);
    assert_eq!(fetcher.calls(), vec![OpenCodeSurface::Go], "one explicit action is one fetch");
}

#[tokio::test]
async fn discovery_propagates_fetch_failures_without_inventing_models() {
    let fetcher = ScriptedFetcher::new(Err(String::from("OpenCode zen model metadata HTTP 401")));

    let error = discover_models(fetcher.as_ref(), OpenCodeSurface::Zen)
        .await
        .expect_err("a fetch failure is not a model list");

    assert!(error.contains("HTTP 401"), "{error}");
}

#[tokio::test]
async fn http_fetcher_refuses_a_surface_it_was_not_built_for() {
    let fetcher = HttpMetadataFetcher::new(&fetcher_config(OpenCodeSurface::Zen, "gpt-5.5"))
        .expect("fetcher builds");

    let error = fetcher.fetch(OpenCodeSurface::Go).await.expect_err("wrong surface");

    assert!(error.contains("cannot be fetched"), "{error}");
}

#[tokio::test]
async fn http_fetcher_targets_the_surface_root_and_resolves_the_credential_first() {
    let mut config = fetcher_config(OpenCodeSurface::Go, "kimi-k3");
    config.api_key = None;
    let fetcher = HttpMetadataFetcher::new(&config).expect("fetcher builds");
    assert_eq!(fetcher.endpoint(), "https://opencode.ai/zen/go/v1/models");

    let error = fetcher.fetch(OpenCodeSurface::Go).await.expect_err("no credential");

    assert!(error.contains(MISSING_API_KEY), "{error}");
    assert!(!error.contains("Bearer"), "the header never reaches an error: {error}");
}

#[tokio::test]
async fn http_fetcher_never_rewrites_the_endpoint_from_the_route() {
    for (surface, model_id, expected) in [
        (OpenCodeSurface::Zen, "gpt-5.5", "https://opencode.ai/zen/v1/models"),
        (OpenCodeSurface::Go, "kimi-k3", "https://opencode.ai/zen/go/v1/models"),
    ] {
        let fetcher =
            HttpMetadataFetcher::new(&fetcher_config(surface, model_id)).expect("fetcher builds");
        let route = crate::routes::resolve_route(surface, model_id).expect("documented route");

        assert_eq!(fetcher.endpoint(), expected);
        assert_ne!(fetcher.endpoint(), route.endpoint, "the dialect URL is never the metadata URL");
    }
}

#[tokio::test]
async fn advertised_secrets_stay_unrouted_and_unprinted_as_headers() {
    let document = document(json!([{ "id": TEST_API_KEY }, { "id": "gpt-5.5" }]));
    let fetcher = ScriptedFetcher::new(Ok(document));

    let report = discover_models(fetcher.as_ref(), OpenCodeSurface::Zen).await.expect("report");
    let printed = report.to_json().to_string();

    // The report echoes whatever the surface advertises as an id; it never
    // invents a value, routing still refuses it, and no header text is added.
    assert!(!report.models[0].routed);
    assert!(!printed.contains("Authorization"), "{printed}");
}
