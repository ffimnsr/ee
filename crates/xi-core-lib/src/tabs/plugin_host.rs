//! Core-side integration for the v2 editor plugin host.
//!
//! This module keeps `CoreState` access in one place: it builds the host,
//! exposes `WeakCoreAccess` for services, applies queued effects on the core
//! thread, and forwards queued status/contribution notifications to the client.

use log::warn;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::WeakXiCore;
use crate::plugin_runtime::bridge;
use crate::plugin_runtime::core_api::{
    DocumentSnapshot, EditorAccess, EffectSink, FoldRange, SyntaxScope, Viewport,
};
use crate::plugin_runtime::effects::apply_effects;
use crate::plugin_runtime::host::{EditorPluginHost, NotifierEvent, QueuedNotifier};
use crate::plugin_runtime::services::fs::FsScope;
use crate::plugins::rpc::PluginEdit;
use crate::selection::{SelRegion, Selection};
use crate::tabs::{BufferId, CoreState, ViewId};
use ee_plugin_api::effect::{DecorationPatch, ScrollRequest, SelectionRequest, StatusItem};
use ee_plugin_api::error::PluginError;
use ee_plugin_api::host::HostKind;
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::model::{
    BufferId as ApiBufferId, EditAck, EditRequest, LanguageId as ApiLanguageId,
    SelectionRange as ApiSelectionRange, SymbolItem as ApiSymbolItem, ViewId as ApiViewId,
};
use ee_plugin_runtime::builtin::BuiltinCatalog;
use ee_plugin_runtime::discovery::{DiscoveryOptions, RootKind};
use ee_plugin_runtime::manager::PluginManagerConfig;
use serde_json::json;

/// Tests without an isolated config home must not execute plugins installed in
/// the developer's real configuration tree. Tests that install fixtures set
/// `XDG_CONFIG_HOME` to a temp path, which keeps them isolated.
fn include_test_plugin_root(path: &Path) -> bool {
    let test_process = cfg!(test) || std::env::var_os("EE_TEST_DISABLE_USER_PLUGINS").is_some();
    if !test_process {
        return true;
    }
    path.starts_with(std::env::temp_dir())
}

impl CoreState {
    /// Start the v2 host during `finish_setup`.
    pub(crate) fn plugin_host_start(&mut self) {
        let Some(weak_core) = self.self_ref.clone() else {
            return;
        };
        let config_dir =
            self.config_manager.config_root().map(Path::to_path_buf).unwrap_or_default();
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let mut discovery = DiscoveryOptions::new();
        for (index, path) in self.config_manager.get_plugin_paths().iter().enumerate() {
            if !include_test_plugin_root(path) {
                continue;
            }
            let kind = if index == 0 { RootKind::ClientExtra } else { RootKind::User };
            discovery.push_root(kind, path);
        }
        discovery.push_root(RootKind::Workspace, workspace.join(".ee").join("plugins"));
        discovery = discovery.trust_workspace(false);

        let mut env = std::collections::BTreeMap::new();
        if let Ok(paths) = std::env::var("EE_PLUGIN_PATH") {
            env.insert("EE_PLUGIN_PATH".to_owned(), paths);
        }
        let manager_config = PluginManagerConfig::new(HostKind::Editor, config_dir, workspace)
            .with_discovery(discovery)
            .with_host_config(json!({}));
        let mut manager_config = manager_config;
        manager_config.env = env;

        let access: Arc<dyn EditorAccess> = Arc::new(WeakCoreAccess::new(weak_core));
        let notifier = Arc::new(QueuedNotifier::new());
        let host = match EditorPluginHost::new(
            access,
            manager_config,
            BuiltinCatalog::new(),
            FsScope { read: true, write: false, allow: Vec::new() },
            notifier.clone(),
        ) {
            Ok(host) => Arc::new(host),
            Err(error) => {
                warn!("failed to create plugin host: {error}");
                return;
            }
        };
        // Activation is deferred to the first plugin cycle so plugin callbacks
        // never run while the core mutex is held by `finish_setup`.
        self.plugin_host = Some(Arc::clone(&host));
        self.plugin_notifier = Some(notifier);
        self.plugin_generation = host.generation();
        self.plugin_send_notifications();
    }

