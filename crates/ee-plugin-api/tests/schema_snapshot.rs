//! Schema snapshot tests.
//!
//! Both schema files are checked in and compared byte-for-byte so a contract
//! change cannot land without an explicit schema update.

use std::path::Path;

fn schemas_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas")
}

#[test]
fn manifest_schema_matches_checked_in_file() {
    let path = schemas_dir().join("plugin-manifest-v2.json");
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let expected: serde_json::Value = serde_json::from_str(&expected).unwrap();
    assert_eq!(ee_plugin_api::manifest_schema(), expected);
}

#[test]
fn model_schema_matches_checked_in_file() {
    let path = schemas_dir().join("plugin-model-v2.json");
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let expected: serde_json::Value = serde_json::from_str(&expected).unwrap();
    assert_eq!(ee_plugin_api::model_schema(), expected);
}
