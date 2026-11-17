//! Configuration-file parsing tests.

use serde_json::json;

use super::*;

fn lookup(name: &str) -> Option<String> {
    match name {
        "OPENCODE_API_KEY" => Some(String::from("sk-test")),
        "OPENCODE_ORG_ID" => Some(String::from("org-1")),
        _ => None,
    }
}

#[test]
fn parses_selection_overlays_and_model_blocks() {
    let value = json!({
        "default_surface": "zen",
        "default_model": "gpt-5.5",
        "small_model": "gpt-5.5-mini",
        "org_id": "org-1",
        "headers": { "x-request-source": "ee" },
        "body": { "safety_identifier": "ee-agent" },
        "models": {
            "gpt-5.5": {
                "variant": "high",
                "headers": { "x-model-source": "ee" },
                "body": { "metadata": { "tier": "primary" } }
            }
        }
    });
    let config = parse(&value).expect("parses");
    assert_eq!(config.default_surface.as_deref(), Some("zen"));
    assert_eq!(config.default_model.as_deref(), Some("gpt-5.5"));
    assert_eq!(config.small_model.as_deref(), Some("gpt-5.5-mini"));

    let resolved = config.resolve("gpt-5.5");
    assert_eq!(resolved.variant.as_deref(), Some("high"));
    assert_eq!(resolved.org_id.as_deref(), Some("org-1"));
    assert_eq!(resolved.headers.len(), 2);
    assert_eq!(resolved.body.expect("body")["metadata"]["tier"], "primary");
}

#[test]
fn provider_body_is_preserved_when_a_model_adds_fields() {
    let value = json!({
        "body": { "metadata": { "tier": "primary", "team": "ee" } },
        "models": { "m": { "body": { "metadata": { "tier": "critic" } } } }
    });
    let config = parse(&value).expect("parses");
    let body = config.resolve("m").body.expect("body");
    assert_eq!(body["metadata"]["tier"], "critic", "model wins");
    assert_eq!(body["metadata"]["team"], "ee", "sibling keys survive");
}

#[test]
fn disabled_and_variant_are_reported_per_model() {
    let value = json!({ "models": { "m": { "disabled": true } } });
    let config = parse(&value).expect("parses");
    assert!(config.resolve("m").disabled);
    assert!(!config.resolve("other").disabled);
}

#[test]
fn endpoint_overrides_are_rejected_at_any_level() {
    let error = parse(&json!({ "base_url": "https://example.com" })).expect_err("rejected");
    assert!(error.contains("static catalog"), "{error}");

    let error = parse(&json!({
        "models": { "m": { "baseURL": "https://example.com" } }
    }))
    .expect_err("rejected");
    assert!(error.contains("models.m"), "{error}");
}

#[test]
fn wrong_types_name_the_key() {
    let error = parse(&json!({ "default_model": 5 })).expect_err("rejected");
    assert!(error.contains("default_model"), "{error}");

    let error = parse(&json!({ "headers": { "x": 5 } })).expect_err("rejected");
    assert!(error.contains("headers.x"), "{error}");
}

#[test]
fn interpolation_is_bounded_to_the_allowlist() {
    let mut value = json!({ "org_id": "${OPENCODE_ORG_ID}" });
    ee_chat_completions::interpolate(&mut value, &ALLOWED_ENV, &mut lookup).expect("resolves");
    assert_eq!(value["org_id"], "org-1");

    let mut value = json!({ "org_id": "${HOME}" });
    let error = ee_chat_completions::interpolate(&mut value, &ALLOWED_ENV, &mut lookup)
        .expect_err("denied");
    assert!(error.contains("HOME"), "{error}");
}

#[test]
fn missing_file_is_not_an_error() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = load_from(&directory.path().join("missing.json"), &mut lookup).expect("absent");
    assert_eq!(config, FileConfig::default());
}

#[test]
fn oversized_file_fails_before_parsing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("agent-opencode.json");
    std::fs::write(&path, "x".repeat(ee_chat_completions::MAX_CONFIG_BYTES as usize + 1))
        .expect("write");
    let error = load_from(&path, &mut lookup).expect_err("oversized");
    assert!(error.contains("the limit is"), "{error}");
}

#[test]
fn loaded_file_interpolates_before_parsing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("agent-opencode.json");
    std::fs::write(&path, r#"{"org_id":"${OPENCODE_ORG_ID}","default_model":"gpt-5.5"}"#)
        .expect("write");
    let config = load_from(&path, &mut lookup).expect("loads");
    assert_eq!(config.org_id.as_deref(), Some("org-1"));
    assert_eq!(config.default_model.as_deref(), Some("gpt-5.5"));
}
