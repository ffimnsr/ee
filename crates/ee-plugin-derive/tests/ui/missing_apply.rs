use ee_plugin_derive::plugin;

struct MissingApply;

#[plugin(name = "missing-apply")]
impl MissingApply {}

fn main() {}
