//! Scoped filesystem and network brokers for native plugins.
//!
//! Brokers gate the *service* path (`editor.fs@1`, network permission) rather
//! than blanket-disabling process isolation. Every path is resolved inside the
//! workspace root and escapes fail closed.

use std::path::{Component, Path, PathBuf};

use ee_plugin_api::error::PluginError;
use ee_plugin_api::manifest::{ManifestV2, Permissions};
use ee_plugin_api::wire::ServiceRequestParams;

/// Stable id of the filesystem service.
pub const FS_SERVICE: &str = "editor.fs@1";

/// Declared broker permissions for one plugin instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerPolicy {
    workspace: PathBuf,
    read: bool,
    write: bool,
    network: bool,
}

impl BrokerPolicy {
    /// Deny every brokered capability. Used by low-level test constructors.
    pub fn deny_all() -> Self {
        Self { workspace: PathBuf::new(), read: false, write: false, network: false }
    }

    /// Build a policy from a manifest, workspace root, and canonicalized roots.
    pub fn from_manifest(manifest: &ManifestV2, workspace: impl Into<PathBuf>) -> Self {
        Self::from_permissions(&manifest.permissions, workspace)
    }

    pub fn from_permissions(permissions: &Permissions, workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            read: permissions.grants("editor.fs.read"),
            write: permissions.grants("editor.fs.write"),
            network: permissions.grants("editor.network") || permissions.grants("agent.network"),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn can_read(&self) -> bool {
        self.read
    }

    pub fn can_write(&self) -> bool {
        self.write
    }

    pub fn can_network(&self) -> bool {
        self.network
    }

    /// Resolve a workspace-relative path for reading.
    pub fn check_read(&self, path: impl AsRef<Path>) -> Result<PathBuf, PluginError> {
        self.require_read()?;
        self.resolve_existing(path.as_ref())
    }

    /// Resolve a workspace-relative path for writing.
    pub fn check_write(&self, path: impl AsRef<Path>) -> Result<PathBuf, PluginError> {
        self.require_write()?;
        self.resolve_for_write(path.as_ref())
    }

    /// Authorize a filesystem service call before it reaches the host adapter.
    ///
    /// Unknown services and methods are left to the adapter, which fails closed
    /// with its own typed error.
    pub fn check_service_request(&self, request: &ServiceRequestParams) -> Result<(), PluginError> {
        if request.service.as_str() != FS_SERVICE {
            return Ok(());
        }
        match request.method.as_str() {
            "read" | "stat" | "list" => {
                self.require_read()?;
                let path = request.params.get("path").and_then(|value| value.as_str());
                if let Some(path) = path {
                    self.check_read(path)?;
                }
                Ok(())
            }
            "write" => {
                self.require_write()?;
                let path = request.params.get("path").and_then(|value| value.as_str());
                if let Some(path) = path {
                    self.check_write(path)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Fail closed when a plugin uses the network without declaring it.
    pub fn require_network(&self) -> Result<(), PluginError> {
        if self.network {
            Ok(())
        } else {
            Err(PluginError::Permission { permission: "editor.network".to_owned() })
        }
    }

    fn require_read(&self) -> Result<(), PluginError> {
        if self.read {
            Ok(())
        } else {
            Err(PluginError::Permission { permission: "editor.fs.read".to_owned() })
        }
    }

    fn require_write(&self) -> Result<(), PluginError> {
        if self.write {
            Ok(())
        } else {
            Err(PluginError::Permission { permission: "editor.fs.write".to_owned() })
        }
    }

    fn require_workspace(&self) -> Result<&Path, PluginError> {
        if self.workspace.as_os_str().is_empty() {
            return Err(PluginError::Permission { permission: "editor.fs.read".to_owned() });
        }
        Ok(&self.workspace)
    }

    fn resolve_existing(&self, path: &Path) -> Result<PathBuf, PluginError> {
        let workspace = self.require_workspace()?;
        let candidate = join_checked(workspace, path)?;
        let canonical = candidate.canonicalize().map_err(|error| PluginError::Io {
            message: format!("path `{}`: {error}", candidate.display()),
        })?;
        ensure_inside(workspace, &canonical)?;
        Ok(canonical)
    }

    fn resolve_for_write(&self, path: &Path) -> Result<PathBuf, PluginError> {
        let workspace = self.require_workspace()?;
        let candidate = join_checked(workspace, path)?;
        let parent = candidate.parent().unwrap_or(workspace);
        let canonical_parent = parent.canonicalize().map_err(|error| PluginError::Io {
            message: format!("path `{}`: {error}", parent.display()),
        })?;
        ensure_inside(workspace, &canonical_parent)?;
        let name = candidate.file_name().ok_or_else(|| PluginError::Io {
            message: format!("invalid write path `{}`", candidate.display()),
        })?;
        Ok(canonical_parent.join(name))
    }
}

fn join_checked(root: &Path, path: &Path) -> Result<PathBuf, PluginError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(PluginError::Permission {
                permission: "editor.fs.read (path escape)".to_owned(),
            });
        }
    }
    Ok(root.join(path))
}

fn ensure_inside(root: &Path, candidate: &Path) -> Result<(), PluginError> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if candidate.starts_with(&root) {
        Ok(())
    } else {
        Err(PluginError::Permission {
            permission: format!("editor.fs.read (path `{}` not allowed)", candidate.display()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn policy(perms: &[(&str, bool)], workspace: &Path) -> BrokerPolicy {
        let permissions = Permissions(
            perms.iter().map(|(name, enabled)| ((*name).to_owned(), *enabled)).collect(),
        );
        BrokerPolicy::from_permissions(&permissions, workspace)
    }

    #[test]
    fn undeclared_read_is_denied() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = policy(&[], workspace.path());
        let error = policy.check_read("file.txt").unwrap_err();
        assert!(matches!(error, PluginError::Permission { .. }));
    }

    #[test]
    fn declared_read_stays_inside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("file.txt"), b"ok").unwrap();
        let policy = policy(&[("editor.fs.read", true)], workspace.path());
        let resolved = policy.check_read("file.txt").unwrap();
        assert_eq!(resolved, workspace.path().canonicalize().unwrap().join("file.txt"));
    }

    #[test]
    fn parent_escape_is_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let policy = policy(&[("editor.fs.read", true)], workspace.path());
        let error = policy.check_read("../outside.txt").unwrap_err();
        assert!(matches!(error, PluginError::Permission { .. }));
    }

    #[test]
    fn absolute_outside_path_is_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"no").unwrap();
        let policy = policy(&[("editor.fs.read", true)], workspace.path());
        let target = outside.path().join("secret");
        let error = policy.check_read(target).unwrap_err();
        assert!(matches!(error, PluginError::Permission { .. }));
    }

    #[test]
    fn write_requires_write_permission() {
        let workspace = tempfile::tempdir().unwrap();
        let read_only = policy(&[("editor.fs.read", true)], workspace.path());
        assert!(read_only.check_write("new.txt").is_err());
        let writable = policy(&[("editor.fs.write", true)], workspace.path());
        assert!(writable.check_write("new.txt").is_ok());
    }

    #[test]
    fn service_requests_are_gated() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("file.txt"), b"ok").unwrap();
        let denied = policy(&[], workspace.path());
        let request = ServiceRequestParams {
            service: FS_SERVICE.parse().unwrap(),
            method: "read".to_owned(),
            request_id: 1,
            params: serde_json::json!({"path": "file.txt"}),
        };
        assert!(denied.check_service_request(&request).is_err());

        let allowed = policy(&[("editor.fs.read", true)], workspace.path());
        assert!(allowed.check_service_request(&request).is_ok());
    }

    #[test]
    fn network_requires_declaration() {
        let workspace = tempfile::tempdir().unwrap();
        let denied = policy(&[], workspace.path());
        assert!(denied.require_network().is_err());
        let granted = policy(&[("editor.network", true)], workspace.path());
        assert!(granted.require_network().is_ok());
    }

    #[test]
    fn deny_all_policy_grants_nothing() {
        let policy = BrokerPolicy::deny_all();
        assert!(!policy.can_read());
        assert!(!policy.can_write());
        assert!(!policy.can_network());
        let permissions = Permissions(BTreeMap::new());
        assert!(!permissions.grants("editor.fs.read"));
    }
}
