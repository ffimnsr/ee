//! `ee do agent harness adopt`: validated, backed-up application of a chosen
//! harness tree to the workspace `.agents` managed surface.
//!
//! Complete-tree semantics: managed files absent from the adopted tree are
//! removed. `.agents/experience` is always preserved. `.agents/memories` is
//! preserved unless `--include-memories` is set. Top-level `skills/`,
//! top-level `scripts/`, and user-config skills are never touched.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ee_agent_harnessing::{
    ExperienceRegistry, HarnessTree, WORKSPACE_EXPERIENCE_DIR, external_scripts, materialize,
    validate_with_external_scripts,
};
use serde::Serialize;

use crate::args::HarnessAdoptArgs;

const MANAGED_ROOT_FILES: &[&str] = &["guardrails.md", "tool_guidance.md", "middleware.toml"];
const MANAGED_DIRS: &[&str] = &["scripts", "skills", "agents"];
const MEMORIES_DIR: &str = "memories";
const BACKUP_DIR: &str = ".backup";
const MANIFEST_FILE: &str = "manifest.json";

/// Files and roots in one adoption plan.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AdoptPlan {
    added: Vec<String>,
    removed: Vec<String>,
    modified: Vec<String>,
}

impl AdoptPlan {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

#[derive(Debug, Serialize)]
struct BackupManifest {
    created_unix: u64,
    workspace: String,
    files: Vec<String>,
}

/// Runs one `harness adopt` invocation.
pub(crate) fn run(args: HarnessAdoptArgs) -> io::Result<()> {
    let workspace = std::fs::canonicalize(&args.workspace).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("invalid --workspace {}: {error}", args.workspace.display()),
        )
    })?;
    if !workspace.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--workspace is not a directory"));
    }
    let source = load_source(&args, &workspace)?;
    let adopted = validate_source(source, &workspace, args.include_memories)?;
    let plan = plan(&workspace, &adopted, args.include_memories)?;
    print_plan(&plan, args.dry_run);
    if args.dry_run {
        println!("ee: harness adopt dry-run; no files written");
        return Ok(());
    }
    if !approve(args.yes, io::stdin().is_terminal(), || prompt_confirmation(&workspace, &plan)) {
        println!("ee: harness adoption declined; no files written");
        return Ok(());
    }
    let backup = apply(&workspace, &adopted, args.include_memories)?;
    println!(
        "ee: harness-adopt {}",
        serde_json::json!({
            "workspace": workspace.display().to_string(),
            "added": plan.added.len(),
            "removed": plan.removed.len(),
            "modified": plan.modified.len(),
            "backup": backup.display().to_string(),
            "include_memories": args.include_memories,
        })
    );
    Ok(())
}

fn load_source(args: &HarnessAdoptArgs, workspace: &Path) -> io::Result<HarnessTree> {
    match (&args.run_root, &args.experience) {
        (Some(run_root), None) => {
            let agents = run_root.join("adopted/.agents");
            if !agents.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("run root has no adopted tree: {}", agents.display()),
                ));
            }
            HarnessTree::load_components(&agents).map_err(invalid_data)
        }
        (None, Some(spec)) => {
            let (domain, seq) = parse_experience(spec)?;
            let library = args
                .library_root
                .clone()
                .unwrap_or_else(|| workspace.join(WORKSPACE_EXPERIENCE_DIR));
            let registry = ExperienceRegistry::discover_at(&library);
            let entry = registry
                .iter()
                .find(|entry| entry.domain == domain && entry.seq == seq)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "unknown experience entry {domain}/{seq} under {}",
                            library.display()
                        ),
                    )
                })?;
            let harness = entry.harness_dir().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "experience entry has no directory")
            })?;
            if !harness.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("experience entry has no harness tree: {}", harness.display()),
                ));
            }
            HarnessTree::load_components(&harness).map_err(invalid_data)
        }
        (None, None) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "harness adopt requires --run-root or --experience <domain>/<seq>",
        )),
        (Some(_), Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "use either --run-root or --experience, not both",
        )),
    }
}

