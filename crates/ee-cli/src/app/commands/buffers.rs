//! `impl App` command methods: buffers.
use super::*;

impl App {
    pub(super) fn current_workspace_root(&self) -> PathBuf {
        self.backend
            .active()
            .path
            .as_deref()
            .and_then(|path| crate::config::find_git_root(path.parent().unwrap_or(path)))
            .or_else(|| {
                std::env::current_dir().ok().and_then(|cwd| crate::config::find_git_root(&cwd))
            })
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }
    pub(super) fn current_picker_root(&self) -> Option<PathBuf> {
        self.backend.active().path.as_ref().and_then(|path| path.parent().map(Path::to_path_buf))
    }
    pub(super) fn open_file_picker_at(&mut self, cwd: PathBuf, title: &str) {
        let mut picker = PickerState::new_files(cwd);
        picker.title = title.to_owned();
        self.open_picker(picker);
    }
    pub(crate) fn open_file_picker_for_buffer_directory(&mut self) {
        let cwd = self
            .current_picker_root()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Files");
    }
    pub(crate) fn open_file_picker_in_current_directory(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Files (cwd)");
    }
    pub(crate) fn open_file_explorer(&mut self) {
        self.open_file_picker_at(self.current_workspace_root(), "Explorer");
    }
    pub(crate) fn open_file_explorer_for_buffer_directory(&mut self) {
        let cwd = self
            .current_picker_root()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Explorer (buffer dir)");
    }
    pub(crate) fn open_file_explorer_in_current_directory(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.open_file_picker_at(cwd, "Explorer (cwd)");
    }
    pub(crate) fn open_buffer_picker(&mut self) {
        let entries: Vec<_> = self
            .backend
            .all_bufs()
            .iter()
            .map(|buffer| (buffer.id, buffer.title(), buffer.path.clone()))
            .collect();
        self.open_picker(PickerState::new_buffers(entries));
    }
    pub(super) fn existing_buffer_id_for_path(&self, path: &Path) -> Option<BufferId> {
        let requested = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.backend
            .all_bufs()
            .iter()
            .find(|buffer| {
                buffer.path.as_ref().is_some_and(|buffer_path| {
                    std::fs::canonicalize(buffer_path).unwrap_or_else(|_| buffer_path.clone())
                        == requested
                })
            })
            .map(|buffer| buffer.id)
    }
    pub(super) fn open_or_reuse_buffer(&mut self, path: PathBuf) -> Result<BufferId, String> {
        match self.existing_buffer_id_for_path(&path) {
            Some(id) => Ok(id),
            None => {
                self.backend.open_buffer(Some(path)).map_err(|err| format!("open failed: {err}"))
            }
        }
    }
    pub(super) fn open_path_in_current_view(&mut self, path: PathBuf) -> Result<(), String> {
        let buf_id = self.open_or_reuse_buffer(path)?;
        self.backend.switch_to_id(buf_id).map_err(|err| format!("open failed: {err}"))?;
        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
        self.viewport = Viewport::default();
        Ok(())
    }
    pub(super) fn open_location_picker(
        &mut self,
        title: &str,
        empty_message: &str,
        items: Vec<crate::picker::PickerItem>,
    ) {
        if items.is_empty() {
            self.backend.status_message = Some(empty_message.to_owned());
            return;
        }
        self.open_picker(PickerState::new_locations(title, items));
    }
    pub(crate) fn open_jump_list_picker(&mut self) {
        let items = self
            .jump_list
            .iter()
            .enumerate()
            .map(|(index, (line, col))| crate::picker::PickerItem {
                label: format!(
                    "{}:{}:{} {}",
                    index + 1,
                    line + 1,
                    col + 1,
                    self.backend
                        .active()
                        .get_line(*line)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .unwrap_or("<blank>")
                ),
                detail: None,
                path: None,
                buf_id: None,
                line: Some(*line),
                col: Some(*col),
                choice_index: None,
            })
            .collect();
        self.open_location_picker("Jumplist", "no jumplist entries", items);
    }
    pub(crate) fn open_changed_file_picker(&mut self) {
        let repo_root = self
            .backend
            .active()
            .path
            .as_deref()
            .and_then(|path| crate::config::find_git_root(path.parent().unwrap_or(path)))
            .or_else(|| {
                std::env::current_dir().ok().and_then(|cwd| crate::config::find_git_root(&cwd))
            });
        let Some(repo_root) = repo_root else {
            self.backend.status_message = Some(String::from("changed files: not inside git repo"));
            return;
        };
        match crate::git::changed_files(&repo_root) {
            Ok(files) => {
                let items = files
                    .into_iter()
                    .map(|path| {
                        let label = path
                            .strip_prefix(&repo_root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned();
                        crate::picker::PickerItem {
                            label,
                            detail: None,
                            path: Some(path),
                            buf_id: None,
                            line: None,
                            col: None,
                            choice_index: None,
                        }
                    })
                    .collect();
                self.open_location_picker("Changed Files", "no changed files", items);
            }
            Err(err) => {
                self.backend.status_message = Some(format!("changed files failed: {err}"));
            }
        }
    }
    pub(crate) fn open_diagnostics_picker(&mut self) {
        let active_id = self.backend.active().id;
        let items = self
            .active_diagnostic_items()
            .into_iter()
            .map(|(_, entry)| crate::picker::PickerItem {
                label: entry.display_label(),
                detail: entry.path.as_ref().map(|path| path.to_string_lossy().into_owned()),
                path: entry.path,
                buf_id: Some(active_id),
                line: Some(entry.line),
                col: Some(entry.col),
                choice_index: None,
            })
            .collect();
        self.open_location_picker("Diagnostics", "no diagnostics", items);
    }
    pub(crate) fn open_workspace_diagnostics_picker(&mut self) {
        let items = self
            .backend
            .all_bufs()
            .iter()
            .flat_map(|buffer| {
                let prefix = buffer
                    .path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| buffer.title());
                buffer.diagnostics.iter().map(move |diagnostic| {
                    // Whole-buffer policy-allowed: diagnostic offset→line/col requires full text mirror.
                    let (line, col) = line_col_for_offset(&buffer.lines, diagnostic.range.start);
                    crate::picker::PickerItem {
                        label: format!("{prefix}:{}: {}", line + 1, diagnostic.message),
                        detail: buffer
                            .path
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned()),
                        path: buffer.path.clone(),
                        buf_id: Some(buffer.id),
                        line: Some(line),
                        col: Some(col),
                        choice_index: None,
                    }
                })
            })
            .collect();
        self.open_location_picker("Workspace Diagnostics", "no workspace diagnostics", items);
    }
    pub(in crate::app) fn open_global_search(&mut self) {
        let cwd = self.current_workspace_root();
        let mut picker = PickerState::new_grep(String::new(), cwd);
        picker.title = String::from("Global Search");
        self.open_picker(picker);
        self.enter_normal_mode();
    }
    pub(in crate::app) fn open_command_palette(&mut self) {
        self.open_help_picker("Command Palette", Self::command_help_items());
    }
    pub(crate) fn reopen_last_picker(&mut self) {
        let Some(picker) = self.last_picker.clone() else {
            self.backend.status_message = Some(String::from("no previous picker"));
            return;
        };
        self.picker = Some(picker);
    }
}
