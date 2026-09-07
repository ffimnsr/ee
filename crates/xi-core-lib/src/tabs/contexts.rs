//! `impl CoreState` methods: contexts.
use super::*;

impl CoreState {
    /// Creates an `EventContext` for the provided `ViewId`. This context
    /// holds references to the `Editor` and `View` backing this `ViewId`,
    /// as well as to sibling views, plugins, and other state necessary
    /// for handling most events.
    pub(crate) fn make_context(&self, view_id: ViewId) -> Option<EventContext<'_>> {
        self.views.get(&view_id).map(|view| {
            let buffer_id = view.borrow().get_buffer_id();

            let editor = &self.editors[&buffer_id];
            let info = self.file_manager.get_info(buffer_id);
            let language = self.config_manager.get_buffer_language(buffer_id);
            let plugins = self
                .running_plugins
                .iter()
                .filter(|plugin| plugin.receives_updates_for(&language))
                .collect::<Vec<_>>();
            let config = self.config_manager.get_buffer_config(buffer_id);

            EventContext {
                view_id,
                buffer_id,
                view,
                editor,
                config: &config.items,
                language,
                info,
                siblings: Vec::new(),
                plugins,
                client: &self.peer,
                width_cache: &self.width_cache,
                kill_ring: &self.kill_ring,
                weak_core: self.self_ref.as_ref().unwrap(),
            }
        })
    }

    /// Produces an iterator over all event contexts, with each view appearing
    /// exactly once.
    pub(super) fn iter_groups<'a>(&'a self) -> Iter<'a, Box<dyn Iterator<Item = &'a ViewId> + 'a>> {
        Iter { views: Box::new(self.views.keys()), seen: HashSet::new(), inner: self }
    }

    pub(crate) fn client_notification(&mut self, cmd: CoreNotification) {
        use self::CoreNotification::*;
        use self::CorePluginNotification as PN;
        match cmd {
            Edit(crate::rpc::EditCommand { view_id, cmd }) => self.do_edit(view_id, cmd),
            Save { view_id, file_path } => self.do_save(view_id, file_path),
            CloseView { view_id } => self.do_close_view(view_id),
            SetConfig { domain, changes } => self.do_set_config(domain, changes),
            Plugin(cmd) => match cmd {
                PN::Start { view_id, plugin_name } => self.do_start_plugin(view_id, &plugin_name),
                PN::Stop { view_id, plugin_name } => self.do_stop_plugin(view_id, &plugin_name),
                PN::Restart { view_id, plugin_name } => {
                    self.do_restart_plugin(view_id, &plugin_name)
                }
                PN::PluginRpc { view_id, receiver, rpc } => {
                    self.do_plugin_rpc(view_id, &receiver, &rpc.method, &rpc.params)
                }
            },
            // handled at the top level
            ClientStarted { .. } => (),
        }
    }

    pub(crate) fn client_request(&mut self, cmd: CoreRequest) -> Result<Value, RemoteError> {
        use self::CoreRequest::*;
        match cmd {
            NewView { file_path } => self.do_new_view(file_path.map(PathBuf::from)),
            SubstitutePreview {
                view_id,
                start_line,
                end_line,
                pattern,
                replacement,
                global,
                case_sensitive,
            } => self.do_substitute_preview(
                view_id,
                start_line,
                end_line,
                &pattern,
                &replacement,
                global,
                case_sensitive,
            ),
            FilterSelectionsPreview { view_id, pattern, remove } => {
                self.do_filter_selections_preview(view_id, &pattern, remove)
            }
            SelectedTextPreview { view_id, linewise } => {
                self.do_selected_text_preview(view_id, linewise)
            }
            SelectionsPreview { view_id } => self.do_selections_preview(view_id),
            BufferPristine { view_id } => self.do_buffer_pristine(view_id),
            SaveStatus { view_id } => self.do_save_status(view_id),
            PrepareElevatedSaveDraft { view_id } => self.do_prepare_elevated_save_draft(view_id),
            FinalizeElevatedSave { view_id, file_path, saved_rev_id } => {
                self.do_finalize_elevated_save(view_id, PathBuf::from(file_path), saved_rev_id)
            }
            BlockTextPreview { view_id, start_line, end_line, left_col, right_col } => {
                self.do_block_text_preview(view_id, start_line, end_line, left_col, right_col)
            }
            FoldRangesPreview { view_id, start_line, end_line } => {
                self.do_fold_ranges_preview(view_id, start_line, end_line)
            }
            SelectCharsPreview { view_id, count } => self.do_select_chars_preview(view_id, count),
            SymbolDependencyMap { view_id, line, character, language_id, path } => {
                self.do_symbol_dependency_map(view_id, line, character, &language_id, path)
            }
        }
    }

    pub(super) fn do_edit(&mut self, view_id: ViewId, cmd: EditNotification) {
        if let EditNotification::NormalizeLineEndings { line_ending } = cmd {
            self.do_normalize_line_endings(view_id, line_ending);
            return;
        }

        if let Some(mut edit_ctx) = self.make_context(view_id) {
            edit_ctx.do_edit(cmd);
        }
    }

    pub(super) fn do_normalize_line_endings(&mut self, view_id: ViewId, line_ending: String) {
        let Some(view) = self.views.get(&view_id) else {
            return;
        };
        let buffer_id = view.borrow().get_buffer_id();
        let current = self.config_manager.get_buffer_config(buffer_id).items.line_ending.clone();
        if current == line_ending {
            return;
        }

        let mut changes = Table::new();
        changes.insert("line_ending".into(), line_ending.clone().into());
        self.set_buffer_user_config(buffer_id, changes);
        self.peer.alert(match line_ending.as_str() {
            "\n" => "Line endings normalized to LF.",
            "\r\n" => "Line endings normalized to CRLF.",
            "\r" => "Line endings normalized to CR.",
            _ => "Line endings updated.",
        });
    }

    pub(super) fn do_symbol_dependency_map(
        &mut self,
        view_id: ViewId,
        line: u32,
        character: u32,
        language_id: &str,
        path: String,
    ) -> Result<Value, RemoteError> {
        let buffer_id = self
            .views
            .get(&view_id)
            .map(|view| view.borrow().get_buffer_id())
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        let editor = self
            .editors
            .get(&buffer_id)
            .ok_or_else(|| RemoteError::custom(404, "missing editor", None))?;
        let editor = editor.borrow();
        let revision = editor.get_head_rev_token();
        let snapshot = editor.text_store_snapshot();
        let result = crate::symbol_index::symbol_dependency_map(
            &snapshot,
            revision,
            path,
            line,
            character,
            language_id,
        )
        .map_err(|error| RemoteError::custom(409, error.message(), None))?;
        serde_json::to_value(result)
            .map_err(|error| RemoteError::custom(500, error.to_string(), None))
    }

    pub(super) fn do_select_chars_preview(
        &mut self,
        view_id: ViewId,
        count: usize,
    ) -> Result<Value, RemoteError> {
        let mut ctx = self
            .make_context(view_id)
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        Ok(json!(ctx.preview_select_chars(count)))
    }

    pub(super) fn do_selected_text_preview(
        &mut self,
        view_id: ViewId,
        linewise: bool,
    ) -> Result<Value, RemoteError> {
        let mut ctx = self
            .make_context(view_id)
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        Ok(json!(ctx.preview_selected_text(linewise)))
    }

    pub(super) fn do_selections_preview(&mut self, view_id: ViewId) -> Result<Value, RemoteError> {
        let mut ctx = self
            .make_context(view_id)
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        Ok(json!(ctx.preview_selections()))
    }

    pub(super) fn do_buffer_pristine(&mut self, view_id: ViewId) -> Result<Value, RemoteError> {
        let buffer_id = self
            .views
            .get(&view_id)
            .map(|view| view.borrow().get_buffer_id())
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        let pristine = self
            .editors
            .get(&buffer_id)
            .map(|editor| editor.borrow().is_pristine())
            .ok_or_else(|| RemoteError::custom(404, "missing editor", None))?;
        Ok(json!(pristine))
    }

    pub(super) fn do_save_status(&mut self, view_id: ViewId) -> Result<Value, RemoteError> {
        let buffer_id = self
            .views
            .get(&view_id)
            .map(|view| view.borrow().get_buffer_id())
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;

        self.handle_save_callback(usize::from(view_id));

        let editor = self
            .editors
            .get(&buffer_id)
            .ok_or_else(|| RemoteError::custom(404, "missing editor", None))?
            .borrow();
        let generation = editor.save_task.generation();
        let complete = generation > 0 && !editor.save_task.is_in_progress();
        Ok(json!({
            "generation": generation,
            "complete": complete,
        }))
    }

    pub(super) fn do_prepare_elevated_save_draft(
        &mut self,
        view_id: ViewId,
    ) -> Result<Value, RemoteError> {
        let buffer_id = self
            .views
            .get(&view_id)
            .map(|view| view.borrow().get_buffer_id())
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        let path = self
            .file_manager
            .get_info(buffer_id)
            .map(|info| info.path.clone())
            .ok_or_else(|| RemoteError::custom(404, "missing file metadata", None))?;
        let draft_path = elevated_save_temp_path(&path).map_err(|err| {
            RemoteError::custom(500, format!("elevated save temp file failed: {err}"), None)
        })?;

        let saved_rev_id = {
            let editor = self
                .editors
                .get(&buffer_id)
                .ok_or_else(|| RemoteError::custom(404, "missing editor", None))?;
            let editor = editor.borrow();
            let saved_rev_id = editor.get_head_rev_id();
            if let Some(store) = editor.vlf_store.as_ref() {
                let plan = store.prepare_save_plan().map_err(|err| {
                    RemoteError::custom(500, format!("prepare elevated save failed: {err}"), None)
                })?;
                crate::vlf::save::stream_save_snapshot(
                    &plan,
                    &draft_path,
                    &crate::vlf::overlay::VlfSavePolicy::SaveAs(draft_path.clone()),
                    &mut |_| true,
                )
                .map_err(|err| {
                    RemoteError::custom(
                        500,
                        format!("write elevated save draft failed: {err}"),
                        None,
                    )
                })?;
            } else {
                let mut save_ctx = self
                    .make_context(view_id)
                    .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
                let (text, _) = save_ctx.rope_snapshot_for_save();
                let request =
                    self.file_manager.prepare_rope_save(&draft_path, buffer_id).map_err(|err| {
                        RemoteError::custom(
                            500,
                            format!("prepare elevated save failed: {err}"),
                            None,
                        )
                    })?;
                crate::file::execute_prepared_rope_save(&request, &text, &mut || true).map_err(
                    |err| {
                        RemoteError::custom(
                            500,
                            format!("write elevated save draft failed: {err}"),
                            None,
                        )
                    },
                )?;
            }
            saved_rev_id
        };

        Ok(json!({
            "draft_path": draft_path.to_string_lossy(),
            "file_path": path.to_string_lossy(),
            "saved_rev_id": saved_rev_id,
        }))
    }

    pub(super) fn do_finalize_elevated_save(
        &mut self,
        view_id: ViewId,
        path: PathBuf,
        saved_rev_id: xi_rope::engine::RevId,
    ) -> Result<Value, RemoteError> {
        let buffer_id = self
            .views
            .get(&view_id)
            .map(|view| view.borrow().get_buffer_id())
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;

        let is_vlf = self
            .editors
            .get(&buffer_id)
            .ok_or_else(|| RemoteError::custom(404, "missing editor", None))?
            .borrow()
            .is_vlf();

        if is_vlf {
            let request = self
                .file_manager
                .prepare_vlf_save(
                    &path,
                    buffer_id,
                    crate::vlf::overlay::VlfSavePolicy::TempFileRewrite { temp_dir: None },
                )
                .map_err(|err| {
                    RemoteError::custom(500, format!("finalize elevated save failed: {err}"), None)
                })?;
            self.file_manager.finish_vlf_save(&request).map_err(|err| {
                RemoteError::custom(500, format!("finalize elevated save failed: {err}"), None)
            })?;
            let editor = self
                .editors
                .get(&buffer_id)
                .ok_or_else(|| RemoteError::custom(404, "missing editor", None))?;
            editor.borrow_mut().refresh_after_vlf_save(&path).map_err(|err| {
                RemoteError::custom(500, format!("finalize elevated save failed: {err}"), None)
            })?;
        } else {
            let request = self.file_manager.prepare_rope_save(&path, buffer_id).map_err(|err| {
                RemoteError::custom(500, format!("finalize elevated save failed: {err}"), None)
            })?;
            self.file_manager.finish_rope_save(&request).map_err(|err| {
                RemoteError::custom(500, format!("finalize elevated save failed: {err}"), None)
            })?;
        }

        let changes = self.config_manager.update_buffer_path(buffer_id, &path);
        let language = self.config_manager.get_buffer_language(buffer_id);
        if let Some(mut ctx) = self.make_context(view_id) {
            ctx.after_save_with_rev(&path, saved_rev_id);
            ctx.language_changed(&language);
            if let Some(changes) = changes {
                ctx.config_changed(&changes);
            }
        }

        Ok(json!({ "ok": true }))
    }

    pub(super) fn do_block_text_preview(
        &mut self,
        view_id: ViewId,
        start_line: usize,
        end_line: usize,
        left_col: usize,
        right_col: usize,
    ) -> Result<Value, RemoteError> {
        let mut ctx = self
            .make_context(view_id)
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        Ok(json!(ctx.preview_block_text(start_line, end_line, left_col, right_col)))
    }

    pub(super) fn do_fold_ranges_preview(
        &mut self,
        view_id: ViewId,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<Value, RemoteError> {
        let ctx = self
            .make_context(view_id)
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        Ok(json!(ctx.preview_fold_ranges(start_line, end_line)))
    }

    pub(super) fn do_set_config(&mut self, domain: ConfigDomainExternal, changes: Table) {
        let Some(domain) = self.resolve_config_domain(domain) else {
            return;
        };
        self.set_config(domain, changes);
    }

    pub(super) fn do_new_view(&mut self, path: Option<PathBuf>) -> Result<Value, RemoteError> {
        let view_id = self.next_view_id();
        let buffer_id = self.next_buffer_id();

        let open_result = match path.as_ref() {
            Some(p) => self.file_manager.open(p, buffer_id)?,
            None => OpenResult::Rope { text: Rope::from(""), mode: DocumentMode::Normal },
        };
        let editor = match open_result {
            OpenResult::Rope { text, mode } => RefCell::new(Editor::with_text_mode(text, mode)),
            OpenResult::Vlf(store) => {
                let mut editor = Editor::with_vlf_store(*store);
                editor.enable_vlf_editing();
                RefCell::new(editor)
            }
        };
        let view = RefCell::new(View::new(view_id, buffer_id));

        self.editors.insert(buffer_id, editor);
        self.views.insert(view_id, view);

        let config = self.config_manager.add_buffer(buffer_id, path.as_deref());
        let language = self.config_manager.get_buffer_language(buffer_id);
        self.ensure_plugins_for_language(&language);

        // NOTE: because this is a synchronous call, we have to initialize the
        // view and return the view_id before we can send any events to this
        // view. We call view_init(), mark the view as pending and schedule the
        // idle handler so that we can finish setting up this view on the next
        // runloop pass, in finalize_new_views.

        let mut edit_ctx = self.make_context(view_id).unwrap();
        edit_ctx.view_init();

        self.pending_views.push((view_id, config));
        self.peer.schedule_idle(NEW_VIEW_IDLE_TOKEN);

        Ok(json!(view_id))
    }

    pub(super) fn do_substitute_preview(
        &mut self,
        view_id: ViewId,
        start_line: usize,
        end_line: usize,
        pattern: &str,
        replacement: &str,
        global: bool,
        case_sensitive: bool,
    ) -> Result<Value, RemoteError> {
        let ctx = self.make_context(view_id).ok_or_not_found("view not found")?;
        Ok(json!(ctx.preview_substitute(
            start_line,
            end_line,
            pattern,
            replacement,
            global,
            case_sensitive,
        )?))
    }

    pub(super) fn do_filter_selections_preview(
        &mut self,
        view_id: ViewId,
        pattern: &str,
        remove: bool,
    ) -> Result<Value, RemoteError> {
        let mut ctx = self.make_context(view_id).ok_or_not_found("view not found")?;
        let selections: Vec<SelectionRange> = ctx.preview_filter_selections(pattern, remove)?;
        Ok(json!(selections))
    }

    pub(super) fn do_save<P>(&mut self, view_id: ViewId, path: P)
    where
        P: AsRef<Path>,
    {
        let _t = tracing::trace_span!("CoreState::do_save", categories = "core").entered();
        let path = path.as_ref();
        let buffer_id = self.views.get(&view_id).map(|v| v.borrow().get_buffer_id());
        let buffer_id = match buffer_id {
            Some(id) => id,
            None => return,
        };

        if let Some(editor) = self.editors.get(&buffer_id) {
            let mut editor = editor.borrow_mut();
            if let Some(store) = editor.vlf_store.as_ref() {
                match store.edit_permission() {
                    EditPermission::Forbidden { reason } => {
                        self.peer.alert(format!("save disabled in VLF: {reason}"));
                        return;
                    }
                    EditPermission::Allowed => {}
                }

                if !editor.vlf_save_enabled() {
                    self.peer.alert("save disabled in VLF: streaming save path is not ready");
                    return;
                }

                let Some(current_path) =
                    self.file_manager.get_info(buffer_id).map(|info| info.path.clone())
                else {
                    self.peer.alert("VLF save missing file metadata");
                    return;
                };

                let requested_path = path.to_owned();
                let explicit_save_as = current_path != requested_path;
                let suggested_policy = store.suggested_save_policy().unwrap_or(
                    crate::vlf::overlay::VlfSavePolicy::TempFileRewrite { temp_dir: None },
                );

                if !explicit_save_as
                    && matches!(suggested_policy, crate::vlf::overlay::VlfSavePolicy::SaveAs(_))
                {
                    self.peer.alert(
                        "save-as required for VLF: explicit destination must be chosen before saving",
                    );
                    return;
                }

                let policy = if explicit_save_as {
                    crate::vlf::overlay::VlfSavePolicy::SaveAs(requested_path.clone())
                } else {
                    suggested_policy
                };

                let request = match self.file_manager.prepare_vlf_save(path, buffer_id, policy) {
                    Ok(request) => request,
                    Err(e) => {
                        let error_message = save_error_alert(&e, path);
                        error!("File error: {:?}", error_message);
                        self.peer.alert(error_message);
                        return;
                    }
                };

                let plan = match store.prepare_save_plan() {
                    Ok(plan) => plan,
                    Err(e) => {
                        let error_message = format!("save failed: {}", e);
                        error!("File error: {:?}", error_message);
                        self.peer.alert(error_message);
                        return;
                    }
                };

                let saved_rev_id = editor.get_head_rev_id();
                let generation = editor.save_task.start_vlf_save(request, plan, saved_rev_id);
                self.peer.save_progress(view_id, 0, 0, false, generation);
                let view_id_usize: usize = view_id.into();
                self.peer.schedule_idle(SAVE_VIEW_IDLE_MASK | view_id_usize);
                return;
            }
        }

        let request = match self.file_manager.prepare_rope_save(path, buffer_id) {
            Ok(request) => request,
            Err(e) => {
                let error_message = save_error_alert(&e, path);
                error!("File error: {:?}", error_message);
                self.peer.alert(error_message);
                return;
            }
        };

        let mut save_ctx = self.make_context(view_id).unwrap();
        let (fin_text, saved_rev_id) = save_ctx.rope_snapshot_for_save();
        drop(save_ctx);

        if let Some(editor) = self.editors.get(&buffer_id) {
            let generation =
                editor.borrow_mut().save_task.start_rope_save(request, fin_text, saved_rev_id);
            self.peer.save_progress(view_id, 0, 0, false, generation);
            let view_id_usize: usize = view_id.into();
            self.peer.schedule_idle(SAVE_VIEW_IDLE_MASK | view_id_usize);
        } else {
            let error_message = format!("missing editor for buffer {:?}", buffer_id);
            error!("File error: {:?}", error_message);
            self.peer.alert(error_message);
        }
    }

    pub(super) fn finish_async_save(
        &mut self,
        view_id: ViewId,
        result: crate::whole_scan::SaveTaskResult,
    ) {
        let (path, buffer_id) = match &result.request {
            crate::whole_scan::CompletedSaveRequest::Rope(request) => {
                (request.path.clone(), request.buffer_id)
            }
            crate::whole_scan::CompletedSaveRequest::Vlf(request) => {
                (request.path.clone(), request.buffer_id)
            }
        };

        match result.result {
            Ok(()) => {
                let finish_result = match &result.request {
                    crate::whole_scan::CompletedSaveRequest::Rope(request) => {
                        self.file_manager.finish_rope_save(request)
                    }
                    crate::whole_scan::CompletedSaveRequest::Vlf(request) => {
                        self.file_manager.finish_vlf_save(request)
                    }
                };

                if let Err(e) = finish_result {
                    let (error_message, permission_denied) = save_error_details(&e, &path);
                    self.peer.save_result(
                        view_id,
                        &path,
                        result.generation,
                        false,
                        permission_denied,
                        Some(error_message.as_str()),
                    );
                    error!("File error: {:?}", error_message);
                    self.peer.alert(error_message);
                    return;
                }

                if matches!(result.request, crate::whole_scan::CompletedSaveRequest::Vlf(_)) {
                    let Some(editor_cell) = self.editors.get(&buffer_id) else {
                        let error_message = format!(
                            "save failed: missing editor for buffer {:?}. File path: {}",
                            buffer_id,
                            path.display()
                        );
                        error!("File error: {:?}", error_message);
                        self.peer.alert(error_message);
                        return;
                    };

                    if let Err(err) = editor_cell.borrow_mut().refresh_after_vlf_save(&path) {
                        let error_message =
                            format!("save failed: failed to refresh VLF save state: {err}");
                        error!("File error: {:?}", error_message);
                        self.peer.alert(error_message);
                        return;
                    }
                }

                self.peer.save_progress(view_id, 0, 0, true, result.generation);
                self.peer.save_result(view_id, &path, result.generation, true, false, None::<&str>);

                let changes = self.config_manager.update_buffer_path(buffer_id, &path);
                let language = self.config_manager.get_buffer_language(buffer_id);
                let notify_view_id = self
                    .views
                    .iter()
                    .find_map(|(candidate_id, view)| {
                        (view.borrow().get_buffer_id() == buffer_id).then_some(*candidate_id)
                    })
                    .unwrap_or(view_id);

                if let Some(mut ctx) = self.make_context(notify_view_id) {
                    ctx.after_save_with_rev(&path, result.saved_rev_id);
                    ctx.language_changed(&language);
                    if let Some(changes) = changes {
                        ctx.config_changed(&changes);
                    }
                }

                self.peer.alert(save_complete_alert(&path));
            }
            Err(e) => {
                self.peer.save_progress(view_id, 0, 0, true, result.generation);
                let (error_message, permission_denied) = save_error_details(&e, &path);
                self.peer.save_result(
                    view_id,
                    &path,
                    result.generation,
                    false,
                    permission_denied,
                    Some(error_message.as_str()),
                );
                if matches!(&e, crate::file::FileError::Io(io_error, _) if io_error.kind() == ErrorKind::Interrupted)
                {
                    info!("File save cancelled: {:?}", error_message);
                } else {
                    error!("File error: {:?}", error_message);
                }
                self.peer.alert(error_message);
            }
        }
    }

    pub(super) fn handle_save_callback(&mut self, token: usize) {
        let id: ViewId = token.into();
        let Some(buffer_id) = self.views.get(&id).map(|view| view.borrow().get_buffer_id()) else {
            return;
        };

        let maybe_result =
            self.editors.get(&buffer_id).and_then(|editor| editor.borrow_mut().save_task.poll());

        if let Some(progress) = self
            .editors
            .get(&buffer_id)
            .and_then(|editor| editor.borrow_mut().save_task.poll_progress())
        {
            self.peer.save_progress(
                id,
                progress.bytes_written,
                progress.total_bytes,
                false,
                progress.generation,
            );
        }

        if let Some(result) = maybe_result {
            self.finish_async_save(id, result);
            return;
        }

        if self
            .editors
            .get(&buffer_id)
            .is_some_and(|editor| editor.borrow().save_task.is_in_progress())
        {
            self.peer.schedule_idle(SAVE_VIEW_IDLE_MASK | token);
        }
    }

    pub(super) fn do_close_view(&mut self, view_id: ViewId) {
        let close_buffer = self.make_context(view_id).map(|ctx| ctx.close_view()).unwrap_or(true);

        let buffer_id = self.views.remove(&view_id).map(|v| v.borrow().get_buffer_id());

        if let Some(buffer_id) = buffer_id {
            if close_buffer {
                self.editors.remove(&buffer_id);
                self.file_manager.close(buffer_id);
                self.config_manager.remove_buffer(buffer_id);
            }
        }
    }

    pub(super) fn resolve_config_domain(
        &self,
        domain: ConfigDomainExternal,
    ) -> Option<ConfigDomain> {
        match domain {
            ConfigDomainExternal::General => Some(ConfigDomain::General),
            ConfigDomainExternal::Language(language) => Some(ConfigDomain::Language(language)),
            ConfigDomainExternal::Plugin(plugin) => Some(ConfigDomain::PluginConfig(plugin)),
            ConfigDomainExternal::UserOverride(view_id) => self
                .views
                .get(&view_id)
                .map(|view| ConfigDomain::UserOverride(view.borrow().get_buffer_id()))
                .or_else(|| {
                    warn!("ignoring config update for unknown view {:?}", view_id);
                    None
                }),
        }
    }

    pub(super) fn do_start_plugin(&mut self, _view_id: ViewId, plugin: &str) {
        if self.running_plugins.iter().any(|p| p.name == plugin) {
            info!("plugin {} already running", plugin);
            return;
        }

        if let Some(manifest) = self.plugins.get_named(plugin) {
            self.start_plugin(manifest);
        } else {
            warn!("no plugin found with name '{}'", plugin);
        }
    }

    pub(super) fn do_stop_plugin(&mut self, _view_id: ViewId, plugin: &str) {
        if let Some(plugin) = self.running_plugins.iter().find(|running| running.name == plugin) {
            self.begin_plugin_shutdown(plugin.id, StopReason::Manual);
        }
    }

    pub(super) fn do_restart_plugin(&mut self, view_id: ViewId, plugin: &str) {
        if let Some(plugin) = self.running_plugins.iter().find(|running| running.name == plugin) {
            self.begin_plugin_shutdown(plugin.id, StopReason::Restart);
            return;
        }

        self.do_start_plugin(view_id, plugin);
    }

    pub(super) fn do_plugin_rpc(
        &mut self,
        view_id: ViewId,
        receiver: &str,
        method: &str,
        params: &Value,
    ) {
        let mut dispatched = false;
        self.running_plugins.iter().filter(|plugin| plugin.name == receiver).for_each(|plugin| {
            dispatched = true;
            plugin.dispatch_command(view_id, method, params);
        });

        if dispatched {
            return;
        }

        let Some(manifest) = self.plugins.get_named(receiver) else {
            warn!("plugin {} is not available for command {}", receiver, method);
            return;
        };

        if !manifest.activates_on_command() {
            warn!("plugin {} is not running and is not command-activated", receiver);
            return;
        }

        self.pending_plugin_commands.push(PendingPluginCommand {
            plugin_name: manifest.name.clone(),
            view_id,
            method: method.to_string(),
            params: params.clone(),
            shutdown_after_dispatch: matches!(
                manifest.scope,
                crate::plugins::manifest::PluginScope::SingleInvocation
            ),
        });
        self.start_plugin(manifest);
    }

    pub(super) fn after_stop_plugin(&mut self, plugin: &Plugin) {
        self.iter_groups().for_each(|mut cx| cx.plugin_stopped(plugin));
    }

    pub(super) fn notify_plugin_terminated(
        &mut self,
        plugin_name: &str,
        reason: &PluginTerminationReason,
    ) {
        self.iter_groups().for_each(|cx| cx.plugin_terminated(plugin_name, reason));
    }
}
