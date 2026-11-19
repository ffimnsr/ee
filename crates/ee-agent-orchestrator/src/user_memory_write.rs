//! Workspace-scoped create, update, and delete for file-backed memory entries.
//!
//! Mutations always target `<workspace-root>/.agents/memories/<name>.md`.
//! Personal entries under `$XDG_CONFIG_HOME/ee/memories` stay read-only.
//! Writes are atomic (`temp` file + rename) and containment-checked before
//! the file is replaced, so a symlinked memory directory cannot escape the
//! workspace root.
//!
//! The caller owns user approval: the `save_memory` and `delete_memory` tools
//! ask for an elicitation before calling these helpers.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::user_memories::{
    MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_DESCRIPTION_CHARS, MAX_USER_MEMORIES, MemoryImportance,
    UserMemoryEntry, UserMemoryRegistry, UserMemoryScope, WORKSPACE_MEMORY_DIR, normalize_tags,
};

/// One save request from the model.
pub struct MemorySaveRequest<'a> {
    /// Entry name; also the created file stem.
    pub name: &'a str,
    /// Replacement description; omitted on update keeps the existing value.
    pub description: Option<&'a str>,
    /// Replacement tags; omitted on update keeps the existing values.
    pub tags: Option<&'a [String]>,
    /// Replacement importance; omitted on update keeps the existing value.
    pub importance: Option<MemoryImportance>,
    /// Complete entry body.
    pub content: &'a str,
}

/// Result of one successful save.
#[derive(Debug)]
pub struct MemorySaveOutcome {
    /// The entry as it will be discovered on the next pass.
    pub entry: UserMemoryEntry,
    /// Whether a new file was created.
    pub created: bool,
}

/// Creates or replaces one workspace memory entry.
pub fn save(
    cwd: &Path,
    workspace_roots: &[PathBuf],
    request: MemorySaveRequest<'_>,
) -> Result<MemorySaveOutcome, String> {
    let name = request.name.trim();
    crate::markdown_config::validate_identifier(name, "memory name")?;
    let content = request.content.trim();
    if content.is_empty() {
        return Err("memory content is empty".to_string());
    }
    if content.len() > MAX_MEMORY_CONTENT_BYTES {
        return Err(format!("memory content exceeds the {MAX_MEMORY_CONTENT_BYTES}-byte limit"));
    }
    let roots = resolve_workspace_roots(cwd, workspace_roots);
    let root = roots.first().ok_or_else(|| "no workspace root is available".to_string())?;
    // Personal entries are read-only; discover only workspace entries so a
    // same-name personal entry is shadowed rather than modified.
    let registry = UserMemoryRegistry::discover(cwd, &roots, None);
    let existing = registry.get(name);
    let (target, target_root, description, tags, importance, created): (
        PathBuf,
        PathBuf,
        String,
        Vec<String>,
        MemoryImportance,
        bool,
    ) = if let Some(entry) = existing {
        if entry.scope != UserMemoryScope::Workspace {
            return Err(format!("memory entry {name:?} is personal and read-only"));
        }
        let target_root = roots
            .iter()
            .find(|root| entry.path.starts_with(root))
            .cloned()
            .ok_or_else(|| format!("memory entry {name:?} is outside every workspace root"))?;
        let description = request
            .description
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map_or_else(
                || entry.description.clone(),
                |description| {
                    crate::markdown_config::truncate_chars(
                        description,
                        MAX_MEMORY_DESCRIPTION_CHARS,
                    )
                },
            );
        let tags = match request.tags {
            Some(tags) => normalize_tags(tags.to_vec())?,
            None => entry.tags.clone(),
        };
        let importance = request.importance.unwrap_or(entry.importance);
        (entry.path.clone(), target_root, description, tags, importance, false)
    } else {
        if registry.len() >= MAX_USER_MEMORIES {
            return Err(format!(
                "workspace memory limit {MAX_USER_MEMORIES} reached; delete an entry before creating another"
            ));
        }
        let description = request
            .description
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map_or_else(
                || default_description(content),
                |description| {
                    crate::markdown_config::truncate_chars(
                        description,
                        MAX_MEMORY_DESCRIPTION_CHARS,
                    )
                },
            );
        let tags = normalize_tags(request.tags.unwrap_or_default().to_vec())?;
        let importance = request.importance.unwrap_or_default();
        (
            root.join(WORKSPACE_MEMORY_DIR).join(format!("{name}.md")),
            root.clone(),
            description,
            tags,
            importance,
            true,
        )
    };
    if created && target.exists() {
        return Err(format!(
            "memory file {} already exists but is not an active entry; fix or remove it first",
            target.display()
        ));
    }
    let canonical_target = prepare_target(&target_root, &target)?;
    let frontmatter =
        MemoryFileFrontmatter { name, description: &description, tags: &tags, importance };
    let yaml = serde_yaml_ng::to_string(&frontmatter)
        .map_err(|error| format!("cannot serialize memory frontmatter: {error}"))?;
    let body = format!("---\n{yaml}---\n\n{content}\n");
    atomic_write(&canonical_target, body.as_bytes())?;
    let display_path = canonical_target
        .strip_prefix(&target_root)
        .unwrap_or(&canonical_target)
        .display()
        .to_string();
    Ok(MemorySaveOutcome {
        entry: UserMemoryEntry {
            name: name.to_string(),
            description,
            tags,
            importance,
            content: content.to_string(),
            scope: UserMemoryScope::Workspace,
            path: canonical_target,
            display_path,
        },
        created,
    })
}

