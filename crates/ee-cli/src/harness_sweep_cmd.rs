//! `ee do agent harness sweep`: a JSON task manifest driven through the
//! existing per-domain stream scheduler.
//!
//! The manifest is validated before any host or model call. The library root is
//! invocation-level: sweeps default to `<run-root>/library/<mode>-k<k>` so a
//! benchmark never contaminates the workspace library.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ee_agent_harnessing::{
    HarnessConfig, HarnessMode, HarnessTask, ImproverModel, LibraryLocation, ReflectionSource,
    ReflectionWriter, driver::dry_run_plan, run_stream,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::args::HarnessSweepArgs;
use crate::harness_cmd::{CliWriter, start_host_channel};

/// Maximum tasks accepted in one manifest.
const MAX_SWEEP_TASKS: usize = 128;
/// Wall-clock bound for one sweep child session.
const SWEEP_SESSION_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskManifest {
    tasks: Vec<TaskSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskSpec {
    id: String,
    prompt: String,
    domain: String,
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default)]
    source_roots: Vec<PathBuf>,
    #[serde(default)]
    attachments: Vec<PathBuf>,
}

/// Runs one `harness sweep` invocation.
pub(crate) fn run(args: HarnessSweepArgs) -> io::Result<()> {
    let manifest = std::fs::canonicalize(&args.tasks).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot read task manifest {}: {error}", args.tasks.display()),
        )
    })?;
    let tasks = parse_manifest(&manifest)?;
    let mode: HarnessMode = args.mode.into();
    let run_root = args.run_root.clone().unwrap_or_else(|| default_sweep_run_root(&manifest));
    let default_library = LibraryLocation::run_scoped(&run_root, mode.as_str(), args.k);
    let library = LibraryLocation::resolve(default_library, args.library_root.as_deref());
    let config = HarnessConfig {
        k: args.k,
        mode,
        dry_run: args.dry_run,
        reflect: !args.no_reflect,
        reflection_source: if args.no_reflect {
            ReflectionSource::None
        } else {
            ReflectionSource::Invocation
        },
        improver: ImproverModel { model_id: None, effort: None, max_steps: 12, timeout_secs: 300 },
        max_workers: args.workers,
        resume: true,
        progress: true,
    };
    config.validate().map_err(|error| io::Error::other(error.to_string()))?;
    let tasks: Vec<HarnessTask> =
        tasks.into_iter().map(|task| task.with_library_root(library.root())).collect();
    if args.dry_run {
        for task in &tasks {
            println!("ee: sweep task={} domain={}", task.id, task.domain);
            print!("{}", dry_run_plan(task, &config).render());
        }
        println!("ee: sweep dry-run; no model calls and no run state written");
        return Ok(());
    }
    std::fs::create_dir_all(&run_root)?;
    std::fs::create_dir_all(library.root())?;
    let workspace = tasks
        .first()
        .map(|task| task.workspace.clone())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "manifest has no tasks"))?;
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let (host, handler, channel) =
            start_host_channel(&workspace, &run_root, SWEEP_SESSION_TIMEOUT)?;
        let writer = CliWriter { library_root: library.root().to_path_buf() };
        let writer: Option<&dyn ReflectionWriter> =
            if config.reflect { Some(&writer) } else { None };
        let result = run_stream(&channel, &tasks, &config, &run_root, writer).await;
        host.shutdown().await;
        drop(handler);
        result
    });
    let runs = result.map_err(|error| io::Error::other(error.to_string()))?;
    let mut failures = 0usize;
    for run in &runs {
        match &run.result {
            Ok(harness) if harness.ledger.base_failed || harness.selection.chosen.is_none() => {
                failures += 1;
                let reason =
                    if harness.ledger.base_failed { "base-failed" } else { "no-deliverable" };
                println!(
                    "ee: sweep domain={} task={} status=failed reason={reason}",
                    run.domain, run.task_id
                );
            }
            Ok(harness) => println!(
                "ee: sweep domain={} task={} status=ok chosen={} resumed={}",
                run.domain,
                run.task_id,
                harness.selection.chosen.as_deref().unwrap_or("none"),
                harness.resumed
            ),
            Err(error) => {
                failures += 1;
                println!(
                    "ee: sweep domain={} task={} status=failed error={}",
                    run.domain,
                    run.task_id,
                    truncate(&error.to_string(), 240)
                );
            }
        }
    }
    println!(
        "ee: sweep complete tasks={} failed={failures} library={}",
        runs.len(),
        library.root().display()
    );
    if failures > 0 {
        return Err(io::Error::other(format!("{failures} sweep task(s) failed")));
    }
    Ok(())
}

