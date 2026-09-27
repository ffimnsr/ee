// Copyright 2017 The xi-editor Authors.
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

//! Very basic syntax detection.

use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::Table;

/// The canonical identifier for a particular `LanguageDefinition`.
#[derive(
    Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord, JsonSchema,
)]
pub struct LanguageId(#[schemars(with = "String")] Arc<str>);

/// Describes a `LanguageDefinition`. Although these are provided by plugins,
/// they are a fundamental concept in core, used to determine things like
/// plugin activations and active user config tables.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LanguageDefinition {
    pub name: LanguageId,
    pub extensions: Vec<String>,
    /// Exact basename matches (e.g. `Makefile`), matched before extensions.
    #[serde(default)]
    pub filenames: Vec<String>,
    /// Glob patterns matched against the full path (e.g. `**/Dockerfile*`),
    /// matched before exact filenames.
    #[serde(default)]
    pub globs: Vec<String>,
    pub first_line_match: Option<String>,
    pub scope: String,
    #[serde(skip)]
    #[schemars(skip)]
    pub default_config: Option<Table>,
}

/// A repository of all loaded `LanguageDefinition`s.
#[derive(Debug, Default)]
pub struct Languages {
    // NOTE: BTreeMap is used for sorting the languages by name alphabetically
    named: BTreeMap<LanguageId, Arc<LanguageDefinition>>,
    extensions: HashMap<String, Arc<LanguageDefinition>>,
    filenames: HashMap<String, Arc<LanguageDefinition>>,
    globs: Vec<(String, Arc<LanguageDefinition>)>,
}

impl Languages {
    pub fn new(language_defs: &[LanguageDefinition]) -> Self {
        let mut named = BTreeMap::new();
        let mut extensions = HashMap::new();
        let mut filenames = HashMap::new();
        let mut globs = Vec::new();
        for lang in language_defs.iter() {
            let lang_arc = Arc::new(lang.clone());
            named.insert(lang.name.clone(), lang_arc.clone());
            for ext in &lang.extensions {
                extensions.insert(ext.clone(), lang_arc.clone());
            }
            for filename in &lang.filenames {
                filenames.insert(filename.clone(), lang_arc.clone());
            }
            for pattern in &lang.globs {
                globs.push((pattern.clone(), lang_arc.clone()));
            }
        }
        Languages { named, extensions, filenames, globs }
    }

    pub fn language_for_path(&self, path: &Path) -> Option<Arc<LanguageDefinition>> {
        let file_name = path.file_name().and_then(|name| name.to_str());
        // Precedence: glob > exact basename > extension.
        if let Some(file_name) = file_name {
            if let Some(lang) = self
                .globs
                .iter()
                .find_map(|(pattern, lang)| glob_matches(pattern, path).then(|| lang.clone()))
            {
                return Some(lang);
            }
            if let Some(lang) = self.filenames.get(file_name) {
                return Some(lang.clone());
            }
        }
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| self.extensions.get(ext))
            .map(Arc::clone)
    }

    pub fn language_for_name<S>(&self, name: S) -> Option<Arc<LanguageDefinition>>
    where
        S: AsRef<str>,
    {
        self.named.get(name.as_ref()).map(Arc::clone)
    }

    /// Returns a Vec of any `LanguageDefinition`s which exist
    /// in `self` but not `other`.
    pub fn difference(&self, other: &Languages) -> Vec<Arc<LanguageDefinition>> {
        self.named
            .iter()
            .filter(|(k, _)| !other.named.contains_key(*k))
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<LanguageDefinition>> {
        self.named.values()
    }
}

