//! Named reference sources (I25).
//!
//! `[agents.references]` pins read-only local directories or cached remote git
//! repositories. Remote sources clone into a bounded host cache on first use
//! and refresh on a daily throttle; resolution failures are named errors, never
//! hangs. Reference roots are read-only: they are absent from every write root
//! and every workspace-root list.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use ee_agent_host::AgentError;
use ee_agent_protocol::{ContentBlock, TextContent};

use super::*;

use crate::app::agents_mcp::ProxyToolCall;
use crate::config::AgentReferenceSettings;

/// Refresh window for cached remote references.
pub(super) const REFERENCE_REFRESH_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// Bound on clone/fetch so a dead remote fails as a named error.
const REFERENCE_GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on one cached checkout after clone/refresh.
const REFERENCE_MAX_CHECKOUT_BYTES: u64 = 256 * 1024 * 1024;
/// Bound on the reference section added to prompts.
const REFERENCE_PROMPT_MAX_BYTES: usize = 2_048;

/// Resolution context shared by the `App` methods and tests.
pub(super) struct ReferenceContext<'a> {
    pub(super) references: &'a BTreeMap<String, AgentReferenceSettings>,
    pub(super) working_dir: &'a Path,
    pub(super) cache_base: Option<PathBuf>,
    pub(super) refresh_window: Duration,
}

impl ReferenceContext<'_> {
    /// Resolves a relative `<reference>/<path>` target. Absolute and
    /// non-reference paths return `None`.
    pub(super) fn resolve_path(&self, raw: &Path) -> Result<Option<PathBuf>, AgentError> {
        if raw.is_absolute() {
            return Ok(None);
        }
        let raw_text = raw.to_string_lossy();
        let Some((name, relative)) = split_reference_prefix(&raw_text) else {
            return Ok(None);
        };
        let Some(source) = self.references.get(name) else {
            return Ok(None);
        };
        let relative_path = Path::new(relative);
        if relative.is_empty()
            || relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(AgentError::invalid_params(format!(
                "invalid reference path `{raw_text}`: use `<name>/<relative-path>` without `..`"
            )));
        }
        let root = self.root(name, source)?;
        let candidate = root.join(relative_path);
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            AgentError::Io(format!("cannot read reference `{name}` path `{relative}`: {error}"))
        })?;
        if !canonical.starts_with(&root) {
            return Err(AgentError::PermissionDenied {
                reason: format!(
                    "reference `{name}` path escapes its root (symlink or traversal): `{relative}`"
                ),
            });
        }
        Ok(Some(canonical))
    }

    /// Resolves one reference root without requiring the target to exist.
    fn root(&self, name: &str, source: &AgentReferenceSettings) -> Result<PathBuf, AgentError> {
        match source {
            AgentReferenceSettings::Local(path) => {
                let path =
                    if path.is_absolute() { path.clone() } else { self.working_dir.join(path) };
                let canonical = std::fs::canonicalize(&path).map_err(|error| {
                    AgentError::HandlerError(format!(
                        "reference `{name}` unavailable: {}: {error}",
                        path.display()
                    ))
                })?;
                if !canonical.is_dir() {
                    return Err(AgentError::HandlerError(format!(
                        "reference `{name}` is not a directory: {}",
                        canonical.display()
                    )));
                }
                Ok(canonical)
            }
            AgentReferenceSettings::Remote { repository, branch } => {
                let Some(base) = &self.cache_base else {
                    return Err(AgentError::HandlerError(format!(
                        "reference `{name}` unavailable: no ee state directory for the reference cache"
                    )));
                };
                ReferenceCache::new(base.clone(), self.refresh_window)
                    .remote_root(name, repository, branch.as_deref())
                    .map_err(|error| {
                        AgentError::HandlerError(format!("reference `{name}` unavailable: {error}"))
                    })
            }
        }
    }

    /// Whether `path` is inside a configured reference root. Reads inside a
    /// reference bypass external-directory approval; writes never consult this.
    pub(super) fn contains(&self, path: &Path) -> bool {
        if self.references.is_empty() {
            return false;
        }
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.references.iter().any(|(name, source)| {
            let root = match source {
                AgentReferenceSettings::Local(path) => {
                    let path =
                        if path.is_absolute() { path.clone() } else { self.working_dir.join(path) };
                    std::fs::canonicalize(path).unwrap_or_default()
                }
                AgentReferenceSettings::Remote { .. } => {
                    self.cache_base.as_ref().map(|base| base.join(name)).unwrap_or_default()
                }
            };
            !root.as_os_str().is_empty() && canonical.starts_with(root)
        })
    }

    /// Bounded prompt section listing the configured read-only references.
    pub(super) fn prompt_text(&self) -> Option<String> {
        if self.references.is_empty() {
            return None;
        }
        let mut text =
            String::from("Reference sources (read-only; read files with `<name>/relative/path`):");
        for (name, source) in self.references {
            let line = match source {
                AgentReferenceSettings::Local(path) => format!("\n- {name}: {}", path.display()),
                AgentReferenceSettings::Remote { repository, branch } => match branch {
                    Some(branch) => format!(
                        "\n- {name}: {repository} @ {branch} (cached under the ee state directory on first use)"
                    ),
                    None => format!(
                        "\n- {name}: {repository} (cached under the ee state directory on first use)"
                    ),
                },
            };
            if text.len() + line.len() > REFERENCE_PROMPT_MAX_BYTES {
                text.push_str("\n- (more references omitted)");
                break;
            }
            text.push_str(&line);
        }
        Some(text)
    }
}

