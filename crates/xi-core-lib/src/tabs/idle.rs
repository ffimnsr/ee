//! `impl CoreState` methods: idle.
use super::*;

impl CoreState {
    pub(crate) fn handle_idle(&mut self, token: usize) {
        match token {
            NEW_VIEW_IDLE_TOKEN => self.finalize_new_views(),
            VERIFY_LINE_ENDINGS_IDLE_TOKEN => self.verify_pending_line_endings(),
            WATCH_IDLE_TOKEN => self.handle_fs_events(),
            other if (other & RENDER_VIEW_IDLE_MASK) != 0 => {
                self.handle_render_timer(other ^ RENDER_VIEW_IDLE_MASK)
            }
            other if (other & REWRAP_VIEW_IDLE_MASK) != 0 => {
                self.handle_rewrap_callback(other ^ REWRAP_VIEW_IDLE_MASK)
            }
            other if (other & FIND_VIEW_IDLE_MASK) != 0 => {
                self.handle_find_callback(other ^ FIND_VIEW_IDLE_MASK)
            }
            other if (other & SAVE_VIEW_IDLE_MASK) != 0 => {
                self.handle_save_callback(other ^ SAVE_VIEW_IDLE_MASK)
            }
            other if (other & WHOLE_SCAN_IDLE_MASK) != 0 => {
                self.handle_whole_scan_callback(other ^ WHOLE_SCAN_IDLE_MASK)
            }
            other => panic!("unexpected idle token {}", other),
        };
    }

    pub(super) fn finalize_new_views(&mut self) {
        let to_start = mem::take(&mut self.pending_views);

        to_start.iter().for_each(|(id, config)| {
            let modified = self.detect_whitespace(*id, config);
            let config = modified.as_ref().unwrap_or(config);
            let mut edit_ctx = self.make_context(*id).unwrap();
            edit_ctx.finish_init(config);
        });
    }

    // Detects whitespace settings from the file and merges them with the config
    pub(super) fn detect_whitespace(&mut self, id: ViewId, config: &Table) -> Option<Table> {
        let buffer_id = self.views.get(&id).map(|v| v.borrow().get_buffer_id())?;
        let editor = self
            .editors
            .get(&buffer_id)
            .expect("existing buffer_id must have corresponding editor");

        if editor.borrow().get_buffer().is_empty() {
            return None;
        }

        let autodetect_whitespace =
            self.config_manager.get_buffer_config(buffer_id).items.autodetect_whitespace;
        if !autodetect_whitespace {
            return None;
        }

        let mut changes = Table::new();
        let open_analysis = self.file_manager.get_info(buffer_id).map(|info| info.open_analysis);

        let indentation = open_analysis.map(|analysis| analysis.indentation).unwrap_or_else(|| {
            SampledIndentation::from(Indentation::parse(editor.borrow().get_buffer()))
        });
        match indentation {
            SampledIndentation::Tabs => {
                changes.insert("translate_tabs_to_spaces".into(), false.into());
            }
            SampledIndentation::Spaces(n) => {
                changes.insert("translate_tabs_to_spaces".into(), true.into());
                changes.insert("tab_size".into(), n.into());
            }
            SampledIndentation::Mixed => info!("detected mixed indentation"),
            SampledIndentation::None => info!("file contains no indentation"),
        }

        match open_analysis {
            Some(analysis) if analysis.needs_line_ending_verification() => {
                self.schedule_line_ending_verification(buffer_id);
            }
            Some(analysis) => match analysis.line_ending {
                SampledLineEnding::CrLf => {
                    changes.insert("line_ending".into(), "\r\n".into());
                }
                SampledLineEnding::Lf => {
                    changes.insert("line_ending".into(), "\n".into());
                }
                SampledLineEnding::Mixed | SampledLineEnding::LegacyCr => {
                    info!("detected mixed line endings")
                }
                SampledLineEnding::None => info!("file contains no supported line endings"),
            },
            None => match LineEnding::parse(editor.borrow().get_buffer()) {
                Ok(Some(LineEnding::CrLf)) => {
                    changes.insert("line_ending".into(), "\r\n".into());
                }
                Ok(Some(LineEnding::Lf)) => {
                    changes.insert("line_ending".into(), "\n".into());
                }
                Err(_) => info!("detected mixed line endings"),
                Ok(None) => info!("file contains no supported line endings"),
            },
        }

        if changes.is_empty() {
            return None;
        }

        let config_delta =
            self.config_manager.table_for_update(ConfigDomain::SysOverride(buffer_id), changes);
        match self
            .config_manager
            .set_user_config(ConfigDomain::SysOverride(buffer_id), config_delta)
        {
            Ok(ref mut items) if !items.is_empty() => {
                assert!(
                    items.len() == 1,
                    "whitespace overrides can only update a single buffer's config\n{:?}",
                    items
                );
                let table = items.remove(0).1;
                let mut config = config.clone();
                config.extend(table);
                Some(config)
            }
            Ok(_) => {
                warn!("set_user_config failed to update config, no tables were returned");
                None
            }
            Err(err) => {
                warn!("detect_whitespace failed to update config: {:?}", err);
                None
            }
        }
    }