/// Parses and validates a manifest before any host or model call.
pub(crate) fn parse_manifest(manifest: &Path) -> io::Result<Vec<HarnessTask>> {
    let text = std::fs::read_to_string(manifest).map_err(|error| {
        io::Error::new(error.kind(), format!("cannot read {}: {error}", manifest.display()))
    })?;
    let parsed: TaskManifest = serde_json::from_str(&text).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("task manifest {} is invalid: {error}", manifest.display()),
        )
    })?;
    if parsed.tasks.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "task manifest has no tasks"));
    }
    if parsed.tasks.len() > MAX_SWEEP_TASKS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("task manifest exceeds {MAX_SWEEP_TASKS} tasks"),
        ));
    }
    let base = manifest.parent().unwrap_or_else(|| Path::new("."));
    let mut tasks = Vec::with_capacity(parsed.tasks.len());
    let mut seen = std::collections::BTreeSet::new();
    for spec in parsed.tasks {
        if !seen.insert(spec.id.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate task id {:?} in the manifest", spec.id),
            ));
        }
        let workspace = resolve(base, spec.workspace.unwrap_or_else(|| PathBuf::from(".")));
        let workspace = std::fs::canonicalize(&workspace).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("task {:?} workspace {}: {error}", spec.id, workspace.display()),
            )
        })?;
        let mut task = HarnessTask::new(spec.id, spec.prompt, spec.domain, workspace);
        task.source_roots = spec.source_roots.into_iter().map(|path| resolve(base, path)).collect();
        task.attachments = spec.attachments.into_iter().map(|path| resolve(base, path)).collect();
        task.validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        tasks.push(task);
    }
    Ok(tasks)
}

fn resolve(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() { path } else { base.join(path) }
}

fn default_sweep_run_root(manifest: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(manifest.as_os_str().to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect::<String>();
    dirs::state_dir()
        .or_else(dirs::config_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("ee")
        .join("harness-runs")
        .join(format!("sweep-{hex}"))
}

fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [truncated]", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::HarnessModeArg;

    fn manifest(root: &Path, body: &str) -> PathBuf {
        let path = root.join("tasks.json");
        std::fs::write(&path, body).expect("manifest");
        path
    }

    #[test]
    fn manifest_resolves_relative_paths_and_validates_tasks() {
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join("workspace")).expect("workspace");
        let path = manifest(
            root.path(),
            r#"{
                "tasks": [{
                    "id": "t1",
                    "prompt": "fix it",
                    "domain": "rust",
                    "workspace": "workspace",
                    "source_roots": ["workspace/src"],
                    "attachments": ["notes.md"]
                }]
            }"#,
        );
        let tasks = parse_manifest(&path).expect("manifest");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "t1");
        // macOS tempdirs live under /var, a symlink to /private/var, and
        // parse_manifest canonicalizes the workspace (source roots and
        // attachments stay as resolved). Canonicalize the expectation too so
        // the test is stable on both layouts.
        let expected_workspace =
            std::fs::canonicalize(root.path().join("workspace")).expect("canonical workspace");
        assert_eq!(tasks[0].workspace, expected_workspace);
        assert_eq!(tasks[0].source_roots, vec![root.path().join("workspace/src")]);
        assert_eq!(tasks[0].attachments, vec![root.path().join("notes.md")]);
    }

    #[test]
    fn bad_manifests_fail_before_any_model_call() {
        let root = tempfile::tempdir().expect("root");
        let empty = manifest(root.path(), r#"{"tasks": []}"#);
        assert!(parse_manifest(&empty).is_err());

        let duplicate = manifest(
            root.path(),
            r#"{"tasks": [
                {"id":"t","prompt":"p","domain":"rust","workspace":"."},
                {"id":"t","prompt":"p","domain":"rust","workspace":"."}
            ]}"#,
        );
        assert!(parse_manifest(&duplicate).is_err());

        let unknown = manifest(
            root.path(),
            r#"{"tasks": [{"id":"t","prompt":"p","domain":"rust","extra":true}]}"#,
        );
        assert!(parse_manifest(&unknown).is_err());

        let missing = manifest(
            root.path(),
            r#"{"tasks": [{"id":"t","prompt":"p","domain":"rust","workspace":"missing"}]}"#,
        );
        assert!(parse_manifest(&missing).is_err());

        let invalid_domain = manifest(
            root.path(),
            r#"{"tasks": [{"id":"t","prompt":"p","domain":"Rust!","workspace":"."}]}"#,
        );
        assert!(parse_manifest(&invalid_domain).is_err());
    }

    #[test]
    fn dry_run_prints_plans_without_creating_run_state() {
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join("workspace")).expect("workspace");
        let path = manifest(
            root.path(),
            r#"{"tasks": [{"id":"t","prompt":"p","domain":"rust","workspace":"workspace"}]}"#,
        );
        let run_root = root.path().join("run");
        run(HarnessSweepArgs {
            tasks: path,
            k: 1,
            mode: HarnessModeArg::Parallel,
            workers: 2,
            run_root: Some(run_root.clone()),
            library_root: None,
            no_reflect: false,
            dry_run: true,
        })
        .expect("dry run");
        assert!(!run_root.exists(), "dry-run writes no run state");
    }
}
