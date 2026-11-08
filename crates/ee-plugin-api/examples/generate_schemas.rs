//! Regenerate the checked-in JSON schemas.
//!
//! Run with `cargo run -p ee-plugin-api --example generate_schemas`.

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas");
    std::fs::create_dir_all(&dir).expect("create schemas directory");

    let manifest = serde_json::to_string_pretty(&ee_plugin_api::manifest_schema())
        .expect("serialize manifest schema");
    std::fs::write(dir.join("plugin-manifest-v2.json"), manifest).expect("write manifest schema");

    let model = serde_json::to_string_pretty(&ee_plugin_api::model_schema())
        .expect("serialize model schema");
    std::fs::write(dir.join("plugin-model-v2.json"), model).expect("write model schema");
}
