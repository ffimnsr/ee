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

use crate::text_store::DocumentMode;
use serde::Serialize;
use tree_sitter::{Language, Node, ParseOptions, Parser, Point, Tree};

/// Default byte budget for one visible VLF parse window.
pub(crate) const DEFAULT_VISIBLE_SYNTAX_MAX_BYTES: usize = 128 * 1024;
/// Default hard wall-clock budget for one visible VLF parse window.
pub(crate) const DEFAULT_VISIBLE_SYNTAX_TIMEOUT: Duration = Duration::from_millis(4);
/// Default hard wall-clock budget for one whole-buffer reindent parse.
pub(crate) const DEFAULT_REINDENT_PARSE_TIMEOUT: Duration = Duration::from_millis(25);
/// Default maximum highlighted node matches for one visible VLF parse window.
pub(crate) const DEFAULT_VISIBLE_SYNTAX_MAX_MATCHES: usize = 2_048;
/// Default maximum emitted captures/spans for one visible VLF parse window.
pub(crate) const DEFAULT_VISIBLE_SYNTAX_MAX_CAPTURES: usize = 4_096;

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

#[derive(Debug, Clone, Copy)]
struct LanguageRegistryEntry {
    canonical_name: &'static str,
    aliases: &'static [&'static str],
    path_aliases: &'static [&'static str],
    language: fn() -> Language,
    metadata: LanguageMetadata,
}

const NO_UNSUPPORTED_TARGETS: &[SemanticTargetKind] = &[];
const DATA_ONLY_TARGETS: &[SemanticTargetKind] = &[
    SemanticTargetKind::Function,
    SemanticTargetKind::Class,
    SemanticTargetKind::Parameter,
    SemanticTargetKind::Test,
];
const COMMENT_ONLY_TARGETS: &[SemanticTargetKind] = &[
    SemanticTargetKind::Function,
    SemanticTargetKind::Class,
    SemanticTargetKind::Parameter,
    SemanticTargetKind::Test,
];

const fn language_metadata(
    line_comment: LineCommentStyle,
    block_comment: BlockCommentStyle,
    indentation: IndentationStrategy,
    unsupported_semantic_targets: &'static [SemanticTargetKind],
) -> LanguageMetadata {
    LanguageMetadata { line_comment, block_comment, indentation, unsupported_semantic_targets }
}

fn bash_language() -> Language {
    tree_sitter_bash::LANGUAGE.into()
}

fn c_language() -> Language {
    tree_sitter_c::LANGUAGE.into()
}

fn c_sharp_language() -> Language {
    tree_sitter_c_sharp::LANGUAGE.into()
}

fn cpp_language() -> Language {
    tree_sitter_cpp::LANGUAGE.into()
}

fn css_language() -> Language {
    tree_sitter_css::LANGUAGE.into()
}

fn elixir_language() -> Language {
    tree_sitter_elixir::LANGUAGE.into()
}

fn go_language() -> Language {
    tree_sitter_go::LANGUAGE.into()
}

fn haskell_language() -> Language {
    tree_sitter_haskell::LANGUAGE.into()
}

fn html_language() -> Language {
    tree_sitter_html::LANGUAGE.into()
}

fn java_language() -> Language {
    tree_sitter_java::LANGUAGE.into()
}

fn javascript_language() -> Language {
    tree_sitter_javascript::LANGUAGE.into()
}

fn json_language() -> Language {
    tree_sitter_json::LANGUAGE.into()
}

fn php_language() -> Language {
    tree_sitter_php::LANGUAGE_PHP.into()
}

fn python_language() -> Language {
    tree_sitter_python::LANGUAGE.into()
}

fn ruby_language() -> Language {
    tree_sitter_ruby::LANGUAGE.into()
}

fn rust_language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

fn scala_language() -> Language {
    tree_sitter_scala::LANGUAGE.into()
}

fn typescript_language() -> Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

fn tsx_language() -> Language {
    tree_sitter_typescript::LANGUAGE_TSX.into()
}