    pub(super) fn schedule_line_ending_verification(&mut self, buffer_id: BufferId) {
        if self.pending_line_ending_verifications.contains(&buffer_id) {
            return;
        }
        self.pending_line_ending_verifications.push(buffer_id);
        self.peer.schedule_idle(VERIFY_LINE_ENDINGS_IDLE_TOKEN);
    }

    pub(super) fn verify_pending_line_endings(&mut self) {
        let pending = mem::take(&mut self.pending_line_ending_verifications);

        for buffer_id in pending {
            let Some(editor) = self.editors.get(&buffer_id) else {
                continue;
            };
            let line_ending = {
                let editor = editor.borrow();
                if editor.is_vlf() || editor.get_buffer().is_empty() {
                    continue;
                }
                if !self.config_manager.get_buffer_config(buffer_id).items.autodetect_whitespace {
                    continue;
                }
                LineEnding::parse_bounded(editor.get_buffer(), usize::MAX)
            };

            let mut changes = Table::new();
            match line_ending {
                Ok(Some(LineEnding::CrLf)) => {
                    changes.insert("line_ending".into(), "\r\n".into());
                }
                Ok(Some(LineEnding::Lf)) => {
                    changes.insert("line_ending".into(), "\n".into());
                }
                Err(_) => info!("detected mixed line endings"),
                Ok(None) => info!("file contains no supported line endings"),
            }

            if changes.is_empty() {
                continue;
            }

            let config_delta =
                self.config_manager.table_for_update(ConfigDomain::SysOverride(buffer_id), changes);
            match self
                .config_manager
                .set_user_config(ConfigDomain::SysOverride(buffer_id), config_delta)
            {
                Ok(changes) if !changes.is_empty() => self.handle_config_changes(changes),
                Ok(_) => {}
                Err(err) => warn!("line ending verification failed to update config: {:?}", err),
            }
        }
    }

    pub(super) fn handle_render_timer(&mut self, token: usize) {
        let id: ViewId = token.into();
        if let Some(mut ctx) = self.make_context(id) {
            ctx._finish_delayed_render();
        }
    }

    /// Callback for doing word wrap on a view
    pub(super) fn handle_rewrap_callback(&mut self, token: usize) {
        let id: ViewId = token.into();
        if let Some(mut ctx) = self.make_context(id) {
            ctx.do_rewrap_batch();
        }
    }

    /// Callback for doing incremental find in a view
    pub(super) fn handle_find_callback(&mut self, token: usize) {
        let id: ViewId = token.into();
        if let Some(mut ctx) = self.make_context(id) {
            ctx.do_incremental_find();
        }
    }

    /// Callback for picking up a completed async whole-document scan result.
    pub(super) fn handle_whole_scan_callback(&mut self, token: usize) {
        let id: ViewId = token.into();
        if let Some(mut ctx) = self.make_context(id) {
            ctx.apply_whole_scan_result();
        }
    }

    #[cfg(feature = "notify")]
    pub(super) fn handle_fs_events(&mut self) {
        let _t = tracing::trace_span!("CoreState::handle_fs_events", categories = "core").entered();
        let mut events = self.file_manager.watcher().take_events();

        for (token, event) in events.drain(..) {
            match token {
                OPEN_FILE_EVENT_TOKEN => self.handle_open_file_fs_event(event),
                PLUGIN_EVENT_TOKEN => self.handle_plugin_fs_event(event),
                _ => warn!("unexpected fs event token {:?}", token),
            }
        }
    }

    #[cfg(not(feature = "notify"))]
    pub(super) fn handle_fs_events(&mut self) {}

