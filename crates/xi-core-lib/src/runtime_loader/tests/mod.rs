//! Runtime-loader tests: shared harness.
use super::*;

use std::collections::BTreeSet;
use std::env;
use std::fs;

use ee_ts_test_grammars as test_grammars;

use crate::syntax::{LanguageDefinition, Languages};
use crate::tree_sitter_support::{
    BlockCommentStyle, IndentationStrategy, LanguageMetadata, LineCommentStyle,
};
use std::iter;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use tempfile::TempDir;

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|error| error.into_inner())
}

fn language_definition(name: &str, extensions: &[&str]) -> LanguageDefinition {
    LanguageDefinition {
        name: name.into(),
        extensions: extensions.iter().map(|value| (*value).to_string()).collect(),
        first_line_match: None,
        scope: format!("source.{}", name.to_ascii_lowercase()),
        default_config: None,
    }
}

fn runtime_grammar_config(library: &str, symbol: &str, version: &str) -> RuntimeGrammarConfig {
    RuntimeGrammarConfig {
        library: Some(library.to_string()),
        symbol: Some(symbol.to_string()),
        source: Some(RuntimeGrammarSource::Crate(RuntimeGrammarCrateSource {
            name: library.to_string(),
            version: version.to_string(),
        })),
    }
}

fn runtime_language_override(library: &str, symbol: &str) -> RuntimeLanguageConfig {
    RuntimeLanguageConfig {
        grammar: Some(runtime_grammar_config(library, symbol, "0.0.0")),
        ..RuntimeLanguageConfig::default()
    }
}

fn write_until_modified(path: &Path, contents: impl Into<Vec<u8>>) {
    let mut contents = contents.into();
    let original = metadata_modified_time(path);
    for marker in 0u8..=32 {
        fs::write(path, &contents).unwrap();
        if metadata_modified_time(path) != original {
            return;
        }
        contents.extend(iter::once(marker));
    }
    panic!("mtime did not change for {}", path.display());
}

mod assets_tests;
mod cache_tests;
mod fetch_tests;
mod git_tests;
mod health_tests;
mod indent_tests;
mod inheritance_tests;
mod layers_tests;
mod query_tests;
mod roots_tests;
mod source_tests;
