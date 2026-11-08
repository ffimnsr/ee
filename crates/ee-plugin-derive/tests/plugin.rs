use ee_plugin_api::{Plugin, PluginContext, PluginError};
use ee_plugin_derive::plugin;

struct DemoPlugin;

#[plugin(name = "demo")]
impl DemoPlugin {
    fn apply(&mut self, _ctx: &PluginContext) -> Result<(), PluginError> {
        Ok(())
    }
}

struct DefaultNamePlugin;

#[plugin]
impl DefaultNamePlugin {
    fn apply(&mut self, _ctx: &PluginContext) -> Result<(), PluginError> {
        Ok(())
    }
}

fn assert_plugin<P: Plugin>(plugin: &mut P) -> &str {
    plugin.name()
}

#[test]
fn derive_implements_plugin_with_explicit_name() {
    let mut plugin = DemoPlugin;
    assert_eq!(assert_plugin(&mut plugin), "demo");
    let ctx = PluginContext::new("demo");
    <DemoPlugin as Plugin>::apply(&mut plugin, &ctx).unwrap();
}

#[test]
fn derive_defaults_name_to_type_identifier() {
    let mut plugin = DefaultNamePlugin;
    assert_eq!(assert_plugin(&mut plugin), "DefaultNamePlugin");
}

#[test]
fn derive_rejects_impl_without_apply() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/missing_apply.rs");
}
