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

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jsonschema::validator_for;
use log::{error, info};
use semver::{Version, VersionReq};
use serde::Deserialize;
use toml::Table as TomlTable;

use super::{ManifestValidationError, PluginDescription, PluginName};
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
    InvalidManifest {
        path: PathBuf,
        err: ManifestValidationError,
    },
    SchemaValidation {
        path: PathBuf,
        pointer: String,
        message: String,
    },
    InvalidRequirement {
        path: PathBuf,
        plugin: PluginName,
        requirement: String,
        message: String,
    },
    UnsatisfiedRequirement {
        path: PathBuf,
        plugin: PluginName,
        requirement: String,
        message: String,
    },
    CyclicRequirements {
        cycle: Vec<PluginName>,
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
            PluginLoadError::InvalidManifest { path, err } => {
                write!(f, "invalid plugin manifest {}: {err}", path.display())
            }
            PluginLoadError::SchemaValidation { path, pointer, message } => {
                write!(f, "invalid plugin manifest {} at {}: {}", path.display(), pointer, message)
            }
            PluginLoadError::InvalidRequirement { path, plugin, requirement, message } => write!(
                f,
                "invalid plugin requirement {requirement:?} for {plugin:?} in {}: {message}",
                path.display()
            ),
            PluginLoadError::UnsatisfiedRequirement { path, plugin, requirement, message } => {
                write!(
                    f,
                    "unsatisfied plugin requirement {requirement:?} for {plugin:?} in {}: {message}",
                    path.display()
                )
            }
            PluginLoadError::CyclicRequirements { cycle } => {
                write!(f, "cyclic plugin requirements: {}", cycle.join(" -> "))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct PendingManifest {
    path: PathBuf,
    plugin: Arc<PluginDescription>,
}

#[derive(Debug, Clone)]
struct ParsedRequirement {
    dependency: String,
    version_req: VersionReq,
    raw: String,
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
        let mut staged = HashMap::<PluginName, PendingManifest>::new();
        for manifest_path in &all_manifests {
            match load_manifest(manifest_path) {
                Err(e) => errors.push(e),
                Ok(manifest) => {
                    let manifest_path = canonicalize_for_lookup(manifest_path);
                    if let Some(existing) = staged.get(&manifest.name) {
                        errors.push(PluginLoadError::DuplicatePluginName {
                            name: manifest.name.clone(),
                            first_path: existing.path.clone(),
                            second_path: manifest_path,
                        });
                        continue;
                    }

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
                    staged.insert(
                        manifest.name.clone(),
                        PendingManifest { path: manifest_path, plugin: manifest },
                    );
                }
            }
        }

        let mut next_items = self.items.clone();
        let mut next_locations = self.locations.clone();
        for pending in staged.values() {
            if let Some(previous) =
                next_locations.insert(pending.path.clone(), pending.plugin.clone())
            {
                next_items.remove(&previous.name);
            }
            next_items.insert(pending.plugin.name.clone(), pending.plugin.clone());
        }

        let mut name_to_path = next_locations
            .iter()
            .map(|(path, plugin)| (plugin.name.clone(), path.clone()))
            .collect::<HashMap<_, _>>();
        for pending in staged.values() {
            name_to_path.insert(pending.plugin.name.clone(), pending.path.clone());
        }

        let requirement_errors = validate_requirements(&next_items, &name_to_path);
        if !requirement_errors.is_empty() {
            errors.extend(requirement_errors);
            return errors;
        }

        self.items = next_items;
        self.locations = next_locations;
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
    let raw_manifest: toml::Value = toml::from_str(&contents)?;
    validate_manifest_schema(path, &raw_manifest)?;
    let manifest: PluginManifest = toml::from_str(&contents)?;
    if manifest.manifest_version != SUPPORTED_MANIFEST_VERSION {
        return Err(PluginLoadError::UnsupportedManifestVersion {
            path: path.to_path_buf(),
            found: manifest.manifest_version,
            supported: SUPPORTED_MANIFEST_VERSION,
        });
    }

    let mut manifest = manifest.plugin;
    manifest
        .validates()
        .map_err(|err| PluginLoadError::InvalidManifest { path: path.to_path_buf(), err })?;
    let manifest_dir = path.parent().ok_or_else(|| {
        PluginLoadError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("manifest path has no parent directory: {}", path.display()),
        ))
    })?;
    if manifest.exec_path.is_relative() {
        manifest.exec_path = manifest_dir.join(&manifest.exec_path).canonicalize()?;
    }
    if let Some(working_dir) = manifest.launch.working_dir.as_mut()
        && working_dir.is_relative()
    {
        *working_dir = manifest_dir.join(&*working_dir);
    }

    for lang in &mut manifest.languages {
        let lang_config_path = manifest_dir.join(lang.name.as_ref()).with_extension("toml");
        if !lang_config_path.exists() {
            continue;
        }
        let lang_defaults = fs::read_to_string(&lang_config_path)?;
        let lang_defaults = table_from_toml_str(&lang_defaults)?;
        lang.default_config = Some(lang_defaults);
    }
    Ok(manifest)
}