const LANGUAGE_REGISTRY: &[LanguageRegistryEntry] = &[
    LanguageRegistryEntry {
        canonical_name: "Bash",
        aliases: &["bash", "shell", "shellscript", "sh"],
        path_aliases: &["sh", "bash", ".bashrc", ".zshrc"],
        language: bash_language,
        metadata: language_metadata(
            LineCommentStyle::Token("#"),
            BlockCommentStyle::Unsupported,
            IndentationStrategy::Unsupported,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "C",
        aliases: &["c"],
        path_aliases: &["c", "h"],
        language: c_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "C#",
        aliases: &["c#", "csharp", "cs"],
        path_aliases: &["cs"],
        language: c_sharp_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "C++",
        aliases: &["c++", "cpp", "cplusplus"],
        path_aliases: &["cc", "cpp", "cxx", "hh", "hpp", "hxx"],
        language: cpp_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "CSS",
        aliases: &["css"],
        path_aliases: &["css", "less", "scss"],
        language: css_language,
        metadata: language_metadata(
            LineCommentStyle::Unsupported,
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::Unsupported,
            DATA_ONLY_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "Elixir",
        aliases: &["elixir", "ex", "exs"],
        path_aliases: &["ex", "exs"],
        language: elixir_language,
        metadata: language_metadata(
            LineCommentStyle::Token("#"),
            BlockCommentStyle::Unsupported,
            IndentationStrategy::Unsupported,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "Go",
        aliases: &["go", "golang"],
        path_aliases: &["go"],
        language: go_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "Haskell",
        aliases: &["haskell", "hs"],
        path_aliases: &["hs"],
        language: haskell_language,
        metadata: language_metadata(
            LineCommentStyle::Token("--"),
            BlockCommentStyle::Tokens { open: "{-", close: "-}" },
            IndentationStrategy::Unsupported,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "HTML",
        aliases: &["html"],
        path_aliases: &["htm", "html", "xhtml"],
        language: html_language,
        metadata: language_metadata(
            LineCommentStyle::Unsupported,
            BlockCommentStyle::Tokens { open: "<!--", close: "-->" },
            IndentationStrategy::Unsupported,
            COMMENT_ONLY_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "Java",
        aliases: &["java"],
        path_aliases: &["java"],
        language: java_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "JavaScript",
        aliases: &["javascript", "javascriptreact", "js", "jsx"],
        path_aliases: &["cjs", "js", "jsx", "mjs"],
        language: javascript_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "JSON",
        aliases: &["json"],
        path_aliases: &["json"],
        language: json_language,
        metadata: language_metadata(
            LineCommentStyle::Unsupported,
            BlockCommentStyle::Unsupported,
            IndentationStrategy::Unsupported,
            DATA_ONLY_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "PHP",
        aliases: &["php"],
        path_aliases: &["php", "phtml"],
        language: php_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "Python",
        aliases: &["py", "python", "python3"],
        path_aliases: &["py", "pyw"],
        language: python_language,
        metadata: language_metadata(
            LineCommentStyle::Token("#"),
            BlockCommentStyle::Unsupported,
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "Ruby",
        aliases: &["rb", "ruby"],
        path_aliases: &["rb", "gemspec", "gemfile", "rake", "rakefile"],
        language: ruby_language,
        metadata: language_metadata(
            LineCommentStyle::Token("#"),
            BlockCommentStyle::Tokens { open: "=begin", close: "=end" },
            IndentationStrategy::Unsupported,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "Rust",
        aliases: &["rs", "rust"],
        path_aliases: &["rs"],
        language: rust_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "Scala",
        aliases: &["scala"],
        path_aliases: &["sc", "scala"],
        language: scala_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::Unsupported,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "TypeScript",
        aliases: &["ts", "typescript"],
        path_aliases: &["cts", "mts", "ts"],
        language: typescript_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
    LanguageRegistryEntry {
        canonical_name: "TypeScript",
        aliases: &["tsx", "typescriptreact"],
        path_aliases: &["tsx"],
        language: tsx_language,
        metadata: language_metadata(
            LineCommentStyle::Token("//"),
            BlockCommentStyle::Tokens { open: "/*", close: "*/" },
            IndentationStrategy::TreeSitter,
            NO_UNSUPPORTED_TARGETS,
        ),
    },
];

fn normalize_language_key(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn registry_entry_for_name(language_name: &str) -> Option<&'static LanguageRegistryEntry> {
    let key = normalize_language_key(language_name);
    LANGUAGE_REGISTRY.iter().find(|entry| {
        normalize_language_key(entry.canonical_name) == key
            || entry.aliases.iter().any(|alias| normalize_language_key(alias) == key)
    })
}

fn registry_entry_for_path(path: &Path) -> Option<&'static LanguageRegistryEntry> {
    let key = path
        .extension()
        .or_else(|| path.file_name())
        .and_then(|segment| segment.to_str())?
        .to_ascii_lowercase();
    LANGUAGE_REGISTRY
        .iter()
        .find(|entry| entry.path_aliases.iter().any(|alias| alias.eq_ignore_ascii_case(&key)))
}

/// Syntax span encoded relative to one rendered line.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VisibleSyntaxSpan {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) scope: &'static str,
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

// ---------------------------------------------------------------------------
// Language registry
// ---------------------------------------------------------------------------

/// Returns canonical xi language name for a requested alias.
pub fn canonical_language_name(requested: &str) -> Option<&'static str> {
    registry_entry_for_name(requested).map(|entry| entry.canonical_name)
}

/// Returns canonical xi language name resolved from a path or file name.
pub fn language_name_for_path(path: &Path) -> Option<&'static str> {
    registry_entry_for_path(path).map(|entry| entry.canonical_name)
}

#[allow(dead_code)]
pub(crate) fn language_path_aliases(language_name: &str) -> Option<&'static [&'static str]> {
    registry_entry_for_name(language_name).map(|entry| entry.path_aliases)
}

pub(crate) fn language_metadata_for_name(language_name: &str) -> Option<&'static LanguageMetadata> {
    registry_entry_for_name(language_name).map(|entry| &entry.metadata)
}

pub(crate) fn syntax_feature_availability(
    language_name: Option<&str>,
    file_path: Option<&Path>,
    mode: DocumentMode,
) -> SyntaxFeatureAvailability {
    let entry = language_name
        .and_then(registry_entry_for_name)
        .or_else(|| file_path.and_then(registry_entry_for_path));
    let gates = mode.feature_gates();
    let Some(entry) = entry else {
        return SyntaxFeatureAvailability {
            syntax_spans: false,
            semantic_motions: false,
            line_comments: false,
            block_comments: false,
            reindent: false,
        };
    };

    let metadata = entry.metadata;
    let has_semantic_target = [
        SemanticTargetKind::Function,
        SemanticTargetKind::Class,
        SemanticTargetKind::Parameter,
        SemanticTargetKind::Comment,
        SemanticTargetKind::Test,
    ]
    .iter()
    .any(|target| !metadata.unsupported_semantic_targets.contains(target));

    SyntaxFeatureAvailability {
        syntax_spans: gates.syntax,
        semantic_motions: gates.syntax && has_semantic_target,
        line_comments: gates.editing && matches!(metadata.line_comment, LineCommentStyle::Token(_)),
        block_comments: gates.editing
            && matches!(metadata.block_comment, BlockCommentStyle::Tokens { .. }),
        reindent: gates.whole_doc_ops
            && matches!(metadata.indentation, IndentationStrategy::TreeSitter),
    }
}

pub(crate) fn language_supports_semantic_target(
    language_name: &str,
    target: SemanticTargetKind,
) -> bool {
    language_metadata_for_name(language_name)
        .map(|metadata| !metadata.unsupported_semantic_targets.contains(&target))
        .unwrap_or(false)
}

pub(crate) fn resolve_ts_language(
    language_name: Option<&str>,
    file_path: Option<&Path>,
) -> Option<Language> {
    language_name
        .and_then(ts_language_for_name)
        .or_else(|| file_path.and_then(ts_language_for_path))
}

/// Returns tree-sitter `Language` for given xi language name.
pub(crate) fn ts_language_for_name(language_name: &str) -> Option<Language> {
    registry_entry_for_name(language_name).map(|entry| (entry.language)())
}

pub(crate) fn ts_language_for_path(path: &Path) -> Option<Language> {
    registry_entry_for_path(path).map(|entry| (entry.language)())
}

// ---------------------------------------------------------------------------
// VLF visible-range highlighting
// ---------------------------------------------------------------------------

/// Parse only the supplied visible text and return line-relative syntax spans.
///
/// This is intentionally bounded by bytes, wall time, match count, and capture
/// count so VLF highlighting cannot become whole-file work. The input must be a
/// viewport chunk, optionally with small overscan; callers must not pass full
/// VLF file contents.
pub(crate) fn visible_syntax_spans(
    language_name: &str,
    visible_text: &str,
    limits: VisibleSyntaxLimits,
) -> Vec<Vec<VisibleSyntaxSpan>> {
    let line_starts = line_start_offsets(visible_text);
    let segments = line_segments(visible_text, &line_starts);
    chunk_syntax_spans(language_name, visible_text, &segments, limits)
}

pub(crate) fn chunk_syntax_spans(
    language_name: &str,
    chunk_text: &str,
    segments: &[Range<usize>],
    limits: VisibleSyntaxLimits,
) -> Vec<Vec<VisibleSyntaxSpan>> {
    let mut per_segment = vec![Vec::new(); segments.len().max(1)];
    if chunk_text.is_empty()
        || chunk_text.len() > limits.max_bytes
        || limits.timeout.is_zero()
        || limits.max_matches == 0
        || limits.max_captures == 0
    {
        return per_segment;
    }

    let Some(language) = ts_language_for_name(language_name) else {
        return per_segment;
    };

    let started = Instant::now();
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return per_segment;
    }
    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() >= limits.timeout {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = chunk_text.as_bytes();
    let mut read = |offset: usize, _: Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: Some(&mut progress) };
    let Some(tree) = parser.parse_with_options(&mut read, None, Some(options)) else {
        return per_segment;
    };
    if started.elapsed() >= limits.timeout {
        return per_segment;
    }

    let mut state = VisibleSyntaxWalk {
        text: chunk_text,
        segments,
        per_segment: &mut per_segment,
        started,
        limits,
        matches: 0,
        captures: 0,
    };
    state.walk(tree.root_node());
    for spans in &mut per_segment {
        compact_visible_spans(spans);
    }
    per_segment
}

struct VisibleSyntaxWalk<'a> {
    text: &'a str,
    segments: &'a [Range<usize>],
    per_segment: &'a mut [Vec<VisibleSyntaxSpan>],
    started: Instant,
    limits: VisibleSyntaxLimits,
    matches: usize,
    captures: usize,
}

impl VisibleSyntaxWalk<'_> {
    fn exhausted(&self) -> bool {
        self.matches >= self.limits.max_matches
            || self.captures >= self.limits.max_captures
            || self.started.elapsed() >= self.limits.timeout
    }

    fn walk(&mut self, node: Node<'_>) {
        if self.exhausted() {
            return;
        }

        if let Some(scope) = scope_for_node(node, self.text) {
            self.matches += 1;
            self.push_node_spans(node, scope);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child);
            if self.exhausted() {
                break;
            }
        }
    }

    fn push_node_spans(&mut self, node: Node<'_>, scope: &'static str) {
        let start = node.start_byte().min(self.text.len());
        let end = node.end_byte().min(self.text.len());
        if end <= start {
            return;
        }

        for (segment_idx, segment) in self.segments.iter().enumerate() {
            if self.captures >= self.limits.max_captures {
                break;
            }
            let span_start = start.max(segment.start).saturating_sub(segment.start);
            let span_end = end.min(segment.end).saturating_sub(segment.start);
            if span_end > span_start {
                self.per_segment[segment_idx].push(VisibleSyntaxSpan {
                    start_byte: span_start,
                    end_byte: span_end,
                    scope,
                });
                self.captures += 1;
            }
        }
    }
}