fn parse_experience(spec: &str) -> io::Result<(String, u64)> {
    let (domain, seq) = spec.split_once('/').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--experience must be <domain>/<seq>; got {spec:?}"),
        )
    })?;
    if domain.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "experience domain is empty"));
    }
    let seq: u64 = seq.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("experience seq {seq:?} is not a number"),
        )
    })?;
    if seq == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "experience seq must be positive"));
    }
    Ok((domain.to_string(), seq))
}

fn invalid_data(error: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn validate_source(
    source: HarnessTree,
    workspace: &Path,
    include_memories: bool,
) -> io::Result<HarnessTree> {
    let external = external_scripts(workspace);
    let verdict = validate_with_external_scripts(&source, None, &external);
    if !verdict.ok {
        for error in &verdict.errors {
            eprintln!("ee: adopt rejected {} [{:?}] {}", error.file, error.rule, error.message);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "adopted harness failed validation; no files were written",
        ));
    }
    for repair in &verdict.repairs_applied {
        eprintln!("ee: adopt repair {} [{:?}] {}", repair.file, repair.rule, repair.detail);
    }
    let adopted = managed_tree(&verdict.tree, include_memories);
    if adopted.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "adopted harness has no files in the managed surface; nothing to apply",
        ));
    }
    Ok(adopted)
}

fn managed_tree(tree: &HarnessTree, include_memories: bool) -> HarnessTree {
    HarnessTree::new(
        tree.files
            .iter()
            .filter(|(path, _)| is_managed(path, include_memories))
            .map(|(path, content)| (path.clone(), content.clone()))
            .collect(),
    )
}

fn is_managed(path: &str, include_memories: bool) -> bool {
    MANAGED_ROOT_FILES.contains(&path)
        || MANAGED_DIRS.iter().any(|directory| path.starts_with(&format!("{directory}/")))
        || (include_memories && path.starts_with(&format!("{MEMORIES_DIR}/")))
}

fn plan(workspace: &Path, adopted: &HarnessTree, include_memories: bool) -> io::Result<AdoptPlan> {
    let current = current_managed_tree(workspace, include_memories)?;
    let mut added = Vec::new();
    let mut modified = Vec::new();
    for (path, content) in &adopted.files {
        match current.files.get(path) {
            None => added.push(path.clone()),
            Some(existing) if existing != content => modified.push(path.clone()),
            Some(_) => {}
        }
    }
    let removed =
        current.files.keys().filter(|path| !adopted.files.contains_key(*path)).cloned().collect();
    Ok(AdoptPlan { added, removed, modified })
}

fn current_managed_tree(workspace: &Path, include_memories: bool) -> io::Result<HarnessTree> {
    let agents = workspace.join(".agents");
    if !agents.is_dir() {
        return Ok(HarnessTree::default());
    }
    let tree = HarnessTree::load_components(&agents).map_err(invalid_data)?;
    Ok(managed_tree(&tree, include_memories))
}

fn print_plan(plan: &AdoptPlan, dry_run: bool) {
    let prefix = if dry_run { "would " } else { "" };
    println!(
        "ee: harness adopt {prefix}apply added={} removed={} modified={}",
        plan.added.len(),
        plan.removed.len(),
        plan.modified.len()
    );
    for path in &plan.added {
        println!("ee: adopt + {path}");
    }
    for path in &plan.removed {
        println!("ee: adopt - {path}");
    }
    for path in &plan.modified {
        println!("ee: adopt ~ {path}");
    }
    if plan.is_empty() {
        println!("ee: adopt no managed-surface changes");
    }
}

fn approve(yes: bool, interactive: bool, prompt: impl FnOnce() -> bool) -> bool {
    yes || (interactive && prompt())
}

