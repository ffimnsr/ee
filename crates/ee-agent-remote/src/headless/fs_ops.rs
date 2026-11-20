//! Bounded filesystem primitives for the headless handler.
//!
//! Every path crossing this module is absolute, canonicalized, and contained
//! by one of the configured roots. Reads are line/byte bounded; writes are
//! byte bounded and gated by the approval broker before touching disk.

use std::path::{Component, Path, PathBuf};

use ee_agent_host::AgentError;

/// Largest file the headless handler will read without an explicit line range.
pub const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
/// Largest write the headless handler accepts.
pub const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;
/// Largest response produced by a bounded read.
pub const MAX_READ_RESPONSE_BYTES: usize = 1024 * 1024;

/// Configured workspace roots, canonicalized at construction.
#[derive(Debug, Clone, Default)]
pub struct PathPolicy {
    roots: Vec<PathBuf>,
}

impl PathPolicy {
    /// Canonicalizes every root that exists. Missing roots are dropped; an
    /// empty policy denies every path.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let roots = roots
            .into_iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .filter(|root| root.is_dir())
            .collect();
        Self { roots }
    }

    /// The canonical roots.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether any root is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    fn contains(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
    }

    fn deny_outside(&self) -> AgentError {
        AgentError::PermissionDenied {
            reason: String::from("path is outside the configured workspace roots"),
        }
    }

    /// Resolves an existing path (file or directory) and requires containment.
    ///
    /// # Errors
    ///
    /// `InvalidParams` for relative paths, `PermissionDenied` for symlink
    /// escapes or paths outside every root, `Io` for canonicalization failure.
    pub fn contain_existing(&self, path: &Path) -> Result<PathBuf, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::InvalidParams(format!(
                "path must be absolute, got {}",
                path.display()
            )));
        }
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| AgentError::Io(format!("{}: {error}", path.display())))?;
        if !self.contains(&canonical) {
            return Err(self.deny_outside());
        }
        Ok(canonical)
    }

    /// Resolves a directory that must exist and be contained.
    ///
    /// # Errors
    ///
    /// Same as [`Self::contain_existing`], plus `InvalidParams` when the path
    /// is not a directory.
    pub fn contain_directory(&self, path: &Path) -> Result<PathBuf, AgentError> {
        let canonical = self.contain_existing(path)?;
        if !canonical.is_dir() {
            return Err(AgentError::InvalidParams(format!(
                "{} is not a directory",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    /// Resolves a path that may not exist yet. The parent must exist and be
    /// contained; the final component must be a normal file name.
    ///
    /// # Errors
    ///
    /// `InvalidParams` for relative paths or invalid file names,
    /// `PermissionDenied` for escapes, `Io` for canonicalization failure.
    pub fn contain_new(&self, path: &Path) -> Result<PathBuf, AgentError> {
        if !path.is_absolute() {
            return Err(AgentError::InvalidParams(format!(
                "path must be absolute, got {}",
                path.display()
            )));
        }
        if path.exists() {
            return self.contain_existing(path);
        }
        let parent = path.parent().ok_or_else(|| {
            AgentError::InvalidParams(format!("{} has no parent directory", path.display()))
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            AgentError::InvalidParams(format!("{} has no file name", path.display()))
        })?;
        if !matches!(Path::new(file_name).components().next(), Some(Component::Normal(_))) {
            return Err(AgentError::InvalidParams(String::from(
                "target file name must be a normal path component",
            )));
        }
        let canonical_parent = std::fs::canonicalize(parent)
            .map_err(|error| AgentError::Io(format!("{}: {error}", parent.display())))?;
        if !self.contains(&canonical_parent) {
            return Err(self.deny_outside());
        }
        Ok(canonical_parent.join(file_name))
    }
}

/// Reads a contained file with ACP line/limit semantics and byte caps.
///
/// `line` is 1-based. When `limit` is absent and the file exceeds
/// [`MAX_READ_BYTES`], the read fails closed instead of silently truncating.
///
/// # Errors
///
/// `Io` for read failures, `InvalidParams` for oversized unbounded reads.
pub fn read_text(path: &Path, line: Option<u32>, limit: Option<u32>) -> Result<String, AgentError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| AgentError::Io(format!("{}: {error}", path.display())))?;
    if limit.is_none() && metadata.len() > MAX_READ_BYTES {
        return Err(AgentError::InvalidParams(format!(
            "{} is {} bytes; pass line/limit to read a bounded range",
            path.display(),
            metadata.len()
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| AgentError::Io(format!("{}: {error}", path.display())))?;
    let start = line.unwrap_or(1).max(1) as usize - 1;
    let take = limit.map(|limit| limit as usize);
    let sliced: String = match take {
        Some(take) => text.split_inclusive('\n').skip(start).take(take).collect(),
        None => text.split_inclusive('\n').skip(start).collect(),
    };
    Ok(truncate_utf8(sliced, MAX_READ_RESPONSE_BYTES))
}

/// Truncates `text` to at most `max_bytes` without splitting a character.
#[must_use]
pub fn truncate_utf8(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn reads_are_line_bounded() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("file.txt");
        fs::write(&path, "a\nb\nc\nd\n").expect("write");
        let policy = PathPolicy::new(vec![dir.path().to_path_buf()]);
        let target = policy.contain_existing(&path).expect("contained");
        assert_eq!(read_text(&target, Some(2), Some(2)).expect("read"), "b\nc\n");
        assert_eq!(read_text(&target, Some(3), None).expect("read"), "c\nd\n");
    }

    #[test]
    fn outside_paths_are_denied_even_when_the_root_exists() {
        let inside = tempdir().expect("inside");
        let outside = tempdir().expect("outside");
        let target = outside.path().join("file.txt");
        fs::write(&target, "secret").expect("write");
        let policy = PathPolicy::new(vec![inside.path().to_path_buf()]);
        let error = policy.contain_existing(&target).expect_err("outside");
        assert!(matches!(error, AgentError::PermissionDenied { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_are_denied() {
        let inside = tempdir().expect("inside");
        let outside = tempdir().expect("outside");
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "secret").expect("write");
        let link = inside.path().join("link.txt");
        std::os::unix::fs::symlink(&secret, &link).expect("symlink");
        let policy = PathPolicy::new(vec![inside.path().to_path_buf()]);
        let error = policy.contain_existing(&link).expect_err("escape");
        assert!(matches!(error, AgentError::PermissionDenied { .. }));
    }

    #[test]
    fn new_files_must_live_under_an_existing_root() {
        let inside = tempdir().expect("inside");
        let outside = tempdir().expect("outside");
        let policy = PathPolicy::new(vec![inside.path().to_path_buf()]);
        assert!(policy.contain_new(&inside.path().join("new.txt")).is_ok());
        assert!(policy.contain_new(&outside.path().join("new.txt")).is_err());
    }

    #[test]
    fn relative_paths_are_invalid() {
        let policy = PathPolicy::new(vec![PathBuf::from("/tmp")]);
        let error = policy.contain_existing(Path::new("relative.txt")).expect_err("relative");
        assert!(matches!(error, AgentError::InvalidParams(_)));
    }

    #[test]
    fn empty_policy_denies_everything() {
        let dir = tempdir().expect("dir");
        let path = dir.path().join("x");
        fs::write(&path, "x").expect("write");
        let policy = PathPolicy::default();
        assert!(policy.is_empty());
        assert!(policy.contain_existing(&path).is_err());
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let text = String::from("éééé");
        let truncated = truncate_utf8(text, 3);
        assert_eq!(truncated, "é");
    }
}
