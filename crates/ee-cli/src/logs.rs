use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogPathCandidate {
    pub(crate) label: &'static str,
    pub(crate) path: PathBuf,
}

pub(crate) fn state_dir() -> Option<PathBuf> {
    xi_core_lib::user_state_dir()
}

pub(crate) fn preferred_editor_log_path() -> PathBuf {
    if let Some(path) =
        std::env::var_os("EE_EDITOR_LOG").filter(|value| !value.is_empty()).map(PathBuf::from)
    {
        return path;
    }
    if let Some(state_dir) = state_dir() {
        return state_dir.join("editor.log");
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join("ee.log")
}

pub(crate) fn ensure_editor_log_file() -> io::Result<PathBuf> {
    let path = preferred_editor_log_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = OpenOptions::new().create(true).append(true).open(&path)?;
    Ok(path)
}

pub(crate) fn append_editor_log_line(message: &str) -> io::Result<PathBuf> {
    let path = ensure_editor_log_file()?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "[{timestamp}] {message}")?;
    Ok(path)
}

fn existing_path_identity(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }

    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(canonical_parent) = fs::canonicalize(parent)
    {
        return canonical_parent.join(name);
    }

    path.to_path_buf()
}

fn env_log_override(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn logical_cwd_for_logs() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd_identity = existing_path_identity(&cwd);

    for key in ["EE_EDITOR_LOG", "EE_PLUGIN_LOG"] {
        let Some(parent) =
            env_log_override(key).and_then(|path| path.parent().map(Path::to_path_buf))
        else {
            continue;
        };
        if existing_path_identity(&parent) == cwd_identity {
            return parent;
        }
    }

    cwd
}

pub(crate) fn discover_log_paths() -> Vec<LogPathCandidate> {
    fn push_log_path(
        items: &mut Vec<LogPathCandidate>,
        seen: &mut HashSet<PathBuf>,
        label: &'static str,
        path: PathBuf,
    ) {
        let key = existing_path_identity(&path);
        if seen.insert(key) {
            items.push(LogPathCandidate { label, path });
        }
    }

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let cwd = logical_cwd_for_logs();
    let state_dir = state_dir();

    if let Some(path) = env_log_override("EE_EDITOR_LOG") {
        push_log_path(&mut items, &mut seen, "editor", path);
    }
    push_log_path(&mut items, &mut seen, "editor", cwd.join("ee.log"));
    push_log_path(&mut items, &mut seen, "editor", cwd.join("editor.log"));
    if let Some(state_dir) = state_dir.as_ref() {
        push_log_path(&mut items, &mut seen, "editor", state_dir.join("editor.log"));
    }

    if let Some(path) = env_log_override("EE_PLUGIN_LOG") {
        push_log_path(&mut items, &mut seen, "plugin", path);
    }
    push_log_path(&mut items, &mut seen, "plugin", cwd.join("xi-lsp-plugin.log"));
    if let Some(state_dir) = state_dir.as_ref() {
        push_log_path(&mut items, &mut seen, "plugin", state_dir.join("xi-lsp-plugin.log"));
    }

    items
}

/// Process-global guard that redirects stderr (fd 2) into the editor log for
/// the lifetime of the TUI session. Warnings, panics, and stray diagnostics
/// then land in the log file instead of corrupting the alternate screen. The
/// original descriptor is restored on drop so post-TUI diagnostics still
/// reach the caller's terminal. Stdout is deliberately left untouched: the
/// alternate screen renders through it.
#[cfg(unix)]
pub(crate) struct TuiStderrRedirect {
    saved_stderr: std::os::unix::io::RawFd,
}

/// Starts the TUI stderr redirect against the preferred editor log path.
#[cfg(unix)]
pub(crate) fn redirect_tui_stderr() -> io::Result<TuiStderrRedirect> {
    redirect_stderr_to_path(&preferred_editor_log_path())
}

#[cfg(unix)]
fn redirect_stderr_to_path(path: &Path) -> io::Result<TuiStderrRedirect> {
    use std::os::unix::io::AsRawFd;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
    if saved < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        let error = io::Error::last_os_error();
        unsafe { libc::close(saved) };
        return Err(error);
    }
    Ok(TuiStderrRedirect { saved_stderr: saved })
}

#[cfg(unix)]
impl Drop for TuiStderrRedirect {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved_stderr, libc::STDERR_FILENO);
            libc::close(self.saved_stderr);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn stderr_redirect_lands_in_log_and_restores_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run.log");
        let guard = redirect_stderr_to_path(&path).expect("redirect stderr");
        // libtest intercepts Rust-level eprintln (capture buffer), so probe the
        // fd directly: this is exactly what production eprintln/tracing/panics
        // write through once the descriptor is redirected.
        let probe = b"TUI-STDERR-PROBE-12345\n";
        unsafe { libc::write(libc::STDERR_FILENO, probe.as_ptr().cast(), probe.len()) };
        drop(guard);

        let content = fs::read_to_string(&path).expect("read log");
        assert!(content.contains("TUI-STDERR-PROBE-12345"), "{content}");

        // After restore, further fd writes must not touch the file again.
        let marker = b"TUI-STDERR-RESTORED-MARKER\n";
        unsafe { libc::write(libc::STDERR_FILENO, marker.as_ptr().cast(), marker.len()) };
        let after = fs::read_to_string(&path).expect("read log again");
        assert!(
            !after.contains("TUI-STDERR-RESTORED-MARKER"),
            "stderr must be restored after the guard drops: {after}"
        );
    }
}
