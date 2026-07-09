use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use tree_sitter::QueryError;
use tree_sitter_loader::LoaderError;

use super::types::RuntimeQueryKind;

#[derive(Debug)]
pub enum RuntimeLoaderError {
    Loader(LoaderError),
    RuntimeDisabled {
        reason: &'static str,
    },
    InvalidConfig {
        message: String,
    },
    AmbiguousAlias {
        alias: String,
        first_language: String,
        second_language: String,
    },
    AmbiguousFileType {
        file_type: String,
        first_language: String,
        second_language: String,
    },
    UnknownLanguage {
        requested: String,
    },
    MissingGrammar {
        language_id: String,
        path: Option<PathBuf>,
    },
    GrammarOutsideRuntimeRoot {
        path: PathBuf,
        allowed_roots: Vec<PathBuf>,
    },
    QueryIo {
        kind: RuntimeQueryKind,
        path: PathBuf,
        error: io::Error,
    },
    QueryCompile {
        kind: RuntimeQueryKind,
        file: Option<PathBuf>,
        error: QueryError,
    },
    InvalidQueryCapture {
        kind: RuntimeQueryKind,
        file: Option<PathBuf>,
        capture: String,
        allowed: Vec<&'static str>,
    },
    QueryInheritanceCycle {
        kind: RuntimeQueryKind,
        chain: Vec<String>,
    },
    UnknownInheritedLanguage {
        kind: RuntimeQueryKind,
        language: String,
    },
}

impl fmt::Display for RuntimeLoaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loader(error) => write!(f, "tree-sitter loader error: {error}"),
            Self::RuntimeDisabled { reason } => {
                write!(f, "runtime tree-sitter disabled: {reason}")
            }
            Self::InvalidConfig { message } => write!(f, "invalid runtime config: {message}"),
            Self::AmbiguousAlias { alias, first_language, second_language } => write!(
                f,
                "alias `{alias}` is claimed by both `{first_language}` and `{second_language}`"
            ),
            Self::AmbiguousFileType { file_type, first_language, second_language } => write!(
                f,
                "file type `{file_type}` is claimed by both `{first_language}` and `{second_language}` without explicit precedence"
            ),
            Self::UnknownLanguage { requested } => {
                write!(f, "unknown runtime language `{requested}`")
            }
            Self::MissingGrammar { language_id, path } => match path {
                Some(path) => {
                    write!(f, "missing runtime grammar for `{language_id}` at {}", path.display())
                }
                None => write!(f, "missing runtime grammar for `{language_id}`"),
            },
            Self::GrammarOutsideRuntimeRoot { path, allowed_roots } => write!(
                f,
                "grammar path {} is outside known runtime roots {:?}",
                path.display(),
                allowed_roots
            ),
            Self::QueryIo { kind, path, error } => {
                write!(f, "failed reading {} for {:?}: {error}", path.display(), kind)
            }
            Self::QueryCompile { kind, file, error } => match file {
                Some(file) => {
                    write!(f, "failed compiling {:?} query {}: {error}", kind, file.display())
                }
                None => write!(f, "failed compiling {:?} query: {error}", kind),
            },
            Self::InvalidQueryCapture { kind, file, capture, allowed } => match file {
                Some(file) => write!(
                    f,
                    "invalid capture `@{capture}` in {:?} query {} (allowed: {})",
                    kind,
                    file.display(),
                    allowed.join(", ")
                ),
                None => write!(
                    f,
                    "invalid capture `@{capture}` in {:?} query (allowed: {})",
                    kind,
                    allowed.join(", ")
                ),
            },
            Self::QueryInheritanceCycle { kind, chain } => {
                write!(f, "query inheritance cycle for {:?}: {}", kind, chain.join(" -> "))
            }
            Self::UnknownInheritedLanguage { kind, language } => {
                write!(f, "unknown inherited language `{language}` for {:?}", kind)
            }
        }
    }
}

impl Error for RuntimeLoaderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Loader(error) => Some(error),
            Self::QueryIo { error, .. } => Some(error),
            Self::QueryCompile { error, .. } => Some(error),
            Self::InvalidConfig { .. }
            | Self::InvalidQueryCapture { .. }
            | Self::RuntimeDisabled { .. }
            | Self::AmbiguousAlias { .. }
            | Self::AmbiguousFileType { .. }
            | Self::UnknownLanguage { .. }
            | Self::MissingGrammar { .. }
            | Self::GrammarOutsideRuntimeRoot { .. }
            | Self::QueryInheritanceCycle { .. }
            | Self::UnknownInheritedLanguage { .. } => None,
        }
    }
}

impl From<LoaderError> for RuntimeLoaderError {
    fn from(value: LoaderError) -> Self {
        Self::Loader(value)
    }
}