    /// Recompute the language map after plugin activation or reload.
    pub(crate) fn plugin_refresh_languages(&mut self) {
        let Some(host) = self.plugin_host.clone() else {
            return;
        };
        if host.language_contributions().is_empty() {
            return;
        }
        let mut definitions = self
            .config_manager
            .languages()
            .iter()
            .map(|language| (**language).clone())
            .collect::<Vec<_>>();
        definitions.extend(host.language_contributions().into_iter().map(|language| {
            crate::syntax::LanguageDefinition {
                name: crate::syntax::LanguageId::from(language.name.as_str()),
                extensions: language.extensions,
                filenames: language.filenames,
                globs: language.globs,
                first_line_match: language.first_line_match,
                scope: language.scope,
                default_config: language
                    .default_config
                    .and_then(|value| value.as_object().cloned()),
            }
        }));
        let languages = crate::syntax::Languages::new(&definitions);
        let ids = languages.iter().map(|language| language.name.clone()).collect::<Vec<_>>();
        let changes = self.config_manager.set_languages(languages);
        if let Err(error) = crate::runtime_loader::reload_default_runtime_loader_languages(
            self.config_manager.languages(),
        ) {
            warn!("failed reloading runtime languages after plugin activation: {error}");
        }
        self.peer.available_languages(ids);
        self.handle_config_changes(changes);
        self.plugin_send_notifications();
    }

    /// Drain host notifications and forward them to the client.
    pub(crate) fn plugin_send_notifications(&mut self) {
        let Some(notifier) = self.plugin_notifier.clone() else {
            return;
        };
        let events = notifier.drain();
        for event in events {
            match event {
                NotifierEvent::Contributions { revision, upsert, remove } => {
                    self.peer.plugin_contributions(revision, &upsert, &remove);
                }
                NotifierEvent::Status(status) => self.peer.plugin_status(&status),
                NotifierEvent::Health { plugin, state, reason } => {
                    self.peer.plugin_health(&plugin, &state, reason.as_deref());
                }
                NotifierEvent::Alert(message) => self.peer.alert(message),
            }
        }
    }

    /// Apply queued effects produced by commands or native messages.
    pub(crate) fn apply_plugin_effects(
        &mut self,
        queued: Vec<crate::plugin_runtime::host::QueuedEffect>,
    ) {
        let host = self.plugin_host.clone();
        let ui = host.as_ref().map(|host| host.ui().as_ref());
        let tasks = host.as_ref().map(|host| host.tasks().as_ref());
        for queued in queued {
            let effect = queued.effect.clone();
            let report = apply_effects(self, queued.owner.as_ref(), vec![queued.effect], ui, tasks);
            let applied = report.is_ok();
            for (_, error) in report.errors {
                warn!("plugin effect failed: {error}");
                self.peer.alert(format!("plugin effect failed: {error}"));
            }
            if applied {
                self.forward_ui_effect(&effect);
            }
        }
    }

    fn forward_ui_effect(&self, effect: &ee_plugin_api::effect::Effect) {
        use ee_plugin_api::effect::Effect;
        let payload = match effect {
            Effect::ShowPanel(update) => Some(json!({
                "kind": "panel",
                "id": update.id,
                "revision": update.revision,
                "visible": update.visible,
                "data": update.data,
            })),
            Effect::ShowOverlay(update) => Some(json!({
                "kind": "overlay",
                "id": update.id,
                "revision": update.revision,
                "visible": update.visible,
                "data": update.data,
            })),
            Effect::ShowPicker(update) => Some(json!({
                "kind": "picker",
                "id": update.id,
                "revision": update.revision,
                "items": update.items,
            })),
            _ => None,
        };
        if let Some(payload) = payload {
            self.peer.plugin_ui_update(&payload);
        }
    }

