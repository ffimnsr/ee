//! Manifest discovery from bundled, user, workspace, client-extra, and
//! environment-provided roots.
//!
//! Discovery never executes plugin code. It parses and validates v2 manifests,
//! computes a content hash over the manifest and entrypoint, and records a
//! blocked entry instead of silently skipping untrusted or duplicate plugins.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ee_plugin_api::error::ErrorClass;
use ee_plugin_api::manifest::{MANIFEST_VERSION, ManifestV2};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::diagnostics::{Diagnostic, DiagnosticPhase};

/// Maximum directory depth walked below a plugin root.
pub const MAX_DISCOVERY_DEPTH: usize = 8;

/// Where a plugin root came from. Ordering is the discovery precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Bundled,
    User,
    Workspace,
    ClientExtra,
    Environment,
}

impl RootKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::User => "user",
            Self::Workspace => "workspace",
            Self::ClientExtra => "client-extra",
            Self::Environment => "environment",
        }
    }
}

/// One discovery root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PluginRoot {
    pub kind: RootKind,
    pub path: PathBuf,
}

impl PluginRoot {
    pub fn new(kind: RootKind, path: impl Into<PathBuf>) -> Self {
        Self { kind, path: path.into() }
    }
}

/// Discovery configuration. Tests build this directly so no process
/// environment is required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryOptions {
    pub roots: Vec<PluginRoot>,
    pub trust_workspace: bool,
    pub max_depth: usize,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self { roots: Vec::new(), trust_workspace: false, max_depth: MAX_DISCOVERY_DEPTH }
    }
}

impl DiscoveryOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_root(&mut self, kind: RootKind, path: impl Into<PathBuf>) {
        self.roots.push(PluginRoot::new(kind, path));
    }

    /// Add `EE_PLUGIN_PATH` entries. The platform separator is `:` on Unix.
    pub fn push_env_paths(&mut self, value: &str) {
        for entry in value.split(':').filter(|entry| !entry.is_empty()) {
            self.push_root(RootKind::Environment, entry);
        }
    }

    pub fn trust_workspace(mut self, trust: bool) -> Self {
        self.trust_workspace = trust;
        self
    }

    pub fn with_root(mut self, kind: RootKind, path: impl Into<PathBuf>) -> Self {
        self.push_root(kind, path);
        self
    }
}

/// One discovered manifest.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    pub manifest: ManifestV2,
    pub manifest_path: PathBuf,
    pub manifest_dir: PathBuf,
    pub root: RootKind,
    pub origin_hash: String,
    pub entry_path: Option<PathBuf>,
    /// Present when the plugin may not be activated but should still appear in
    /// `status` as blocked.
    pub blocked: Option<Diagnostic>,
}

/// Result of scanning every root.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    pub plugins: Vec<DiscoveredPlugin>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Scan every configured root.
///
/// Roots are processed in [`RootKind`] order and each root is walked
/// deterministically. The first plugin with a given name wins; later duplicates
/// are kept as blocked entries so `status` can show both origins.
pub fn discover(options: &DiscoveryOptions) -> DiscoveryResult {
    let mut roots = options.roots.clone();
    roots.sort();
    roots.dedup();

    let mut result = DiscoveryResult::default();
    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();

    for root in &roots {
        let manifest_paths = match collect_manifests(&root.path, options.max_depth) {
            Ok(paths) => paths,
            Err(error) => {
                result.diagnostics.push(
                    Diagnostic::new(
                        ErrorClass::Io,
                        "plugin.io",
                        DiagnosticPhase::Discovery,
                        format!("cannot read plugin root `{}`: {error}", root.path.display()),
                    )
                    .with_origin(root.path.clone()),
                );
                continue;
            }
        };

        for manifest_path in manifest_paths {
            match load_discovered(manifest_path, root.kind) {
                Ok(mut plugin) => {
                    if let Some(first) = by_name.get(&plugin.manifest.name) {
                        let message = format!(
                            "duplicate plugin name `{}`: first `{}`, second `{}`",
                            plugin.manifest.name,
                            first.display(),
                            plugin.manifest_path.display()
                        );
                        let diagnostic = Diagnostic::new(
                            ErrorClass::Manifest,
                            "plugin.manifest",
                            DiagnosticPhase::Discovery,
                            message,
                        )
                        .with_plugin(plugin.manifest.name.clone())
                        .with_origin(plugin.manifest_path.clone());
                        result.diagnostics.push(diagnostic.clone());
                        plugin.blocked = Some(diagnostic);
                    } else {
                        by_name.insert(plugin.manifest.name.clone(), plugin.manifest_path.clone());
                    }

                    if root.kind == RootKind::Workspace && !options.trust_workspace {
                        let diagnostic = Diagnostic::new(
                            ErrorClass::Permission,
                            "plugin.permission",
                            DiagnosticPhase::Discovery,
                            "workspace plugins require workspace trust",
                        )
                        .with_plugin(plugin.manifest.name.clone())
                        .with_origin(plugin.manifest_path.clone());
                        result.diagnostics.push(diagnostic.clone());
                        if plugin.blocked.is_none() {
                            plugin.blocked = Some(diagnostic);
                        }
                    }

                    result.plugins.push(plugin);
                }
                Err(diagnostic) => result.diagnostics.push(diagnostic),
            }
        }
    }

    result.plugins.sort_by(|left, right| left.manifest.name.cmp(&right.manifest.name));
    result.diagnostics.sort_by(|left, right| {
        left.plugin
            .cmp(&right.plugin)
            .then(left.origin.cmp(&right.origin))
            .then(left.code.cmp(&right.code))
            .then(left.message.cmp(&right.message))
    });
    result
}