fn line_segments(text: &str, line_starts: &[usize]) -> Vec<Range<usize>> {
    line_starts
        .iter()
        .enumerate()
        .map(|(line, &line_start)| {
            let line_end = line_starts
                .get(line + 1)
                .copied()
                .unwrap_or(text.len())
                .saturating_sub(usize::from(line + 1 < line_starts.len()));
            line_start..line_end
        })
        .collect()
}

fn line_start_offsets(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(idx, _)| idx + 1));
    starts
}

fn scope_for_node(node: Node<'_>, text: &str) -> Option<&'static str> {
    match node.kind() {
        "line_comment" | "block_comment" | "comment" => Some("comment.line"),
        "string_literal" | "raw_string_literal" | "interpreted_string_literal" | "string" => {
            Some("string.quoted")
        }
        "integer_literal" | "float_literal" => Some("constant.numeric.decimal"),
        "char_literal" | "boolean_literal" => Some("constant.language"),
        "primitive_type" | "type_identifier" => Some("entity.name.type"),
        kind if is_keyword_node(kind, node, text) => Some("keyword.control"),
        _ => None,
    }
}

fn compact_visible_spans(spans: &mut Vec<VisibleSyntaxSpan>) {
    spans.sort_by_key(|span| (span.start_byte, span.end_byte));
    let mut compacted: Vec<VisibleSyntaxSpan> = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        if compacted.last().is_some_and(|last| span.start_byte < last.end_byte) {
            continue;
        }
        compacted.push(span);
    }
    *spans = compacted;
}