    fn plugin_apply_edit(&mut self, request: &EditRequest) -> Result<EditAck, PluginError> {
        let view_id = core_view(request.view_id);
        let buffer_id =
            self.views.get(&view_id).map(|view| view.borrow().get_buffer_id()).ok_or_else(
                || PluginError::Schema {
                    message: format!("view {} is not open", request.view_id.0),
                },
            )?;
        let editor_cell = self.editors.get(&buffer_id).ok_or_else(|| PluginError::Schema {
            message: format!("buffer {} is not open", buffer_id.0),
        })?;

        let ack = {
            let mut editor = editor_cell.borrow_mut();
            if editor.is_vlf() {
                return Err(PluginError::Permission {
                    permission: "editor.document.write (VLF buffer)".to_owned(),
                });
            }
            let base_len = editor.get_rev(request.rev).map(|text| text.len()).ok_or_else(|| {
                PluginError::Runtime {
                    message: format!("stale revision {} (unknown)", request.rev),
                }
            })?;
            let base = editor
                .get_rev(request.rev)
                .map(|text| text.slice_to_cow(0..text.len()).into_owned())
                .unwrap_or_default();
            for edit in &request.edits {
                if !base.is_char_boundary(edit.range.start)
                    || !base.is_char_boundary(edit.range.end)
                {
                    return Err(PluginError::Schema {
                        message: "edit range is not on a UTF-8 boundary".to_owned(),
                    });
                }
            }
            let delta = bridge::text_edits_to_delta(base_len, &request.edits)?;
            let edit = PluginEdit {
                rev: request.rev,
                delta,
                priority: request.priority,
                after_cursor: request.after_cursor,
                undo_group: request.undo_group,
                author: request.author.clone(),
            };
            editor.apply_plugin_edit(edit)
        };

        if ack.applied {
            if let Some(mut context) = self.make_context(view_id) {
                context.after_edit(&request.author);
            }
            Ok(EditAck { applied: true, rev: ack.rev, reason: None })
        } else {
            Err(PluginError::Runtime {
                message: ack.reason.unwrap_or_else(|| "edit rejected".to_owned()),
            })
        }
    }

    fn plugin_core_snapshot(&self, buffer: ApiBufferId) -> Result<DocumentSnapshot, PluginError> {
        let buffer_id = core_buffer(buffer);
        let editor_cell = self.editors.get(&buffer_id).ok_or_else(|| PluginError::Schema {
            message: format!("buffer {} is not open", buffer.0),
        })?;
        let editor = editor_cell.borrow();
        let info = self.file_manager.get_info(buffer_id);
        let language = self.config_manager.get_buffer_language(buffer_id);
        Ok(DocumentSnapshot {
            buffer_id: buffer,
            rev: editor.get_head_rev_token(),
            len: editor.get_buffer().len(),
            line_count: editor.plugin_n_lines(),
            path: info.map(|info| info.path.display().to_string()),
            language: ApiLanguageId::new(language.as_ref()),
            vlf: editor.is_vlf(),
        })
    }
}

impl EffectSink for CoreState {
    fn effect_apply_edit(&mut self, request: &EditRequest) -> Result<EditAck, PluginError> {
        self.plugin_apply_edit(request)
    }

    fn effect_set_selection(&mut self, request: &SelectionRequest) -> Result<(), PluginError> {
        let view_id = core_view(request.view_id);
        let view_cell = self.views.get(&view_id).ok_or_else(|| PluginError::Schema {
            message: format!("view {} is not open", request.view_id.0),
        })?;
        let buffer_id = view_cell.borrow().get_buffer_id();
        let editor = self.editors[&buffer_id].borrow();
        if request.rev != editor.get_head_rev_token() {
            return Err(PluginError::Runtime {
                message: format!("stale selection revision {}", request.rev),
            });
        }
        let len = editor.get_buffer().len();
        let mut selection = Selection::new();
        for range in &request.selections {
            selection.add_region(SelRegion::new(range.start.min(len), range.end.min(len)));
        }
        view_cell.borrow_mut().set_selection(editor.get_buffer(), selection);
        drop(editor);
        if let Some(mut context) = self.make_context(view_id) {
            context.selection_changed();
            context.render_if_needed();
        }
        Ok(())
    }

    fn effect_scroll_to(&mut self, request: &ScrollRequest) -> Result<(), PluginError> {
        let view_id = core_view(request.view_id);
        let view_cell = self.views.get(&view_id).ok_or_else(|| PluginError::Schema {
            message: format!("view {} is not open", request.view_id.0),
        })?;
        let mut view = view_cell.borrow_mut();
        let height = view.scroll_height();
        view.set_scroll(request.line as i64, request.line.saturating_add(height) as i64);
        drop(view);
        if let Some(mut context) = self.make_context(view_id) {
            context.render_if_needed();
        }
        Ok(())
    }

    fn effect_set_status(
        &mut self,
        owner: Option<&PluginId>,
        item: &StatusItem,
    ) -> Result<(), PluginError> {
        let view_id = self
            .views
            .keys()
            .next()
            .copied()
            .ok_or_else(|| PluginError::Schema { message: "no open view".to_owned() })?;
        let source = owner.map(PluginId::as_str).unwrap_or("plugin");
        let alignment = match item.alignment {
            ee_plugin_api::contribution::StatusAlignment::Left => "left",
            ee_plugin_api::contribution::StatusAlignment::Right => "right",
        };
        self.peer.add_status_item(view_id, source, item.key.as_str(), &item.text, alignment);
        Ok(())
    }

