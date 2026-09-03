use std::path::Path;

use super::helpers::normalize_lookup_key;
use super::types::RuntimeLanguage;

pub(super) fn candidate_dir_matches_language(candidate: &Path, language: &RuntimeLanguage) -> bool {
    let Some(candidate_name) = candidate.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let candidate_name = normalize_lookup_key(candidate_name);

    [
        Some(language.grammar_id()),
        Some(language.canonical_id()),
        Some(language.query_language()),
        language.grammar_symbol_name(),
    ]
    .into_iter()
    .flatten()
    .any(|name| normalize_lookup_key(name) == candidate_name)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::runtime_loader::{default_runtime_loader, resolve_staged_grammar_build_dir};

    #[test]
    fn markdown_symbol_selects_block_parser_from_multi_parser_crate() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("tree-sitter-md");
        for parser in ["tree-sitter-markdown", "tree-sitter-markdown-inline"] {
            let source_dir = root.join(parser).join("src");
            fs::create_dir_all(&source_dir).unwrap();
            fs::write(source_dir.join("parser.c"), "int parser(void) { return 0; }\n").unwrap();
        }

        let loader = default_runtime_loader();
        let markdown = loader.language_for_name("markdown").unwrap();
        let resolved = resolve_staged_grammar_build_dir(&root, markdown).unwrap();

        assert_eq!(resolved, root.join("tree-sitter-markdown"));
    }
}
