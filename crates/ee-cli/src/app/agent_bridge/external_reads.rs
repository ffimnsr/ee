//! External directory read approval (I21).
//!
//! Read-class tools fail closed to the workspace roots unless the user grants
//! one lexical ancestor directory for the app session. Grants are in-memory,
//! canonical directory paths, bounded in number, and consulted only by read
//! paths: writes and execute-class tools keep using the workspace roots and
//! never see these grants. The approved call resumes through the same read
//! path after the user answers the approval prompt.

use std::path::{Path, PathBuf};

use ee_agent_host::{AgentError, ClientRequestResult};
use tokio::sync::oneshot;

use super::super::*;

use crate::app::agents_mcp::ProxyToolCall;

use super::approval::{ApprovalKind, DeferredExternalRead};
use super::prompt::ApprovalPrompt;

/// Upper bound on remembered external read directories. The oldest entry
/// (lexical order) is not evicted automatically; further session grants fall
/// back to allow-once so authority cannot grow without bound.
pub(super) const MAX_EXTERNAL_READ_ROOTS: usize = 32;

/// Upper bound on queued external read approvals, so a misbehaving agent
/// cannot flood the approval queue.
const MAX_PENDING_EXTERNAL_READ_APPROVALS: usize = 8;

impl App {
    /// Whether an approved external directory covers `path` (canonical).
    pub(super) fn external_read_allowed(&self, path: &Path) -> bool {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.agents.external_read_roots.iter().any(|root| canonical.starts_with(root))
    }

    /// Read-scope check for proxy reads that use `path_in_workspace`.
    pub(super) fn path_in_read_scope(&self, path: &Path) -> bool {
        self.path_in_workspace(path)
            || self.path_in_reference_root(path)
            || self.external_read_allowed(path)
    }

    /// Read-scope check for `fs/read_text_file`, which uses the active
    /// workspace root only.
    pub(super) fn path_in_effective_read_scope(&self, path: &Path) -> bool {
        self.path_in_effective_workspace(path)
            || self.path_in_reference_root(path)
            || self.external_read_allowed(path)
    }

    /// Returns the canonical path and ancestor directory when a read-class
    /// proxy call resolves outside the read scope and no grant covers it.
    /// `fs/read_text_file` and git file diffs use the active-root-only guard;
    /// every other proxy read uses all canonical workspace roots.
    pub(super) fn external_read_target(&self, call: &ProxyToolCall) -> Option<(PathBuf, PathBuf)> {
        let (path, effective) = match call {
            ProxyToolCall::Read(request) => (request.path.as_path(), true),
            ProxyToolCall::GitDiffFile { path } => (Path::new(path), true),
            ProxyToolCall::ListDirectory { path }
            | ProxyToolCall::ListDirectoryAll { path }
            | ProxyToolCall::ReadBuffer { path }
            | ProxyToolCall::ReadBufferLines { path, .. }
            | ProxyToolCall::GetFileDiagnostics { path }
            | ProxyToolCall::DocumentSymbols { path }
            | ProxyToolCall::References { path, .. }
            | ProxyToolCall::ListCodeActions { path, .. }
            | ProxyToolCall::PreviewRenameSymbol { path, .. }
            | ProxyToolCall::FileDependencyMap { path }
            | ProxyToolCall::SymbolDependencyMap { path, .. } => (Path::new(path), false),
            _ => return None,
        };
        // Open buffers answer from editor memory and keep their existing
        // behavior; approval only covers disk reads outside the workspace.
        if self.buffer_id_for_path(path).is_some() {
            return None;
        }
        let (canonical, directory) = canonical_read_candidate(path)?;
        let allowed = if effective {
            self.path_in_effective_workspace(&canonical)
        } else {
            self.path_in_workspace(&canonical)
        };
        if allowed
            || self.path_in_reference_root(&canonical)
            || self.external_read_allowed(&canonical)
        {
            return None;
        }
        Some((canonical, directory))
    }

    /// Returns the canonical path and ancestor directory when a native
    /// `fs/read_text_file` target resolves outside the active workspace read
    /// scope and no grant covers it.
    pub(super) fn external_read_target_for_path(&self, path: &Path) -> Option<(PathBuf, PathBuf)> {
        // Open buffers answer from editor memory and keep their existing
        // behavior; approval only covers disk reads outside the workspace.
        if self.buffer_id_for_path(path).is_some() {
            return None;
        }
        let (canonical, directory) = canonical_read_candidate(path)?;
        if self.path_in_effective_workspace(&canonical)
            || self.path_in_reference_root(&canonical)
            || self.external_read_allowed(&canonical)
        {
            return None;
        }
        Some((canonical, directory))
    }

