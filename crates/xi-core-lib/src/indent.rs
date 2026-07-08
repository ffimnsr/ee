use std::ops::ControlFlow;
use std::path::Path;
#[cfg(test)]
use std::sync::MutexGuard;
use std::time::{Duration, Instant};

use tree_sitter::{Node, ParseOptions, Parser, Point, Query, QueryCursor, StreamingIterator, Tree};
use xi_rope::Rope;

use crate::runtime_loader::{IndentQueryCapture, with_default_runtime_loader_mut};
use crate::selection::SelRegion;
use crate::text_store::DocumentMode;
use crate::tree_sitter_support::resolve_ts_language;

pub(crate) const DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndentOutcome {
    Inherit,
    IndentOneLevel,
    DedentOneLevel,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SyntaxIndentContext<'a> {
    pub(crate) language_name: &'a str,
    pub(crate) file_path: Option<&'a Path>,
    pub(crate) document_mode: DocumentMode,
}

impl<'a> SyntaxIndentContext<'a> {
    pub(crate) fn new(
        language_name: &'a str,
        file_path: Option<&'a Path>,
        document_mode: DocumentMode,
    ) -> Self {
        Self { language_name, file_path, document_mode }
    }
}

pub(crate) fn syntax_indent_outcome(
    text: &Rope,
    region: &SelRegion,
    context: &SyntaxIndentContext<'_>,
) -> Option<IndentOutcome> {
    if context.language_name.is_empty() || !context.document_mode.feature_gates().whole_doc_ops {
        return None;
    }

    let anchor = region.min().min(text.len());
    let end = region.max().min(text.len());
    if text.line_of_offset(anchor) != text.line_of_offset(end) {
        return None;
    }

    let snapshot = text.slice_to_cow(0..text.len()).into_owned();
    syntax_indent_outcome_for_text(
        context.language_name,
        context.file_path,
        &snapshot,
        anchor,
        DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
    )
}

fn syntax_indent_outcome_for_text(
    language_name: &str,
    file_path: Option<&Path>,
    text: &str,
    anchor: usize,
    timeout: Duration,
) -> Option<IndentOutcome> {
    let compiled =
        with_default_runtime_loader_mut(|loader| loader.compile_indent_query(language_name).ok())
            .flatten()?;
    let tree = parse_tree_with_timeout(language_name, file_path, text, timeout)?;
    let anchor = anchor.min(text.len());
    let anchor_line = line_of_offset(text, anchor);
    let line_start = line_start_offset(text, anchor_line);
    let line_end = line_end_offset(text, line_start);
    let line_text = &text[line_start..line_end];
    let anchor_in_line = anchor.saturating_sub(line_start).min(line_text.len());
    let indent_end = line_text
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
        .unwrap_or(line_text.len());
    let dedent_allowed = anchor_in_line <= indent_end;

    let (indent, dedent) =
        query_indent_signals(&compiled.query, &tree, text, anchor, anchor_line, dedent_allowed);

    if indent {
        Some(IndentOutcome::IndentOneLevel)
    } else if dedent {
        Some(IndentOutcome::DedentOneLevel)
    } else {
        Some(IndentOutcome::Inherit)
    }
}

fn query_indent_signals(
    query: &Query,
    tree: &Tree,
    text: &str,
    anchor: usize,
    anchor_line: usize,
    dedent_allowed: bool,
) -> (bool, bool) {
    let bytes = text.as_bytes();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), bytes);
    let capture_names = query.capture_names();
    let mut indent = false;
    let mut dedent = false;

    loop {
        matches.advance();
        let Some(query_match) = matches.get() else {
            break;
        };
        for capture in query_match.captures.iter().copied() {
            let Some(kind) = capture_names
                .get(capture.index as usize)
                .and_then(|name| IndentQueryCapture::from_capture_name(name))
            else {
                continue;
            };

            match kind {
                IndentQueryCapture::Indent => {
                    if indent_capture_applies(capture.node, anchor, anchor_line) {
                        indent = true;
                    }
                }
                IndentQueryCapture::Dedent => {
                    if dedent_allowed && dedent_capture_applies(capture.node, anchor, anchor_line) {
                        dedent = true;
                    }
                }
            }
        }

        if indent {
            break;
        }
    }

    (indent, dedent)
}

fn indent_capture_applies(node: Node<'_>, anchor: usize, anchor_line: usize) -> bool {
    node.start_position().row == anchor_line
        && anchor <= node.end_byte()
        && node.end_position().row >= anchor_line
}

fn dedent_capture_applies(node: Node<'_>, _anchor: usize, anchor_line: usize) -> bool {
    node.start_position().row == anchor_line && node.end_position().row >= anchor_line
}

