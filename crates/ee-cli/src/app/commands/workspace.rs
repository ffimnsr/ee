//! `impl App` command methods: workspace.
use super::*;

impl App {
    pub(super) fn set_current_buffer_language(
        &mut self,
        requested: &str,
    ) -> Result<String, String> {
        let language = xi_core_lib::tree_sitter_support::canonical_language_name(requested)
            .ok_or_else(|| format!("set_language: unknown language `{requested}`"))?;
        self.syntax_overrides.insert(self.backend.active().id, language.clone());
        Ok(language)
    }
    pub(super) fn reload_runtime_config(&mut self) -> Result<String, String> {
        let active_path = self.backend.active().path.clone();
        self.config = crate::config::load_config(active_path.as_deref());
        self.key_bindings = crate::keymap::bindings_for(&self.config.keymap);
        self.key_sequences = crate::keymap::sequence_bindings_for(&self.config.keymap);
        self.backend
            .reload_editor_config()
            .map_err(|err| format!("config reload failed: {err}"))?;
        Ok(String::from("config reloaded"))
    }
    pub(super) fn resolve_workspace_path(&self, target: &str) -> Result<PathBuf, String> {
        let target = target.trim();
        if target.is_empty() {
            return Err(String::from("path cannot be empty"));
        }

        let workspace_root = self.current_workspace_root();
        let relative = if Path::new(target).is_absolute() {
            Path::new(target).strip_prefix(&workspace_root).map_err(|_| {
                format!("path must stay under workspace {}", workspace_root.display())
            })?
        } else {
            Path::new(target)
        };

        let mut resolved = workspace_root.clone();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => resolved.push(part),
                Component::ParentDir => {
                    if resolved == workspace_root {
                        return Err(format!(
                            "path must stay under workspace {}",
                            workspace_root.display()
                        ));
                    }
                    resolved.pop();
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!(
                        "path must stay under workspace {}",
                        workspace_root.display()
                    ));
                }
            }
        }

        Ok(resolved)
    }
    pub(super) fn create_directory_in_workspace(&mut self, target: &str) -> Result<String, String> {
        let workspace_root = self.current_workspace_root();
        let path = self
            .resolve_workspace_path(target)
            .map_err(|message| format!("create_directory: {message}"))?;

        if path.exists() && !path.is_dir() {
            return Err(format!(
                "create_directory failed: {} exists and is not a directory",
                path.display()
            ));
        }

        std::fs::create_dir_all(&path).map_err(|err| format!("create_directory failed: {err}"))?;
        let display = path.strip_prefix(&workspace_root).unwrap_or(&path);
        Ok(format!("created {}", display.display()))
    }
    pub(super) fn read_file_into_buffer(&mut self, path: &str) -> Result<String, String> {
        let path = PathBuf::from(path);
        let content =
            std::fs::read_to_string(&path).map_err(|err| format!("read failed: {err}"))?;
        self.backend
            .send_edit("insert", json!({ "chars": content }))
            .map_err(|err| format!("read failed: {err}"))?;
        Ok(format!("read {}", path.display()))
    }
    pub(super) fn move_current_buffer(&mut self, target: &str) -> Result<String, String> {
        let Some(source) = self.backend.active().path.clone() else {
            return Err(String::from("move: current buffer has no backing file"));
        };
        let target = PathBuf::from(target);
        if source == target {
            return Err(String::from("move: source and destination are the same"));
        }
        if target.exists() {
            return Err(format!("move failed: {} already exists", target.display()));
        }
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| format!("move failed: {err}"))?;
        }

        let buffer_id = self.backend.active().id;
        let pristine =
            self.backend.buffer_pristine(buffer_id).map_err(|err| format!("move failed: {err}"))?;
        if pristine {
            std::fs::rename(&source, &target).map_err(|err| format!("move failed: {err}"))?;
        }
        self.backend
            .set_buffer_path(buffer_id, target.clone())
            .map_err(|err| format!("move failed: {err}"))?;
        self.backend.save_buffer(buffer_id).map_err(|err| format!("move failed: {err}"))?;
        if !pristine {
            std::fs::remove_file(&source).map_err(|err| format!("move failed: {err}"))?;
        }
        Ok(format!("moved {} -> {}", source.display(), target.display()))
    }
    pub(super) fn clear_register_command(&mut self, target: &str) -> Result<String, String> {
        let target = target.trim();
        if target.is_empty() {
            self.registers.clear(None);
            return Ok(String::from("registers cleared"));
        }
        let mut chars = target.chars();
        let Some(name) = chars.next() else {
            return Err(String::from("clear_register: usage: :clear_register [register]"));
        };
        if chars.next().is_some() {
            return Err(String::from("clear_register: usage: :clear_register [register]"));
        }
        let register = RegisterName::from_char(name)
            .ok_or_else(|| format!("clear_register: invalid register `{name}`"))?;
        self.registers.clear(Some(&register));
        Ok(format!("register {name} cleared"))
    }
    pub(super) fn insert_register_command(&mut self, target: &str) -> Result<String, String> {
        let target = target.trim();
        let mut chars = target.chars();
        let Some(name) = chars.next() else {
            return Err(String::from("insert_register: usage: :insert_register <register>"));
        };
        if chars.next().is_some() {
            return Err(String::from("insert_register: usage: :insert_register <register>"));
        }
        let register = RegisterName::from_char(name)
            .ok_or_else(|| format!("insert_register: invalid register `{name}`"))?;
        let text = self.registers.get(&register);
        if text.is_empty() {
            return Ok(format!("register {name} empty"));
        }
        self.backend
            .send_edit("insert", json!({ "chars": text }))
            .map_err(|err| format!("insert_register failed: {err}"))?;
        Ok(format!("inserted register {name}"))
    }
    pub(super) fn save_current_buffer(&mut self) -> Result<SaveOutcome, String> {
        match self.backend.save() {
            Ok(()) => Ok(SaveOutcome::Saved),
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                self.start_privileged_save_confirm()?;
                Ok(SaveOutcome::AwaitingPrivilegeConfirm)
            }
            Err(err) => Err(format!("save failed: {err}")),
        }
    }
    pub(super) fn save_all_dirty_buffers(&mut self) -> Result<SaveOutcome, String> {
        use std::collections::HashSet;

        self.backend.flush_all_pending_edits().map_err(|err| format!("save failed: {err}"))?;
        let mut seen_paths = HashSet::new();
        let candidate_ids = self
            .backend
            .all_bufs()
            .iter()
            .rev()
            .filter_map(|buf| {
                let path = buf.path.as_ref()?;
                let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                seen_paths.insert(key).then_some(buf.id)
            })
            .collect::<Vec<_>>();
        for id in candidate_ids {
            if self.backend.buffer_pristine(id).map_err(|err| format!("save failed: {err}"))? {
                continue;
            }
            match self.backend.save_buffer(id) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                    if id == self.backend.active().id {
                        self.start_privileged_save_confirm()?;
                        return Ok(SaveOutcome::AwaitingPrivilegeConfirm);
                    }
                    return Err(format!(
                        "save failed: permission denied for {}",
                        self.backend
                            .all_bufs()
                            .iter()
                            .find(|buf| buf.id == id)
                            .and_then(|buf| buf.path.as_ref())
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| String::from("buffer"))
                    ));
                }
                Err(err) => return Err(format!("save failed: {err}")),
            }
        }
        Ok(SaveOutcome::Saved)
    }
    pub(super) fn reload_all_buffers(&mut self) -> Result<(), String> {
        let ids = self.backend.all_bufs().iter().map(|buf| buf.id).collect::<Vec<_>>();
        for id in ids {
            self.backend.reload_buffer(id).map_err(|err| format!("reload failed: {err}"))?;
        }
        Ok(())
    }
    pub(super) fn open_scratch_buffer(&mut self) -> Result<(), String> {
        let buf_id = self.backend.open_buffer(None).map_err(|err| format!("open failed: {err}"))?;
        self.backend.switch_to_id(buf_id).map_err(|err| format!("open failed: {err}"))?;
        self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
        self.viewport = Viewport::default();
        Ok(())
    }
    pub(super) fn close_all_buffers(&mut self, force: bool) -> Result<(), String> {
        if !force && self.backend.all_bufs().iter().any(|buf| !buf.pristine) {
            return Err("unsaved changes (use :wa to save or :bca! to force)".to_owned());
        }

        let keep_id = if self.backend.buf_count() == 1 && self.backend.active().path.is_none() {
            self.backend.active().id
        } else {
            let buf_id =
                self.backend.open_buffer(None).map_err(|err| format!("open failed: {err}"))?;
            self.backend.switch_to_id(buf_id).map_err(|err| format!("open failed: {err}"))?;
            self.tabs.focused_windows_mut().set_focused_buffer(buf_id);
            self.viewport = Viewport::default();
            buf_id
        };

        let ids = self
            .backend
            .all_bufs()
            .iter()
            .map(|buf| buf.id)
            .filter(|id| *id != keep_id)
            .collect::<Vec<_>>();
        self.close_buffers(&ids, true, "")
    }
    pub(super) fn close_buffers(
        &mut self,
        ids: &[BufferId],
        force: bool,
        unsaved_message: &str,
    ) -> Result<(), String> {
        if !force
            && ids.iter().copied().any(|id| {
                self.backend
                    .all_bufs()
                    .iter()
                    .find(|buf| buf.id == id)
                    .is_some_and(|buf| !buf.pristine)
            })
        {
            return Err(unsaved_message.to_owned());
        }

        let active_id = self.backend.active().id;
        let closed_active = ids.contains(&active_id);
        for id in ids {
            if self.backend.all_bufs().iter().any(|buf| buf.id == *id) {
                self.backend.close_buffer(*id).map_err(|err| format!("close failed: {err}"))?;
            }
        }

        let fallback = self.backend.active().id;
        let valid_buffers = self
            .backend
            .all_bufs()
            .iter()
            .map(|buf| buf.id)
            .collect::<std::collections::HashSet<_>>();
        self.tabs.retarget_invalid_buffers(&valid_buffers, fallback);
        self.tabs.focused_windows_mut().set_focused_buffer(fallback);
        if closed_active {
            self.viewport = Viewport::default();
        }
        Ok(())
    }
}