impl AsRef<str> for LanguageId {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

// let's us use &str to query a HashMap with `LanguageId` keys
impl Borrow<str> for LanguageId {
    fn borrow(&self) -> &str {
        self.0.as_ref()
    }
}

impl<'a> From<&'a str> for LanguageId {
    fn from(src: &'a str) -> LanguageId {
        LanguageId(Arc::from(src))
    }
}

/// Match a glob pattern against a file path.
///
/// Supported syntax:
/// - `*` matches any run of characters except `/`
/// - `**` matches any run of characters including `/`
/// - everything else matches literally
///
/// Patterns anchor at the path start, so `**/Dockerfile*` matches both
/// `Dockerfile` (zero leading segments) and `ci/Dockerfile.dev`.
pub(crate) fn glob_matches(pattern: &str, path: &Path) -> bool {
    let pattern = pattern.replace('\\', "/");
    let text = path.to_string_lossy().replace('\\', "/");
    wildcard_match(pattern.as_bytes(), text.as_bytes())
}

fn wildcard_match(pattern: &[u8], text: &[u8]) -> bool {
    let Some(first) = pattern.first() else {
        return text.is_empty();
    };
    match first {
        b'*' => {
            let rest = &pattern[1..];
            if rest.first() == Some(&b'*') {
                let rest = &rest[1..];
                if rest.first() == Some(&b'/') {
                    // `**/`: zero or more leading directory segments.
                    let after = &rest[1..];
                    wildcard_match(after, text)
                        || text
                            .iter()
                            .position(|byte| *byte == b'/')
                            .is_some_and(|split| wildcard_match(pattern, &text[split + 1..]))
                } else {
                    // `**`: zero or more characters, `/` included.
                    (0..=text.len()).any(|split| wildcard_match(rest, &text[split..]))
                }
            } else {
                // `*`: zero or more characters, `/` excluded.
                let max = text.iter().position(|byte| *byte == b'/').unwrap_or(text.len());
                (0..=max).any(|split| wildcard_match(rest, &text[split..]))
            }
        }
        byte => text.first() == Some(byte) && wildcard_match(&pattern[1..], &text[1..]),
    }
}

// for testing
#[cfg(test)]
impl LanguageDefinition {
    pub(crate) fn simple(name: &str, exts: &[&str], scope: &str, config: Option<Table>) -> Self {
        LanguageDefinition {
            name: name.into(),
            extensions: exts.iter().map(|s| (*s).into()).collect(),
            filenames: Vec::new(),
            globs: Vec::new(),
            first_line_match: None,
            scope: scope.into(),
            default_config: config,
        }
    }
}

#[cfg(test)]
mod glob_tests {
    use std::path::Path;

    use super::glob_matches;

    fn matches(pattern: &str, path: &str) -> bool {
        glob_matches(pattern, Path::new(path))
    }

    #[test]
    fn star_does_not_cross_directories() {
        assert!(matches("*.mk", "rules.mk"));
        assert!(!matches("*.mk", "sub/rules.mk"));
        assert!(matches("Makefile*", "Makefile"));
        assert!(matches("Makefile*", "Makefile.am"));
        assert!(!matches("Makefile*", "src/Makefile"));
    }

    #[test]
    fn double_star_crosses_directories_and_matches_zero_segments() {
        assert!(matches("**/Dockerfile*", "Dockerfile"));
        assert!(matches("**/Dockerfile*", "Dockerfile.dev"));
        assert!(matches("**/Dockerfile*", "ci/images/Dockerfile.dev"));
        assert!(!matches("**/Dockerfile*", "ci/images/Containerfile"));
        assert!(matches("**/*.mk", "sub/rules.mk"));
        assert!(matches("**/*.mk", "rules.mk"));
    }

    #[test]
    fn literal_and_exact_matches() {
        assert!(matches("Justfile", "Justfile"));
        assert!(!matches("Justfile", "build/Justfile"));
        assert!(matches("Makefile", "Makefile"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_id_preserves_string_wire_format() {
        let id = LanguageId::from("rust");
        let cloned = id.clone();

        assert!(Arc::ptr_eq(&id.0, &cloned.0));
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""rust""#);
        assert_eq!(serde_json::from_str::<LanguageId>(r#""rust""#).unwrap(), id);
    }

    #[test]
    pub fn language_for_path() {
        let ld_rust = LanguageDefinition {
            name: LanguageId::from("rust"),
            extensions: vec![String::from("rs")],
            filenames: Vec::new(),
            globs: Vec::new(),
            scope: String::from("source.rust"),
            first_line_match: None,
            default_config: None,
        };
        let ld_commit_msg = LanguageDefinition {
            name: LanguageId::from("Git Commit"),
            filenames: vec![
                String::from("COMMIT_EDITMSG"),
                String::from("MERGE_MSG"),
                String::from("TAG_EDITMSG"),
            ],
            extensions: Vec::new(),
            globs: Vec::new(),
            scope: String::from("text.git.commit"),
            first_line_match: None,
            default_config: None,
        };
        let languages = Languages::new(&[ld_rust.clone(), ld_commit_msg.clone()]);

        assert_eq!(
            ld_rust.name,
            languages.language_for_path(Path::new("/path/test.rs")).unwrap().name
        );
        assert_eq!(
            ld_commit_msg.name,
            languages.language_for_path(Path::new("/path/COMMIT_EDITMSG")).unwrap().name
        );
        assert_eq!(
            ld_commit_msg.name,
            languages.language_for_path(Path::new("/path/MERGE_MSG")).unwrap().name
        );
        assert_eq!(
            ld_commit_msg.name,
            languages.language_for_path(Path::new("/path/TAG_EDITMSG")).unwrap().name
        );
    }
}
