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
        let buf_id = self.backend.active().id;
        if self.pending_format_saves.contains_key(&buf_id) {
            return Ok(SaveOutcome::AwaitingFormat);
        }
        let (actions, format) = self.save_pipeline_plan();
        if !actions.is_empty() {
            self.backend
                .request_code_actions_on_save(actions)
                .map_err(|err| format!("code actions on save failed: {err}"))?;
            self.pending_format_saves.insert(
                buf_id,
                PendingSavePipeline {
                    needs_format: format,
                    phase: SavePipelinePhase::PreSave,
                    phase_ticks: 0,
                    total_ticks: 0,
                },
            );
            self.backend.status_message = Some(String::from("code actions on save running..."));
            return Ok(SaveOutcome::AwaitingFormat);
        }
        if format {
            self.backend
                .format_document()
                .map_err(|err| format!("format on save failed: {err}"))?;
            self.pending_format_saves.insert(
                buf_id,
                PendingSavePipeline {
                    needs_format: true,
                    phase: SavePipelinePhase::Format,
                    phase_ticks: 0,
                    total_ticks: 0,
                },
            );
            self.backend.status_message = Some(String::from("format on save running..."));
            return Ok(SaveOutcome::AwaitingFormat);
        }
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

    /// Resolve `code_actions_on_save` kinds + `format_on_save` for the active
    /// buffer: per-language overrides win, then the global `format_on_save`.
    fn save_pipeline_plan(&mut self) -> (Vec<String>, bool) {
        let language_id = self.backend.active().path.clone().and_then(|path| {
            xi_core_lib::runtime_loader::with_default_runtime_loader_mut(|loader| {
                loader.language_for_path(&path).map(|language| language.canonical_id().to_string())
            })
        });
        let actions = language_id
            .as_deref()
            .and_then(|language| self.config.lsp.language_code_actions_on_save.get(language))
            .cloned()
            .unwrap_or_default();
        let format = language_id
            .as_deref()
            .and_then(|language| self.config.lsp.language_format_on_save.get(language))
            .copied()
            .unwrap_or(self.config.format_on_save);
        (actions, format)
    }

    /// Advance pending pre-save pipelines. Called every pump tick after
    /// backend events have been drained so plugin edits have been applied.
    /// Runs code actions, then formatting, then the actual save; a bounded
    /// wait guards against a stuck plugin (save happens anyway on timeout).
    pub(crate) fn pump_format_on_save(&mut self) {
        const SETTLE_TICKS: u32 = 3;
        const MAX_WAIT_TICKS: u32 = 180;

        let mut ready = Vec::new();
        let mut expired: Vec<(u32, u32)> = Vec::new();
        for (buf_id, pending) in self.pending_format_saves.iter_mut() {
            pending.total_ticks += 1;
            pending.phase_ticks += 1;
            if pending.total_ticks >= MAX_WAIT_TICKS {
                expired.push((*buf_id, pending.total_ticks));
                continue;
            }
            if pending.phase_ticks < SETTLE_TICKS {
                continue;
            }
            match pending.phase {
                SavePipelinePhase::PreSave => {
                    if pending.needs_format {
                        if let Err(err) = self.backend.format_document() {
                            self.backend.status_message =
                                Some(format!("format on save failed: {err}"));
                        }
                        pending.phase = SavePipelinePhase::Format;
                    } else {
                        ready.push(*buf_id);
                    }
                    pending.phase_ticks = 0;
                }
                SavePipelinePhase::Format => ready.push(*buf_id),
            }
        }

        for buf_id in expired.into_iter().map(|(id, _)| id) {
            self.pending_format_saves.remove(&buf_id);
            self.backend.status_message =
                Some(String::from("format on save timed out; saved unformatted"));
            self.complete_pending_save(buf_id);
        }
        for buf_id in ready {
            self.pending_format_saves.remove(&buf_id);
            self.complete_pending_save(buf_id);
        }
    }

    /// Writes the deferred buffer and honours a pending `:wq`/`:x` quit once
    /// the pipeline actually saved (failed saves clear the quit request).
    fn complete_pending_save(&mut self, buf_id: u32) {
        let saved = self.finish_pending_save(buf_id);
        if self.quit_after_format_save {
            self.quit_after_format_save = false;
            if saved {
                self.should_quit = true;
            }
        }
    }

    /// Returns `true` when the buffer was written to disk.
    fn finish_pending_save(&mut self, buf_id: u32) -> bool {
        match self.backend.save_buffer(buf_id) {
            Ok(()) => {
                if buf_id == self.backend.active().id {
                    self.backend.status_message = Some(String::from("saved (format on save)"));
                }
                true
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                if buf_id == self.backend.active().id {
                    let _ = self.start_privileged_save_confirm();
                } else {
                    self.backend.status_message =
                        Some(format!("save failed after format-on-save: {err}"));
                }
                false
            }
            Err(err) => {
                self.backend.status_message =
                    Some(format!("save failed after format-on-save: {err}"));
                false
            }
        }
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