    fn effect_remove_status(
        &mut self,
        _owner: Option<&PluginId>,
        key: &str,
    ) -> Result<(), PluginError> {
        if let Some(view_id) = self.views.keys().next().copied() {
            self.peer.remove_status_item(view_id, key);
        }
        Ok(())
    }

    fn effect_set_decorations(
        &mut self,
        _owner: Option<&PluginId>,
        patch: &DecorationPatch,
    ) -> Result<(), PluginError> {
        let view_id = core_view(patch.view_id);
        let buffer_id =
            self.views.get(&view_id).map(|view| view.borrow().get_buffer_id()).ok_or_else(
                || PluginError::Schema { message: format!("view {} is not open", patch.view_id.0) },
            )?;
        let editor = self.editors[&buffer_id].borrow();
        if patch.rev != editor.get_head_rev_token() {
            return Err(PluginError::Runtime {
                message: format!("stale decoration revision {}", patch.rev),
            });
        }
        let len = editor.get_buffer().len();
        for span in &patch.spans {
            if span.start > span.end || span.end > len {
                return Err(PluginError::Schema {
                    message: format!(
                        "decoration {}..{} exceeds length {len}",
                        span.start, span.end
                    ),
                });
            }
        }
        self.peer.plugin_decorations(
            view_id,
            &json!({
                "rev": patch.rev,
                "kind": patch.decoration_kind,
                "spans": patch.spans,
            }),
        );
        Ok(())
    }

    fn effect_emit_agent_tool_result(
        &mut self,
        call_id: &str,
        is_error: bool,
        content: &serde_json::Value,
    ) -> Result<(), PluginError> {
        if let Some(view_id) = self.views.keys().next().copied() {
            self.peer.agent_tool_result(
                view_id,
                "plugin",
                json!({"call_id": call_id, "is_error": is_error, "content": content}),
            );
        }
        Ok(())
    }
}

/// `EditorAccess` implementation backed by a weak core handle.
pub(crate) struct WeakCoreAccess {
    core: WeakXiCore,
}

impl WeakCoreAccess {
    pub(crate) fn new(core: WeakXiCore) -> Self {
        Self { core }
    }

    fn with_core<T>(
        &self,
        operation: impl FnOnce(&mut CoreState) -> Result<T, PluginError>,
    ) -> Result<T, PluginError> {
        self.core.with_state(operation).unwrap_or_else(|| {
            Err(PluginError::Runtime { message: "core is busy; service call deferred".to_owned() })
        })
    }

    fn buffer_for_view(&self, view: ApiViewId) -> Result<BufferId, PluginError> {
        self.with_core(|core| {
            core.views.get(&core_view(view)).map(|view| view.borrow().get_buffer_id()).ok_or_else(
                || PluginError::Schema { message: format!("view {} is not open", view.0) },
            )
        })
    }
}

impl EditorAccess for WeakCoreAccess {
    fn document_snapshot(&self, buffer: ApiBufferId) -> Result<DocumentSnapshot, PluginError> {
        self.with_core(|core| core.plugin_core_snapshot(buffer))
    }

    fn read_range(
        &self,
        buffer: ApiBufferId,
        start: usize,
        end: usize,
    ) -> Result<String, PluginError> {
        self.with_core(|core| {
            let buffer_id = core_buffer(buffer);
            let editor = core.editors.get(&buffer_id).ok_or_else(|| PluginError::Schema {
                message: format!("buffer {} is not open", buffer.0),
            })?;
            let editor = editor.borrow();
            let text = editor.get_buffer();
            if start > end || end > text.len() {
                return Err(PluginError::Schema { message: "invalid read range".to_owned() });
            }
            let full = text.slice_to_cow(0..text.len());
            if !full.is_char_boundary(start) || !full.is_char_boundary(end) {
                return Err(PluginError::Schema {
                    message: "read range is not on a UTF-8 boundary".to_owned(),
                });
            }
            Ok(text.slice_to_cow(start..end).into_owned())
        })
    }

