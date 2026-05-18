use tree_sitter::Language;

macro_rules! language_fn {
    ($name:ident, $language:path) => {
        pub fn $name() -> Language {
            $language.into()
        }
    };
}

language_fn!(bash, tree_sitter_bash::LANGUAGE);
language_fn!(c, tree_sitter_c::LANGUAGE);
language_fn!(c_sharp, tree_sitter_c_sharp::LANGUAGE);
language_fn!(cpp, tree_sitter_cpp::LANGUAGE);
language_fn!(css, tree_sitter_css::LANGUAGE);
language_fn!(elixir, tree_sitter_elixir::LANGUAGE);
language_fn!(go, tree_sitter_go::LANGUAGE);
language_fn!(haskell, tree_sitter_haskell::LANGUAGE);
language_fn!(html, tree_sitter_html::LANGUAGE);
language_fn!(java, tree_sitter_java::LANGUAGE);
language_fn!(javascript, tree_sitter_javascript::LANGUAGE);
language_fn!(json, tree_sitter_json::LANGUAGE);
language_fn!(php, tree_sitter_php::LANGUAGE_PHP);
language_fn!(python, tree_sitter_python::LANGUAGE);
language_fn!(ruby, tree_sitter_ruby::LANGUAGE);
language_fn!(rust, tree_sitter_rust::LANGUAGE);
language_fn!(scala, tree_sitter_scala::LANGUAGE);
language_fn!(typescript, tree_sitter_typescript::LANGUAGE_TYPESCRIPT);