fn collect_manifests(root: &Path, max_depth: usize) -> std::io::Result<Vec<PathBuf>> {
    let metadata = fs::metadata(root)?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("`{}` is not a directory", root.display()),
        ));
    }

    let mut manifests = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > max_depth {
            continue;
        }
        let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                let name = entry.file_name();
                if matches!(name.to_str(), Some(".git" | "target" | "node_modules")) {
                    continue;
                }
                pending.push((path, depth + 1));
            } else if file_type.is_file() && entry.file_name() == "manifest.toml" {
                manifests.push(path);
            }
        }
    }
    manifests.sort();
    Ok(manifests)
}

fn load_discovered(manifest_path: PathBuf, root: RootKind) -> Result<DiscoveredPlugin, Diagnostic> {
    let manifest_dir = manifest_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let bytes = fs::read(&manifest_path).map_err(|error| {
        Diagnostic::new(
            ErrorClass::Io,
            "plugin.io",
            DiagnosticPhase::Discovery,
            format!("cannot read `{}`: {error}", manifest_path.display()),
        )
        .with_origin(manifest_path.clone())
    })?;

    let value: toml::Value = toml::from_str(std::str::from_utf8(&bytes).map_err(|error| {
        Diagnostic::new(
            ErrorClass::Manifest,
            "plugin.manifest",
            DiagnosticPhase::Discovery,
            format!("manifest is not UTF-8: {error}"),
        )
        .with_origin(manifest_path.clone())
    })?)
    .map_err(|error| {
        Diagnostic::new(
            ErrorClass::Manifest,
            "plugin.manifest",
            DiagnosticPhase::Discovery,
            format!("cannot parse manifest: {error}"),
        )
        .with_origin(manifest_path.clone())
    })?;

    let version = value.get("manifest_version").and_then(toml::Value::as_integer);
    if version != Some(i64::from(MANIFEST_VERSION)) {
        return Err(Diagnostic::new(
            ErrorClass::Manifest,
            "plugin.manifest",
            DiagnosticPhase::Discovery,
            format!("unsupported manifest_version: found {version:?}, expected {MANIFEST_VERSION}"),
        )
        .with_origin(manifest_path));
    }

    let manifest: ManifestV2 = value.try_into().map_err(|error| {
        Diagnostic::new(
            ErrorClass::Manifest,
            "plugin.manifest",
            DiagnosticPhase::Discovery,
            format!("invalid manifest: {error}"),
        )
        .with_origin(manifest_path.clone())
    })?;

    manifest.validate().map_err(|error| {
        Diagnostic::new(
            ErrorClass::Manifest,
            "plugin.manifest",
            DiagnosticPhase::Discovery,
            format!("manifest validation failed: {error}"),
        )
        .with_plugin(manifest.name.clone())
        .with_origin(manifest_path.clone())
    })?;

    let entry_path = manifest
        .entry
        .strip_prefix("builtin:")
        .is_none()
        .then(|| manifest_dir.join(&manifest.entry));
    let origin_hash = origin_hash(&bytes, entry_path.as_deref());

    Ok(DiscoveredPlugin {
        manifest,
        manifest_path,
        manifest_dir,
        root,
        origin_hash,
        entry_path,
        blocked: None,
    })
}

