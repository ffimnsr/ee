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

//! Keeping track of available plugins.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::{error, info};
use serde::Deserialize;

use super::{PluginDescription, PluginName};
use crate::config::table_from_toml_str;
use crate::syntax::Languages;

const SUPPORTED_MANIFEST_VERSION: u32 = 1;

/// A catalog of all available plugins.
#[derive(Debug, Clone, Default)]
pub struct PluginCatalog {
    items: HashMap<PluginName, Arc<PluginDescription>>,
    locations: HashMap<PathBuf, Arc<PluginDescription>>,
}

/// Errors that can occur while trying to load a plugin.
#[derive(Debug)]
pub enum PluginLoadError {
    Io(io::Error),
    /// Malformed manifest
    Parse(toml::de::Error),
    UnsupportedManifestVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    DuplicatePluginName {
        name: PluginName,
        first_path: PathBuf,
        second_path: PathBuf,
    },
}

impl fmt::Display for PluginLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginLoadError::Io(err) => write!(f, "{err}"),
            PluginLoadError::Parse(err) => write!(f, "{err}"),
            PluginLoadError::UnsupportedManifestVersion { path, found, supported } => {
                write!(
                    f,
                    "unsupported manifest_version {found} in {} (supported: {supported})",
                    path.display()
                )
            }
            PluginLoadError::DuplicatePluginName { name, first_path, second_path } => write!(
                f,
                "duplicate plugin name {name:?} in {} and {}",
                first_path.display(),
                second_path.display()
            ),
        }
    }
}

#[derive(Deserialize)]
struct PluginManifest {
    manifest_version: u32,
    #[serde(flatten)]
    plugin: PluginDescription,
}