fn validate_manifest_schema(
    path: &Path,
    raw_manifest: &toml::Value,
) -> Result<(), PluginLoadError> {
    let Some(table) = raw_manifest.as_table() else {
        return Ok(());
    };

    let mut plugin_table = TomlTable::new();
    for (key, value) in table {
        if key != "manifest_version" {
            plugin_table.insert(key.clone(), value.clone());
        }
    }

    let instance = serde_json::to_value(toml::Value::Table(plugin_table)).map_err(|err| {
        PluginLoadError::Io(io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
    })?;
    let schema = PluginDescription::json_schema();
    let validator = validator_for(&schema).map_err(|err| PluginLoadError::SchemaValidation {
        path: path.to_path_buf(),
        pointer: String::from("/"),
        message: format!("schema compilation failed: {err}"),
    })?;
    if let Some(err) = validator.iter_errors(&instance).next() {
        let pointer = err.instance_path.to_string();
        return Err(PluginLoadError::SchemaValidation {
            path: path.to_path_buf(),
            pointer: if pointer.is_empty() { String::from("/") } else { pointer },
            message: err.to_string(),
        });
    }
    Ok(())
}

fn validate_requirements(
    items: &HashMap<PluginName, Arc<PluginDescription>>,
    name_to_path: &HashMap<PluginName, PathBuf>,
) -> Vec<PluginLoadError> {
    let mut errors = Vec::new();
    let core_version = Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("xi-core-lib package version should be valid semver");
    let mut dependency_graph = HashMap::<PluginName, Vec<PluginName>>::new();

    for (plugin_name, plugin) in items {
        let path = name_to_path
            .get(plugin_name)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(format!("<{}>", plugin_name)));
        let plugin_version = match Version::parse(&plugin.version) {
            Ok(version) => version,
            Err(err) => {
                errors.push(PluginLoadError::UnsatisfiedRequirement {
                    path,
                    plugin: plugin_name.clone(),
                    requirement: String::from("version"),
                    message: format!(
                        "plugin version {:?} is not valid semver: {err}",
                        plugin.version
                    ),
                });
                continue;
            }
        };

        let mut deps = Vec::new();
        for requirement in &plugin.requires {
            let parsed = match parse_requirement(requirement) {
                Ok(parsed) => parsed,
                Err(message) => {
                    errors.push(PluginLoadError::InvalidRequirement {
                        path: path.clone(),
                        plugin: plugin_name.clone(),
                        requirement: requirement.clone(),
                        message,
                    });
                    continue;
                }
            };

            if parsed.dependency == "xi-core" {
                if !parsed.version_req.matches(&core_version) {
                    errors.push(PluginLoadError::UnsatisfiedRequirement {
                        path: path.clone(),
                        plugin: plugin_name.clone(),
                        requirement: parsed.raw,
                        message: format!(
                            "xi-core version {} does not satisfy {}",
                            core_version, parsed.version_req
                        ),
                    });
                }
                continue;
            }

            let Some(dependency) = items.get(&parsed.dependency) else {
                errors.push(PluginLoadError::UnsatisfiedRequirement {
                    path: path.clone(),
                    plugin: plugin_name.clone(),
                    requirement: parsed.raw,
                    message: format!("required plugin {:?} is not present", parsed.dependency),
                });
                continue;
            };

            let dependency_version = match Version::parse(&dependency.version) {
                Ok(version) => version,
                Err(err) => {
                    errors.push(PluginLoadError::UnsatisfiedRequirement {
                        path: path.clone(),
                        plugin: plugin_name.clone(),
                        requirement: parsed.raw,
                        message: format!(
                            "required plugin {:?} has invalid semver {:?}: {err}",
                            parsed.dependency, dependency.version
                        ),
                    });
                    continue;
                }
            };

            if !parsed.version_req.matches(&dependency_version) {
                errors.push(PluginLoadError::UnsatisfiedRequirement {
                    path: path.clone(),
                    plugin: plugin_name.clone(),
                    requirement: parsed.raw,
                    message: format!(
                        "plugin {:?} version {} does not satisfy {}",
                        parsed.dependency, dependency_version, parsed.version_req
                    ),
                });
                continue;
            }

            let _ = plugin_version;
            deps.push(parsed.dependency);
        }
        dependency_graph.insert(plugin_name.clone(), deps);
    }

    if let Some(cycle) = detect_dependency_cycle(&dependency_graph) {
        errors.push(PluginLoadError::CyclicRequirements { cycle });
    }

    errors
}