fn prompt_confirmation(workspace: &Path, plan: &AdoptPlan) -> bool {
    eprint!(
        "ee: apply adopted harness to {}/.agents ({} added, {} removed, {} modified)? [y/N] ",
        workspace.display(),
        plan.added.len(),
        plan.removed.len(),
        plan.modified.len()
    );
    let _ = io::stderr().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn apply(workspace: &Path, adopted: &HarnessTree, include_memories: bool) -> io::Result<PathBuf> {
    let agents = workspace.join(".agents");
    std::fs::create_dir_all(&agents)?;
    let backup = backup_managed_surface(workspace, include_memories)?;
    let staging = staging_dir(&agents);
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    materialize(adopted, &staging).map_err(invalid_data)?;

    for file in MANAGED_ROOT_FILES {
        replace_root(&agents, &staging, file)?;
    }
    for directory in MANAGED_DIRS {
        replace_root(&agents, &staging, directory)?;
    }
    if include_memories {
        replace_root(&agents, &staging, MEMORIES_DIR)?;
    }
    std::fs::remove_dir_all(&staging)?;
    Ok(backup)
}

fn replace_root(agents: &Path, staging: &Path, name: &str) -> io::Result<()> {
    let target = agents.join(name);
    remove_managed(&target)?;
    let staged = staging.join(name);
    if staged.exists() {
        std::fs::rename(&staged, &target)?;
    }
    Ok(())
}

fn remove_managed(path: &Path) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to replace symlinked managed path: {}", path.display()),
        ));
    }
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn backup_managed_surface(workspace: &Path, include_memories: bool) -> io::Result<PathBuf> {
    let current = current_managed_tree(workspace, include_memories)?;
    let stamp = unique_stamp();
    let backup = workspace.join(".agents").join(BACKUP_DIR).join(stamp);
    std::fs::create_dir_all(&backup)?;
    for relative in current.files.keys() {
        let source = workspace.join(".agents").join(relative);
        let target = backup.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source, &target)?;
    }
    let manifest = BackupManifest {
        created_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
        workspace: workspace.display().to_string(),
        files: current.files.keys().cloned().collect(),
    };
    let text = serde_json::to_string_pretty(&manifest)
        .map_err(|error| io::Error::other(error.to_string()))?;
    std::fs::write(backup.join(MANIFEST_FILE), text)?;
    Ok(backup)
}

fn unique_stamp() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{}-{counter}", std::process::id())
}