#[allow(dead_code)]
impl<'a> PluginCatalog {
    /// Loads any plugins discovered in these paths, replacing any existing
    /// plugins.
    pub fn reload_from_paths(&mut self, paths: &[PathBuf]) -> Vec<PluginLoadError> {
        self.items.clear();
        self.locations.clear();
        self.load_from_paths(paths)
    }

    /// Loads plugins from paths and adds them to existing plugins.
    pub fn load_from_paths(&mut self, paths: &[PathBuf]) -> Vec<PluginLoadError> {
        let all_manifests = find_all_manifests(paths);
        let mut errors = Vec::new();
        for manifest_path in &all_manifests {
            match load_manifest(manifest_path) {
                Err(e) => errors.push(e),
                Ok(manifest) => {
                    let manifest_path = canonicalize_for_lookup(manifest_path);
                    if let Some(existing_path) = self.manifest_path_for_name(&manifest.name)
                        && existing_path != manifest_path
                    {
                        errors.push(PluginLoadError::DuplicatePluginName {
                            name: manifest.name.clone(),
                            first_path: existing_path,
                            second_path: manifest_path,
                        });
                        continue;
                    }

                    info!("loaded {}", manifest.name);
                    let manifest = Arc::new(manifest);
                    if let Some(previous) =
                        self.locations.insert(manifest_path.clone(), manifest.clone())
                    {
                        self.items.remove(&previous.name);
                    }
                    self.items.insert(manifest.name.clone(), manifest.clone());
                }
            }
        }
        errors
    }

    pub fn make_languages_map(&self) -> Languages {
        let all_langs =
            self.items.values().flat_map(|plug| plug.languages.iter().cloned()).collect::<Vec<_>>();
        Languages::new(all_langs.as_slice())
    }

    /// Returns an iterator over all plugins in the catalog, in arbitrary order.
    pub fn iter(&'a self) -> impl Iterator<Item = Arc<PluginDescription>> + 'a {
        self.items.values().cloned()
    }

    /// Returns an iterator over all plugin names in the catalog,
    /// in arbitrary order.
    pub fn iter_names(&'a self) -> impl Iterator<Item = &'a PluginName> {
        self.items.keys()
    }

    /// Returns the plugin located at the provided file path.
    pub fn get_from_path(&self, path: &Path) -> Option<Arc<PluginDescription>> {
        let path = canonicalize_for_lookup(path);
        self.locations.iter().find_map(|(manifest_path, plugin)| {
            let manifest_dir = manifest_path.parent()?;
            let matches_manifest = path == *manifest_path
                || path.starts_with(manifest_dir)
                || manifest_path.starts_with(&path);
            let matches_exec = path == plugin.exec_path
                || path.starts_with(&plugin.exec_path)
                || plugin.exec_path.starts_with(&path);
            (matches_manifest || matches_exec).then(|| Arc::clone(plugin))
        })
    }

    /// Returns a reference to the named plugin if it exists in the catalog.
    pub fn get_named(&self, plugin_name: &str) -> Option<Arc<PluginDescription>> {
        self.items.get(plugin_name).map(Arc::clone)
    }

    /// Removes the named plugin.
    pub fn remove_named(&mut self, plugin_name: &str) {
        self.items.remove(plugin_name);
        self.locations.retain(|_, plugin| plugin.name != plugin_name);
    }

    fn manifest_path_for_name(&self, plugin_name: &str) -> Option<PathBuf> {
        self.locations
            .iter()
            .find_map(|(path, plugin)| (plugin.name == plugin_name).then(|| path.clone()))
    }
}

fn find_all_manifests(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut manifest_paths = Vec::new();
    for path in paths.iter() {
        if path.file_name().is_some_and(|name| name == "manifest.toml") {
            manifest_paths.push(path.clone());
            continue;
        }

        let manif_path = path.join("manifest.toml");
        if manif_path.exists() {
            manifest_paths.push(manif_path);
            continue;
        }

        let result = path.read_dir().map(|dir| {
            dir.flat_map(|item| item.map(|p| p.path()).ok())
                .map(|dir| dir.join("manifest.toml"))
                .filter(|f| f.exists())
                .for_each(|f| manifest_paths.push(f))
        });
        if let Err(e) = result {
            error!("error reading plugin path {:?}, {:?}", path, e);
        }
    }
    manifest_paths
}

fn load_manifest(path: &Path) -> Result<PluginDescription, PluginLoadError> {
    let mut file = fs::File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let manifest: PluginManifest = toml::from_str(&contents)?;
    if manifest.manifest_version != SUPPORTED_MANIFEST_VERSION {
        return Err(PluginLoadError::UnsupportedManifestVersion {
            path: path.to_path_buf(),
            found: manifest.manifest_version,
            supported: SUPPORTED_MANIFEST_VERSION,
        });
    }

    let mut manifest = manifest.plugin;
    if manifest.exec_path.is_relative() {
        manifest.exec_path = path.parent().unwrap().join(&manifest.exec_path).canonicalize()?;
    }

    for lang in &mut manifest.languages {
        let lang_config_path =
            path.parent().unwrap().join(lang.name.as_ref()).with_extension("toml");
        if !lang_config_path.exists() {
            continue;
        }
        let lang_defaults = fs::read_to_string(&lang_config_path)?;
        let lang_defaults = table_from_toml_str(&lang_defaults)?;
        lang.default_config = Some(lang_defaults);
    }
    Ok(manifest)
}

fn canonicalize_for_lookup(path: &Path) -> PathBuf {
    if path.exists() {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map(|cwd| cwd.join(path)).unwrap_or_else(|_| path.to_path_buf())
    }
}

impl From<io::Error> for PluginLoadError {
    fn from(err: io::Error) -> PluginLoadError {
        PluginLoadError::Io(err)
    }
}

impl From<toml::de::Error> for PluginLoadError {
    fn from(err: toml::de::Error) -> PluginLoadError {
        PluginLoadError::Parse(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::TempDir;

    fn write_plugin(root: &Path, name: &str, exec_rel: &str, manifest_version: u32) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        let exec_path = root.join(exec_rel);
        if let Some(parent) = exec_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&exec_path, b"#!/bin/sh\n").unwrap();

        let manifest_path = root.join("manifest.toml");
        fs::write(
            &manifest_path,
            format!(
                "manifest_version = {manifest_version}\nname = \"{name}\"\nversion = \"0.1.0\"\nexec_path = \"{exec_rel}\"\n"
            ),
        )
        .unwrap();
        manifest_path
    }

    #[test]
    fn load_manifest_rejects_unsupported_manifest_version() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = write_plugin(temp_dir.path(), "test-plugin", "bin/test-plugin", 2);

        match load_manifest(&manifest_path) {
            Err(PluginLoadError::UnsupportedManifestVersion { found, supported, .. }) => {
                assert_eq!(found, 2);
                assert_eq!(supported, SUPPORTED_MANIFEST_VERSION);
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn load_manifest_normalizes_relative_exec_paths() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = write_plugin(temp_dir.path(), "test-plugin", "bin/test-plugin", 1);

        let manifest = load_manifest(&manifest_path).unwrap();

        assert_eq!(
            manifest.exec_path,
            temp_dir.path().join("bin/test-plugin").canonicalize().unwrap()
        );
    }

    #[test]
    fn load_from_paths_reports_duplicate_plugin_names() {
        let temp_dir = TempDir::new().unwrap();
        let first_dir = temp_dir.path().join("first-plugin");
        let second_dir = temp_dir.path().join("second-plugin");
        let first_manifest =
            write_plugin(&first_dir, "dup-plugin", "bin/first", 1).canonicalize().unwrap();
        let second_manifest =
            write_plugin(&second_dir, "dup-plugin", "bin/second", 1).canonicalize().unwrap();

        let mut catalog = PluginCatalog::default();
        let errors = catalog.load_from_paths(&[first_dir, second_dir]);

        assert_eq!(catalog.iter().count(), 1);
        match errors.as_slice() {
            [PluginLoadError::DuplicatePluginName { name, first_path, second_path }] => {
                assert_eq!(name, "dup-plugin");
                assert_eq!(first_path, &first_manifest);
                assert_eq!(second_path, &second_manifest);
            }
            other => panic!("unexpected errors: {other:?}"),
        }
    }

    #[test]
    fn get_from_path_uses_canonical_path_boundaries() {
        let temp_dir = TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("sample-plugin");
        let manifest_path = write_plugin(&plugin_dir, "sample-plugin", "bin/run-plugin", 1);
        let extra_file = plugin_dir.join("notes.txt");
        fs::write(&extra_file, b"notes").unwrap();

        let mut catalog = PluginCatalog::default();
        let errors = catalog.load_from_paths(std::slice::from_ref(&plugin_dir));

        assert!(errors.is_empty());
        assert!(catalog.get_from_path(&manifest_path).is_some());
        assert!(catalog.get_from_path(&extra_file).is_some());
        assert!(catalog.get_from_path(&plugin_dir.join("bin/run-plugin")).is_some());
        assert!(catalog.get_from_path(&temp_dir.path().join("sample")).is_none());
    }
}
