//! Language-list diagnostics helpers plus `ee do language list`.
use super::runtime_cmd::{effective_runtime_language_rows, render_runtime_languages_report};
use super::*;

pub(crate) fn print_language_query_diagnostics() {
    let diagnostics = with_default_runtime_loader_mut(|loader| loader.language_query_diagnostics());
    if diagnostics.is_empty() {
        return;
    }

    println!("  tree-sitter languages");
    for lang in &diagnostics {
        let grammar_tag = grammar_health_tag(&lang.grammar_status);
        print!("    {:<12} [grammar: {grammar_tag}]", lang.display_name);
        for qr in &lang.query_reports {
            let tag = query_health_tag(&qr.status);
            let kind = query_kind_label(qr.kind);
            print!("  {kind}: {tag}");
        }
        println!();
    }
}

pub(crate) fn grammar_health_tag(status: &RuntimeGrammarHealth) -> &'static str {
    match status {
        RuntimeGrammarHealth::Loaded => "ok",
        RuntimeGrammarHealth::Missing => "missing",
        RuntimeGrammarHealth::Unresolved => "unresolved",
        RuntimeGrammarHealth::Error(_) => "err",
    }
}

pub(crate) fn query_health_tag(status: &RuntimeQueryHealth) -> &'static str {
    match status {
        RuntimeQueryHealth::Loaded => "ok",
        RuntimeQueryHealth::Missing => "-",
        RuntimeQueryHealth::Unsupported => "unsupported",
        RuntimeQueryHealth::Error(_) => "err",
    }
}

pub(crate) fn query_kind_label(kind: RuntimeQueryKind) -> &'static str {
    match kind {
        RuntimeQueryKind::Highlights => "highlights",
        RuntimeQueryKind::Injections => "injections",
        RuntimeQueryKind::Locals => "locals",
        RuntimeQueryKind::Tags => "tags",
        RuntimeQueryKind::Textobjects => "textobjects",
        RuntimeQueryKind::Indents => "indents",
        RuntimeQueryKind::Folds => "folds",
        RuntimeQueryKind::Rainbows => "rainbows",
    }
}

pub(crate) fn cmd_language_list(anchor_path: Option<&Path>) {
    if let Err(error) = config::configure_runtime_loader_for_file(anchor_path, true) {
        eprintln!("language config failed: {error}");
        std::process::exit(EXIT_RUNTIME_CONFIG_MERGE);
    }
    print!(
        "{}",
        render_runtime_languages_report("ee do language list", &effective_runtime_language_rows())
    );
}