    /// Handles a file system event related to a currently open file
    #[cfg(feature = "notify")]
    pub(super) fn handle_open_file_fs_event(&mut self, event: Event) {
        use notify::event::*;
        let path = match event.kind {
            EventKind::Create(CreateKind::Any)
            | EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any))
            | EventKind::Modify(ModifyKind::Any) => &event.paths[0],
            other => {
                debug!("Ignoring event in open file {:?}", other);
                return;
            }
        };

        let buffer_id = match self.file_manager.get_editor(path) {
            Some(id) => id,
            None => return,
        };

        let has_changes = self.file_manager.check_file(path, buffer_id);
        let is_pristine = self.editors.get(&buffer_id).map(|ed| ed.borrow().is_pristine()).unwrap();
        // External-change detection currently uses mtime, file length, and
        // on Unix a device/inode/ctime change cookie. A content hash would be
        // stronger still, but would cost an extra full-file read.

        if has_changes && is_pristine {
            if self.editors.get(&buffer_id).is_some_and(|editor| editor.borrow().is_vlf()) {
                return;
            }
            if let Ok(open_result) = self.file_manager.open(path, buffer_id) {
                match open_result {
                    OpenResult::Rope { text, mode } => {
                        // this is ugly; we don't map buffer_id -> view_id anywhere
                        // but we know we must have a view.
                        let view_id = self
                            .views
                            .values()
                            .find(|v| v.borrow().get_buffer_id() == buffer_id)
                            .map(|v| v.borrow().get_view_id())
                            .unwrap();
                        self.make_context(view_id).unwrap().reload(text);
                        if let Some(editor) = self.editors.get(&buffer_id) {
                            editor.borrow_mut().set_document_mode(mode);
                        }
                    }
                    // VLF files are read-only and paged; reload is a no-op.
                    OpenResult::Vlf(_) => {}
                }
            }
        }
    }

    /// Handles changes in plugin files.
    #[cfg(feature = "notify")]
    pub(super) fn handle_plugin_fs_event(&mut self, event: Event) {
        use notify::event::*;
        match event.kind {
            EventKind::Create(CreateKind::Any) | EventKind::Modify(ModifyKind::Any) => {
                self.plugins.load_from_paths(&[event.paths[0].clone()]).into_iter().for_each(
                    |err| {
                        let message = format!("error loading plugin {err:?}");
                        warn!("{message}");
                        self.peer.alert(message);
                    },
                );
                if let Some(plugin) = self.plugins.get_from_path(&event.paths[0]) {
                    if plugin.activates_on_startup()
                        || self
                            .views
                            .values()
                            .map(|view| {
                                self.config_manager
                                    .get_buffer_language(view.borrow().get_buffer_id())
                            })
                            .any(|language| plugin.receives_updates_for(&language))
                    {
                        self.do_start_plugin(ViewId(0), &plugin.name);
                    }
                }
            }
            // the way FSEvents on macOS work, we want to verify that this path
            // has actually be removed before we do anything.
            EventKind::Remove(RemoveKind::Any) if !event.paths[0].exists() => {
                if let Some(plugin) = self.plugins.get_from_path(&event.paths[0]) {
                    self.do_stop_plugin(ViewId(0), &plugin.name);
                    self.plugins.remove_named(&plugin.name);
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                let old = &event.paths[0];
                let new = &event.paths[1];
                if let Some(old_plugin) = self.plugins.get_from_path(old) {
                    self.do_stop_plugin(ViewId(0), &old_plugin.name);
                    self.plugins.remove_named(&old_plugin.name);
                }

                self.plugins.load_from_paths(std::slice::from_ref(new)).into_iter().for_each(
                    |err| {
                        let message = format!("error loading plugin {err:?}");
                        warn!("{message}");
                        self.peer.alert(message);
                    },
                );
                if let Some(new_plugin) = self.plugins.get_from_path(new) {
                    if new_plugin.activates_on_startup()
                        || self
                            .views
                            .values()
                            .map(|view| {
                                self.config_manager
                                    .get_buffer_language(view.borrow().get_buffer_id())
                            })
                            .any(|language| new_plugin.receives_updates_for(&language))
                    {
                        self.do_start_plugin(ViewId(0), &new_plugin.name);
                    }
                }
            }
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any))
            | EventKind::Remove(RemoveKind::Any) => {
                if let Some(plugin) = self.plugins.get_from_path(&event.paths[0]) {
                    self.do_stop_plugin(ViewId(0), &plugin.name);
                    if plugin.activates_on_startup()
                        || self
                            .views
                            .values()
                            .map(|view| {
                                self.config_manager
                                    .get_buffer_language(view.borrow().get_buffer_id())
                            })
                            .any(|language| plugin.receives_updates_for(&language))
                    {
                        self.do_start_plugin(ViewId(0), &plugin.name);
                    }
                }
            }
            _ => (),
        }

        self.views.keys().for_each(|view_id| {
            let available_plugins = self
                .plugins
                .iter()
                .map(|plugin| ClientPluginInfo { name: plugin.name.clone(), running: true })
                .collect::<Vec<_>>();
            self.peer.available_plugins(*view_id, &available_plugins);
        });
    }
}
