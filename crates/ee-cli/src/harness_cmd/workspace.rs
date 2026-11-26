//! Isolated rollout workspace staging.
//!
//! Harness2 stages only the evidence a proposer needs and gives each session a
//! private working directory; it has no global workspace file/byte budget. ee
//! previously copied the user workspace with hard 512-file / 8 MiB limits,
//! sorted by path, so target crates sorted after the cutoff never reached the
//! session. This module keeps the isolation boundary but copies the complete
//! tracked (and non-ignored untracked) workspace instead.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use ee_agent_harnessing::HarnessError;

/// Directories that never belong to a rollout workspace.
const IGNORED: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".atlas",
    "dist",
    "build",
    ".agents",
    // Vendored reference checkout with nested git data; never stage it.
    "harness-example",
];

/// Maximum directory depth for the non-git fallback walk.
const MAX_FALLBACK_DEPTH: usize = 64;

/// Copies the workspace into `target`.
///
/// Git workspaces use the index plus non-ignored untracked files, so the copy
/// includes current work without pulling in build output. Non-git workspaces
/// fall back to a full bounded-depth walk. There is deliberately no global
/// file or byte budget: a path cutoff is what made the target crate invisible
/// to earlier harness runs.
pub(super) fn copy_workspace(source: &Path, target: &Path) -> Result<(), HarnessError> {
    std::fs::create_dir_all(target)?;
    if copy_git_workspace(source, target)? {
        return Ok(());
    }
    copy_directory(source, target, source, 0)
}

fn copy_git_workspace(source: &Path, target: &Path) -> Result<bool, HarnessError> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Ok(false),
    };
    if output.stdout.is_empty() {
        return Ok(false);
    }
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let relative = PathBuf::from(String::from_utf8_lossy(raw).into_owned());
        if !safe_relative(&relative) || is_ignored(&relative) {
            continue;
        }
        copy_file(&source.join(&relative), &target.join(&relative))?;
    }
    Ok(true)
}

fn copy_directory(
    root: &Path,
    target: &Path,
    directory: &Path,
    depth: usize,
) -> Result<(), HarnessError> {
    if depth > MAX_FALLBACK_DEPTH {
        return Ok(());
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    let mut paths = entries.filter_map(Result::ok).map(|entry| entry.path()).collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if IGNORED.contains(&name) {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let destination = target.join(relative);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else { continue };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            std::fs::create_dir_all(&destination)?;
            copy_directory(root, target, &path, depth + 1)?;
        } else if metadata.is_file() {
            copy_file(&path, &destination)?;
        }
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), HarnessError> {
    let Ok(metadata) = std::fs::symlink_metadata(source) else { return Ok(()) };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, destination)?;
    Ok(())
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| matches!(component, Component::Normal(_)))
}

fn is_ignored(path: &Path) -> bool {
    path.components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|first| IGNORED.contains(&first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_more_than_the_old_file_budget() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::create_dir_all(source.join("crates/ee-cli/src")).expect("source");
        for index in 0..520 {
            std::fs::write(source.join(format!("crates/ee-cli/src/file-{index}.rs")), "x\n")
                .expect("file");
        }
        copy_workspace(&source, &target).expect("copy");
        assert_eq!(
            std::fs::read_dir(target.join("crates/ee-cli/src")).expect("copied").count(),
            520
        );
    }

    #[test]
    fn copies_files_larger_than_the_old_byte_budget() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::write(source.join("big.txt"), vec![b'x'; 9 * 1024 * 1024]).expect("big");
        copy_workspace(&source, &target).expect("copy");
        assert_eq!(
            std::fs::metadata(target.join("big.txt")).expect("copied").len(),
            9 * 1024 * 1024
        );
    }

    #[test]
    fn ignores_private_and_byproduct_directories() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::create_dir_all(source.join("src")).expect("source");
        std::fs::create_dir_all(source.join(".agents")).expect("agents");
        std::fs::create_dir_all(source.join("target")).expect("target dir");
        std::fs::write(source.join("src/lib.rs"), "pub fn one() {}\n").expect("source file");
        std::fs::write(source.join(".agents/guardrails.md"), "rule\n").expect("agents file");
        std::fs::write(source.join("target/out"), "ignored\n").expect("target file");
        copy_workspace(&source, &target).expect("copy");
        assert!(target.join("src/lib.rs").is_file());
        assert!(!target.join(".agents").exists());
        assert!(!target.join("target").exists());
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::write(source.join("real.txt"), "real\n").expect("real");
        symlink(source.join("real.txt"), source.join("link.txt")).expect("symlink");
        copy_workspace(&source, &target).expect("copy");
        assert!(target.join("real.txt").is_file());
        assert!(!target.join("link.txt").exists());
    }

    #[test]
    fn git_workspaces_include_tracked_and_untracked_but_not_ignored_files() {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::write(source.join(".gitignore"), "ignored.txt\n").expect("ignore");
        std::fs::write(source.join("tracked.txt"), "tracked\n").expect("tracked");
        std::fs::write(source.join("untracked.txt"), "untracked\n").expect("untracked");
        std::fs::write(source.join("ignored.txt"), "ignored\n").expect("ignored");
        std::fs::create_dir_all(source.join("aaa")).expect("filler dir");
        for index in 0..520 {
            std::fs::write(source.join(format!("aaa/file-{index}.rs")), "x\n").expect("filler");
        }
        std::fs::create_dir_all(source.join("crates/ee-cli")).expect("target dir");
        std::fs::write(source.join("crates/ee-cli/Cargo.toml"), "[package]\n")
            .expect("target file");
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&source)
            .status()
            .expect("git init");
        assert!(status.success());
        let status = Command::new("git")
            .args(["add", ".gitignore", "tracked.txt", "aaa", "crates"])
            .current_dir(&source)
            .status()
            .expect("git add");
        assert!(status.success());
        copy_workspace(&source, &target).expect("copy");
        assert!(target.join("tracked.txt").is_file());
        assert!(target.join("untracked.txt").is_file());
        assert!(!target.join("ignored.txt").exists());
        assert!(target.join("aaa/file-519.rs").is_file());
        assert!(target.join("crates/ee-cli/Cargo.toml").is_file());
    }
}
