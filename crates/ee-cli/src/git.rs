use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use git2::{Diff, DiffFormat, DiffOptions, ErrorCode, Repository, Status, StatusOptions};
use similar::{ChangeTag, TextDiff};

pub(crate) const DEFAULT_GIT_STATUS_FILE_LIMIT: usize = 512;
pub(crate) const DEFAULT_GIT_DIFF_BYTE_LIMIT: usize = 256 * 1024;

/// Per-request output limits for read-only Git operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GitReadLimits {
    pub(crate) max_status_files: usize,
    pub(crate) max_diff_bytes: usize,
}

impl Default for GitReadLimits {
    fn default() -> Self {
        Self {
            max_status_files: DEFAULT_GIT_STATUS_FILE_LIMIT,
            max_diff_bytes: DEFAULT_GIT_DIFF_BYTE_LIMIT,
        }
    }
}

/// Canonical working-tree identity for library-backed Git reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitRepository {
    root: PathBuf,
}

#[allow(dead_code)] // `staged_diff` stays library-backed for a future optional MCP tool.
impl GitRepository {
    /// Discovers repository containing `path`. A non-repository path returns `Ok(None)`.
    pub(crate) fn discover(path: &Path) -> Result<Option<Self>, git2::Error> {
        let search_path = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        let repository = match Repository::discover(search_path) {
            Ok(repository) => repository,
            Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let root =
            repository.workdir().unwrap_or(repository.path()).canonicalize().map_err(|error| {
                git2::Error::from_str(&format!("cannot canonicalize Git repository root: {error}"))
            })?;
        Ok(Some(Self { root }))
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn status(&self, limits: GitReadLimits) -> Result<GitStatusReport, git2::Error> {
        let repository = self.open()?;
        let mut options = StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_unmodified(false)
            .renames_head_to_index(true)
            .renames_index_to_workdir(true);
        let statuses = repository.statuses(Some(&mut options))?;
        let total_file_count = statuses.len();
        let mut report = GitStatusReport {
            repo_root: self.root.clone(),
            branch: branch_from_repository(&repository)?,
            detached: repository.head_detached()?,
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            conflicts: Vec::new(),
            file_limit: limits.max_status_files,
            returned_file_count: total_file_count.min(limits.max_status_files),
            total_file_count,
            omitted_file_count: total_file_count.saturating_sub(limits.max_status_files),
            truncated: total_file_count > limits.max_status_files,
        };

        for entry in statuses.iter().take(limits.max_status_files) {
            let Some(path) = entry.path() else { continue };
            let path = PathBuf::from(path);
            let status = entry.status();
            if status.intersects(
                Status::INDEX_NEW
                    | Status::INDEX_MODIFIED
                    | Status::INDEX_DELETED
                    | Status::INDEX_RENAMED
                    | Status::INDEX_TYPECHANGE,
            ) {
                report.staged.push(path.clone());
            }
            if status.intersects(
                Status::WT_MODIFIED
                    | Status::WT_DELETED
                    | Status::WT_RENAMED
                    | Status::WT_TYPECHANGE,
            ) {
                report.unstaged.push(path.clone());
            }
            if status.contains(Status::WT_NEW) {
                report.untracked.push(path.clone());
            }
            if status.contains(Status::CONFLICTED) {
                report.conflicts.push(path);
            }
        }

        Ok(report)
    }

    pub(crate) fn unstaged_diff(&self, limits: GitReadLimits) -> Result<GitDiff, git2::Error> {
        self.unstaged_diff_for_relative_path(None, limits)
    }

    /// Produces an unstaged diff for one repository-relative path.
    pub(crate) fn unstaged_diff_for_path(
        &self,
        path: &Path,
        limits: GitReadLimits,
    ) -> Result<GitDiff, git2::Error> {
        let path = self.relative_path(path)?;
        self.unstaged_diff_for_relative_path(Some(&path), limits)
    }

    pub(crate) fn staged_diff(&self, limits: GitReadLimits) -> Result<GitDiff, git2::Error> {
        let repository = self.open()?;
        let index = repository.index()?;
        let head_tree = repository.head().ok().and_then(|head| head.peel_to_tree().ok());
        let diff = repository.diff_tree_to_index(head_tree.as_ref(), Some(&index), None)?;
        render_git_diff(&diff, limits.max_diff_bytes)
    }

    fn open(&self) -> Result<Repository, git2::Error> {
        Repository::open(&self.root)
    }

    fn unstaged_diff_for_relative_path(
        &self,
        path: Option<&Path>,
        limits: GitReadLimits,
    ) -> Result<GitDiff, git2::Error> {
        let repository = self.open()?;
        let index = repository.index()?;
        let mut options = DiffOptions::new();
        if let Some(path) = path {
            options.pathspec(path.to_string_lossy().as_ref());
        }
        let diff = repository.diff_index_to_workdir(Some(&index), Some(&mut options))?;
        render_git_diff(&diff, limits.max_diff_bytes)
    }

    fn relative_path(&self, path: &Path) -> Result<PathBuf, git2::Error> {
        let path = if path.is_absolute() {
            let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            path.strip_prefix(&self.root)
                .map_err(|_| git2::Error::from_str("Git path is outside repository root"))?
                .to_path_buf()
        } else {
            path.to_path_buf()
        };

        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(component) => relative.push(component),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(git2::Error::from_str("Git path must be repository-relative"));
                }
            }
        }
        if relative.as_os_str().is_empty() {
            return Err(git2::Error::from_str("Git path must name a file"));
        }
        Ok(relative)
    }
}