fn parse_tree_with_timeout(
    language_name: &str,
    file_path: Option<&Path>,
    text: &str,
    timeout: Duration,
) -> Option<Tree> {
    if timeout.is_zero() {
        return None;
    }

    let language = resolve_ts_language(Some(language_name), file_path)?;
    let started = Instant::now();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;

    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() > timeout { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    };
    let bytes = text.as_bytes();
    let mut read = |offset: usize, _: Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: Some(&mut progress) };
    let tree = parser.parse_with_options(&mut read, None, Some(options))?;
    (started.elapsed() <= timeout).then_some(tree)
}

fn line_of_offset(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())].iter().filter(|&&byte| byte == b'\n').count()
}

fn line_start_offset(text: &str, target_line: usize) -> usize {
    if target_line == 0 {
        return 0;
    }

    let mut line = 0usize;
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            line += 1;
            if line == target_line {
                return idx + 1;
            }
        }
    }
    text.len()
}

fn line_end_offset(text: &str, line_start: usize) -> usize {
    text[line_start.min(text.len())..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_loader::{
        RuntimeLanguageConfig, RuntimeLanguageOverrides, RuntimeQueryKind,
        configure_default_runtime_loader_overrides,
        ensure_default_runtime_loader_has_test_grammars, with_default_runtime_loader_mut,
    };
    use std::collections::BTreeSet;

    fn runtime_loader_test_guard() -> MutexGuard<'static, ()> {
        crate::runtime_loader::runtime_loader_test_guard()
    }

    struct RuntimeLoaderOverrideGuard;

    impl RuntimeLoaderOverrideGuard {
        fn install(languages: &[&str]) -> Self {
            let mut overrides = RuntimeLanguageOverrides::new();
            for language in languages {
                overrides.insert(
                    (*language).to_string(),
                    RuntimeLanguageConfig {
                        supported_query_kinds: Some(BTreeSet::from([RuntimeQueryKind::Indents])),
                        ..RuntimeLanguageConfig::default()
                    },
                );
            }
            configure_default_runtime_loader_overrides(
                overrides,
                RuntimeLanguageOverrides::new(),
                false,
            )
            .expect("configure runtime loader overrides");
            ensure_default_runtime_loader_has_test_grammars();
            Self
        }
    }

    impl Drop for RuntimeLoaderOverrideGuard {
        fn drop(&mut self) {
            let _ = configure_default_runtime_loader_overrides(
                RuntimeLanguageOverrides::new(),
                RuntimeLanguageOverrides::new(),
                false,
            );
            ensure_default_runtime_loader_has_test_grammars();
            with_default_runtime_loader_mut(|loader| {
                for language in ["rust", "json", "python"] {
                    loader.invalidate_language(language);
                }
            });
        }
    }

    fn install_indent_query(language: &str, query: &str) {
        with_default_runtime_loader_mut(|loader| {
            loader.invalidate_language(language);
            loader.record_query_artifact(
                language,
                RuntimeQueryKind::Indents,
                query.to_string(),
                Vec::new(),
                Vec::new(),
            );
        });
    }

    #[test]
    fn syntax_indent_outcome_uses_indent_capture_for_block_open_line() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(_) @indent");

        let text = "fn main() {}";
        let anchor = text.find('{').expect("find opener") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_uses_dedent_capture_before_closer() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(_) @dedent");

        let text = "    }";
        let outcome = syntax_indent_outcome_for_text(
            "rust",
            None,
            text,
            4,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::DedentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_returns_none_when_mode_disables_whole_doc_syntax_work() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        install_indent_query("rust", "(_) @indent");

        let text: Rope = "fn main() {}".into();
        let region = SelRegion::caret(10);
        let context = SyntaxIndentContext::new("rust", None, DocumentMode::ConstrainedNormal);

        assert_eq!(syntax_indent_outcome(&text, &region, &context), None);
    }

    #[test]
    fn syntax_indent_outcome_returns_none_when_query_missing() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["rust"]);
        with_default_runtime_loader_mut(|loader| loader.invalidate_language("rust"));

        let text = "fn main() {}";
        let anchor = text.find('{').expect("find opener") + 1;

        assert_eq!(
            syntax_indent_outcome_for_text(
                "rust",
                None,
                text,
                anchor,
                DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
            ),
            None
        );
    }

    #[test]
    fn syntax_indent_outcome_supports_json_indent_query() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["json"]);
        install_indent_query("json", "(_) @indent");

        let text = "{}";
        let anchor = 1;
        let outcome = syntax_indent_outcome_for_text(
            "json",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }

    #[test]
    fn syntax_indent_outcome_supports_python_indent_query_when_semantics_ready() {
        let _guard = runtime_loader_test_guard();
        let _override_guard = RuntimeLoaderOverrideGuard::install(&["python"]);
        install_indent_query("python", "(_) @indent");

        let text = "if ok:\n    pass\n";
        let anchor = text.find(':').expect("find colon") + 1;
        let outcome = syntax_indent_outcome_for_text(
            "python",
            None,
            text,
            anchor,
            DEFAULT_NEWLINE_INDENT_PARSE_TIMEOUT,
        );

        assert_eq!(outcome, Some(IndentOutcome::IndentOneLevel));
    }
}