fn staging_dir(agents: &Path) -> PathBuf {
    agents.join(format!(".adopt-staging-{}-{}", std::process::id(), unique_stamp()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn tree(files: &[(&str, &str)]) -> HarnessTree {
        HarnessTree::new(
            files
                .iter()
                .map(|(path, content)| ((*path).to_string(), (*content).to_string()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn workspace_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("workspace");
        for (path, content) in files {
            let full = root.path().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("parent");
            std::fs::write(full, content).expect("write");
        }
        root
    }

    #[test]
    fn plan_reports_additions_removals_and_modifications() {
        let workspace = workspace_with(&[
            (".agents/guardrails.md", "old\n"),
            (".agents/skills/stale/SKILL.md", "stale\n"),
            (".agents/experience/rust/entries/1-a/EXPERIENCE.md", "keep\n"),
            (".agents/custom.md", "keep\n"),
        ]);
        let adopted = tree(&[("guardrails.md", "new\n"), ("tool_guidance.md", "guide\n")]);
        let plan = plan(workspace.path(), &adopted, false).expect("plan");
        assert_eq!(plan.added, vec!["tool_guidance.md"]);
        assert_eq!(plan.removed, vec!["skills/stale/SKILL.md"]);
        assert_eq!(plan.modified, vec!["guardrails.md"]);
    }

    #[test]
    fn apply_replaces_managed_surface_and_preserves_experience_memories_and_unknowns() {
        let workspace = workspace_with(&[
            (".agents/guardrails.md", "old\n"),
            (".agents/skills/stale/SKILL.md", "stale\n"),
            (".agents/experience/rust/entries/1-a/EXPERIENCE.md", "keep\n"),
            (".agents/memories/note.md", "keep memory\n"),
            (".agents/custom.md", "keep\n"),
            ("skills/top.md", "top skill\n"),
            ("scripts/top.sh", "top script\n"),
        ]);
        let adopted = tree(&[("guardrails.md", "new\n"), ("skills/fresh/SKILL.md", "fresh\n")]);
        let backup = apply(workspace.path(), &adopted, false).expect("apply");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(".agents/guardrails.md")).unwrap(),
            "new\n"
        );
        assert!(workspace.path().join(".agents/skills/fresh/SKILL.md").is_file());
        assert!(!workspace.path().join(".agents/skills/stale").exists());
        assert!(
            workspace.path().join(".agents/experience/rust/entries/1-a/EXPERIENCE.md").is_file()
        );
        assert!(workspace.path().join(".agents/memories/note.md").is_file());
        assert!(workspace.path().join(".agents/custom.md").is_file());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("skills/top.md")).unwrap(),
            "top skill\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("scripts/top.sh")).unwrap(),
            "top script\n"
        );
        assert!(backup.join("guardrails.md").is_file());
        assert!(backup.join(MANIFEST_FILE).is_file());
        assert_eq!(std::fs::read_to_string(backup.join("guardrails.md")).unwrap(), "old\n");
    }

    #[test]
    fn include_memories_replaces_the_memory_surface() {
        let workspace = workspace_with(&[(".agents/memories/note.md", "old memory\n")]);
        let adopted = tree(&[("memories/note.md", "new memory\n")]);
        apply(workspace.path(), &adopted, true).expect("apply");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(".agents/memories/note.md")).unwrap(),
            "new memory\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_managed_paths_are_rejected() {
        let workspace = workspace_with(&[(".agents/custom.md", "keep\n")]);
        let outside = workspace.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, workspace.path().join(".agents/skills"))
            .expect("symlink");
        let adopted = tree(&[("guardrails.md", "new\n")]);
        let error = apply(workspace.path(), &adopted, false).expect_err("symlink rejected");
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[test]
    fn invalid_or_empty_managed_trees_are_rejected() {
        let workspace = workspace_with(&[]);
        let invalid = tree(&[("skills/Bad Name/SKILL.md", "x\n")]);
        let error = validate_source(invalid, workspace.path(), false).expect_err("invalid tree");
        assert!(error.to_string().contains("validation"), "{error}");

        let empty = tree(&[("memories/note.md", "kept\n")]);
        let error = validate_source(empty, workspace.path(), false).expect_err("no managed files");
        assert!(error.to_string().contains("managed surface"), "{error}");
        assert!(
            validate_source(tree(&[("memories/note.md", "kept\n")]), workspace.path(), true)
                .is_ok(),
            "--include-memories makes the memory surface managed"
        );
    }

    #[test]
    fn dry_run_writes_nothing_and_shows_the_plan() {
        let workspace = workspace_with(&[(".agents/guardrails.md", "old\n")]);
        let run_root = tempfile::tempdir().expect("run root");
        let adopted = run_root.path().join("adopted/.agents");
        std::fs::create_dir_all(&adopted).expect("adopted");
        std::fs::write(adopted.join("guardrails.md"), "new\n").expect("adopted rule");
        run(HarnessAdoptArgs {
            run_root: Some(run_root.path().to_path_buf()),
            experience: None,
            library_root: None,
            workspace: workspace.path().to_path_buf(),
            dry_run: true,
            yes: true,
            include_memories: false,
        })
        .expect("dry run");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(".agents/guardrails.md")).unwrap(),
            "old\n",
            "dry-run must not write"
        );
        assert!(!workspace.path().join(".agents/.backup").exists());
    }

    #[test]
    fn non_interactive_without_yes_declines() {
        assert!(!approve(false, false, || true));
        assert!(approve(true, false, || false));
        assert!(approve(false, true, || true));
    }

    #[test]
    fn experience_spec_is_parsed_strictly() {
        assert_eq!(parse_experience("rust/2").expect("valid"), (String::from("rust"), 2));
        assert!(parse_experience("rust").is_err());
        assert!(parse_experience("rust/0").is_err());
        assert!(parse_experience("/2").is_err());
    }
}