fn parse_tree_with_timeout(language_name: &str, text: &str, timeout: Duration) -> Option<Tree> {
    if timeout.is_zero() {
        return None;
    }

    let language = ts_language_for_name(language_name)?;
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

fn is_keyword_node(kind: &str, node: Node<'_>, text: &str) -> bool {
    if node.is_named() {
        return false;
    }
    matches!(
        kind,
        "fn" | "let"
            | "mut"
            | "pub"
            | "impl"
            | "struct"
            | "enum"
            | "trait"
            | "mod"
            | "use"
            | "match"
            | "if"
            | "else"
            | "for"
            | "while"
            | "loop"
            | "return"
            | "async"
            | "await"
            | "move"
            | "const"
            | "static"
            | "where"
            | "in"
            | "crate"
            | "self"
            | "super"
            | "def"
            | "class"
            | "import"
            | "from"
            | "elif"
            | "try"
            | "except"
            | "finally"
            | "with"
            | "as"
            | "pass"
            | "yield"
    ) || node
        .utf8_text(text.as_bytes())
        .is_ok_and(|token| matches!(token, "True" | "False" | "None"))
}

// ---------------------------------------------------------------------------
// Fold detection
// ---------------------------------------------------------------------------

/// A line range that can be folded: the header line stays visible; lines
/// `body_start..=body_end` are hidden when the fold is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoldRange {
    /// The line containing the fold opener (e.g. the `fn` declaration line).
    pub(crate) header_line: usize,
    /// First line of the foldable body (inclusive).
    pub(crate) body_start: usize,
    /// Last line of the foldable body (inclusive).
    pub(crate) body_end: usize,
}