/// SHA-256 over the manifest bytes plus entrypoint bytes.
///
/// The entrypoint path itself is not hashed, so moving an unchanged plugin
/// keeps the same origin hash.
pub fn origin_hash(manifest_bytes: &[u8], entry_path: Option<&Path>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ee-plugin-origin-v2\0");
    hasher.update((manifest_bytes.len() as u64).to_le_bytes());
    hasher.update(manifest_bytes);
    match entry_path.and_then(|path| fs::read(path).ok()) {
        Some(entry_bytes) => {
            hasher.update(b"\0entry\0");
            hasher.update((entry_bytes.len() as u64).to_le_bytes());
            hasher.update(entry_bytes);
        }
        None => hasher.update(b"\0entry-missing\0"),
    }
    format!("sha256:{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tempfile::TempDir;

    use super::*;

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    fn plugin_name() -> String {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        format!("demo.plugin{id}")
    }

    fn manifest_text(name: &str) -> String {
        format!(
            r#"
manifest_version = 2
name = "{name}"
version = "1.0.0"
api = "ee-plugin/2"
targets = ["editor"]
kind = "native"
entry = "bin/plugin"
"#
        )
    }

    fn write_plugin(root: &Path, dir_name: &str, name: &str) -> PathBuf {
        let dir = root.join(dir_name);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("manifest.toml"), manifest_text(name)).unwrap();
        fs::write(dir.join("bin/plugin"), b"#!/bin/sh\nexit 0\n").unwrap();
        dir
    }

    #[test]
    fn discovers_recursively_and_hashes_origin() {
        let temp = TempDir::new().unwrap();
        let name = plugin_name();
        let dir = write_plugin(temp.path(), "nested/plugin", &name);
        let options = DiscoveryOptions::new().with_root(RootKind::User, temp.path().to_path_buf());
        let result = discover(&options);
        assert_eq!(result.plugins.len(), 1);
        assert_eq!(result.plugins[0].manifest.name, name);
        assert_eq!(result.plugins[0].manifest_dir, dir);

        let hash = result.plugins[0].origin_hash.clone();
        let again = discover(&options);
        assert_eq!(again.plugins[0].origin_hash, hash);

        fs::write(dir.join("bin/plugin"), b"changed").unwrap();
        let changed = discover(&options);
        assert_ne!(changed.plugins[0].origin_hash, hash);
    }

    #[test]
    fn duplicate_names_keep_first_and_block_second() {
        let temp = TempDir::new().unwrap();
        let name = plugin_name();
        write_plugin(temp.path(), "a", &name);
        write_plugin(temp.path(), "b", &name);
        let options = DiscoveryOptions::new().with_root(RootKind::User, temp.path().to_path_buf());
        let result = discover(&options);
        assert_eq!(result.plugins.len(), 2);
        assert!(result.plugins.iter().any(|plugin| plugin.blocked.is_some()));
        let message = &result.diagnostics[0].message;
        assert!(message.contains("duplicate plugin name"));
        assert!(message.contains("first"));
        assert!(message.contains("second"));
    }

    #[test]
    fn missing_manifest_fields_are_reported() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("broken");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("manifest.toml"), "manifest_version = 2\nname = \"x\"\n").unwrap();
        let options = DiscoveryOptions::new().with_root(RootKind::User, temp.path().to_path_buf());
        let result = discover(&options);
        assert!(result.plugins.is_empty());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].class, ErrorClass::Manifest);
    }

    #[test]
    fn rejects_manifest_v1() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("legacy");
        fs::create_dir_all(&dir).unwrap();
        let legacy = format!("manifest_version = {}\nname = \"legacy\"\n", 1);
        fs::write(dir.join("manifest.toml"), legacy).unwrap();
        let options = DiscoveryOptions::new().with_root(RootKind::User, temp.path().to_path_buf());
        let result = discover(&options);
        assert!(result.plugins.is_empty());
        assert!(result.diagnostics[0].message.contains("unsupported manifest_version"));
    }

    #[test]
    fn unreadable_root_is_reported() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("not-a-dir");
        fs::write(&file, b"x").unwrap();
        let options = DiscoveryOptions::new().with_root(RootKind::User, file.clone());
        let result = discover(&options);
        assert!(result.plugins.is_empty());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].class, ErrorClass::Io);
        assert_eq!(result.diagnostics[0].origin.as_deref(), Some(file.as_path()));
    }

    #[test]
    fn workspace_plugins_require_trust() {
        let temp = TempDir::new().unwrap();
        let name = plugin_name();
        write_plugin(temp.path(), "ws", &name);
        let mut options =
            DiscoveryOptions::new().with_root(RootKind::Workspace, temp.path().to_path_buf());
        let untrusted = discover(&options);
        assert!(untrusted.plugins[0].blocked.is_some());
        assert_eq!(untrusted.diagnostics[0].class, ErrorClass::Permission);

        options.trust_workspace = true;
        let trusted = discover(&options);
        assert!(trusted.plugins[0].blocked.is_none());
        assert!(trusted.diagnostics.is_empty());
    }
}
