//! Reference Wasm guest plugin.
//!
//! Build with `scripts/check-wasm-example.sh`, which targets
//! `wasm32-unknown-unknown` and runs the ignored runtime load test.

#![cfg(target_arch = "wasm32")]

use ee_plugin_sdk::wasm::{self, WasmPlugin};

/// Minimal plugin: reports health on apply and answers `example.ping`.
#[derive(Default)]
struct Example;

impl WasmPlugin for Example {
    fn apply(&mut self, input: &[u8]) -> Vec<u8> {
        let plugin_id = serde_json::from_slice::<serde_json::Value>(input)
            .ok()
            .and_then(|value| value.get("plugin_id").and_then(|id| id.as_str()).map(str::to_owned))
            .unwrap_or_else(|| "example-wasm".to_owned());
        let health = serde_json::json!({ "state": "loaded", "reason": plugin_id });
        let _ = wasm::report_health(&health.to_string());
        Vec::new()
    }

    fn command(&mut self, input: &[u8]) -> Vec<u8> {
        let params: serde_json::Value = serde_json::from_slice(input).unwrap_or_default();
        let command = params.get("id").and_then(|id| id.as_str()).unwrap_or_default();
        if command != "example.ping" {
            return Vec::new();
        }
        let response = serde_json::json!({
            "request_id": params.get("request_id").and_then(|id| id.as_u64()).unwrap_or(0),
            "result": { "ok": true, "plugin": "example-wasm" },
            "error": null,
        });
        serde_json::to_vec(&response).unwrap_or_default()
    }
}

ee_plugin_sdk::wasm_plugin!(Example);