impl App {
    /// Resolves a relative `<reference>/<path>` read target into the cached or
    /// local reference root. Absolute and non-reference paths pass through.
    pub(super) fn resolve_reference_path(&self, raw: &Path) -> Result<Option<PathBuf>, AgentError> {
        self.reference_context()?.resolve_path(raw)
    }

    /// Rewrites read-class proxy calls that use a reference prefix to their
    /// absolute root path. Write-class calls are never rewritten.
    pub(super) fn resolve_proxy_reference_paths(
        &self,
        call: &mut ProxyToolCall,
    ) -> Result<(), AgentError> {
        if self.config.agents.references.is_empty() {
            return Ok(());
        }
        let context = self.reference_context()?;
        match call {
            ProxyToolCall::Read(request) => {
                if let Some(resolved) = context.resolve_path(&request.path)? {
                    request.path = resolved;
                }
            }
            ProxyToolCall::ListDirectory { path }
            | ProxyToolCall::ListDirectoryAll { path }
            | ProxyToolCall::ReadBuffer { path }
            | ProxyToolCall::ReadBufferLines { path, .. }
            | ProxyToolCall::GetFileDiagnostics { path }
            | ProxyToolCall::DocumentSymbols { path }
            | ProxyToolCall::References { path, .. }
            | ProxyToolCall::ListCodeActions { path, .. }
            | ProxyToolCall::PreviewRenameSymbol { path, .. }
            | ProxyToolCall::GitDiffFile { path }
            | ProxyToolCall::FileDependencyMap { path }
            | ProxyToolCall::SymbolDependencyMap { path, .. } => {
                if let Some(resolved) = context.resolve_path(Path::new(path))? {
                    *path = resolved.display().to_string();
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Resolves a relative `fs/read_text_file` path through a reference name.
    pub(super) fn resolve_fs_reference_path(&self, path: &mut PathBuf) -> Result<(), AgentError> {
        if let Some(resolved) = self.resolve_reference_path(path)? {
            *path = resolved;
        }
        Ok(())
    }

    /// Whether `path` is inside a configured reference root.
    pub(super) fn path_in_reference_root(&self, path: &Path) -> bool {
        self.reference_context().is_ok_and(|context| context.contains(path))
    }

    /// Bounded prompt section listing the configured read-only references.
    pub(super) fn reference_prompt_block(&self) -> Option<ContentBlock> {
        let context = self.reference_context().ok()?;
        context.prompt_text().map(|text| ContentBlock::Text(TextContent::new(text)))
    }

    fn reference_context(&self) -> Result<ReferenceContext<'_>, AgentError> {
        Ok(ReferenceContext {
            references: &self.config.agents.references,
            working_dir: &self.working_dir,
            cache_base: self.reference_cache_base(),
            refresh_window: REFERENCE_REFRESH_WINDOW,
        })
    }

    fn reference_cache_base(&self) -> Option<PathBuf> {
        if let Some(base) = &self.agents.test_reference_cache_base {
            return Some(base.clone());
        }
        crate::logs::state_dir().map(|dir| dir.join("references"))
    }
}

/// Splits `<name>/<rest>` or `<name>:<rest>`; names never contain separators.
fn split_reference_prefix(raw: &str) -> Option<(&str, &str)> {
    let (name, rest) = match raw.split_once(':') {
        Some((name, rest)) if !name.is_empty() && !name.contains('/') => (name, rest),
        _ => {
            let (name, rest) = raw.split_once('/')?;
            (name, rest)
        }
    };
    let valid_name = !name.is_empty()
        && name.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        && name != "."
        && name != "..";
    valid_name.then_some((name, rest))
}

/// Bounded remote-reference cache under the ee state directory.
struct ReferenceCache {
    base: PathBuf,
    refresh_window: Duration,
}

impl ReferenceCache {
    fn new(base: PathBuf, refresh_window: Duration) -> Self {
        Self { base, refresh_window }
    }

    /// Returns the cached checkout root, cloning on first use and refreshing
    /// no more than once per window.
    fn remote_root(
        &self,
        name: &str,
        repository: &str,
        branch: Option<&str>,
    ) -> Result<PathBuf, String> {
        let root = self.base.join(name);
        let stamp = self.base.join(format!("{name}.stamp"));
        let fresh = stamp_fresh(&stamp, self.refresh_window);
        if root.is_dir() && fresh {
            return Ok(root);
        }
        std::fs::create_dir_all(&self.base)
            .map_err(|error| format!("cannot create reference cache: {error}"))?;
        if root.is_dir() {
            self.refresh(&root, branch)?;
        } else {
            self.clone(repository, branch, &root)?;
        }
        if let Err(error) = checkout_within_budget(&root) {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_file(&stamp);
            return Err(error);
        }
        let _ = std::fs::write(&stamp, b"");
        Ok(root)
    }

    fn clone(&self, repository: &str, branch: Option<&str>, root: &Path) -> Result<(), String> {
        let temp = self.base.join(format!(
            ".tmp-{}-{}-{}",
            root.file_name().and_then(|name| name.to_str()).unwrap_or("reference"),
            std::process::id(),
            nonce()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        let mut args = vec![String::from("clone"), String::from("--depth"), String::from("1")];
        if let Some(branch) = branch {
            args.push(String::from("--branch"));
            args.push(branch.to_string());
        }
        args.push(String::from("--"));
        args.push(repository.to_string());
        args.push(temp.display().to_string());
        run_git_bounded(&args, &self.base)?;
        match std::fs::rename(&temp, root) {
            Ok(()) => Ok(()),
            Err(_) if root.is_dir() => {
                // Another session won the race; keep the existing checkout.
                let _ = std::fs::remove_dir_all(&temp);
                Ok(())
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&temp);
                Err(format!("cannot finalize clone: {error}"))
            }
        }
    }

    fn refresh(&self, root: &Path, branch: Option<&str>) -> Result<(), String> {
        let mut fetch = vec![String::from("fetch"), String::from("--depth"), String::from("1")];
        fetch.push(String::from("origin"));
        fetch.push(branch.unwrap_or("HEAD").to_string());
        run_git_bounded(&fetch, root)?;
        run_git_bounded(
            &[String::from("reset"), String::from("--hard"), String::from("FETCH_HEAD")],
            root,
        )
    }
}

/// Fails when a checkout exceeds the cache budget; the caller removes it.
fn checkout_within_budget(root: &Path) -> Result<(), String> {
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("cannot inspect cached checkout: {error}"))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("cannot inspect cached checkout: {error}"))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect cached checkout: {error}"))?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
                if total > REFERENCE_MAX_CHECKOUT_BYTES {
                    return Err(format!(
                        "cached checkout exceeds the {REFERENCE_MAX_CHECKOUT_BYTES} byte reference cache budget"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn stamp_fresh(stamp: &Path, window: Duration) -> bool {
    let Ok(modified) = std::fs::metadata(stamp).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    SystemTime::now().duration_since(modified).is_ok_and(|age| age < window)
}

fn nonce() -> u128 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos())
}

/// Runs one bounded git command with nulled input and capped diagnostics.
///
/// `cwd` is required: git children must never inherit the process-wide
/// directory, which another thread can move or delete while the child starts.
fn run_git_bounded(args: &[String], cwd: &Path) -> Result<(), String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        format!("cannot run git {}: {error}", args.first().map_or("", String::as_str))
    })?;
    let stderr = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.by_ref().take(4_096).read_to_string(&mut text);
            text
        })
    });
    let deadline = Instant::now() + REFERENCE_GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(error) => return Err(format!("cannot wait for git: {error}")),
        }
    };
    let stderr = stderr.map(|handle| handle.join().unwrap_or_default()).unwrap_or_default();
    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(format!(
            "git {} exited with {status}: {}",
            args.first().map_or("", String::as_str),
            stderr.trim()
        )),
        None => Err(format!(
            "git {} timed out after {REFERENCE_GIT_TIMEOUT:?}",
            args.first().map_or("", String::as_str)
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::config::parse_agent_reference;

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "ee")
            .env("GIT_AUTHOR_EMAIL", "ee@example.com")
            .env("GIT_COMMITTER_NAME", "ee")
            .env("GIT_COMMITTER_EMAIL", "ee@example.com")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    fn source_repo(temp: &Path) -> PathBuf {
        let repo = temp.join("source");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git(&repo, &["init", "--quiet"]);
        std::fs::write(repo.join("notes.txt"), "v1\n").expect("seed");
        git(&repo, &["add", "notes.txt"]);
        git(&repo, &["commit", "--quiet", "-m", "seed"]);
        repo
    }

    fn references(
        entries: &[(&str, AgentReferenceSettings)],
    ) -> BTreeMap<String, AgentReferenceSettings> {
        entries.iter().map(|(name, source)| ((*name).to_string(), source.clone())).collect()
    }

    #[test]
    fn reference_prefixes_split_names_and_relative_paths() {
        assert_eq!(split_reference_prefix("docs/src/lib.rs"), Some(("docs", "src/lib.rs")));
        assert_eq!(split_reference_prefix("docs:src/lib.rs"), Some(("docs", "src/lib.rs")));
        assert_eq!(
            split_reference_prefix("upstream-2/README.md"),
            Some(("upstream-2", "README.md"))
        );
        assert_eq!(split_reference_prefix("/abs/path"), None);
        assert_eq!(split_reference_prefix("../escape"), None);
        assert_eq!(split_reference_prefix("single"), None);
    }

    #[test]
    fn local_reference_reads_resolve_read_only() {
        let workspace = tempfile::tempdir().expect("workspace");
        let reference = tempfile::tempdir().expect("reference");
        std::fs::write(reference.path().join("README.md"), "pinned\n").expect("seed");
        let refs =
            references(&[("docs", AgentReferenceSettings::Local(reference.path().to_path_buf()))]);
        let context = ReferenceContext {
            references: &refs,
            working_dir: workspace.path(),
            cache_base: None,
            refresh_window: REFERENCE_REFRESH_WINDOW,
        };
        let resolved = context
            .resolve_path(Path::new("docs/README.md"))
            .expect("resolves")
            .expect("reference path");
        assert_eq!(resolved, std::fs::canonicalize(reference.path().join("README.md")).unwrap());
        assert!(context.contains(&resolved));
        assert!(context.prompt_text().is_some());
        assert!(
            context.resolve_path(Path::new("docs/../escape")).is_err(),
            "traversal fails closed"
        );
        assert!(context.resolve_path(Path::new("unknown/README.md")).expect("unknown").is_none());
    }

    #[test]
    fn remote_reference_clones_once_then_refreshes_after_the_window() {
        let temp = tempfile::tempdir().expect("temp");
        let repo = source_repo(temp.path());
        let base = temp.path().join("cache");
        let cache = ReferenceCache::new(base.clone(), Duration::from_secs(60 * 60));
        let root =
            cache.remote_root("upstream", repo.to_str().expect("utf8"), None).expect("clone");
        assert_eq!(std::fs::read_to_string(root.join("notes.txt")).unwrap(), "v1\n");

        std::fs::write(repo.join("notes.txt"), "v2\n").expect("edit");
        git(&repo, &["add", "notes.txt"]);
        git(&repo, &["commit", "--quiet", "-m", "v2"]);
        let again =
            cache.remote_root("upstream", repo.to_str().expect("utf8"), None).expect("cached");
        assert_eq!(
            std::fs::read_to_string(again.join("notes.txt")).unwrap(),
            "v1\n",
            "within the refresh window the cache is reused"
        );

        let refreshed = ReferenceCache::new(base, Duration::ZERO)
            .remote_root("upstream", repo.to_str().expect("utf8"), None)
            .expect("refresh");
        assert_eq!(std::fs::read_to_string(refreshed.join("notes.txt")).unwrap(), "v2\n");
    }

    #[test]
    fn missing_remote_fails_with_a_named_error() {
        let temp = tempfile::tempdir().expect("temp");
        let cache = ReferenceCache::new(temp.path().join("cache"), Duration::from_secs(60));
        let missing = temp.path().join("does-not-exist");
        let error = cache
            .remote_root("broken", missing.to_str().expect("utf8"), None)
            .expect_err("missing repository fails");
        assert!(error.contains("git clone"), "{error}");
    }

    #[test]
    fn reference_prompt_block_is_bounded_and_lists_sources() {
        let workspace = tempfile::tempdir().expect("workspace");
        let refs = references(&[
            ("docs", AgentReferenceSettings::Local(PathBuf::from("/tmp/docs"))),
            (
                "upstream",
                AgentReferenceSettings::Remote {
                    repository: String::from("https://example.com/upstream.git"),
                    branch: Some(String::from("main")),
                },
            ),
        ]);
        let context = ReferenceContext {
            references: &refs,
            working_dir: workspace.path(),
            cache_base: None,
            refresh_window: REFERENCE_REFRESH_WINDOW,
        };
        let text = context.prompt_text().expect("text");
        assert!(text.contains("docs: /tmp/docs"));
        assert!(text.contains("upstream: https://example.com/upstream.git @ main"));
        assert!(text.len() <= REFERENCE_PROMPT_MAX_BYTES);
    }

    #[test]
    fn agents_settings_parse_local_and_remote_references() {
        assert!(parse_agent_reference("docs", &serde_json::json!("/tmp/docs")).is_ok());
        assert!(
            parse_agent_reference(
                "up",
                &serde_json::json!({ "repository": "https://example.com/r.git", "branch": "main" })
            )
            .is_ok()
        );
        assert!(parse_agent_reference("bad/name", &serde_json::json!("/tmp")).is_err());
        assert!(
            parse_agent_reference("up", &serde_json::json!({ "repository": "r", "branch": "-x" }))
                .is_err()
        );
    }
}
