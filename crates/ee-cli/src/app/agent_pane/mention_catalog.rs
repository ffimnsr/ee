//! Workspace and user skill/memory name discovery for `@` mention completion.
//!
//! Mirrors the prompt-side discovery roots: `<root>/.agents/skills`,
//! `<root>/skills`, `<root>/.agents/memories`, and the user `ee` config
//! equivalents. Reads are bounded, malformed entries are skipped, and names
//! never include whitespace so they form valid `@skill:<name>` /
//! `@memory:<name>` tokens.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

const SKILL_FILE: &str = "SKILL.md";
const MAX_SCAN_DEPTH: usize = 4;
const MAX_MEMORY_FILES: usize = 512;
const MAX_FRONTMATTER_BYTES: usize = 8 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct MentionCatalog {
    pub(super) skills: Vec<String>,
    pub(super) memories: Vec<String>,
}

pub(super) fn discover(
    cwd: &Path,
    workspace_roots: &[PathBuf],
    config_root: Option<&Path>,
) -> MentionCatalog {
    let roots = canonical_roots(cwd, workspace_roots);
    let mut skills = BTreeSet::new();
    let mut memories = BTreeSet::new();
    for root in &roots {
        collect_skills(&root.join(".agents").join("skills"), &mut skills);
        collect_nested_skills(&root.join("skills"), &mut skills);
        collect_memories(&root.join(".agents").join("memories"), &mut memories);
    }
    if let Some(config_root) = config_root {
        collect_skills(&config_root.join("skills"), &mut skills);
        collect_memories(&config_root.join("memories"), &mut memories);
    }
    MentionCatalog {
        skills: skills.into_iter().collect(),
        memories: memories.into_iter().collect(),
    }
}

fn canonical_roots(cwd: &Path, workspace_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in std::iter::once(cwd).chain(workspace_roots.iter().map(PathBuf::as_path)) {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    roots
}

/// Flat `<skill>/SKILL.md` layout: `.agents/skills` and user `skills`.
fn collect_skills(root: &Path, names: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let manifest = entry.path().join(SKILL_FILE);
        if !manifest.is_file() {
            continue;
        }
        let fallback = entry.file_name().to_string_lossy().into_owned();
        if let Some(name) = manifest_name(&manifest, &fallback) {
            names.insert(name);
        }
    }
}

/// Alternate workspace `skills/` tree, searched recursively like prompt-side
/// skill discovery.
fn collect_nested_skills(root: &Path, names: &mut BTreeSet<String>) {
    for (manifest, _) in markdown_files(root, MAX_SCAN_DEPTH, MAX_MEMORY_FILES, Some(SKILL_FILE)) {
        let fallback = manifest
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(name) = manifest_name(&manifest, &fallback) {
            names.insert(name);
        }
    }
}

fn collect_memories(root: &Path, names: &mut BTreeSet<String>) {
    for (file, frontmatter) in markdown_files(root, MAX_SCAN_DEPTH, MAX_MEMORY_FILES, None) {
        let Some(frontmatter) = frontmatter else {
            continue;
        };
        if frontmatter.disabled {
            continue;
        }
        let fallback =
            file.file_stem().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let name = match frontmatter.name {
            Some(name) => name,
            None => fallback,
        };
        if valid_mention_name(&name) {
            names.insert(name);
        }
    }
}

/// Bounded markdown walk. Returns `(path, frontmatter)` pairs; the frontmatter
/// is `None` when the file has no valid frontmatter block.
fn markdown_files(
    root: &Path,
    max_depth: usize,
    max_files: usize,
    required_file_name: Option<&str>,
) -> Vec<(PathBuf, Option<Frontmatter>)> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .max_depth(Some(max_depth))
        .build()
        .filter_map(Result::ok)
    {
        if files.len() >= max_files {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        match required_file_name {
            Some(name) if path.file_name().and_then(|file| file.to_str()) != Some(name) => continue,
            None if path.extension().and_then(|ext| ext.to_str()) != Some("md") => continue,
            _ => {}
        }
        let frontmatter = read_frontmatter(path);
        files.push((path.to_path_buf(), frontmatter));
    }
    files
}

fn manifest_name(manifest: &Path, fallback: &str) -> Option<String> {
    let name = match read_frontmatter(manifest).and_then(|frontmatter| frontmatter.name) {
        Some(name) => name,
        None => fallback.to_string(),
    };
    valid_mention_name(&name).then_some(name)
}

#[derive(Debug, Default)]
struct Frontmatter {
    name: Option<String>,
    disabled: bool,
}

fn read_frontmatter(path: &Path) -> Option<Frontmatter> {
    let bytes = std::fs::read(path).ok()?;
    let bounded = &bytes[..bytes.len().min(MAX_FRONTMATTER_BYTES)];
    let text = String::from_utf8_lossy(bounded);
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut frontmatter = Frontmatter::default();
    for line in lines {
        let line = line.trim_end();
        if line == "---" {
            return Some(frontmatter);
        }
        if let Some(value) = line.strip_prefix("name:") {
            frontmatter.name = clean_scalar(value);
        } else if let Some(value) = line.strip_prefix("disabled:") {
            frontmatter.disabled = value.trim().eq_ignore_ascii_case("true");
        }
    }
    None
}

fn clean_scalar(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(['"', '\'']).trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Mention tokens split on whitespace, so names with whitespace or control
/// characters cannot round-trip through `@skill:<name>` / `@memory:<name>`.
fn valid_mention_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|ch| ch.is_whitespace() || ch.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_workspace_skills_and_memories_with_frontmatter_names() {
        let workspace = tempfile::tempdir().unwrap();
        let skill = workspace.path().join(".agents/skills/atlas/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "---\nname: atlas\ndescription: Atlas\n---\nBody\n").unwrap();
        let nested = workspace.path().join("skills/tooling/SKILL.md");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "no frontmatter\n").unwrap();
        let memory = workspace.path().join(".agents/memories/testing.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        std::fs::write(&memory, "---\nname: testing\nimportance: high\n---\nRun quiet.\n").unwrap();
        let disabled = workspace.path().join(".agents/memories/off.md");
        std::fs::write(&disabled, "---\nname: off\ndisabled: true\n---\nOff.\n").unwrap();

        let catalog = discover(workspace.path(), &[workspace.path().to_path_buf()], None);
        assert_eq!(catalog.skills, vec![String::from("atlas"), String::from("tooling")]);
        assert_eq!(catalog.memories, vec![String::from("testing")]);
    }

    #[test]
    fn frontmatter_name_overrides_the_file_stem() {
        let workspace = tempfile::tempdir().unwrap();
        let memory = workspace.path().join(".agents/memories/conventions.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        std::fs::write(&memory, "---\nname: \"code-style\"\n---\nUse rustfmt.\n").unwrap();

        let catalog = discover(workspace.path(), &[], None);
        assert_eq!(catalog.memories, vec![String::from("code-style")]);
    }

    #[test]
    fn whitespace_names_are_not_suggested() {
        let workspace = tempfile::tempdir().unwrap();
        let memory = workspace.path().join(".agents/memories/bad.md");
        std::fs::create_dir_all(memory.parent().unwrap()).unwrap();
        std::fs::write(&memory, "---\nname: two words\n---\nBody.\n").unwrap();

        let catalog = discover(workspace.path(), &[], None);
        assert!(catalog.memories.is_empty(), "names must stay mention-token safe");
    }
}