/// Deletes one workspace memory entry.
pub fn delete(
    cwd: &Path,
    workspace_roots: &[PathBuf],
    name: &str,
) -> Result<UserMemoryEntry, String> {
    let name = name.trim();
    let roots = resolve_workspace_roots(cwd, workspace_roots);
    let registry = UserMemoryRegistry::discover(cwd, &roots, None);
    let entry =
        registry.get(name).ok_or_else(|| format!("unknown workspace memory entry {name:?}"))?;
    let root = roots
        .iter()
        .find(|root| entry.path.starts_with(root))
        .ok_or_else(|| format!("memory entry {name:?} is outside every workspace root"))?;
    let canonical = fs::canonicalize(&entry.path)
        .map_err(|error| format!("cannot resolve {}: {error}", entry.path.display()))?;
    if !canonical.starts_with(root) {
        return Err(format!("memory entry {name:?} escapes its workspace root"));
    }
    fs::remove_file(&canonical)
        .map_err(|error| format!("cannot delete {}: {error}", canonical.display()))?;
    Ok(entry.clone())
}

/// Returns canonical, deduplicated workspace roots, falling back to `cwd`.
fn resolve_workspace_roots(cwd: &Path, workspace_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> =
        if workspace_roots.is_empty() { vec![cwd.to_path_buf()] } else { workspace_roots.to_vec() };
    roots = roots.into_iter().map(|root| fs::canonicalize(&root).unwrap_or(root)).collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Creates missing parent directories and returns a canonical target path that
/// is guaranteed to sit inside `root`.
fn prepare_target(root: &Path, target: &Path) -> Result<PathBuf, String> {
    let parent = target.parent().ok_or_else(|| "memory path has no parent".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let parent = fs::canonicalize(parent)
        .map_err(|error| format!("cannot resolve {}: {error}", parent.display()))?;
    if !parent.starts_with(root) {
        return Err(format!(
            "memory path {} escapes workspace root {}",
            parent.display(),
            root.display()
        ));
    }
    let file_name = target.file_name().ok_or_else(|| "memory path has no file name".to_string())?;
    Ok(parent.join(file_name))
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| "memory path has no parent".to_string())?;
    let file_stem = path.file_name().and_then(|name| name.to_str()).unwrap_or("memory");
    for attempt in 0..100_u32 {
        let temporary = parent.join(format!(".{file_stem}.{}.{attempt}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&temporary) {
            Ok(mut file) => {
                let result = (|| -> std::io::Result<()> {
                    file.write_all(contents)?;
                    file.sync_all()?;
                    fs::rename(&temporary, path)
                })();
                if let Err(error) = result {
                    let _ = fs::remove_file(&temporary);
                    return Err(format!("cannot write {}: {error}", path.display()));
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("cannot create temporary memory file: {error}"));
            }
        }
    }
    Err("cannot allocate a temporary memory file".to_string())
}

fn default_description(content: &str) -> String {
    let first_line = content.lines().find(|line| !line.trim().is_empty()).unwrap_or_default();
    crate::markdown_config::truncate_chars(first_line.trim(), MAX_MEMORY_DESCRIPTION_CHARS)
}

#[derive(Serialize)]
struct MemoryFileFrontmatter<'a> {
    name: &'a str,
    description: &'a str,
    tags: &'a [String],
    importance: MemoryImportance,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(workspace: &Path) -> Vec<PathBuf> {
        vec![workspace.to_path_buf()]
    }

    #[test]
    fn save_creates_and_updates_an_entry_preserving_omitted_fields() {
        let workspace = tempfile::tempdir().expect("workspace");
        let created = save(
            workspace.path(),
            &roots(workspace.path()),
            MemorySaveRequest {
                name: "testing",
                description: Some("Test commands"),
                tags: Some(&[String::from("rust"), String::from("tests")]),
                importance: Some(MemoryImportance::High),
                content: "Run cargo test --quiet.",
            },
        )
        .expect("create");
        assert!(created.created);
        assert_eq!(created.entry.description, "Test commands");
        assert_eq!(created.entry.tags, vec!["rust", "tests"]);
        assert_eq!(created.entry.importance, MemoryImportance::High);
        let file = workspace.path().join(".agents/memories/testing.md");
        assert!(file.is_file());
        let text = std::fs::read_to_string(&file).expect("read");
        assert!(text.starts_with("---\n"));

        let updated = save(
            workspace.path(),
            &roots(workspace.path()),
            MemorySaveRequest {
                name: "testing",
                description: None,
                tags: None,
                importance: None,
                content: "Run the memguard suite.",
            },
        )
        .expect("update");
        assert!(!updated.created);
        assert_eq!(updated.entry.description, "Test commands");
        assert_eq!(updated.entry.tags, vec!["rust", "tests"]);
        assert_eq!(updated.entry.importance, MemoryImportance::High);
        assert_eq!(updated.entry.content, "Run the memguard suite.");

        let registry =
            UserMemoryRegistry::discover(workspace.path(), &roots(workspace.path()), None);
        let discovered = registry.get("testing").expect("rediscovered");
        assert_eq!(discovered.content, "Run the memguard suite.");
        assert_eq!(discovered.tags, vec!["rust", "tests"]);
    }

    #[test]
    fn save_rejects_path_like_names_and_empty_content() {
        let workspace = tempfile::tempdir().expect("workspace");
        let traversal = save(
            workspace.path(),
            &roots(workspace.path()),
            MemorySaveRequest {
                name: "../escape",
                description: None,
                tags: None,
                importance: None,
                content: "body",
            },
        )
        .expect_err("path-like name rejected");
        assert!(traversal.contains("memory name"));
        let empty = save(
            workspace.path(),
            &roots(workspace.path()),
            MemorySaveRequest {
                name: "empty",
                description: None,
                tags: None,
                importance: None,
                content: "   ",
            },
        )
        .expect_err("empty content rejected");
        assert!(empty.contains("content is empty"));
    }

    #[test]
    fn save_refuses_to_clobber_an_unregistered_file() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = workspace.path().join(".agents/memories/broken.md");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, "not a memory file").expect("write");
        let error = save(
            workspace.path(),
            &roots(workspace.path()),
            MemorySaveRequest {
                name: "broken",
                description: None,
                tags: None,
                importance: None,
                content: "body",
            },
        )
        .expect_err("unregistered file rejected");
        assert!(error.contains("not an active entry"));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "not a memory file");
    }

    #[test]
    fn update_uses_the_existing_entry_workspace_root() {
        let first = tempfile::tempdir().expect("first root");
        let second = tempfile::tempdir().expect("second root");
        let path = second.path().join(".agents/memories/testing.md");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(
            &path,
            "---\nname: testing\ndescription: Old\nimportance: normal\n---\n\nOld body.\n",
        )
        .expect("write");
        let roots = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        let outcome = save(
            first.path(),
            &roots,
            MemorySaveRequest {
                name: "testing",
                description: None,
                tags: None,
                importance: None,
                content: "New body.",
            },
        )
        .expect("update");
        assert!(!outcome.created);
        assert!(outcome.entry.path.starts_with(second.path()));
        assert!(!first.path().join(".agents/memories/testing.md").exists());
        assert!(std::fs::read_to_string(&path).expect("read").contains("New body."));
    }

    #[test]
    fn delete_removes_the_entry_and_rejects_unknown_names() {
        let workspace = tempfile::tempdir().expect("workspace");
        save(
            workspace.path(),
            &roots(workspace.path()),
            MemorySaveRequest {
                name: "testing",
                description: Some("Test commands"),
                tags: None,
                importance: None,
                content: "Run tests.",
            },
        )
        .expect("create");
        let deleted =
            delete(workspace.path(), &roots(workspace.path()), "testing").expect("delete");
        assert_eq!(deleted.name, "testing");
        assert!(!workspace.path().join(".agents/memories/testing.md").exists());
        let unknown = delete(workspace.path(), &roots(workspace.path()), "testing")
            .expect_err("already deleted");
        assert!(unknown.contains("unknown workspace memory entry"));
    }
}
