// Copyright 2026 The ee authors. All rights reserved.

//! Bounded workspace filesystem mutations used by approved agent tools.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

const MAX_COPY_ENTRIES: u64 = 10_000;
const MAX_COPY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_COPY_DEPTH: usize = 128;

#[derive(Debug, Clone)]
pub(crate) enum FilesystemOperation {
    CreateDirectory { path: PathBuf },
    DeletePath { path: PathBuf },
    CopyPath { source: PathBuf, destination: PathBuf },
    MovePath { source: PathBuf, destination: PathBuf },
}

impl FilesystemOperation {
    pub(crate) fn tool_name(&self) -> &'static str {
        match self {
            Self::CreateDirectory { .. } => "ee_create_directory",
            Self::DeletePath { .. } => "ee_delete_path",
            Self::CopyPath { .. } => "ee_copy_path",
            Self::MovePath { .. } => "ee_move_path",
        }
    }

    pub(crate) fn detail(&self) -> String {
        match self {
            Self::CreateDirectory { path } | Self::DeletePath { path } => {
                path.display().to_string()
            }
            Self::CopyPath { source, destination } | Self::MovePath { source, destination } => {
                format!("{} → {}", source.display(), destination.display())
            }
        }
    }

    pub(crate) fn fingerprint(&self) -> String {
        match self {
            Self::CreateDirectory { path } => format!("create-directory:{}", path.display()),
            Self::DeletePath { path } => format!("delete-path:{}", path.display()),
            Self::CopyPath { source, destination } => {
                format!("copy-path:{}:{}", source.display(), destination.display())
            }
            Self::MovePath { source, destination } => {
                format!("move-path:{}:{}", source.display(), destination.display())
            }
        }
    }

    pub(crate) fn affected_open_path(&self, open_path: &Path) -> bool {
        match self {
            Self::DeletePath { path } | Self::MovePath { source: path, .. } => {
                paths_overlap(path, open_path)
            }
            Self::CreateDirectory { .. } | Self::CopyPath { .. } => false,
        }
    }
}

pub(crate) fn validate(operation: &FilesystemOperation, roots: &[PathBuf]) -> io::Result<()> {
    if roots.is_empty() {
        return Err(invalid("no active workspace root"));
    }
    let roots = roots.iter().map(fs::canonicalize).collect::<io::Result<Vec<_>>>()?;

    match operation {
        FilesystemOperation::CreateDirectory { path } => {
            let candidate = workspace_candidate(path, &roots)?;
            if path.exists() && !path.is_dir() {
                return Err(invalid("directory target exists and is not a directory"));
            }
            ensure_not_root(&candidate, &roots, "create workspace root")?;
        }
        FilesystemOperation::DeletePath { path } => {
            let candidate = existing_workspace_path(path, &roots)?;
            ensure_not_root(&candidate, &roots, "delete workspace root")?;
        }
        FilesystemOperation::CopyPath { source, destination }
        | FilesystemOperation::MovePath { source, destination } => {
            let source = existing_workspace_path(source, &roots)?;
            reject_symlink(&source)?;
            ensure_not_root(&source, &roots, "copy or move workspace root")?;
            if destination.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("destination already exists: {}", destination.display()),
                ));
            }
            let parent = destination
                .parent()
                .ok_or_else(|| invalid("destination has no parent directory"))?;
            if !parent.is_dir() {
                return Err(invalid(format!(
                    "destination parent does not exist: {}",
                    parent.display()
                )));
            }
            let destination = workspace_candidate(destination, &roots)?;
            if destination.starts_with(&source) {
                return Err(invalid("destination cannot be inside source"));
            }
            if matches!(operation, FilesystemOperation::CopyPath { .. }) {
                inspect_copy_tree(&source, 0, &mut CopyBudget::default())?;
            }
        }
    }
    Ok(())
}

