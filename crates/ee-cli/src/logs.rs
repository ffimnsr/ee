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
    std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .map(|dir| dir.join("ee"))
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
