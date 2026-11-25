// Copyright 2024 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tree-sitter syntax support and shared language registry.
//!
//! This module is canonical parse/query entry point for normal, constrained,
//! and VLF syntax features. Runtime responsibilities that must survive full
//! cutover live here:
//!
//! - syntax highlighting spans for normal and constrained buffers
//! - visible-range syntax spans for VLF buffers
//! - semantic selection and navigation language resolution
//! - language feature gating and downgrade behavior
//! - explicit language metadata for comments, indentation, and known semantic
//!   gaps used by shared edit-feature dispatch
//!
//! ## Evaluation findings
//!
//! tree-sitter is suitable as canonical engine for language-sensitive features
//! beyond basic highlighting:
//!
//! - **Incremental parsing**: `Tree::edit` + re-parse only processes changed
//!   byte ranges, making it O(edit size) rather than O(file size).
//! - **Folds**: AST node boundaries give precise, language-aware fold ranges
//!   (e.g. Rust `function_item`, `struct_item`, `block`).
//! - **Indentation**: Walking the syntax tree to count block depth produces
//!   correct results even for edge cases that confuse bracket-counting
//!   heuristics (template strings, raw string literals, macros).
//!
//! ## Final architecture
//!
//! Backend tree-sitter owns syntax spans, visible-range parsing, semantic
//! feature gates, comment metadata, and reindent inputs across normal,
//! constrained, and VLF modes. Frontends render backend-provided spans and
//! capability results only.
//!
use std::ops::ControlFlow;
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::fold_support::fold_parse_timeout;
use crate::runtime_loader::{
    RuntimeLoader, RuntimeQueryKind, with_default_runtime_loader, with_default_runtime_loader_mut,
};
use crate::text_store::DocumentMode;
use serde::Serialize;
use tree_sitter::{
    Language, Node, ParseOptions, Parser, Point, QueryCursor, StreamingIterator, Tree,
};

/// Default byte budget for one visible VLF parse window.
pub(crate) const DEFAULT_VISIBLE_SYNTAX_MAX_BYTES: usize = 128 * 1024;
/// Default hard wall-clock budget for one visible VLF parse window.
pub(crate) const DEFAULT_VISIBLE_SYNTAX_TIMEOUT: Duration = Duration::from_millis(4);
/// Default maximum highlighted node matches for one visible VLF parse window.
pub(crate) const DEFAULT_VISIBLE_SYNTAX_MAX_MATCHES: usize = 2_048;
/// Default maximum emitted captures/spans for one visible VLF parse window.
pub(crate) const DEFAULT_VISIBLE_SYNTAX_MAX_CAPTURES: usize = 4_096;
const MAX_VISIBLE_INJECTION_DEPTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineCommentStyle {
    Unsupported,
    Token(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockCommentStyle {
    Unsupported,
    Tokens { open: &'static str, close: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndentationStrategy {
    Unsupported,
    TreeSitter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticTargetKind {
    Function,
    Class,
    Parameter,
    Comment,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LanguageMetadata {
    pub(crate) line_comment: LineCommentStyle,
    pub(crate) block_comment: BlockCommentStyle,
    pub(crate) indentation: IndentationStrategy,
    pub(crate) unsupported_semantic_targets: &'static [SemanticTargetKind],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyntaxFeatureAvailability {
    pub(crate) syntax_spans: bool,
    pub(crate) semantic_motions: bool,
    pub(crate) line_comments: bool,
    pub(crate) block_comments: bool,
    pub(crate) reindent: bool,
}

/// Syntax span encoded relative to one rendered line.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VisibleSyntaxSpan {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) scope: String,
}

/// Guardrails for visible-range VLF parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisibleSyntaxLimits {
    pub(crate) max_bytes: usize,
    pub(crate) timeout: Duration,
    pub(crate) max_matches: usize,
    pub(crate) max_captures: usize,
}

impl Default for VisibleSyntaxLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_VISIBLE_SYNTAX_MAX_BYTES,
            timeout: DEFAULT_VISIBLE_SYNTAX_TIMEOUT,
            max_matches: DEFAULT_VISIBLE_SYNTAX_MAX_MATCHES,
            max_captures: DEFAULT_VISIBLE_SYNTAX_MAX_CAPTURES,
        }
    }
}
mod folds;
mod indent;
mod language;
mod parse;
mod visible;