pub(crate) fn execute(
    operation: &FilesystemOperation,
    roots: &[PathBuf],
) -> io::Result<ee_mcp::FilesystemResult> {
    validate(operation, roots)?;
    match operation {
        FilesystemOperation::CreateDirectory { path } => {
            fs::create_dir_all(path)?;
            Ok(result(path, None))
        }
        FilesystemOperation::DeletePath { path } => {
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
            Ok(result(path, None))
        }
        FilesystemOperation::CopyPath { source, destination } => {
            let copy_result = copy_entry(source, destination);
            if copy_result.is_err() {
                remove_partial(destination);
            }
            copy_result?;
            Ok(result(source, Some(destination)))
        }
        FilesystemOperation::MovePath { source, destination } => {
            fs::rename(source, destination).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "atomic move failed for {} to {} (cross-device fallback disabled): {error}",
                        source.display(),
                        destination.display()
                    ),
                )
            })?;
            Ok(result(source, Some(destination)))
        }
    }
}

fn result(path: &Path, destination: Option<&Path>) -> ee_mcp::FilesystemResult {
    ee_mcp::FilesystemResult {
        path: path.display().to_string(),
        destination_path: destination.map(|path| path.display().to_string()),
    }
}

fn workspace_candidate(path: &Path, roots: &[PathBuf]) -> io::Result<PathBuf> {
    validate_absolute_components(path)?;
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| invalid(format!("path has no existing ancestor: {}", path.display())))?;
        suffix.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| invalid(format!("path has no parent: {}", path.display())))?;
    }
    let mut candidate = fs::canonicalize(ancestor)?;
    for component in suffix.iter().rev() {
        candidate.push(component);
    }
    ensure_in_roots(&candidate, roots)?;
    Ok(candidate)
}

fn existing_workspace_path(path: &Path, roots: &[PathBuf]) -> io::Result<PathBuf> {
    validate_absolute_components(path)?;
    let candidate = fs::canonicalize(path)?;
    ensure_in_roots(&candidate, roots)?;
    Ok(candidate)
}

fn validate_absolute_components(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(invalid("path must be absolute"));
    }
    if path.components().any(|component| matches!(component, Component::ParentDir)) {
        return Err(invalid("path must not contain '..' components"));
    }
    Ok(())
}

fn ensure_in_roots(path: &Path, roots: &[PathBuf]) -> io::Result<()> {
    if roots.iter().any(|root| path.starts_with(root)) {
        Ok(())
    } else {
        Err(invalid(format!("path outside allowed workspace: {}", path.display())))
    }
}

fn ensure_not_root(path: &Path, roots: &[PathBuf], action: &str) -> io::Result<()> {
    if roots.iter().any(|root| path == root) {
        Err(invalid(format!("cannot {action}: {}", path.display())))
    } else {
        Ok(())
    }
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        Err(invalid(format!("symlink source is not supported: {}", path.display())))
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct CopyBudget {
    entries: u64,
    bytes: u64,
}

fn inspect_copy_tree(path: &Path, depth: usize, budget: &mut CopyBudget) -> io::Result<()> {
    if depth > MAX_COPY_DEPTH {
        return Err(invalid("copy tree exceeds maximum depth"));
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(invalid(format!("copy tree contains symlink: {}", path.display())));
    }
    budget.entries = budget.entries.saturating_add(1);
    budget.bytes = budget.bytes.saturating_add(metadata.len());
    if budget.entries > MAX_COPY_ENTRIES || budget.bytes > MAX_COPY_BYTES {
        return Err(invalid("copy tree exceeds entry or byte limit"));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            inspect_copy_tree(&entry?.path(), depth + 1, budget)?;
        }
    }
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(invalid(format!("copy tree contains symlink: {}", source.display())));
    }
    if metadata.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        let mut input = fs::File::open(source)?;
        let mut output = OpenOptions::new().write(true).create_new(true).open(destination)?;
        io::copy(&mut input, &mut output)?;
        fs::set_permissions(destination, metadata.permissions())?;
    }
    Ok(())
}