    fn read_document(&self, buffer: ApiBufferId, max_bytes: usize) -> Result<String, PluginError> {
        self.with_core(|core| {
            let buffer_id = core_buffer(buffer);
            let editor = core.editors.get(&buffer_id).ok_or_else(|| PluginError::Schema {
                message: format!("buffer {} is not open", buffer.0),
            })?;
            let editor = editor.borrow();
            let text = editor.get_buffer();
            let mut end = max_bytes.min(text.len());
            if end < text.len() {
                end = text.prev_codepoint_offset(end.saturating_add(1)).unwrap_or(end);
            }
            Ok(text.slice_to_cow(0..end).into_owned())
        })
    }

    fn apply_edit(&self, request: &EditRequest) -> Result<EditAck, PluginError> {
        self.with_core(|core| core.plugin_apply_edit(request))
    }

    fn views_for_buffer(&self, buffer: ApiBufferId) -> Vec<ApiViewId> {
        let buffer_id = core_buffer(buffer);
        self.with_core(|core| {
            Ok(core
                .views
                .iter()
                .filter(|(_, view)| view.borrow().get_buffer_id() == buffer_id)
                .map(|(view, _)| api_view(*view))
                .collect::<Vec<_>>())
        })
        .unwrap_or_default()
    }

    fn active_view(&self) -> Option<ApiViewId> {
        self.with_core(|core| Ok(core.views.keys().next().copied().map(api_view))).ok().flatten()
    }

    fn viewport(&self, view_id: ApiViewId) -> Result<Viewport, PluginError> {
        self.with_core(|core| {
            let view = core.views.get(&core_view(view_id)).ok_or_else(|| PluginError::Schema {
                message: format!("view {} is not open", view_id.0),
            })?;
            let (first_line, height) = view.borrow().scroll_position();
            Ok(Viewport { view_id, first_line, height })
        })
    }

    fn view_buffer(&self, view: ApiViewId) -> Option<ApiBufferId> {
        self.buffer_for_view(view).ok().map(api_buffer)
    }

    fn view_language(&self, view_id: ApiViewId) -> Option<ApiLanguageId> {
        let buffer = self.buffer_for_view(view_id).ok()?;
        self.with_core(|core| {
            Ok(Some(ApiLanguageId::new(core.config_manager.get_buffer_language(buffer).as_ref())))
        })
        .ok()
        .flatten()
    }

    fn selection(&self, view_id: ApiViewId) -> Result<Vec<ApiSelectionRange>, PluginError> {
        self.with_core(|core| {
            let view = core.views.get(&core_view(view_id)).ok_or_else(|| PluginError::Schema {
                message: format!("view {} is not open", view_id.0),
            })?;
            Ok(view
                .borrow()
                .sel_regions()
                .iter()
                .map(|region| ApiSelectionRange { start: region.start, end: region.end })
                .collect())
        })
    }

    fn apply_selection(&self, request: &SelectionRequest) -> Result<(), PluginError> {
        self.with_core(|core| core.effect_set_selection(request))
    }

    fn scroll_to(&self, request: &ScrollRequest) -> Result<(), PluginError> {
        self.with_core(|core| core.effect_scroll_to(request))
    }

    fn reveal(&self, view_id: ApiViewId, line: usize, _column: usize) -> Result<(), PluginError> {
        self.scroll_to(&ScrollRequest { view_id, line, column: 0 })
    }

    fn folds(&self, view_id: ApiViewId) -> Result<Vec<FoldRange>, PluginError> {
        self.with_core(|core| {
            let view = core.views.get(&core_view(view_id)).ok_or_else(|| PluginError::Schema {
                message: format!("view {} is not open", view_id.0),
            })?;
            let buffer = view.borrow().get_buffer_id();
            let editor = core.editors.get(&buffer).ok_or_else(|| PluginError::Schema {
                message: format!("buffer {} is not open", buffer.0),
            })?;
            let editor = editor.borrow();
            if editor.is_vlf() {
                return Ok(Vec::new());
            }
            let text = editor.get_buffer().slice_to_cow(0..editor.get_buffer().len()).into_owned();
            let info = core.file_manager.get_info(buffer);
            let language = core.config_manager.get_buffer_language(buffer);
            Ok(crate::fold_support::fold_ranges_for_text(
                Some(language.as_ref()),
                info.map(|info| info.path.as_path()),
                &text,
                crate::fold_support::fold_parse_timeout(text.len()),
            )
            .into_iter()
            .map(|fold| FoldRange {
                header_line: fold.header_line,
                body_start: fold.body_start,
                body_end: fold.body_end,
            })
            .collect())
        })
    }