fn parse_requirement(spec: &str) -> Result<ParsedRequirement, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(String::from("requirement must not be empty"));
    }

    let split_at = trimmed
        .char_indices()
        .find_map(|(index, ch)| {
            (!(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')).then_some(index)
        })
        .ok_or_else(|| {
            String::from("requirement must include dependency name and semver expression")
        })?;
    let (dependency, req_text) = trimmed.split_at(split_at);
    let dependency = dependency.trim();
    let req_text = req_text.trim();
    if dependency.is_empty() || req_text.is_empty() {
        return Err(String::from("requirement must include dependency name and semver expression"));
    }

    let version_req = VersionReq::parse(req_text)
        .map_err(|err| format!("invalid semver requirement {req_text:?}: {err}"))?;
    Ok(ParsedRequirement {
        dependency: dependency.to_owned(),
        version_req,
        raw: trimmed.to_owned(),
    })
}

fn detect_dependency_cycle(
    graph: &HashMap<PluginName, Vec<PluginName>>,
) -> Option<Vec<PluginName>> {
    fn visit(
        node: &str,
        graph: &HashMap<PluginName, Vec<PluginName>>,
        visited: &mut HashSet<PluginName>,
        visiting: &mut Vec<PluginName>,
    ) -> Option<Vec<PluginName>> {
        if let Some(index) = visiting.iter().position(|candidate| candidate == node) {
            let mut cycle = visiting[index..].to_vec();
            cycle.push(node.to_owned());
            return Some(cycle);
        }
        if !visited.insert(node.to_owned()) {
            return None;
        }

        visiting.push(node.to_owned());
        if let Some(dependencies) = graph.get(node) {
            for dependency in dependencies {
                if let Some(cycle) = visit(dependency, graph, visited, visiting) {
                    return Some(cycle);
                }
            }
        }
        visiting.pop();
        None
    }

    let mut visited = HashSet::new();
    let mut visiting = Vec::new();
    for node in graph.keys() {
        if let Some(cycle) = visit(node, graph, &mut visited, &mut visiting) {
            return Some(cycle);
        }
    }
    None
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
    use crate::plugins::{PluginCapability, PluginTransport};
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

    fn write_plugin_with_capabilities(
        root: &Path,
        name: &str,
        exec_rel: &str,
        manifest_version: u32,
        capabilities: &[&str],
    ) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        let exec_path = root.join(exec_rel);
        if let Some(parent) = exec_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&exec_path, b"#!/bin/sh\n").unwrap();

        let manifest_path = root.join("manifest.toml");
        let capabilities = capabilities
            .iter()
            .map(|capability| format!("\"{capability}\""))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            &manifest_path,
            format!(
                "manifest_version = {manifest_version}\nname = \"{name}\"\nversion = \"0.1.0\"\nexec_path = \"{exec_rel}\"\ncapabilities = [{capabilities}]\n"
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
    fn load_manifest_reads_declared_capabilities() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = write_plugin_with_capabilities(
            temp_dir.path(),
            "test-plugin",
            "bin/test-plugin",
            1,
            &["hover", "status_items", "filesystem"],
        );

        let manifest = load_manifest(&manifest_path).unwrap();

        assert_eq!(
            manifest.capabilities,
            vec![
                PluginCapability::Hover,
                PluginCapability::StatusItems,
                PluginCapability::Filesystem,
            ]
        );
    }

    #[test]
    fn load_manifest_normalizes_relative_launch_working_dir() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_dir = temp_dir.path().join("plugin");
        let exec_path = manifest_dir.join("bin/test-plugin");
        fs::create_dir_all(exec_path.parent().unwrap()).unwrap();
        fs::create_dir_all(manifest_dir.join("runtime-data")).unwrap();
        fs::write(&exec_path, b"#!/bin/sh\n").unwrap();

        let manifest_path = manifest_dir.join("manifest.toml");
        fs::write(
            &manifest_path,
            r#"manifest_version = 1
name = "test-plugin"
version = "0.1.0"
exec_path = "bin/test-plugin"

[launch]
working_dir = "runtime-data"
transport = "stdio_content_length"
"#,
        )
        .unwrap();

        let manifest = load_manifest(&manifest_path).unwrap();

        assert_eq!(manifest.launch.working_dir, Some(manifest_dir.join("runtime-data")));
        assert_eq!(manifest.launch.transport, PluginTransport::StdioContentLength);
    }

    #[test]
    fn load_manifest_rejects_invalid_placeholder_templates() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = temp_dir.path().join("manifest.toml");
        let exec_path = temp_dir.path().join("bin/test-plugin");
        fs::create_dir_all(exec_path.parent().unwrap()).unwrap();
        fs::write(&exec_path, b"#!/bin/sh\n").unwrap();
        fs::write(
            &manifest_path,
            r#"manifest_version = 1
name = "test-plugin"
version = "0.1.0"
exec_path = "bin/test-plugin"

[[commands]]
title = "Broken Command"
description = "broken"

[commands.rpc_cmd]
rpc_type = "notification"
method = "test.cmd"
params = { view = "", missing = "" }

[[commands.args]]
title = "Declared"
description = "declared"
key = "arg_one"
arg_type = "String"
"#,
        )
        .unwrap();

        match load_manifest(&manifest_path) {
            Err(PluginLoadError::InvalidManifest { err, .. }) => {
                assert_eq!(
                    err,
                    ManifestValidationError::MissingCommandArgumentTemplate {
                        command: "Broken Command".to_string(),
                        key: "arg_one".to_string(),
                    }
                );
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn load_manifest_rejects_schema_invalid_field_with_pointer() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = temp_dir.path().join("manifest.toml");
        let exec_path = temp_dir.path().join("bin/test-plugin");
        fs::create_dir_all(exec_path.parent().unwrap()).unwrap();
        fs::write(&exec_path, b"#!/bin/sh\n").unwrap();
        fs::write(
            &manifest_path,
            r#"manifest_version = 1
name = "test-plugin"
version = "0.1.0"
exec_path = "bin/test-plugin"
rpc_timeout_ms = "fast"
"#,
        )
        .unwrap();

        match load_manifest(&manifest_path) {
            Err(PluginLoadError::SchemaValidation { pointer, .. }) => {
                assert_eq!(pointer, "/rpc_timeout_ms");
            }
            other => panic!("unexpected result: {other:?}"),
        }
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

    #[test]
    fn load_from_paths_rejects_unsatisfied_plugin_requirement() {
        let temp_dir = TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("needs-syntax-runtime");
        let exec_path = plugin_dir.join("bin/run-plugin");
        fs::create_dir_all(exec_path.parent().unwrap()).unwrap();
        fs::write(&exec_path, b"#!/bin/sh\n").unwrap();
        fs::write(
            plugin_dir.join("manifest.toml"),
            r#"manifest_version = 1
name = "needs-syntax-runtime"
version = "0.1.0"
exec_path = "bin/run-plugin"
requires = ["syntax-runtime>=0.2.0"]
"#,
        )
        .unwrap();

        let mut catalog = PluginCatalog::default();
        let errors = catalog.load_from_paths(&[plugin_dir]);

        match errors.as_slice() {
            [PluginLoadError::UnsatisfiedRequirement { plugin, requirement, .. }] => {
                assert_eq!(plugin, "needs-syntax-runtime");
                assert_eq!(requirement, "syntax-runtime>=0.2.0");
            }
            other => panic!("unexpected errors: {other:?}"),
        }
        assert!(catalog.iter().next().is_none());
    }

    #[test]
    fn load_from_paths_rejects_cyclic_requirements() {
        let temp_dir = TempDir::new().unwrap();
        let first_dir = temp_dir.path().join("first");
        let second_dir = temp_dir.path().join("second");

        for (dir, name, requires) in
            [(&first_dir, "first", "second>=0.1.0"), (&second_dir, "second", "first>=0.1.0")]
        {
            let exec_path = dir.join("bin/run-plugin");
            fs::create_dir_all(exec_path.parent().unwrap()).unwrap();
            fs::write(&exec_path, b"#!/bin/sh\n").unwrap();
            fs::write(
                dir.join("manifest.toml"),
                format!(
                    "manifest_version = 1\nname = \"{name}\"\nversion = \"0.1.0\"\nexec_path = \"bin/run-plugin\"\nrequires = [\"{requires}\"]\n"
                ),
            )
            .unwrap();
        }

        let mut catalog = PluginCatalog::default();
        let errors = catalog.load_from_paths(&[first_dir, second_dir]);

        match errors.as_slice() {
            [PluginLoadError::CyclicRequirements { cycle }] => {
                assert_eq!(cycle.first().unwrap(), cycle.last().unwrap());
                assert!(cycle.iter().any(|plugin| plugin == "first"));
                assert!(cycle.iter().any(|plugin| plugin == "second"));
            }
            other => panic!("unexpected errors: {other:?}"),
        }
        assert!(catalog.iter().next().is_none());
    }

    #[test]
    fn load_from_paths_accepts_xi_lsp_manifest() {
        let temp_dir = TempDir::new().unwrap();
        let plugin_dir = temp_dir.path().join("xi-lsp-plugin");
        let exec_path = plugin_dir.join("bin/xi-lsp-plugin");
        let manifest_src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../xi-lsp-lib/manifest.toml")
            .canonicalize()
            .unwrap();

        fs::create_dir_all(exec_path.parent().unwrap()).unwrap();
        fs::write(&exec_path, b"#!/bin/sh\n").unwrap();
        fs::copy(manifest_src, plugin_dir.join("manifest.toml")).unwrap();

        let mut catalog = PluginCatalog::default();
        let errors = catalog.load_from_paths(&[plugin_dir]);

        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(catalog.get_named("xi-lsp-plugin").is_some());
    }

    #[test]
    fn load_from_paths_accepts_satisfied_requirements() {
        let temp_dir = TempDir::new().unwrap();
        let provider_dir = temp_dir.path().join("syntax-runtime");
        let consumer_dir = temp_dir.path().join("consumer");

        for (dir, name, version, requires) in [
            (&provider_dir, "syntax-runtime", "0.2.1", None),
            (
                &consumer_dir,
                "consumer",
                "0.1.0",
                Some(r#"requires = ["xi-core>=0.4.0", "syntax-runtime>=0.2.0"]"#),
            ),
        ] {
            let exec_path = dir.join("bin/run-plugin");
            fs::create_dir_all(exec_path.parent().unwrap()).unwrap();
            fs::write(&exec_path, b"#!/bin/sh\n").unwrap();
            let requires = requires.unwrap_or("");
            fs::write(
                dir.join("manifest.toml"),
                format!(
                    "manifest_version = 1\nname = \"{name}\"\nversion = \"{version}\"\nexec_path = \"bin/run-plugin\"\n{requires}\n"
                ),
            )
            .unwrap();
        }

        let mut catalog = PluginCatalog::default();
        let errors = catalog.load_from_paths(&[provider_dir, consumer_dir]);

        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(catalog.get_named("syntax-runtime").is_some());
        assert!(catalog.get_named("consumer").is_some());
    }
}