pub(crate) use self::folds::FoldRange;
pub(crate) use self::folds::fold_ranges_for_text;
pub(crate) use self::indent::indentation_levels_for_text;
pub use self::language::canonical_language_name;
pub(crate) use self::language::language_metadata_for_name;
pub use self::language::language_name_for_path;
pub(crate) use self::language::language_supports_semantic_target;
pub(crate) use self::language::resolve_ts_language;
pub(crate) use self::language::syntax_feature_availability;

use self::parse::QueryWarmup;
use self::parse::parse_tree_with_timeout_and_queries;
use self::parse::runtime_language;
use self::visible::chunk_syntax_spans_with_depth;

#[allow(dead_code)]
fn parse_tree_with_timeout(language_name: &str, text: &str, timeout: Duration) -> Option<Tree> {
    parse_tree_with_timeout_and_queries(language_name, text, timeout, QueryWarmup::None)
}

/// Returns tree-sitter `Language` for given xi language name.
#[allow(dead_code)]
pub(crate) fn ts_language_for_name(language_name: &str) -> Option<Language> {
    runtime_language(Some(language_name), None, QueryWarmup::None)
}

#[allow(dead_code)]
pub(crate) fn language_path_aliases(_language_name: &str) -> Option<&'static [&'static str]> {
    None
}

/// Test-only wrapper over the full-range walk; production calls
/// [`chunk_syntax_spans_for_segments`] so window budgets are not consumed by
/// parse-context rows.
#[cfg(test)]
pub(crate) fn chunk_syntax_spans(
    language_name: &str,
    chunk_text: &str,
    segments: &[Range<usize>],
    limits: VisibleSyntaxLimits,
) -> Vec<Vec<VisibleSyntaxSpan>> {
    chunk_syntax_spans_with_depth(
        language_name,
        chunk_text,
        segments,
        0..segments.len(),
        limits,
        0,
        Instant::now(),
    )
}

