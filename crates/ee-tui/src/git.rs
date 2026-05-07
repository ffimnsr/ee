use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use similar::{ChangeTag, TextDiff};

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
    let parent = path.parent().unwrap_or(path);
    let Some(repo_root) = crate::config::find_git_root(parent) else {
        return Ok(None);
    };
    let Ok(repo_relative) = path.strip_prefix(&repo_root) else {
        return Ok(None);
    };
    let repo_relative = normalize_pathspec(repo_relative);
    let branch = branch_name(&repo_root).unwrap_or_else(|_| String::from("HEAD"));
    let tracked_blob = read_head_blob(&repo_root, &repo_relative)?;
    let tracked = tracked_blob.is_some();
    let base_lines = tracked_blob.unwrap_or_default();
    let (hunks, line_signs) = diff_hunks(&base_lines, current_lines);

    Ok(Some(GitBufferStatus {
        repo_name: repo_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repo")
            .to_owned(),
        repo_root,
        repo_relative,
        branch,
        tracked,
        dirty: !hunks.is_empty() || !tracked,
        hunks,
        line_signs,
    }))
}

pub(crate) fn blame_line(path: &Path, line: usize) -> io::Result<Option<GitBlameInfo>> {
    let parent = path.parent().unwrap_or(path);
    let Some(repo_root) = crate::config::find_git_root(parent) else {
        return Ok(None);
    };
    let Ok(repo_relative) = path.strip_prefix(&repo_root) else {
        return Ok(None);
    };
    let repo_relative = normalize_pathspec(repo_relative);
    let range = format!("{},{}", line + 1, line + 1);
    let output = git_command(&repo_root)
        .arg("blame")
        .arg("--porcelain")
        .arg("-L")
        .arg(&range)
        .arg("--")
        .arg(&repo_relative)
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let first = lines.next().unwrap_or_default();
    let commit = first.split_whitespace().next().unwrap_or_default().to_owned();
    let mut author = String::new();
    let mut summary = String::new();
    let mut author_time = None;

    for entry in lines {
        if let Some(value) = entry.strip_prefix("author ") {
            author = value.to_owned();
        } else if let Some(value) = entry.strip_prefix("summary ") {
            summary = value.to_owned();
        } else if let Some(value) = entry.strip_prefix("author-time ") {
            author_time = Some(value.to_owned());
        }
    }

    if commit.is_empty() {
        return Ok(None);
    }

    Ok(Some(GitBlameInfo { commit, author, summary, author_time }))
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
    let output = git_command(repo_root)
        .arg("status")
        .arg("--porcelain=v1")
        .arg("-z")
        .arg("--untracked-files=all")
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("git status failed"));
    }

    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut records = output.stdout.split(|byte| *byte == 0).filter(|record| !record.is_empty());

    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let status = &record[..2];
        let mut path = String::from_utf8_lossy(&record[3..]).into_owned();
        if matches!(status[0], b'R' | b'C') || matches!(status[1], b'R' | b'C') {
            let Some(rename_target) = records.next() else { continue };
            path = String::from_utf8_lossy(rename_target).into_owned();
        }
        let absolute = repo_root.join(path);
        if seen.insert(absolute.clone()) {
            files.push(absolute);
        }
    }

    Ok(files)
}

fn git_command(repo_root: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo_root);
    command
}

fn branch_name(repo_root: &Path) -> io::Result<String> {
    let output = git_command(repo_root).arg("branch").arg("--show-current").output()?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !branch.is_empty() {
            return Ok(branch);
        }
    }

    let fallback = git_command(repo_root).arg("rev-parse").arg("--short").arg("HEAD").output()?;
    if fallback.status.success() {
        Ok(String::from_utf8_lossy(&fallback.stdout).trim().to_owned())
    } else {
        Ok(String::from("HEAD"))
    }
}

fn read_head_blob(repo_root: &Path, repo_relative: &str) -> io::Result<Option<Vec<String>>> {
    let output =
        git_command(repo_root).arg("show").arg(format!("HEAD:{repo_relative}")).output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(Some(split_blob_lines(&stdout)))
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