    /// Queues one external directory read approval. Full queues fail closed
    /// with an actionable error instead of growing without bound.
    pub(super) fn request_external_read(
        &mut self,
        session_id: &str,
        read: DeferredExternalRead,
        canonical: PathBuf,
        directory: PathBuf,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let pending = self
            .agents
            .approvals
            .iter()
            .filter(|prompt| matches!(prompt.kind, ApprovalKind::ExternalRead { .. }))
            .count();
        if pending >= MAX_PENDING_EXTERNAL_READ_APPROVALS {
            let _ = reply.send(Err(AgentError::PermissionDenied {
                reason: String::from(
                    "too many pending external directory read approvals; resolve the queued prompts first",
                ),
            }));
            return;
        }
        let prompt = ApprovalPrompt::external_read(session_id, directory, &canonical, read, reply);
        self.agents.approvals.push_back(prompt);
        self.backend.status_message =
            Some(String::from("external directory read approval required"));
    }

    /// Resumes an approved external read. `persist` records the directory for
    /// the app session; otherwise the grant lives only for this one call.
    pub(super) fn dispatch_external_read(
        &mut self,
        read: DeferredExternalRead,
        directory: &Path,
        persist: bool,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        let directory = directory.to_path_buf();
        let already = self.agents.external_read_roots.contains(&directory);
        let persisted =
            persist && (already || self.agents.external_read_roots.len() < MAX_EXTERNAL_READ_ROOTS);
        let temporary = if already { false } else { !persisted };
        if !already {
            self.agents.external_read_roots.insert(directory.clone());
        }

        match read {
            DeferredExternalRead::Proxy { call, route } => {
                self.handle_proxy_tool(*call, route, reply);
            }
            DeferredExternalRead::Fs(request) => {
                // Keep the phase 6 audit trail for the resumed read.
                let _ = self.native_read_decision(&request.path, request.limit.map(u64::from));
                self.bridge_read_file(&request, reply);
            }
        }

        if temporary {
            self.agents.external_read_roots.remove(&directory);
        }
        let message = if persisted {
            format!("external reads approved for this session: {}", directory.display())
        } else {
            format!("external read approved once: {}", directory.display())
        };
        self.backend.status_message = Some(message);
    }

    /// `/permissions revoke-read <path|all>`: removes session read grants.
    pub(crate) fn permissions_revoke_read(&mut self, arg: &str) {
        if arg == "all" {
            let removed = self.agents.external_read_roots.len();
            self.agents.external_read_roots.clear();
            self.permissions_notice(&format!("external read grants revoked: {removed}"));
            return;
        }
        let raw = Path::new(arg);
        let absolute =
            if raw.is_absolute() { raw.to_path_buf() } else { self.working_dir.join(raw) };
        let target = std::fs::canonicalize(&absolute).unwrap_or(absolute);
        let before = self.agents.external_read_roots.len();
        self.agents
            .external_read_roots
            .retain(|root| root != &target && !root.starts_with(&target));
        let removed = before - self.agents.external_read_roots.len();
        if removed == 0 {
            self.permissions_notice(&format!(
                "no session external read grant matches {}",
                target.display()
            ));
        } else {
            self.permissions_notice(&format!(
                "external read grants revoked: {removed} ({})",
                target.display()
            ));
        }
    }

    /// Sorted display form of the session external read grants.
    pub(crate) fn external_read_roots_display(&self) -> Vec<String> {
        self.agents.external_read_roots.iter().map(|root| root.display().to_string()).collect()
    }
}

/// Canonical read target plus the lexical ancestor directory to request.
///
/// Files approve their parent; directories approve themselves. Relative
/// paths, missing names, and filesystem roots are not approvable, so callers
/// fall through to the existing fail-closed rejection.
fn canonical_read_candidate(path: &Path) -> Option<(PathBuf, PathBuf)> {
    if !path.is_absolute() {
        return None;
    }
    let canonical = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => {
            let parent = std::fs::canonicalize(path.parent()?).ok()?;
            parent.join(path.file_name()?)
        }
    };
    let directory =
        if canonical.is_dir() { canonical.clone() } else { canonical.parent()?.to_path_buf() };
    // Never approve a filesystem root: it would cover the whole disk.
    directory.parent()?;
    Some((canonical, directory))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn file_approval_targets_its_parent_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = temp.path().join("hosts");
        fs::write(&file, "127.0.0.1 localhost\n").expect("write");
        let (canonical, directory) = canonical_read_candidate(&file).expect("file candidate");
        assert_eq!(canonical, fs::canonicalize(&file).expect("canonical"));
        assert_eq!(directory, fs::canonicalize(temp.path()).expect("dir"));
    }

    #[test]
    fn directory_approval_targets_itself() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (_, directory) = canonical_read_candidate(temp.path()).expect("directory candidate");
        assert_eq!(directory, fs::canonicalize(temp.path()).expect("dir"));
    }

    #[test]
    fn relative_and_root_paths_are_not_approvable() {
        assert!(canonical_read_candidate(Path::new("etc/hosts")).is_none());
        assert!(canonical_read_candidate(Path::new("/")).is_none());
    }

    #[test]
    fn missing_files_approve_the_canonical_parent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("not-yet-created.txt");
        let (canonical, directory) = canonical_read_candidate(&missing).expect("missing candidate");
        assert_eq!(
            canonical,
            fs::canonicalize(temp.path()).expect("dir").join("not-yet-created.txt")
        );
        assert_eq!(directory, fs::canonicalize(temp.path()).expect("dir"));
    }
}
