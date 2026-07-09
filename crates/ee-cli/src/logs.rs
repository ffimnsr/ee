use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogPathCandidate {
    pub(crate) label: &'static str,
    pub(crate) path: PathBuf,
}

pub(crate) fn discover_log_paths() -> Vec<LogPathCandidate> {
    fn push_log_path(
        items: &mut Vec<LogPathCandidate>,
        seen: &mut HashSet<PathBuf>,
        label: &'static str,
        path: PathBuf,
    ) {
        if seen.insert(path.clone()) {
            items.push(LogPathCandidate { label, path });
        }
    }

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let state_dir = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .map(|dir| dir.join("ee"));

    if let Some(path) =
        std::env::var_os("EE_EDITOR_LOG").filter(|value| !value.is_empty()).map(PathBuf::from)
    {
        push_log_path(&mut items, &mut seen, "editor", path);
    }
    push_log_path(&mut items, &mut seen, "editor", cwd.join("ee.log"));
    push_log_path(&mut items, &mut seen, "editor", cwd.join("editor.log"));
    if let Some(state_dir) = state_dir.as_ref() {
        push_log_path(&mut items, &mut seen, "editor", state_dir.join("editor.log"));
    }

    if let Some(path) =
        std::env::var_os("EE_PLUGIN_LOG").filter(|value| !value.is_empty()).map(PathBuf::from)
    {
        push_log_path(&mut items, &mut seen, "plugin", path);
    }
    push_log_path(&mut items, &mut seen, "plugin", cwd.join("xi-lsp-plugin.log"));
    if let Some(state_dir) = state_dir.as_ref() {
        push_log_path(&mut items, &mut seen, "plugin", state_dir.join("xi-lsp-plugin.log"));
    }

    items
}
