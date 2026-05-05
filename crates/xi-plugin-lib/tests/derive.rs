use xi_plugin_lib::{
    ChunkCache, Plugin, ScopeRegistry, SpanType, StateCache, SyntaxDescriptor, SyntaxPlugin,
    xi_plugin,
};

#[xi_plugin]
struct PlainPlugin;

impl xi_plugin_lib::SimplePlugin for PlainPlugin {}

#[xi_plugin(syntax(lang = "rust"))]
struct SyntaxDemo;

impl SyntaxPlugin for SyntaxDemo {
    type State = ();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, SpanType)]
enum DemoScope {
    #[span_type(name = "keyword.control")]
    Keyword,
}

fn assert_plain_plugin<T: Plugin<Cache = ChunkCache>>() {}

fn assert_syntax_plugin<T: Plugin<Cache = StateCache<()>>>() {}

#[test]
fn xi_plugin_macro_assigns_expected_cache_types() {
    assert_plain_plugin::<PlainPlugin>();
    assert_syntax_plugin::<SyntaxDemo>();
    assert_eq!(<SyntaxDemo as SyntaxDescriptor>::LANGUAGE, "rust");
}

#[test]
fn derived_span_type_works_in_integration_context() {
    let mut registry = ScopeRegistry::default();
    let spans = [DemoScope::Keyword.span(4, 9)];

    let resolved = registry.build(&spans);

    assert_eq!(registry.scopes()[0], vec![String::from("keyword.control")]);
    assert_eq!(resolved[0].start, 4);
    assert_eq!(resolved[0].end, 9);
    assert_eq!(resolved[0].scope_id, 0);
    xi_plugin_lib::log!(
        "syntax-demo",
        "info",
        "registered spans",
        serde_json::json!({ "count": 1 })
    );
}