/// Returns all foldable regions in `tree` that span more than one line.
///
/// Foldable node kinds are chosen to cover blocks, functions, structs, and
/// similar multi-line constructs across supported languages.
pub(crate) fn fold_ranges(tree: &Tree) -> Vec<FoldRange> {
    /// Node kinds that make good fold points across the supported grammars.
    const FOLDABLE_KINDS: &[&str] = &[
        // Rust
        "function_item",
        "impl_item",
        "struct_item",
        "enum_item",
        "trait_item",
        "mod_item",
        "block",
        "match_expression",
        "if_expression",
        "while_expression",
        "for_expression",
        "macro_definition",
        // Python
        "function_definition",
        "class_definition",
        "if_statement",
        "while_statement",
        "for_statement",
        "with_statement",
        "try_statement",
        "decorated_definition",
        "block",
    ];

    let mut folds = Vec::new();
    collect_folds(tree.root_node(), FOLDABLE_KINDS, &mut folds);
    folds
}

pub(crate) fn fold_ranges_for_text(
    language_name: Option<&str>,
    file_path: Option<&Path>,
    text: &str,
    timeout: Duration,
) -> Vec<FoldRange> {
    if timeout.is_zero() || text.is_empty() {
        return Vec::new();
    }

    let Some(language) = resolve_ts_language(language_name, file_path) else {
        return Vec::new();
    };
    let started = Instant::now();
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }

    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() >= timeout {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = text.as_bytes();
    let mut read = |offset: usize, _: Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: Some(&mut progress) };
    let Some(tree) = parser.parse_with_options(&mut read, None, Some(options)) else {
        return Vec::new();
    };
    if started.elapsed() >= timeout {
        return Vec::new();
    }

    fold_ranges(&tree)
}

