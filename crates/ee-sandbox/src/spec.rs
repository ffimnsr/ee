//! Sandbox policy types and validation shared by Linux and macOS backends.

use std::fmt;
use std::path::{Path, PathBuf};

/// Whether sandboxed commands may reach the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAccess {
    /// No outbound or local network sockets.
    Deny,
    /// Inherit the host network namespace.
    Allow,
}

impl NetworkAccess {
    /// Parses a config value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "deny" => Ok(Self::Deny),
            "allow" => Ok(Self::Allow),
            other => Err(format!("network must be \"deny\" or \"allow\", got {other:?}")),
        }
    }

    /// Config label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Allow => "allow",
        }
    }
}

/// Filesystem read breadth for a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAccess {
    /// Read the host filesystem, writes are still restricted.
    Broad,
    /// Read only system paths and the workspace.
    Workspace,
}

impl ReadAccess {
    /// Parses a config value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "broad" => Ok(Self::Broad),
            "workspace" => Ok(Self::Workspace),
            other => Err(format!("read must be \"broad\" or \"workspace\", got {other:?}")),
        }
    }

    /// Config label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Broad => "broad",
            Self::Workspace => "workspace",
        }
    }
}

/// One validated sandbox request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSpec {
    /// Canonical primary workspace root.
    pub workspace: PathBuf,
    /// Canonical working directory inside the workspace.
    pub cwd: PathBuf,
    /// Network policy.
    pub network: NetworkAccess,
    /// Read breadth.
    pub read: ReadAccess,
    /// Additional canonical writable paths. Empty by default.
    pub write_paths: Vec<PathBuf>,
    /// Resource ceilings for the sandboxed process tree.
    pub limits: crate::ProcessLimits,
}

impl SandboxSpec {
    /// Validates and canonicalizes a workspace/cwd pair.
    ///
    /// The workspace and cwd must exist, be directories, and the cwd must stay
    /// inside the workspace. Additional write paths are canonicalized and must
    /// be inside the workspace until an explicit grant phase widens them.
    pub fn new(workspace: &Path, cwd: &Path) -> Result<Self, SandboxError> {
        let workspace = canonical_dir(workspace, "workspace")?;
        let cwd = canonical_dir(cwd, "cwd")?;
        if !cwd.starts_with(&workspace) {
            return Err(SandboxError::Invalid(format!(
                "sandbox cwd {} escapes workspace {}",
                cwd.display(),
                workspace.display()
            )));
        }
        Ok(Self {
            workspace,
            cwd,
            network: NetworkAccess::Deny,
            read: ReadAccess::Broad,
            write_paths: Vec::new(),
            limits: crate::ProcessLimits::default(),
        })
    }

    /// Adds a canonical writable path grant. The path must already exist;
    /// absolute paths outside the workspace are allowed only when the
    /// host-global config explicitly grants them.
    pub fn with_write_path(mut self, path: &Path) -> Result<Self, SandboxError> {
        let path = canonical_dir(path, "write path")?;
        if path != self.workspace && !self.write_paths.contains(&path) {
            self.write_paths.push(path);
        }
        Ok(self)
    }

    /// Builder: network policy.
    #[must_use]
    pub fn with_network(mut self, network: NetworkAccess) -> Self {
        self.network = network;
        self
    }

    /// Builder: read breadth.
    #[must_use]
    pub fn with_read(mut self, read: ReadAccess) -> Self {
        self.read = read;
        self
    }

    /// Builder: resource limits.
    #[must_use]
    pub fn with_limits(mut self, limits: crate::ProcessLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// One fully constructed sandbox command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommand {
    /// Program to execute (bwrap, sandbox-exec, or a direct command).
    pub program: PathBuf,
    /// Arguments passed to the program.
    pub args: Vec<String>,
    /// Environment overlaid on the caller's process environment.
    pub env: Vec<(String, String)>,
}

impl SandboxCommand {
    /// Adds or replaces one environment value.
    pub fn set_env(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        if let Some(existing) = self.env.iter_mut().find(|(key, _)| *key == name) {
            existing.1 = value.into();
        } else {
            self.env.push((name, value.into()));
        }
    }
}

/// Sandbox construction or probe failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// Invalid policy or path input.
    Invalid(String),
    /// The backend is unavailable.
    Unavailable(String),
    /// The backend is not supported on this platform.
    Unsupported(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid sandbox request: {message}"),
            Self::Unavailable(message) => write!(formatter, "sandbox unavailable: {message}"),
            Self::Unsupported(message) => write!(formatter, "sandbox unsupported: {message}"),
        }
    }
}

impl std::error::Error for SandboxError {}

fn canonical_dir(path: &Path, label: &str) -> Result<PathBuf, SandboxError> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        SandboxError::Invalid(format!("{label} {} cannot be resolved: {error}", path.display()))
    })?;
    if !canonical.is_dir() {
        return Err(SandboxError::Invalid(format!(
            "{label} {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// Resolves the workspace Git metadata path when it exists.
///
/// Only a regular directory or worktree pointer file is returned. Symlinks are
/// left alone: a symlinked `.git` is outside the workspace's writable bind and
/// therefore already read-only unless its target is inside the workspace.
#[must_use]
pub fn git_metadata_path(workspace: &Path) -> Option<PathBuf> {
    let path = workspace.join(".git");
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    (metadata.is_dir() || metadata.is_file()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_requires_cwd_inside_canonical_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("src");
        std::fs::create_dir(&child).unwrap();
        let spec = SandboxSpec::new(temp.path(), &child).unwrap();
        assert_eq!(spec.cwd, std::fs::canonicalize(child).unwrap());
        assert!(SandboxSpec::new(temp.path(), Path::new("/")).is_err());
    }

    #[test]
    fn write_paths_are_canonical_grants() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("cache");
        std::fs::create_dir(&child).unwrap();
        let spec = SandboxSpec::new(temp.path(), temp.path()).unwrap();
        assert!(spec.clone().with_write_path(&child).is_ok());
        assert!(
            spec.with_write_path(Path::new("/tmp")).is_ok(),
            "host-global grants may leave the workspace"
        );
        assert!(
            SandboxSpec::new(temp.path(), temp.path())
                .unwrap()
                .with_write_path(&temp.path().join("missing"))
                .is_err(),
            "grant paths must exist and canonicalize"
        );
    }

    #[test]
    fn git_metadata_detects_directory_and_pointer_file() {
        let temp = tempfile::tempdir().unwrap();
        assert!(git_metadata_path(temp.path()).is_none());
        std::fs::write(temp.path().join(".git"), "gitdir: /tmp/worktree\n").unwrap();
        assert_eq!(git_metadata_path(temp.path()), Some(temp.path().join(".git")));
    }

    #[test]
    fn config_values_round_trip() {
        assert_eq!(NetworkAccess::parse("deny"), Ok(NetworkAccess::Deny));
        assert_eq!(ReadAccess::parse("workspace"), Ok(ReadAccess::Workspace));
        assert!(NetworkAccess::parse("sometimes").is_err());
    }
}
