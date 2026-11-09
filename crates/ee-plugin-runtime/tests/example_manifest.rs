//! Manifest fixture checks for the reference Wasm guest.
//!
//! These run without the `wasm` feature so the manifest contract stays covered
//! in the default test graph.

use std::path::PathBuf;

use ee_plugin_api::manifest::{ManifestV2, PluginKind};

fn example_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ee-plugin-example-wasm")
}

#[test]
fn example_manifest_is_valid_v2() {
    let path = example_dir().join("manifest.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let manifest: ManifestV2 = serde_json::from_str(&text).unwrap();
    manifest.validate().unwrap();
    assert_eq!(manifest.kind, PluginKind::Wasm);
    assert!(manifest.entry.ends_with(".wasm"));
    assert!(manifest.limits.max_memory_bytes > 0);
}