#[cfg(test)]
fn byte_to_point(text: &str, byte_offset: usize) -> Point {
    let safe = byte_offset.min(text.len());
    let prefix = &text[..safe];
    let row = prefix.bytes().filter(|&b| b == b'\n').count();
    let col = prefix.rfind('\n').map(|idx| safe - idx - 1).unwrap_or(safe);
    Point::new(row, col)
}

fn collect_folds(node: Node<'_>, kinds: &[&str], out: &mut Vec<FoldRange>) {
    let start_line = node.start_position().row;
    let end_line = node.end_position().row;

    if kinds.contains(&node.kind()) && end_line > start_line {
        out.push(FoldRange {
            header_line: start_line,
            body_start: start_line + 1,
            body_end: end_line,
        });
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect_folds(child, kinds, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Indentation
// ---------------------------------------------------------------------------

pub(crate) fn indentation_levels_for_text(
    language_name: &str,
    text: &str,
    max_line: usize,
) -> Option<Vec<usize>> {
    indentation_levels_for_text_with_timeout(
        language_name,
        text,
        max_line,
        DEFAULT_REINDENT_PARSE_TIMEOUT,
    )
}

fn indentation_levels_for_text_with_timeout(
    language_name: &str,
    text: &str,
    max_line: usize,
    timeout: Duration,
) -> Option<Vec<usize>> {
    let metadata = language_metadata_for_name(language_name)?;
    if metadata.indentation != IndentationStrategy::TreeSitter {
        return None;
    }

    if matches!(canonical_language_name(language_name), Some("Python")) {
        return Some(python_indentation_levels(text, max_line));
    }

    let tree = parse_tree_with_timeout(language_name, text, timeout)?;
    let line_starts = line_start_offsets(text);
    let line_segments = line_segments(text, &line_starts);
    let mut levels = Vec::with_capacity(max_line + 1);

    for line in 0..=max_line {
        let line_text = line_segments.get(line).map(|segment| &text[segment.clone()]).unwrap_or("");
        let mut level = indent_level_at_line(&tree, line);
        if line_requires_dedent(language_name, line_text) {
            level = level.saturating_sub(1);
        }
        levels.push(level);
    }

    Some(levels)
}

fn python_indentation_levels(text: &str, max_line: usize) -> Vec<usize> {
    let line_starts = line_start_offsets(text);
    let line_segments = line_segments(text, &line_starts);
    let mut levels = Vec::with_capacity(max_line + 1);
    let mut current_level = 0usize;

    for line in 0..=max_line {
        let line_text = line_segments.get(line).map(|segment| &text[segment.clone()]).unwrap_or("");
        let trimmed = line_text.trim_start();
        let is_dedent_clause = trimmed.starts_with("elif ")
            || trimmed.starts_with("else:")
            || trimmed.starts_with("except")
            || trimmed.starts_with("finally:");

        if is_dedent_clause {
            current_level = current_level.saturating_sub(1);
        }

        let line_level = current_level;
        levels.push(line_level);

        if !trimmed.is_empty() && trimmed.ends_with(':') {
            current_level = line_level + 1;
        }
    }

    levels
}

/// Returns brace/block-style indent depth for `line` by walking the syntax
/// tree to find enclosing block containers.
///
/// Prefer [`indentation_levels_for_text`] for editor-facing reindent because it
/// also applies language-specific rules such as Python clause dedents.
///
/// The result is based on AST structure, not bracket counting, so it is
/// correct even in the presence of nested string literals, raw strings,
/// macros, and other constructs that confuse heuristic approaches.
pub(crate) fn indent_level_at_line(tree: &Tree, line: usize) -> usize {
    // Seek to any character on the target line; column 0 is fine since we
    // only care about the enclosing block count.
    let point = Point::new(line, 0);
    let node = tree.root_node().descendant_for_point_range(point, point);

    match node {
        None => 0,
        Some(n) => block_depth(n, line),
    }
}

/// Counts the number of block-introducing ancestors of `node` that **start
/// before** `target_line`.
///
/// A block that opens on the target line does not yet indent that line; the
/// opener is written at the parent's indent level.
fn block_depth(node: Node<'_>, target_line: usize) -> usize {
    /// Node kinds that introduce one level of indentation.
    const BLOCK_KINDS: &[&str] = &[
        "block",
        "statement_block",
        "compound_statement",
        "class_body",
        "declaration_list",
        "field_declaration_list",
        "enum_variant_list",
    ];

    let mut depth = 0usize;
    let mut cursor = node;

    loop {
        // Only count this node if it opened before the target line (meaning
        // the target line is inside the body, not on the opener itself).
        if BLOCK_KINDS.contains(&cursor.kind()) && cursor.start_position().row < target_line {
            depth += 1;
        }
        match cursor.parent() {
            Some(p) => cursor = p,
            None => break,
        }
    }

    depth
}

fn line_requires_dedent(language_name: &str, line_text: &str) -> bool {
    let trimmed = line_text.trim_start();
    if trimmed.starts_with('}') || trimmed.starts_with(']') || trimmed.starts_with(')') {
        return true;
    }

    matches!(canonical_language_name(language_name), Some("Python"))
        && (trimmed.starts_with("elif ")
            || trimmed.starts_with("else:")
            || trimmed.starts_with("except")
            || trimmed.starts_with("finally:"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_rust(src: &str) -> Tree {
        parse_tree_with_timeout("Rust", src, Duration::from_secs(1)).expect("parse failed")
    }

    fn parse_python(src: &str) -> Tree {
        parse_tree_with_timeout("Python", src, Duration::from_secs(1)).expect("parse failed")
    }

    // --------------------------------------------------
    // Language registry
    // --------------------------------------------------

    #[test]
    fn test_ts_language_registry_covers_bundled_grammars() {
        for language in [
            "Bash",
            "C",
            "C#",
            "C++",
            "CSS",
            "Elixir",
            "Go",
            "Haskell",
            "HTML",
            "Java",
            "JavaScript",
            "JSON",
            "PHP",
            "Python",
            "Ruby",
            "Rust",
            "Scala",
            "TypeScript",
        ] {
            assert!(ts_language_for_name(language).is_some(), "missing grammar for {language}");
        }
    }

    #[test]
    fn test_ts_language_registry_normalizes_aliases() {
        for (alias, canonical) in [
            ("rust", "Rust"),
            ("python3", "Python"),
            ("sh", "Bash"),
            ("cpp", "C++"),
            ("csharp", "C#"),
            ("js", "JavaScript"),
            ("tsx", "TypeScript"),
        ] {
            assert_eq!(canonical_language_name(alias), Some(canonical));
            assert!(ts_language_for_name(alias).is_some(), "missing alias support for {alias}");
        }
    }

    #[test]
    fn test_ts_language_registry_resolves_paths() {
        for (path, canonical) in [
            ("build.sh", "Bash"),
            ("main.c", "C"),
            ("Program.cs", "C#"),
            ("main.cpp", "C++"),
            ("app.css", "CSS"),
            ("mix.exs", "Elixir"),
            ("main.go", "Go"),
            ("Main.hs", "Haskell"),
            ("index.html", "HTML"),
            ("Main.java", "Java"),
            ("app.jsx", "JavaScript"),
            ("data.json", "JSON"),
            ("index.php", "PHP"),
            ("tool.py", "Python"),
            ("Gemfile", "Ruby"),
            ("lib.rs", "Rust"),
            ("build.sc", "Scala"),
            ("view.tsx", "TypeScript"),
        ] {
            assert_eq!(language_name_for_path(Path::new(path)), Some(canonical));
            assert!(
                ts_language_for_path(Path::new(path)).is_some(),
                "missing path support for {path}"
            );
        }
    }

    #[test]
    fn test_ts_language_resolution_prefers_explicit_name_then_path() {
        assert_eq!(canonical_language_name("Python 3"), Some("Python"));
        assert_eq!(language_name_for_path(Path::new("main.rs")), Some("Rust"));
        assert!(resolve_ts_language(Some("Plain Text"), Some(Path::new("main.rs"))).is_some());
    }

    #[test]
    fn test_ts_language_unknown() {
        assert!(ts_language_for_name("COBOL").is_none());
    }

    #[test]
    fn syntax_feature_availability_uses_language_and_document_mode() {
        let normal = syntax_feature_availability(Some("Rust"), None, DocumentMode::Normal);
        assert!(normal.syntax_spans);
        assert!(normal.semantic_motions);
        assert!(normal.line_comments);
        assert!(normal.block_comments);
        assert!(normal.reindent);

        let vlf = syntax_feature_availability(Some("Rust"), None, DocumentMode::Vlf);
        assert!(vlf.syntax_spans);
        assert!(vlf.semantic_motions);
        assert!(!vlf.line_comments);
        assert!(!vlf.block_comments);
        assert!(!vlf.reindent);

        let constrained =
            syntax_feature_availability(Some("Rust"), None, DocumentMode::ConstrainedNormal);
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
        let src = "fn main() {\n    let answer = 42;\n}\n";
        let spans = visible_syntax_spans("Rust", src, VisibleSyntaxLimits::default());

        assert_eq!(spans.len(), 4);
        assert!(spans[0].iter().any(|span| span.scope == "keyword.control"));
        assert!(spans[1].iter().any(|span| span.scope == "constant.numeric.decimal"));
        assert!(spans.iter().flatten().all(|span| span.end_byte > span.start_byte));
    }

    #[test]
    fn visible_syntax_spans_obeys_byte_and_capture_limits() {
        let src = "let a = 1;\nlet b = 2;\n";
        let too_small = VisibleSyntaxLimits { max_bytes: 4, ..VisibleSyntaxLimits::default() };
        assert!(visible_syntax_spans("Rust", src, too_small).iter().all(Vec::is_empty));

        let limited = VisibleSyntaxLimits {
            max_captures: 1,
            max_matches: 1,
            ..VisibleSyntaxLimits::default()
        };
        let capture_count =
            visible_syntax_spans("Rust", src, limited).iter().map(Vec::len).sum::<usize>();
        assert!(capture_count <= 1);
    }

    #[test]
    fn indentation_levels_returns_none_when_parse_budget_is_zero() {
        let src = "fn main() {\nlet x = 1;\n}\n";
        assert!(indentation_levels_for_text_with_timeout("Rust", src, 2, Duration::ZERO).is_none());
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
        let levels = indentation_levels_for_text("Python", src, 1).unwrap();
        assert_eq!(levels, vec![0, 1]);
    }

    #[test]
    fn indentation_levels_dedent_rust_closing_brace() {
        let src = "fn main() {\nlet x = 1;\n}\n";
        let levels = indentation_levels_for_text("Rust", src, 2).unwrap();
        assert_eq!(levels, vec![0, 1, 0]);
    }

    #[test]
    fn indentation_levels_dedent_python_else_clause() {
        let src = "if ready:\nprint(\"yes\")\nelse:\nprint(\"no\")\n";
        let levels = indentation_levels_for_text("Python", src, 3).unwrap();
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