/// Bounded working-tree status. File paths are relative to `repo_root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitStatusReport {
    pub(crate) repo_root: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) detached: bool,
    pub(crate) staged: Vec<PathBuf>,
    pub(crate) unstaged: Vec<PathBuf>,
    pub(crate) untracked: Vec<PathBuf>,
    pub(crate) conflicts: Vec<PathBuf>,
    pub(crate) file_limit: usize,
    pub(crate) returned_file_count: usize,
    pub(crate) total_file_count: usize,
    pub(crate) omitted_file_count: usize,
    pub(crate) truncated: bool,
}

/// Bounded unified diff output.
#[allow(dead_code)] // `staged_diff` keeps this shared type ready for optional future exposure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitDiff {
    pub(crate) text: String,
    pub(crate) bytes_returned: usize,
    pub(crate) byte_limit: usize,
    pub(crate) truncated: bool,
}

fn branch_from_repository(repository: &Repository) -> Result<Option<String>, git2::Error> {
    let head = match repository.head() {
        Ok(head) => head,
        Err(error) if error.code() == ErrorCode::UnbornBranch => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok((!repository.head_detached()? && head.is_branch())
        .then(|| head.shorthand().map(str::to_owned))
        .flatten())
}

#[allow(dead_code)]
fn render_git_diff(diff: &Diff<'_>, byte_limit: usize) -> Result<GitDiff, git2::Error> {
    let mut text = String::new();
    let mut truncated = false;
    let result = diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        let mut content = String::new();
        if matches!(line.origin(), '+' | '-' | ' ') {
            content.push(line.origin());
        }
        content.push_str(&String::from_utf8_lossy(line.content()));
        let remaining = byte_limit.saturating_sub(text.len());
        if content.len() <= remaining {
            text.push_str(&content);
            true
        } else {
            let mut end = remaining;
            while end > 0 && !content.is_char_boundary(end) {
                end -= 1;
            }
            text.push_str(&content[..end]);
            truncated = true;
            false
        }
    });
    if let Err(error) = result
        && !truncated
    {
        return Err(error);
    }

    Ok(GitDiff { bytes_returned: text.len(), text, byte_limit, truncated })
}

