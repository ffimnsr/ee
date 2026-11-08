//! `editor.fs` service.
//!
//! Every path is resolved inside the workspace root, then checked against the
//! configured permission bits and optional glob allowlist. Escapes fail closed.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use ee_plugin_api::error::PluginError;
use ee_plugin_api::service::Service;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

/// Scoped filesystem permissions.
#[derive(Debug, Clone, Default)]
pub struct FsScope {
    pub read: bool,
    pub write: bool,
    /// Relative glob patterns. Empty means every path under the root.
    pub allow: Vec<String>,
}

/// Result of a file stat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsStat {
    pub path: String,
    pub is_file: bool,
    pub is_dir: bool,
    pub len: u64,
}

/// Workspace-scoped filesystem access.
#[derive(Clone)]
pub struct FsService {
    root: PathBuf,
    scope: FsScope,
    allow: Option<Arc<GlobSet>>,
}

impl FsService {
    pub fn new(root: impl Into<PathBuf>, scope: FsScope) -> Result<Self, PluginError> {
        let root = root.into();
        let root = root.canonicalize().map_err(|error| PluginError::Io {
            message: format!("workspace root `{}`: {error}", root.display()),
        })?;
        let allow = if scope.allow.is_empty() {
            None
        } else {
            let mut builder = GlobSetBuilder::new();
            for pattern in &scope.allow {
                builder.add(Glob::new(pattern).map_err(|error| PluginError::Schema {
                    message: format!("invalid fs glob `{pattern}`: {error}"),
                })?);
            }
            Some(Arc::new(builder.build().map_err(|error| PluginError::Schema {
                message: format!("invalid fs glob set: {error}"),
            })?))
        };
        Ok(Self { root, scope, allow })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn read(&self, path: impl AsRef<Path>) -> Result<Vec<u8>, PluginError> {
        self.require_read()?;
        let path = self.resolve_existing(path.as_ref())?;
        std::fs::read(&path).map_err(|error| PluginError::Io {
            message: format!("read `{}`: {error}", path.display()),
        })
    }

    pub fn write(&self, path: impl AsRef<Path>, bytes: &[u8]) -> Result<(), PluginError> {
        self.require_write()?;
        let path = self.resolve_for_write(path.as_ref())?;
        std::fs::write(&path, bytes).map_err(|error| PluginError::Io {
            message: format!("write `{}`: {error}", path.display()),
        })
    }

    pub fn stat(&self, path: impl AsRef<Path>) -> Result<FsStat, PluginError> {
        self.require_read()?;
        let path = self.resolve_existing(path.as_ref())?;
        let metadata = std::fs::metadata(&path).map_err(|error| PluginError::Io {
            message: format!("stat `{}`: {error}", path.display()),
        })?;
        Ok(FsStat {
            path: path.display().to_string(),
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
            len: metadata.len(),
        })
    }

    pub fn list(&self, path: impl AsRef<Path>) -> Result<Vec<String>, PluginError> {
        self.require_read()?;
        let path = self.resolve_existing(path.as_ref())?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path).map_err(|error| PluginError::Io {
            message: format!("list `{}`: {error}", path.display()),
        })? {
            let entry = entry.map_err(|error| PluginError::Io {
                message: format!("list `{}`: {error}", path.display()),
            })?;
            if let Ok(relative) = entry.path().strip_prefix(&self.root) {
                entries.push(relative.display().to_string());
            }
        }
        entries.sort();
        Ok(entries)
    }

    fn require_read(&self) -> Result<(), PluginError> {
        if self.scope.read {
            Ok(())
        } else {
            Err(PluginError::Permission { permission: "editor.fs.read".to_owned() })
        }
    }

    fn require_write(&self) -> Result<(), PluginError> {
        if self.scope.write {
            Ok(())
        } else {
            Err(PluginError::Permission { permission: "editor.fs.write".to_owned() })
        }
    }

    fn resolve_existing(&self, path: &Path) -> Result<PathBuf, PluginError> {
        self.check_relative(path)?;
        let candidate = self.join(path);
        let canonical = candidate.canonicalize().map_err(|error| PluginError::Io {
            message: format!("path `{}`: {error}", candidate.display()),
        })?;
        self.check_scope(&canonical)?;
        Ok(canonical)
    }

    fn resolve_for_write(&self, path: &Path) -> Result<PathBuf, PluginError> {
        self.check_relative(path)?;
        let candidate = self.join(path);
        let parent = candidate.parent().ok_or_else(|| PluginError::Schema {
            message: "write path has no parent".to_owned(),
        })?;
        let parent = parent.canonicalize().map_err(|error| PluginError::Io {
            message: format!("path `{}`: {error}", parent.display()),
        })?;
        let file_name = candidate.file_name().ok_or_else(|| PluginError::Schema {
            message: "write path has no file name".to_owned(),
        })?;
        let resolved = parent.join(file_name);
        self.check_scope(&resolved)?;
        Ok(resolved)
    }

    fn join(&self, path: &Path) -> PathBuf {
        if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) }
    }

    fn check_relative(&self, path: &Path) -> Result<(), PluginError> {
        if path.components().any(|component| matches!(component, Component::ParentDir)) {
            return Err(PluginError::Permission {
                permission: "editor.fs.read (path escape)".to_owned(),
            });
        }
        Ok(())
    }

    fn check_scope(&self, path: &Path) -> Result<(), PluginError> {
        let relative = path.strip_prefix(&self.root).map_err(|_| PluginError::Permission {
            permission: "editor.fs.write (outside workspace)".to_owned(),
        })?;
        if relative.components().any(|component| matches!(component, Component::ParentDir)) {
            return Err(PluginError::Permission {
                permission: "editor.fs.read (path escape)".to_owned(),
            });
        }
        if let Some(allow) = &self.allow
            && !allow.is_match(relative)
        {
            return Err(PluginError::Permission {
                permission: format!("editor.fs.read (path `{}` not allowed)", relative.display()),
            });
        }
        Ok(())
    }
}

impl Service for FsService {
    const ID: &'static str = "editor.fs@1";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(read: bool, write: bool, allow: Vec<String>) -> (tempfile::TempDir, FsService) {
        let dir = tempfile::tempdir().unwrap();
        let service = FsService::new(dir.path(), FsScope { read, write, allow }).unwrap();
        (dir, service)
    }

    #[test]
    fn read_write_and_list_are_scoped() {
        let (_dir, fs) = service(true, true, Vec::new());
        fs.write("notes.txt", b"hello").unwrap();
        assert_eq!(fs.read("notes.txt").unwrap(), b"hello");
        assert!(fs.stat("notes.txt").unwrap().is_file);
        assert_eq!(fs.list(".").unwrap(), vec!["notes.txt".to_owned()]);
    }

    #[test]
    fn path_escape_is_rejected() {
        let (_dir, fs) = service(true, true, Vec::new());
        let error = fs.read("../ee-fs-escape-test.txt").unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Permission);
    }

    #[test]
    fn permission_bits_fail_closed() {
        let (_dir, fs) = service(false, false, Vec::new());
        assert_eq!(
            fs.read("missing.txt").unwrap_err().class(),
            ee_plugin_api::error::ErrorClass::Permission
        );
        assert_eq!(
            fs.write("missing.txt", b"x").unwrap_err().class(),
            ee_plugin_api::error::ErrorClass::Permission
        );
    }

    #[test]
    fn allowlist_globs_are_enforced() {
        let (_dir, fs) = service(true, true, vec!["**/*.txt".to_owned()]);
        fs.write("ok.txt", b"x").unwrap();
        let error = fs.write("denied.rs", b"x").unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Permission);
    }
}