/// Like [`chunk_syntax_spans`], with the match/capture budgets and span
/// emission scoped to the `requested` segment indices.
///
/// Segments outside the range are still parsed (languages that must start at
/// the document start, YAML, rely on the prefix for block context), but a
/// prefix's captures must not consume the window's budgets or starve its rows:
/// before this, deep YAML windows rendered without spans once the walk hit the
/// match cap on the context above them.
pub(crate) fn chunk_syntax_spans_for_segments(
    language_name: &str,
    chunk_text: &str,
    segments: &[Range<usize>],
    requested: Range<usize>,
    limits: VisibleSyntaxLimits,
) -> Vec<Vec<VisibleSyntaxSpan>> {
    chunk_syntax_spans_with_depth(
        language_name,
        chunk_text,
        segments,
        requested,
        limits,
        0,
        Instant::now(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::folds::byte_to_point;
    use super::folds::fold_ranges;
    use super::indent::indent_level_at_line;
    use super::indent::indentation_levels_for_text_with_timeout;
    use super::language::ts_language_for_path;
    use super::visible::VisibleSyntaxWalk;
    use super::visible::line_segments;
    use super::visible::line_start_offsets;
    use super::visible::visible_syntax_spans;
    use super::*;
    use crate::runtime_loader::{RuntimeLanguageConfig, RuntimeLanguageOverrides};
    use std::sync::MutexGuard;

    fn parse_rust(src: &str) -> Tree {
        parse_tree_with_timeout("rust", src, Duration::from_secs(1)).expect("parse failed")
    }

    fn parse_python(src: &str) -> Tree {
        parse_tree_with_timeout("python", src, Duration::from_secs(1)).expect("parse failed")
    }

    fn test_visible_syntax_limits() -> VisibleSyntaxLimits {
        // Generous test budget: these tests assert span content, not timing,
        // and run in parallel with other tree-sitter work.
        VisibleSyntaxLimits { timeout: Duration::from_secs(5), ..VisibleSyntaxLimits::default() }
    }

    fn runtime_loader_test_guard() -> MutexGuard<'static, ()> {
        crate::runtime_loader::runtime_loader_test_guard()
    }

    fn line_span_text<'a>(src: &'a str, line_index: usize, span: &VisibleSyntaxSpan) -> &'a str {
        let line_start = src.lines().take(line_index).map(|line| line.len() + 1).sum::<usize>();
        &src[(line_start + span.start_byte)..(line_start + span.end_byte)]
    }

    struct RuntimeLoaderOverrideGuard;

    impl RuntimeLoaderOverrideGuard {
        fn install(user_overrides: RuntimeLanguageOverrides) -> Self {
            crate::runtime_loader::configure_default_runtime_loader_overrides(
                user_overrides,
                RuntimeLanguageOverrides::new(),
                false,
            )
            .expect("configure runtime loader overrides");
            crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();
            Self
        }
    }

    impl Drop for RuntimeLoaderOverrideGuard {
        fn drop(&mut self) {
            let _ = crate::runtime_loader::configure_default_runtime_loader_overrides(
                RuntimeLanguageOverrides::new(),
                RuntimeLanguageOverrides::new(),
                false,
            );
            crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();
        }
    }

    // --------------------------------------------------
    // Language registry
    // --------------------------------------------------

    #[test]
    fn test_ts_language_registry_covers_bundled_grammars() {
        for language in [
            "bash",
            "c",
            "csharp",
            "cpp",
            "css",
            "elixir",
            "go",
            "haskell",
            "html",
            "java",
            "javascript",
            "json",
            "php",
            "python",
            "ruby",
            "rust",
            "scala",
            "typescript",
        ] {
            assert!(ts_language_for_name(language).is_some(), "missing grammar for {language}");
        }
    }

    #[test]
    fn test_ts_language_registry_normalizes_aliases() {
        for (alias, canonical) in [
            ("rust", "rust"),
            ("python3", "python"),
            ("sh", "bash"),
            ("cpp", "cpp"),
            ("csharp", "csharp"),
            ("js", "javascript"),
            ("tsx", "typescript"),
        ] {
            assert_eq!(canonical_language_name(alias).as_deref(), Some(canonical));
            assert!(ts_language_for_name(alias).is_some(), "missing alias support for {alias}");
        }
    }

    #[test]
    fn test_ts_language_registry_resolves_paths() {
        for (path, canonical) in [
            ("build.sh", "bash"),
            ("main.c", "c"),
            ("Program.cs", "csharp"),
            ("main.cpp", "cpp"),
            ("app.css", "css"),
            ("mix.exs", "elixir"),
            ("main.go", "go"),
            ("Main.hs", "haskell"),
            ("index.html", "html"),
            ("Main.java", "java"),
            ("app.jsx", "javascript"),
            ("data.json", "json"),
            ("index.php", "php"),
            ("tool.py", "python"),
            ("Gemfile", "ruby"),
            ("lib.rs", "rust"),
            ("build.sc", "scala"),
            ("view.tsx", "typescript"),
        ] {
            assert_eq!(language_name_for_path(Path::new(path)).as_deref(), Some(canonical));
            assert!(
                ts_language_for_path(Path::new(path)).is_some(),
                "missing path support for {path}"
            );
        }
    }

    #[test]
    fn test_ts_language_resolution_prefers_explicit_name_then_path() {
        assert_eq!(canonical_language_name("Python 3").as_deref(), Some("python"));
        assert_eq!(language_name_for_path(Path::new("main.rs")).as_deref(), Some("rust"));
        assert!(resolve_ts_language(Some("Plain Text"), Some(Path::new("main.rs"))).is_some());
    }

    #[test]
    fn test_ts_language_unknown() {
        assert!(ts_language_for_name("COBOL").is_none());
    }

    #[test]
    fn syntax_feature_availability_uses_language_and_document_mode() {
        let normal = syntax_feature_availability(Some("rust"), None, DocumentMode::Normal);
        assert!(normal.syntax_spans);
        assert!(normal.semantic_motions);
        assert!(normal.line_comments);
        assert!(normal.block_comments);
        assert!(normal.reindent);

        let vlf = syntax_feature_availability(Some("rust"), None, DocumentMode::Vlf);
        assert!(vlf.syntax_spans);
        assert!(vlf.semantic_motions);
        assert!(!vlf.line_comments);
        assert!(!vlf.block_comments);
        assert!(!vlf.reindent);

        let constrained =
            syntax_feature_availability(Some("rust"), None, DocumentMode::ConstrainedNormal);
        assert!(constrained.syntax_spans);
        assert!(constrained.semantic_motions);
        assert!(constrained.line_comments);
        assert!(constrained.block_comments);
        assert!(!constrained.reindent);
    }

    #[test]
    fn syntax_feature_availability_resolves_path_fallback() {
        let capabilities = syntax_feature_availability(
            Some("Plain Text"),
            Some(Path::new("component.tsx")),
            DocumentMode::Normal,
        );

        assert!(capabilities.syntax_spans);
        assert!(capabilities.semantic_motions);
        assert!(capabilities.line_comments);
        assert!(capabilities.reindent);

        let unknown = syntax_feature_availability(Some("Plain Text"), None, DocumentMode::Normal);
        assert!(!unknown.syntax_spans);
        assert!(!unknown.semantic_motions);
        assert!(!unknown.line_comments);
        assert!(!unknown.block_comments);
        assert!(!unknown.reindent);
    }

    #[test]
    fn test_full_parse_produces_tree() {
        let src = "fn main() {}\n";
        let tree = parse_rust(src);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn visible_syntax_spans_highlights_only_supplied_lines() {
        let _guard = runtime_loader_test_guard();
        let src = "fn main() {\n    let answer = 42;\n}\n";
        let spans = visible_syntax_spans("rust", src, test_visible_syntax_limits());

        assert_eq!(spans.len(), 4);
        assert!(spans[0].iter().any(|span| span.scope.starts_with("keyword")));
        assert!(spans[1].iter().any(|span| span.scope.starts_with("constant")));
        assert!(spans.iter().flatten().all(|span| span.end_byte > span.start_byte));
    }

    #[test]
    fn visible_syntax_spans_highlight_yaml_mapping_keys() {
        let _guard = runtime_loader_test_guard();
        let src = "tasks:\n  format: cargo fmt\n";
        let spans = visible_syntax_spans("yaml", src, test_visible_syntax_limits());

        assert!(spans[0].iter().any(|span| {
            (span.scope.starts_with("variable") || span.scope.starts_with("property"))
                && &src[span.start_byte..span.end_byte] == "tasks"
        }));
        assert!(spans[1].iter().any(|span| {
            (span.scope.starts_with("variable") || span.scope.starts_with("property"))
                && &src[(src.find('\n').unwrap() + 1 + span.start_byte)
                    ..(src.find('\n').unwrap() + 1 + span.end_byte)]
                    == "format"
        }));
    }

    #[test]
    fn visible_syntax_spans_keep_yaml_block_scalar_punctuation() {
        let _guard = runtime_loader_test_guard();
        let src = "logs:\n  commands:\n    - |\n      set -eu\n";
        let spans = visible_syntax_spans("yaml", src, test_visible_syntax_limits());

        assert!(spans[2].iter().any(|span| {
            span.scope.starts_with("punctuation")
                && &src[(src.lines().take(2).map(|line| line.len() + 1).sum::<usize>()
                    + span.start_byte)
                    ..(src.lines().take(2).map(|line| line.len() + 1).sum::<usize>()
                        + span.end_byte)]
                    == "-"
        }));
        assert!(spans[2].iter().any(|span| {
            (span.scope.starts_with("punctuation") || span.scope.starts_with("operator"))
                && &src[(src.lines().take(2).map(|line| line.len() + 1).sum::<usize>()
                    + span.start_byte)
                    ..(src.lines().take(2).map(|line| line.len() + 1).sum::<usize>()
                        + span.end_byte)]
                    == "|"
        }));
        assert!(spans[3].iter().any(|span| {
            span.scope.starts_with("function") && line_span_text(src, 3, span) == "set"
        }));
        // `-eu` is a command argument: the bundled bash query captures it as
        // `variable.parameter`.
        assert!(spans[3].iter().any(|span| {
            span.scope.starts_with("variable") && line_span_text(src, 3, span) == "-eu"
        }));
    }

    #[test]
    fn visible_syntax_spans_keep_yaml_keys_after_block_scalar() {
        let _guard = runtime_loader_test_guard();
        let src = "logs:\n  commands:\n    - |\n      set -eu\n      echo hi\n  description: Open discovered editor and plugin logs in ee.\nconfig-nearest:\n  commands:\n    - echo hi\n";
        let spans = visible_syntax_spans("yaml", src, test_visible_syntax_limits());

        let description_line = src.lines().take(5).map(|line| line.len() + 1).sum::<usize>();
        let config_line = src.lines().take(6).map(|line| line.len() + 1).sum::<usize>();

        assert!(spans[5].iter().any(|span| {
            (span.scope.starts_with("variable") || span.scope.starts_with("property"))
                && &src[(description_line + span.start_byte)..(description_line + span.end_byte)]
                    == "description"
        }));
        assert!(spans[6].iter().any(|span| {
            (span.scope.starts_with("variable") || span.scope.starts_with("property"))
                && &src[(config_line + span.start_byte)..(config_line + span.end_byte)]
                    == "config-nearest"
        }));
    }

    #[test]
    fn visible_syntax_spans_distribute_multiline_captures_across_segments() {
        let _guard = runtime_loader_test_guard();
        // One capture (the two-line string literal) overlaps two line
        // segments; each must carry a span relative to its own segment. This
        // pins the bounded segment scan in `apply_highlight_query_spans`, which
        // visits only segments a capture can overlap.
        let src = "let s = \"line one\nline two\";\nlet t = 2;\n";
        let spans = visible_syntax_spans("rust", src, test_visible_syntax_limits());

        assert!(
            spans[0].iter().any(|span| span.scope.starts_with("string")
                && line_span_text(src, 0, span) == "\"line one")
        );
        assert!(
            spans[1].iter().any(|span| span.scope.starts_with("string")
                && line_span_text(src, 1, span) == "line two\"")
        );
    }

    #[test]
    fn visible_walk_scopes_budgets_and_spans_to_requested_rows() {
        let _guard = runtime_loader_test_guard();
        // The fallback node walk must mirror the highlight walk's window
        // scoping: rows outside `requested` get no spans (they are parse
        // context), and the match/capture budgets count requested-row work
        // only.
        let src = "let a = 1; // one\nlet b = 2; // two\nlet c = 3; // three\n";
        let tree = parse_rust(src);
        let line_starts = line_start_offsets(src);
        let segments = line_segments(src, &line_starts);
        let mut per_segment = vec![Vec::new(); segments.len()];
        let requested = 1..2;
        let (walked_matches, walked_captures) = {
            let mut walk = VisibleSyntaxWalk {
                text: src,
                segments: &segments,
                requested: &requested,
                per_segment: &mut per_segment,
                started: Instant::now(),
                limits: test_visible_syntax_limits(),
                matches: 0,
                captures: 0,
            };
            walk.walk(tree.root_node());
            (walk.matches, walk.captures)
        };

        assert!(per_segment[0].is_empty(), "row outside requested must stay empty");
        assert!(
            per_segment[1]
                .iter()
                .any(|span| span.scope.starts_with("comment") || span.scope.starts_with("keyword")),
            "requested row must carry fallback spans"
        );
        assert!(per_segment[2].is_empty(), "row outside requested must stay empty");
        assert_eq!(walked_matches, walked_captures, "one emitted span per matched node");
        assert!(walked_matches > 0, "budgets must count requested-row work");
    }

    #[test]
    fn visible_syntax_spans_inject_bash_into_yaml_commands_block_scalars() {
        let _guard = runtime_loader_test_guard();
        crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();
        crate::runtime_loader::with_default_runtime_loader_mut(|loader| {
            loader.invalidate_language("yaml");
            loader.record_query_artifact(
                "yaml",
                crate::runtime_loader::RuntimeQueryKind::Injections,
                include_str!("../../../runtime/queries/yaml/injections.scm").to_string(),
                Vec::new(),
                Vec::new(),
            );
        });

        let src = "logs:\n  commands:\n    - |\n      for candidate in \\\n        \"$PWD/editor.log\"\n      do\n        if [ -n \"$candidate\" ]; then\n          break\n        fi\n      done\n  description: Open logs\n";
        let spans = visible_syntax_spans("yaml", src, test_visible_syntax_limits());

        let for_line = src.lines().take(3).map(|line| line.len() + 1).sum::<usize>();
        let if_line = src.lines().take(6).map(|line| line.len() + 1).sum::<usize>();
        let description_line = src.lines().take(10).map(|line| line.len() + 1).sum::<usize>();

        assert!(spans[3].iter().any(|span| {
            span.scope.starts_with("keyword")
                && &src[(for_line + span.start_byte)..(for_line + span.end_byte)] == "for"
        }));
        assert!(spans[6].iter().any(|span| {
            span.scope.starts_with("keyword")
                && &src[(if_line + span.start_byte)..(if_line + span.end_byte)] == "if"
        }));
        assert!(spans[10].iter().any(|span| {
            (span.scope.starts_with("variable") || span.scope.starts_with("property"))
                && &src[(description_line + span.start_byte)..(description_line + span.end_byte)]
                    == "description"
        }));

        crate::runtime_loader::with_default_runtime_loader_mut(|loader| {
            loader.invalidate_language("yaml");
        });
    }

    #[test]
    fn visible_syntax_spans_apply_injection_queries_without_warming_query_cache() {
        let _guard = runtime_loader_test_guard();
        crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();
        crate::runtime_loader::with_default_runtime_loader_mut(|loader| {
            loader.invalidate_language("yaml");
            loader.record_query_artifact(
                "yaml",
                crate::runtime_loader::RuntimeQueryKind::Injections,
                include_str!("../../../runtime/queries/yaml/injections.scm").to_string(),
                Vec::new(),
                Vec::new(),
            );
        });

        let src = "logs:\n  commands:\n    - |\n      for candidate in \"$PWD/editor.log\"\n      do\n        break\n      done\n";
        let spans = visible_syntax_spans("yaml", src, test_visible_syntax_limits());

        assert!(
            spans[3]
                .iter()
                .any(|span| span.scope.starts_with("keyword")
                    && line_span_text(src, 3, span) == "for")
        );

        crate::runtime_loader::with_default_runtime_loader_mut(|loader| {
            assert!(
                loader
                    .cached_query_artifact(
                        "yaml",
                        crate::runtime_loader::RuntimeQueryKind::Highlights
                    )
                    .is_none()
            );
            assert!(
                loader
                    .cached_query_artifact(
                        "yaml",
                        crate::runtime_loader::RuntimeQueryKind::Injections
                    )
                    .is_some()
            );
            assert!(
                loader
                    .cached_query_artifact("yaml", crate::runtime_loader::RuntimeQueryKind::Locals)
                    .is_none()
            );
            loader.invalidate_language("yaml");
        });
    }

    #[test]
    fn visible_syntax_spans_does_not_warm_runtime_queries() {
        let _guard = runtime_loader_test_guard();
        let spans = visible_syntax_spans(
            "rust",
            "fn main() {\n    let answer = 42;\n}\n",
            test_visible_syntax_limits(),
        );
        assert!(spans.iter().flatten().any(|span| span.scope.starts_with("keyword")));

        crate::runtime_loader::with_default_runtime_loader_mut(|loader| {
            assert!(
                loader
                    .cached_query_artifact(
                        "rust",
                        crate::runtime_loader::RuntimeQueryKind::Highlights
                    )
                    .is_none()
            );
            assert!(
                loader
                    .cached_query_artifact(
                        "rust",
                        crate::runtime_loader::RuntimeQueryKind::Injections
                    )
                    .is_none()
            );
            assert!(
                loader
                    .cached_query_artifact("rust", crate::runtime_loader::RuntimeQueryKind::Locals)
                    .is_none()
            );
        });
    }

    #[test]
    fn visible_syntax_spans_match_injection_language_uses_runtime_regex_matcher() {
        let _guard = runtime_loader_test_guard();
        crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();

        let mut overrides = RuntimeLanguageOverrides::new();
        overrides.insert(
            "javascript".to_string(),
            RuntimeLanguageConfig {
                injection_regex: Some(String::from("^javascript$")),
                match_priority: Some(5),
                ..RuntimeLanguageConfig::default()
            },
        );
        overrides.insert(
            "typescript".to_string(),
            RuntimeLanguageConfig {
                injection_regex: Some(String::from("^javascript$")),
                match_priority: Some(10),
                ..RuntimeLanguageConfig::default()
            },
        );

        let _guard = RuntimeLoaderOverrideGuard::install(overrides);
        crate::runtime_loader::with_default_runtime_loader(|loader| {
            let matched =
                loader.match_injection_language("javascript").expect("javascript injection match");
            assert_eq!(matched.canonical_id, "typescript");
            assert_eq!(matched.display_name, "typescript");
        });
    }

    #[test]
    fn indentation_levels_returns_none_when_parse_budget_is_zero() {
        let src = "fn main() {\nlet x = 1;\n}\n";
        assert!(indentation_levels_for_text_with_timeout("rust", src, 2, Duration::ZERO).is_none());
    }

    #[test]
    fn indentation_levels_uses_shared_scaled_parse_budget() {
        // CI flake guard: the default reindent budget must come from the
        // shared scaled parse budget (250ms floor for tiny buffers), not a
        // flat sub-100ms constant that a descheduled CI thread can exceed
        // before parsing a tiny buffer (macOS-only `reindent("C")` failure).
        let src = "int main() {\nreturn 0;\n}\n";
        assert!(fold_parse_timeout(src.len()) >= Duration::from_millis(250));
        assert!(indentation_levels_for_text("C", src, 2).is_some());
    }

    // --------------------------------------------------
    // Fold detection
    // --------------------------------------------------

    #[test]
    fn test_fold_function() {
        let src = "fn foo() {\n    let x = 1;\n}\n";
        let tree = parse_rust(src);
        let folds = fold_ranges(&tree);
        // There should be at least one fold covering the function or its block.
        assert!(!folds.is_empty(), "expected at least one fold");
        assert!(folds.iter().any(|f| f.body_end >= 2), "fold should span to line 2");
    }

    #[test]
    fn test_fold_python_function() {
        let src = "def foo():\n    x = 1\n    y = 2\n";
        let tree = parse_python(src);
        let folds = fold_ranges(&tree);
        assert!(!folds.is_empty(), "expected fold for Python function");
    }

    #[test]
    fn fold_ranges_for_text_uses_path_fallback() {
        let src = "fn foo() {\n    let x = 1;\n}\n";
        let folds = fold_ranges_for_text(
            Some("Plain Text"),
            Some(Path::new("main.rs")),
            src,
            Duration::from_secs(1),
        );
        assert!(folds.iter().any(|fold| fold.header_line == 0 && fold.body_end >= 2));
    }

    #[test]
    fn test_no_folds_single_line() {
        let src = "fn foo() { 1 }\n";
        let tree = parse_rust(src);
        let folds = fold_ranges(&tree);
        // A single-line function should yield no multi-line folds.
        assert!(
            folds.iter().all(|f| f.body_start > f.body_end || f.body_end == f.header_line),
            "single-line function should have no body folds; got {folds:?}"
        );
    }

    // --------------------------------------------------
    // Indentation
    // --------------------------------------------------

    #[test]
    fn test_indent_level_top_level() {
        let src = "fn main() {\n    let x = 1;\n}\n";
        let tree = parse_rust(src);
        // Line 0 is top-level: no enclosing block.
        let level = indent_level_at_line(&tree, 0);
        assert_eq!(level, 0);
    }

    #[test]
    fn test_indent_level_inside_block() {
        let src = "fn main() {\n    let x = 1;\n}\n";
        let tree = parse_rust(src);
        // Line 1 is inside the function body.
        let level = indent_level_at_line(&tree, 1);
        assert!(level >= 1, "expected level >= 1 inside a function, got {level}");
    }

    #[test]
    fn test_indentation_levels_python() {
        let src = "def foo():\n    x = 1\n";
        let levels = indentation_levels_for_text("python", src, 1).unwrap();
        assert_eq!(levels, vec![0, 1]);
    }

    #[test]
    fn indentation_levels_dedent_rust_closing_brace() {
        let src = "fn main() {\nlet x = 1;\n}\n";
        let levels = indentation_levels_for_text("rust", src, 2).unwrap();
        assert_eq!(levels, vec![0, 1, 0]);
    }

    #[test]
    fn indentation_levels_dedent_python_else_clause() {
        let src = "if ready:\nprint(\"yes\")\nelse:\nprint(\"no\")\n";
        let levels = indentation_levels_for_text("python", src, 3).unwrap();
        assert_eq!(levels, vec![0, 1, 0, 1]);
    }

    #[test]
    fn indentation_levels_cover_c_like_blocks() {
        let src = "int main() {\nreturn 0;\n}\n";
        let levels = indentation_levels_for_text("C", src, 2).unwrap();
        assert_eq!(levels, vec![0, 1, 0]);
    }

    // --------------------------------------------------
    // Helpers
    // --------------------------------------------------

    #[test]
    fn test_byte_to_point_start() {
        assert_eq!(byte_to_point("hello\nworld", 0), Point::new(0, 0));
    }

    #[test]
    fn test_byte_to_point_second_line() {
        assert_eq!(byte_to_point("hello\nworld", 6), Point::new(1, 0));
    }

    #[test]
    fn test_byte_to_point_mid_line() {
        assert_eq!(byte_to_point("hello\nworld", 8), Point::new(1, 2));
    }
}