fn remove_partial(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

fn paths_overlap(container: &Path, candidate: &Path) -> bool {
    let container = fs::canonicalize(container).unwrap_or_else(|_| container.to_path_buf());
    let candidate = fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    candidate == container || candidate.starts_with(container)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn nested_directory_create_and_recursive_copy_are_bounded_to_workspace() {
        let workspace = TempDir::new().expect("workspace");
        let root = fs::canonicalize(workspace.path()).expect("canonical root");
        let nested = root.join("one/two");
        execute(
            &FilesystemOperation::CreateDirectory { path: nested.clone() },
            std::slice::from_ref(&root),
        )
        .expect("create nested directory");
        fs::write(nested.join("file.txt"), "content").expect("seed file");
        let copied = root.join("copy");
        execute(
            &FilesystemOperation::CopyPath {
                source: root.join("one"),
                destination: copied.clone(),
            },
            std::slice::from_ref(&root),
        )
        .expect("copy directory");
        assert_eq!(fs::read_to_string(copied.join("two/file.txt")).unwrap(), "content");
    }

    #[test]
    fn move_and_delete_apply_without_overwriting() {
        let workspace = TempDir::new().expect("workspace");
        let root = fs::canonicalize(workspace.path()).expect("canonical root");
        let source = root.join("source.txt");
        let destination = root.join("destination.txt");
        fs::write(&source, "content").unwrap();
        execute(
            &FilesystemOperation::MovePath {
                source: source.clone(),
                destination: destination.clone(),
            },
            std::slice::from_ref(&root),
        )
        .expect("move file");
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "content");
        execute(
            &FilesystemOperation::DeletePath { path: destination.clone() },
            std::slice::from_ref(&root),
        )
        .expect("delete file");
        assert!(!destination.exists());
    }

    #[test]
    fn delete_root_and_destination_inside_source_are_rejected() {
        let workspace = TempDir::new().expect("workspace");
        let root = fs::canonicalize(workspace.path()).expect("canonical root");
        fs::create_dir(root.join("source")).unwrap();
        assert!(
            validate(
                &FilesystemOperation::DeletePath { path: root.clone() },
                std::slice::from_ref(&root)
            )
            .is_err()
        );
        assert!(
            validate(
                &FilesystemOperation::CopyPath {
                    source: root.join("source"),
                    destination: root.join("source/nested"),
                },
                std::slice::from_ref(&root),
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_rejects_symlink_entries_during_validation_and_mutation() {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().expect("workspace");
        let root = fs::canonicalize(workspace.path()).expect("canonical root");
        fs::create_dir(root.join("source")).unwrap();
        let link = root.join("source/link");
        symlink("/tmp", &link).unwrap();
        assert!(
            validate(
                &FilesystemOperation::CopyPath {
                    source: root.join("source"),
                    destination: root.join("copy"),
                },
                std::slice::from_ref(&root),
            )
            .is_err()
        );
        assert!(copy_entry(&link, &root.join("copied-link")).is_err());
        assert!(!root.join("copied-link").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_and_outside_workspace_paths_are_rejected() {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().expect("workspace");
        let outside = TempDir::new().expect("outside");
        let root = fs::canonicalize(workspace.path()).expect("canonical root");
        let outside_file = outside.path().join("outside.txt");
        fs::write(&outside_file, "secret").unwrap();
        let link = root.join("outside-link");
        symlink(&outside_file, &link).unwrap();

        for path in [&outside_file, &link] {
            assert!(
                validate(
                    &FilesystemOperation::DeletePath { path: path.to_path_buf() },
                    std::slice::from_ref(&root),
                )
                .is_err()
            );
        }
        assert!(outside_file.exists());
        assert!(link.exists());
    }

    #[test]
    fn delete_and_move_detect_open_paths_below_source() {
        let delete = FilesystemOperation::DeletePath { path: PathBuf::from("/work/tree") };
        let move_path = FilesystemOperation::MovePath {
            source: PathBuf::from("/work/tree"),
            destination: PathBuf::from("/work/moved"),
        };
        for operation in [&delete, &move_path] {
            assert!(operation.affected_open_path(Path::new("/work/tree/file.rs")));
            assert!(!operation.affected_open_path(Path::new("/work/other/file.rs")));
        }
    }
}