#[derive(Debug, Clone)]
pub(crate) struct GitBufferCache {
    pub(crate) fingerprint: u64,
    pub(crate) path: Option<PathBuf>,
    pub(crate) last_refresh: Instant,
    pub(crate) status: Option<GitBufferStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GitSign {
    Added,
    Modified,
    Deleted,
}

impl GitSign {
    pub(crate) fn marker(self) -> char {
        match self {
            GitSign::Added => '+',
            GitSign::Modified => '~',
            GitSign::Deleted => '-',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffLineKind {
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffLine {
    pub(crate) kind: DiffLineKind,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitHunk {
    pub(crate) old_start: usize,
    pub(crate) old_count: usize,
    pub(crate) new_start: usize,
    pub(crate) new_count: usize,
    pub(crate) display_line: usize,
    pub(crate) sign: GitSign,
    pub(crate) lines: Vec<DiffLine>,
}

impl GitHunk {
    pub(crate) fn contains_line(&self, line: usize) -> bool {
        if self.new_count == 0 {
            line == self.display_line
        } else {
            line >= self.new_start && line < self.new_start + self.new_count
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitBufferStatus {
    pub(crate) repo_root: PathBuf,
    pub(crate) repo_name: String,
    pub(crate) repo_relative: String,
    pub(crate) branch: String,
    pub(crate) tracked: bool,
    pub(crate) dirty: bool,
    pub(crate) hunks: Vec<GitHunk>,
    pub(crate) line_signs: HashMap<usize, GitSign>,
}

impl GitBufferStatus {
    pub(crate) fn sign_for_line(&self, line: usize) -> Option<GitSign> {
        self.line_signs.get(&line).copied()
    }

    pub(crate) fn hunk_at_line(&self, line: usize) -> Option<&GitHunk> {
        self.hunks.iter().find(|hunk| hunk.contains_line(line))
    }

    pub(crate) fn next_hunk_line(&self, cursor_line: usize) -> Option<usize> {
        self.hunks
            .iter()
            .find(|hunk| hunk.display_line > cursor_line)
            .map(|hunk| hunk.display_line)
            .or_else(|| self.hunks.first().map(|hunk| hunk.display_line))
    }

    pub(crate) fn prev_hunk_line(&self, cursor_line: usize) -> Option<usize> {
        self.hunks
            .iter()
            .rev()
            .find(|hunk| hunk.display_line < cursor_line)
            .map(|hunk| hunk.display_line)
            .or_else(|| self.hunks.last().map(|hunk| hunk.display_line))
    }

    pub(crate) fn first_hunk_line(&self) -> Option<usize> {
        self.hunks.first().map(|hunk| hunk.display_line)
    }

    pub(crate) fn last_hunk_line(&self) -> Option<usize> {
        self.hunks.last().map(|hunk| hunk.display_line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitBlameInfo {
    pub(crate) commit: String,
    pub(crate) author: String,
    pub(crate) summary: String,
    pub(crate) author_time: Option<String>,
}

pub(crate) fn buffer_fingerprint(path: Option<&Path>, lines: &[String]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    for line in lines {
        line.hash(&mut hasher);
    }
    hasher.finish()
}

pub(crate) fn inspect_buffer(
    path: &Path,
    current_lines: &[String],
) -> io::Result<Option<GitBufferStatus>> {
    let Some(repository) = GitRepository::discover(path).map_err(git_error)? else {
        return Ok(None);
    };
    let repo_relative = match repository.relative_path(path) {
        Ok(path) => normalize_pathspec(&path),
        Err(_) => return Ok(None),
    };
    let branch = branch_name(&repository).unwrap_or_else(|_| String::from("HEAD"));
    let tracked_blob = read_head_blob(&repository, &repo_relative)?;
    let tracked = tracked_blob.is_some();
    let base_lines = tracked_blob.unwrap_or_default();
    let (hunks, line_signs) = diff_hunks(&base_lines, current_lines);

    Ok(Some(GitBufferStatus {
        repo_name: repository
            .root()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repo")
            .to_owned(),
        repo_root: repository.root().to_path_buf(),
        repo_relative,
        branch,
        tracked,
        dirty: !hunks.is_empty() || !tracked,
        hunks,
        line_signs,
    }))
}

pub(crate) fn blame_line(path: &Path, line: usize) -> io::Result<Option<GitBlameInfo>> {
    let Some(repository) = GitRepository::discover(path).map_err(git_error)? else {
        return Ok(None);
    };
    let repo_relative = match repository.relative_path(path) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let repository_handle = repository.open().map_err(git_error)?;
    let mut options = git2::BlameOptions::new();
    options.min_line(line + 1).max_line(line + 1);
    let blame = match repository_handle.blame_file(&repo_relative, Some(&mut options)) {
        Ok(blame) => blame,
        Err(_) => return Ok(None),
    };
    let Some(hunk) = blame.get_line(line + 1) else {
        return Ok(None);
    };
    let commit = hunk.final_commit_id();
    let author = hunk.final_signature().name().unwrap_or_default().to_owned();
    let author_time = Some(hunk.final_signature().when().seconds().to_string());
    let summary = repository_handle
        .find_commit(commit)
        .ok()
        .and_then(|commit| commit.summary().map(str::to_owned))
        .unwrap_or_default();

    Ok(Some(GitBlameInfo { commit: commit.to_string(), author, summary, author_time }))
}

pub(crate) fn render_diff(status: &GitBufferStatus, hunk: Option<&GitHunk>) -> String {
    let old_path = if status.tracked {
        format!("a/{}", status.repo_relative)
    } else {
        String::from("/dev/null")
    };
    let new_path = format!("b/{}", status.repo_relative);

    let mut out = String::new();
    out.push_str(&format!("diff --git {old_path} {new_path}\n"));
    out.push_str(&format!("--- {old_path}\n"));
    out.push_str(&format!("+++ {new_path}\n"));

    let hunks = hunk.map(|single| vec![single.clone()]).unwrap_or_else(|| status.hunks.clone());

    for item in hunks {
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            format_hunk_range(item.old_start, item.old_count),
            format_hunk_range(item.new_start, item.new_count)
        ));
        for line in item.lines {
            let prefix = match line.kind {
                DiffLineKind::Added => '+',
                DiffLineKind::Removed => '-',
            };
            out.push(prefix);
            out.push_str(&line.text);
            out.push('\n');
        }
    }

    if status.hunks.is_empty() {
        out.push_str("(no changes)\n");
    }

    out
}

pub(crate) fn format_blame(blame: &GitBlameInfo, line: usize) -> String {
    let short_commit: String = blame.commit.chars().take(8).collect();
    let author = if blame.author.is_empty() { "unknown" } else { &blame.author };
    let summary = if blame.summary.is_empty() { "(no summary)" } else { &blame.summary };
    let time_suffix =
        blame.author_time.as_deref().map(|value| format!(" | t={value}")).unwrap_or_default();
    format!("line {} | {} | {} | {}{}", line + 1, short_commit, author, summary, time_suffix)
}

pub(crate) fn changed_files(repo_root: &Path) -> io::Result<Vec<PathBuf>> {
    let repository = GitRepository::discover(repo_root)
        .map_err(git_error)?
        .ok_or_else(|| io::Error::other("Git repository not found"))?;
    let report = repository
        .status(GitReadLimits {
            max_status_files: usize::MAX,
            max_diff_bytes: DEFAULT_GIT_DIFF_BYTE_LIMIT,
        })
        .map_err(git_error)?;
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for path in report
        .staged
        .iter()
        .chain(&report.unstaged)
        .chain(&report.untracked)
        .chain(&report.conflicts)
    {
        let absolute = repository.root().join(path);
        if seen.insert(absolute.clone()) {
            files.push(absolute);
        }
    }
    Ok(files)
}

fn branch_name(repository: &GitRepository) -> io::Result<String> {
    let repository_handle = repository.open().map_err(git_error)?;
    if let Some(branch) = branch_from_repository(&repository_handle).map_err(git_error)? {
        return Ok(branch);
    }
    if repository_handle.head_detached().unwrap_or(false)
        && let Ok(head) = repository_handle.head()
        && let Some(target) = head.target()
    {
        return Ok(target.to_string().chars().take(8).collect());
    }
    Ok(String::from("HEAD"))
}

fn read_head_blob(
    repository: &GitRepository,
    repo_relative: &str,
) -> io::Result<Option<Vec<String>>> {
    let repository_handle = repository.open().map_err(git_error)?;
    let Some(tree) = repository_handle.head().ok().and_then(|head| head.peel_to_tree().ok()) else {
        return Ok(None);
    };
    let Ok(entry) = tree.get_path(Path::new(repo_relative)) else {
        return Ok(None);
    };
    let Ok(blob) = repository_handle.find_blob(entry.id()) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(blob.content());
    Ok(Some(split_blob_lines(&text)))
}

fn git_error(error: git2::Error) -> io::Error {
    io::Error::other(error)
}

fn split_blob_lines(text: &str) -> Vec<String> {
    text.lines().map(|line| line.to_owned()).collect()
}

fn normalize_pathspec(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn format_hunk_range(start: usize, count: usize) -> String {
    if count == 0 {
        format!("{},0", start)
    } else if count == 1 {
        (start + 1).to_string()
    } else {
        format!("{},{}", start + 1, count)
    }
}

fn diff_hunks(
    old_lines: &[String],
    new_lines: &[String],
) -> (Vec<GitHunk>, HashMap<usize, GitSign>) {
    let old_text = old_lines.join("\n");
    let new_text = new_lines.join("\n");
    let diff = TextDiff::from_lines(&old_text, &new_text);

    let mut hunks = Vec::new();
    let mut signs = HashMap::new();
    let mut next_old = 0usize;
    let mut next_new = 0usize;
    let mut current: Option<PendingHunk> = None;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                if let Some(hunk) = current.take() {
                    finalize_hunk(hunk, new_lines.len(), &mut hunks, &mut signs);
                }
                next_old = change.old_index().map_or(next_old, |index| index + 1);
                next_new = change.new_index().map_or(next_new, |index| index + 1);
            }
            ChangeTag::Delete => {
                let old_index = change.old_index().unwrap_or(next_old);
                let hunk = current.get_or_insert_with(|| PendingHunk::new(next_old, next_new));
                hunk.old_count += 1;
                hunk.has_delete = true;
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Removed,
                    text: trim_line_ending(change.value()),
                });
                next_old = old_index + 1;
            }
            ChangeTag::Insert => {
                let new_index = change.new_index().unwrap_or(next_new);
                let hunk = current.get_or_insert_with(|| PendingHunk::new(next_old, next_new));
                hunk.new_count += 1;
                hunk.has_insert = true;
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::Added,
                    text: trim_line_ending(change.value()),
                });
                next_new = new_index + 1;
            }
        }
    }

    if let Some(hunk) = current.take() {
        finalize_hunk(hunk, new_lines.len(), &mut hunks, &mut signs);
    }

    (hunks, signs)
}

fn trim_line_ending(value: &str) -> String {
    value.strip_suffix('\n').unwrap_or(value).to_owned()
}

fn finalize_hunk(
    hunk: PendingHunk,
    new_len: usize,
    hunks: &mut Vec<GitHunk>,
    signs: &mut HashMap<usize, GitSign>,
) {
    let sign = match (hunk.has_insert, hunk.has_delete) {
        (true, true) => GitSign::Modified,
        (true, false) => GitSign::Added,
        (false, true) => GitSign::Deleted,
        (false, false) => return,
    };
    let display_line = if hunk.new_count == 0 {
        if new_len == 0 { 0 } else { hunk.new_start.min(new_len.saturating_sub(1)) }
    } else {
        hunk.new_start
    };

    if hunk.new_count == 0 {
        signs.insert(display_line, sign);
    } else {
        for line in hunk.new_start..(hunk.new_start + hunk.new_count) {
            signs.insert(line, sign);
        }
    }

    hunks.push(GitHunk {
        old_start: hunk.old_start,
        old_count: hunk.old_count,
        new_start: hunk.new_start,
        new_count: hunk.new_count,
        display_line,
        sign,
        lines: hunk.lines,
    });
}

#[derive(Debug)]
struct PendingHunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    has_insert: bool,
    has_delete: bool,
    lines: Vec<DiffLine>,
}

impl PendingHunk {
    fn new(old_start: usize, new_start: usize) -> Self {
        Self {
            old_start,
            old_count: 0,
            new_start,
            new_count: 0,
            has_insert: false,
            has_delete: false,
            lines: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_hunks_detect_add_modify_delete() {
        let old_lines = vec![String::from("one"), String::from("two"), String::from("three")];
        let new_lines = vec![
            String::from("one"),
            String::from("deux"),
            String::from("three"),
            String::from("four"),
        ];

        let (hunks, signs) = diff_hunks(&old_lines, &new_lines);

        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].sign, GitSign::Modified);
        assert_eq!(hunks[0].display_line, 1);
        assert_eq!(signs.get(&1), Some(&GitSign::Modified));
        assert_eq!(signs.get(&3), Some(&GitSign::Modified));
    }

    #[test]
    fn render_diff_includes_unified_headers() {
        let status = GitBufferStatus {
            repo_root: PathBuf::from("/tmp/repo"),
            repo_name: String::from("repo"),
            repo_relative: String::from("src/main.rs"),
            branch: String::from("main"),
            tracked: true,
            dirty: true,
            hunks: vec![GitHunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                display_line: 1,
                sign: GitSign::Modified,
                lines: vec![
                    DiffLine { kind: DiffLineKind::Removed, text: String::from("old") },
                    DiffLine { kind: DiffLineKind::Added, text: String::from("new") },
                ],
            }],
            line_signs: HashMap::from([(1, GitSign::Modified)]),
        };

        let rendered = render_diff(&status, None);

        assert!(rendered.contains("diff --git a/src/main.rs b/src/main.rs"));
        assert!(rendered.contains("@@ -2 +2 @@"));
        assert!(rendered.contains("-old"));
        assert!(rendered.contains("+new"));
    }
}
