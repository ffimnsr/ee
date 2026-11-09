//! Bounded local smoke coverage for the plugin fuzz target.
//!
//! `cargo fuzz` is nightly-only and excluded from the main workspace, so this
//! test replays a handful of crafted inputs through the same harness the target
//! uses. Run with `cargo test --quiet --manifest-path fuzz/Cargo.toml --test
//! ee_plugin_wasm_smoke`.

use arbitrary::{Arbitrary, Unstructured};
use ee_fuzz::{PluginInput, run_plugin_input};

fn bounded(bytes: &[u8]) -> PluginInput {
    PluginInput {
        manifest: bytes[..bytes.len().min(1024)].to_vec(),
        initialize: bytes[..bytes.len().min(512)].to_vec(),
        event: bytes[..bytes.len().min(512)].to_vec(),
        command: bytes[..bytes.len().min(512)].to_vec(),
    }
}

#[test]
fn replayed_inputs_do_not_panic() {
    let cases: &[&[u8]] = &[
        b"",
        b"{}",
        b"{\"manifest_version\":2}",
        b"{\"manifest_version\":2,\"name\":\"x\",\"version\":\"0.1.0\",\"api\":\"ee-plugin/2\",\"targets\":[\"editor\"],\"kind\":\"wasm\",\"entry\":\"x.wasm\"}",
        b"{\"method\":\"plugin/command\",\"params\":{\"id\":\"x.y\",\"request_id\":1,\"args\":null,\"deadline_ms\":0}}",
        b"{\"method\":\"plugin/event\",\"params\":{\"name\":\"e\",\"revision\":1,\"payload\":[1,2,3]}}",
    ];
    for case in cases {
        run_plugin_input(bounded(case));
    }
}

#[test]
fn arbitrary_inputs_are_bounded() {
    let bytes = vec![b'{'; 8192];
    let mut unstructured = Unstructured::new(&bytes);
    let Ok(input) = PluginInput::arbitrary(&mut unstructured) else {
        return;
    };
    run_plugin_input(input);
}
