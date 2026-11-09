//! Runtime load test for the reference Wasm guest.
//!
//! Building the guest requires the `wasm32-unknown-unknown` target, so this
//! test is ignored by default. `scripts/check-wasm-example.sh` builds the example
//! and then runs `cargo test -p ee-plugin-runtime --features wasm --test wasm_example -- --ignored`.
#![cfg(feature = "wasm")]

use std::sync::Arc;

use ee_plugin_api::error::PluginError;
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::{CommandId, PluginId};
use ee_plugin_runtime::MemoryContributionSink;
use ee_plugin_runtime::wasm::{WasmClient, WasmSpec};

struct NoopAdapter;

impl HostAdapter for NoopAdapter {
    fn host_kind(&self) -> HostKind {
        HostKind::Editor
    }

    fn submit_effects(
        &self,
        _plugin: Option<&PluginId>,
        _effects: Vec<ee_plugin_api::effect::Effect>,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

fn artifact() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-unknown-unknown/release/ee_plugin_example_wasm.wasm")
}

#[test]
#[ignore = "run scripts/check-wasm-example.sh to build the wasm32 example first"]
fn wasm_example_loads_and_answers_ping() {
    let path = artifact();
    assert!(path.is_file(), "missing example artifact at {}", path.display());

    let mut spec = WasmSpec::new(PluginId::new("example-wasm").unwrap(), path);
    spec.limits.rpc_timeout_ms = 2_000;
    let mut client =
        WasmClient::spawn(spec, Arc::new(NoopAdapter), Arc::new(MemoryContributionSink::new()))
            .unwrap();

    assert_eq!(client.health().unwrap().state, "loaded");
    let result =
        client.invoke(&CommandId::new("example.ping").unwrap(), serde_json::json!({})).unwrap();
    assert_eq!(result["plugin"], "example-wasm");
}
