use std::fs;
use std::path::Path;

use git2::{IndexAddOption, Repository, Signature};
use tempfile::TempDir;

use crate::git::{GitReadLimits, GitRepository};

fn fixture() -> (TempDir, Repository) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let repository = Repository::init(directory.path()).expect("initialize repository");
    fs::write(directory.path().join("tracked.txt"), "before\n").expect("write tracked file");
    fs::write(directory.path().join("staged.txt"), "base\n").expect("write staged file");
    commit_all(&repository, "initial");
    (directory, repository)
}

fn commit_all(repository: &Repository, message: &str) {
    let mut index = repository.index().expect("repository index");
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None).expect("stage files");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repository.find_tree(tree_id).expect("find tree");
    let signature = Signature::now("Test Author", "test@example.invalid").expect("signature");
    let parents = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .and_then(|id| repository.find_commit(id).ok())
        .into_iter()
        .collect::<Vec<_>>();
    let parent_refs = parents.iter().collect::<Vec<_>>();
    repository
        .commit(Some("HEAD"), &signature, &signature, message, &tree, &parent_refs)
        .expect("commit");
}

fn stage(repository: &Repository, path: &str) {
    let mut index = repository.index().expect("repository index");
    index.add_path(Path::new(path)).expect("stage path");
    index.write().expect("write index");
}

fn modify_fixture(repository: &Repository, root: &Path) {
    fs::write(root.join("tracked.txt"), "worktree\n").expect("modify tracked file");
    fs::write(root.join("staged.txt"), "index\n").expect("write staged version");
    stage(repository, "staged.txt");
    fs::write(root.join("staged.txt"), "index and worktree\n").expect("modify staged file");
    fs::write(root.join("untracked.txt"), "new\n").expect("write untracked file");
}

#[test]
fn git_status_reports_canonical_root_and_status_categories() {
    let (directory, repository) = fixture();
    modify_fixture(&repository, directory.path());
    fs::create_dir(directory.path().join("nested")).expect("create nested directory");
    let git =
        GitRepository::discover(&directory.path().join("nested")).expect("discover repository");
    let git = git.expect("repository found");

    let status = git.status(GitReadLimits::default()).expect("read status");

    assert_eq!(git.root(), directory.path().canonicalize().expect("canonical root"));
    assert!(status.branch.is_some());
    assert!(!status.detached);
    assert!(status.staged.contains(&Path::new("staged.txt").to_path_buf()));
    assert!(status.unstaged.contains(&Path::new("tracked.txt").to_path_buf()));
    assert!(status.unstaged.contains(&Path::new("staged.txt").to_path_buf()));
    assert!(status.untracked.contains(&Path::new("untracked.txt").to_path_buf()));
    assert!(status.conflicts.is_empty());
    assert!(!status.truncated);
}

#[test]
fn git_status_marks_detached_head_and_file_limit_truncation() {
    let (directory, repository) = fixture();
    modify_fixture(&repository, directory.path());
    let head = repository.head().expect("head").target().expect("head target");
    repository.set_head_detached(head).expect("detach head");
    let git = GitRepository::discover(directory.path())
        .expect("discover repository")
        .expect("repository found");

    let status = git
        .status(GitReadLimits { max_status_files: 1, max_diff_bytes: 1024 })
        .expect("read status");

    assert!(status.detached);
    assert_eq!(status.branch, None);
    assert_eq!(status.file_limit, 1);
    assert_eq!(status.returned_file_count, 1);
    assert!(status.total_file_count > status.returned_file_count);
    assert_eq!(status.omitted_file_count, status.total_file_count - status.returned_file_count);
    assert!(status.truncated);
}

#[test]
fn git_diffs_are_path_scoped_staged_and_byte_bounded() {
    let (directory, repository) = fixture();
    modify_fixture(&repository, directory.path());
    let git = GitRepository::discover(directory.path())
        .expect("discover repository")
        .expect("repository found");
    let limits = GitReadLimits { max_status_files: 8, max_diff_bytes: 8 * 1024 };

    let unstaged = git.unstaged_diff(limits).expect("unstaged diff");
    assert!(unstaged.text.contains("-before"));
    assert!(unstaged.text.contains("+worktree"));
    assert!(!unstaged.truncated);

    let file_diff =
        git.unstaged_diff_for_path(Path::new("tracked.txt"), limits).expect("path-scoped diff");
    assert!(file_diff.text.contains("tracked.txt"));
    assert!(!file_diff.text.contains("staged.txt"));
    assert!(git.unstaged_diff_for_path(Path::new("../outside.txt"), limits).is_err());

    let staged = git.staged_diff(limits).expect("staged diff");
    assert!(staged.text.contains("-base"));
    assert!(staged.text.contains("+index"));

    let bounded = git
        .unstaged_diff(GitReadLimits { max_status_files: 8, max_diff_bytes: 12 })
        .expect("bounded diff");
    assert!(bounded.truncated);
    assert!(bounded.bytes_returned <= bounded.byte_limit);
}
