//! Configuration-file parsing tests.

use serde_json::json;

use super::*;

fn lookup(name: &str) -> Option<String> {
    match name {
        "OPENROUTER_API_KEY" => Some(String::from("sk-or-test")),
        _ => None,
    }
}

#[test]
fn parses_selection_endpoints_and_overrides() {
    let value = json!({
        "default_model": "deepseek/deepseek-v4-flash",
        "small_model": "deepseek/deepseek-v4-flash",
        "base_url": "https://openrouter.ai/api/v1/chat/completions",
        "responses_url": "https://openrouter.ai/api/v1/responses",
        "compaction_mode": "auto",
        "provider": {
            "headers": { "x-request-source": "ee" },
            "body": { "provider": { "sort": "throughput" } }
        },
        "models": {
            "deepseek/deepseek-v4-flash": {
                "variant": "high",
                "body": { "provider": { "allow_fallbacks": false } }
            }
        }
    });
    let config = parse(&value).expect("parses");
    assert_eq!(config.default_model.as_deref(), Some("deepseek/deepseek-v4-flash"));
    assert_eq!(config.compaction_mode.as_deref(), Some("auto"));

    let resolved = config.resolve("deepseek/deepseek-v4-flash");
    assert_eq!(resolved.variant.as_deref(), Some("high"));
    let body = resolved.body.expect("body");
    assert_eq!(body["provider"]["sort"], "throughput", "sibling keys survive");
    assert_eq!(body["provider"]["allow_fallbacks"], false, "model wins");
}

#[test]
fn disabled_model_is_reported() {
    let config = parse(&json!({ "models": { "m": { "disabled": true } } })).expect("parses");
    assert!(config.resolve("m").disabled);
    assert!(!config.resolve("other").disabled);
}

#[test]
fn wrong_types_name_the_key() {
    let error = parse(&json!({ "default_model": 5 })).expect_err("rejected");
    assert!(error.contains("default_model"), "{error}");

    let error = parse(&json!({ "provider": { "body": [] } })).expect_err("rejected");
    assert!(error.contains("provider.body"), "{error}");

    let error = parse(&json!({ "body": {} })).expect_err("rejected");
    assert!(error.contains("nested under"), "{error}");
}

#[test]
fn interpolation_is_bounded_to_the_allowlist() {
    let mut value = json!({ "headers": { "x-key": "${OPENROUTER_API_KEY}" } });
    ee_chat_completions::interpolate(&mut value, &ALLOWED_ENV, &mut lookup).expect("resolves");
    assert_eq!(value["headers"]["x-key"], "sk-or-test");

    let mut value = json!({ "headers": { "x-home": "${HOME}" } });
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
fn loaded_file_interpolates_before_parsing() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("agent-openrouter.json");
    std::fs::write(&path, r#"{"provider":{"headers":{"x-key":"${OPENROUTER_API_KEY}"}}}"#)
        .expect("write");
    let config = load_from(&path, &mut lookup).expect("loads");
    assert_eq!(config.provider.headers[0].1, "sk-or-test");
}