    fn symbols(&self, view_id: ApiViewId) -> Result<Vec<ApiSymbolItem>, PluginError> {
        self.with_core(|core| {
            let view = core.views.get(&core_view(view_id)).ok_or_else(|| PluginError::Schema {
                message: format!("view {} is not open", view_id.0),
            })?;
            let buffer = view.borrow().get_buffer_id();
            let editor = core.editors.get(&buffer).ok_or_else(|| PluginError::Schema {
                message: format!("buffer {} is not open", buffer.0),
            })?;
            let editor = editor.borrow();
            let text = editor.get_buffer().slice_to_cow(0..editor.get_buffer().len()).into_owned();
            let info = core.file_manager.get_info(buffer);
            let language = core.config_manager.get_buffer_language(buffer);
            Ok(crate::document_symbols::document_symbols_for_text(
                Some(language.as_ref()),
                info.map(|info| info.path.as_path()),
                &text,
            )
            .into_iter()
            .map(api_symbol)
            .collect())
        })
    }

    fn scopes(&self, view_id: ApiViewId) -> Result<Vec<SyntaxScope>, PluginError> {
        self.with_core(|core| {
            let view = core.views.get(&core_view(view_id)).ok_or_else(|| PluginError::Schema {
                message: format!("view {} is not open", view_id.0),
            })?;
            let buffer = view.borrow().get_buffer_id();
            let editor = core.editors.get(&buffer).ok_or_else(|| PluginError::Schema {
                message: format!("buffer {} is not open", buffer.0),
            })?;
            let editor = editor.borrow();
            let text = editor.get_buffer().slice_to_cow(0..editor.get_buffer().len()).into_owned();
            let language = core.config_manager.get_buffer_language(buffer);
            if text.is_empty() {
                return Ok(Vec::new());
            }
            let mut starts = vec![0];
            starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
            let segments = starts
                .iter()
                .enumerate()
                .map(|(line, &start)| {
                    let end = starts
                        .get(line + 1)
                        .copied()
                        .unwrap_or(text.len())
                        .saturating_sub(usize::from(line + 1 < starts.len()));
                    start..end
                })
                .collect::<Vec<_>>();
            let spans = crate::tree_sitter_support::chunk_syntax_spans_for_segments(
                language.as_ref(),
                &text,
                &segments,
                0..segments.len(),
                crate::tree_sitter_support::VisibleSyntaxLimits::default(),
            );
            let mut scopes = Vec::new();
            for (index, per_line) in spans.into_iter().enumerate() {
                let base = segments[index].start;
                for span in per_line {
                    scopes.push(SyntaxScope {
                        start: base + span.start_byte,
                        end: base + span.end_byte,
                        scope: span.scope,
                    });
                }
            }
            Ok(scopes)
        })
    }

    fn grammar_available(&self, language: &str) -> bool {
        crate::tree_sitter_support::language_metadata_for_name(language).is_some()
    }

    fn workspace_root(&self) -> Option<PathBuf> {
        std::env::current_dir().ok()
    }

    fn is_vlf(&self, view_id: ApiViewId) -> bool {
        self.with_core(|core| {
            let view = core.views.get(&core_view(view_id)).ok_or_else(|| PluginError::Schema {
                message: format!("view {} is not open", view_id.0),
            })?;
            let buffer = view.borrow().get_buffer_id();
            Ok(core.editors[&buffer].borrow().is_vlf())
        })
        .unwrap_or(false)
    }
}

fn core_buffer(buffer: ApiBufferId) -> BufferId {
    BufferId(buffer.0 as usize)
}

fn api_buffer(buffer: BufferId) -> ApiBufferId {
    ApiBufferId(buffer.0 as u64)
}

fn core_view(view: ApiViewId) -> ViewId {
    ViewId(view.0 as usize)
}

fn api_view(view: ViewId) -> ApiViewId {
    ApiViewId(view.0 as u64)
}

fn api_symbol(symbol: crate::plugins::rpc::SymbolItem) -> ApiSymbolItem {
    ApiSymbolItem {
        name: symbol.name,
        kind: symbol.kind,
        path: symbol.path,
        line: symbol.line,
        column: symbol.column,
        children: symbol.children.into_iter().map(api_symbol).collect(),
    }
}

// Re-exported helpers used by EventContext bridge calls.
