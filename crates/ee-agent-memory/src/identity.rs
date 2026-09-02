use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::MemoryError;

const DOMAIN_SEPARATOR: &[u8] = b"ee.workspace.v1\0";

/// Canonical identity for one existing workspace directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceIdentity {
    canonical_root: PathBuf,
    digest: String,
}

impl WorkspaceIdentity {
    /// Canonicalizes an existing directory and hashes OS-native encoded bytes.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let canonical_root = std::fs::canonicalize(root.as_ref())
            .map_err(|_| MemoryError::InvalidWorkspace("workspace cannot be canonicalized"))?;
        if !canonical_root.is_dir() {
            return Err(MemoryError::InvalidWorkspace("workspace root is not a directory"));
        }
        let mut hasher = Sha256::new();
        hasher.update(DOMAIN_SEPARATOR);
        hasher.update(canonical_root.as_os_str().as_encoded_bytes());
        let digest = format!("sha256:{:x}", hasher.finalize());
        Ok(Self { canonical_root, digest })
    }

    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// Explicit sorted, deduplicated set of canonical workspace roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceRootSet(Vec<WorkspaceIdentity>);

impl WorkspaceRootSet {
    pub fn new(roots: impl IntoIterator<Item = impl AsRef<Path>>) -> Result<Self, MemoryError> {
        let mut unique = BTreeMap::new();
        for root in roots {
            let identity = WorkspaceIdentity::new(root)?;
            unique.insert(identity.digest.clone(), identity);
        }
        Ok(Self(unique.into_values().collect()))
    }

    #[must_use]
    pub fn roots(&self) -> &[WorkspaceIdentity] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn aliases_and_root_sets_are_canonical() {
        let temp = tempdir().unwrap();
        let nested = temp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let alias = nested.join("..").join("nested");
        let first = WorkspaceIdentity::new(&nested).unwrap();
        let second = WorkspaceIdentity::new(alias).unwrap();
        assert_eq!(first, second);
        let roots = WorkspaceRootSet::new([&nested, &nested]).unwrap();
        assert_eq!(roots.roots().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_share_identity() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let link = temp.path().join("link");
        symlink(&root, &link).unwrap();

        assert_eq!(WorkspaceIdentity::new(root).unwrap(), WorkspaceIdentity::new(link).unwrap());
    }

    #[cfg(all(unix, not(target_vendor = "apple")))]
    #[test]
    fn non_utf8_aliases_share_identity() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};

        let temp = tempdir().unwrap();
        let raw = std::ffi::OsString::from_vec(b"root-\xff".to_vec());
        let root = temp.path().join(raw);
        std::fs::create_dir(&root).unwrap();
        let link = temp.path().join("link");
        symlink(&root, &link).unwrap();

        assert_eq!(WorkspaceIdentity::new(root).unwrap(), WorkspaceIdentity::new(link).unwrap());
    }

    #[test]
    fn missing_and_file_roots_fail_closed() {
        let temp = tempdir().unwrap();
        assert!(WorkspaceIdentity::new(temp.path().join("missing")).is_err());
        let file = temp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(WorkspaceIdentity::new(file).is_err());
    }
}
