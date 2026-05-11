use xi_plugin_lib::{ChunkCache, Plugin, xi_plugin};

#[xi_plugin]
struct PlainPlugin;

impl xi_plugin_lib::SimplePlugin for PlainPlugin {}

fn assert_plain_plugin<T: Plugin<Cache = ChunkCache>>() {}

#[test]
fn xi_plugin_macro_assigns_expected_cache_types() {
    assert_plain_plugin::<PlainPlugin>();
}

#[test]
fn log_macro_works_in_integration_context() {
    xi_plugin_lib::log!(
        "plain-plugin",
        "info",
        "integration log",
        serde_json::json!({ "count": 1 })
    );
}
