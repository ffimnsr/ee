// Copyright 2016 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The main container for core state.
//!
//! All events from the frontend or from plugins are handled here first.
//!
//! This file is called 'tabs' for historical reasons, and should probably
//! be renamed.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use serde::de::{self, Deserializer, Unexpected};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use xi_rope::Rope;
use xi_rpc::{self, OptionExt, ReadError, RemoteError, RpcCtx, RpcPeer};

use crate::WeakXiCore;
use crate::client::Client;
use crate::config::{ConfigDomain, ConfigDomainExternal, ConfigManager, Table};
use crate::editor::Editor;
use crate::event_context::EventContext;
use crate::file::{FileManager, OpenResult, SampledIndentation, SampledLineEnding};
use crate::line_ending::LineEnding;
use crate::plugin_rpc::{PluginNotification, PluginRequest};
use crate::plugins::rpc::ClientPluginInfo;
use crate::plugins::rpc::SelectionRange;
use crate::plugins::{
    Plugin, PluginCatalog, PluginDescription, PluginPid, PluginStartError, PluginStartErrorKind,
    PluginTerminationReason, start_plugin_process,
};
use crate::rpc::{
    CoreNotification, CoreRequest, EditNotification, PluginNotification as CorePluginNotification,
};
use crate::syntax::LanguageId;
use crate::text_store::DocumentMode;
use crate::view::View;
use crate::whitespace::Indentation;
use crate::width_cache::WidthCache;

#[cfg(feature = "notify")]
use crate::watcher::{FileWatcher, WatchToken};
#[cfg(feature = "notify")]
use notify::Event;
/// ViewIds are the primary means of routing messages between
/// xi-core and a client view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewId(pub(crate) usize);

/// BufferIds uniquely identify open buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub struct BufferId(pub(crate) usize);

pub type PluginId = crate::plugins::PluginPid;

// old-style names; will be deprecated
pub type BufferIdentifier = BufferId;

/// Totally arbitrary; we reserve this space for `ViewId`s
pub(crate) const RENDER_VIEW_IDLE_MASK: usize = 1 << 25;
pub(crate) const REWRAP_VIEW_IDLE_MASK: usize = 1 << 26;
pub(crate) const FIND_VIEW_IDLE_MASK: usize = 1 << 27;

const NEW_VIEW_IDLE_TOKEN: usize = 1001;
const VERIFY_LINE_ENDINGS_IDLE_TOKEN: usize = 1003;

/// xi_rpc idle Token for watcher related idle scheduling.
pub(crate) const WATCH_IDLE_TOKEN: usize = 1002;

const PLUGIN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PLUGIN_RESTART_BASE_DELAY_MS: u64 = 250;
const PLUGIN_RESTART_MAX_DELAY_MS: u64 = 5_000;
const PLUGIN_STABLE_UPTIME: Duration = Duration::from_secs(30);

/// Token for file-change events in open files
#[cfg(feature = "notify")]
pub const OPEN_FILE_EVENT_TOKEN: WatchToken = WatchToken(1);

#[cfg(feature = "notify")]
const PLUGIN_EVENT_TOKEN: WatchToken = WatchToken(2);

#[allow(dead_code)]
pub struct CoreState {
    editors: BTreeMap<BufferId, RefCell<Editor>>,
    views: BTreeMap<ViewId, RefCell<View>>,
    file_manager: FileManager,
    /// A local pasteboard.
    kill_ring: RefCell<Rope>,
    width_cache: RefCell<WidthCache>,
    /// User and platform specific settings
    config_manager: ConfigManager,
    /// A weak reference to the main state container, stashed so that
    /// it can be passed to plugins.
    self_ref: Option<WeakXiCore>,
    /// Views which need to have setup finished.
    pending_views: Vec<(ViewId, Table)>,
    pending_line_ending_verifications: Vec<BufferId>,
    peer: Client,
    id_counter: Counter,
    plugins: PluginCatalog,
    launching_plugins: HashSet<String>,
    scheduled_plugin_restarts: HashSet<String>,
    stopping_plugins: HashMap<PluginId, StopReason>,
    plugin_restart_state: HashMap<String, PluginRestartState>,
    pending_plugin_commands: Vec<PendingPluginCommand>,
    running_plugins: Vec<Plugin>,
}

#[derive(Debug, Clone)]
struct PendingPluginCommand {
    plugin_name: String,
    view_id: ViewId,
    method: String,
    params: Value,
    shutdown_after_dispatch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StopReason {
    Manual,
    Restart,
    SingleInvocation,
    ResourceLimit(PluginTerminationReason),
}

#[derive(Debug, Default, Clone)]
struct PluginRestartState {
    consecutive_failures: u32,
    last_start: Option<Instant>,
}

/// Initial setup and bookkeeping
impl CoreState {
    pub(crate) fn new(
        peer: &RpcPeer,
        config_dir: Option<PathBuf>,
        extras_dir: Option<PathBuf>,
    ) -> Self {
        #[cfg(feature = "notify")]
        let mut watcher = FileWatcher::new(peer.clone());

        let config_manager = ConfigManager::new(config_dir, extras_dir);

        let plugins_dir = config_manager.get_plugins_dir();
        if let Some(p) = plugins_dir.as_ref() {
            #[cfg(feature = "notify")]
            watcher.watch_filtered(p, true, PLUGIN_EVENT_TOKEN, |p| p.is_dir() || !p.exists());
        }

        CoreState {
            views: BTreeMap::new(),
            editors: BTreeMap::new(),
            #[cfg(feature = "notify")]
            file_manager: FileManager::new(watcher),
            #[cfg(not(feature = "notify"))]
            file_manager: FileManager::new(),
            kill_ring: RefCell::new(Rope::from("")),
            width_cache: RefCell::new(WidthCache::new()),
            config_manager,
            self_ref: None,
            pending_views: Vec::new(),
            pending_line_ending_verifications: Vec::new(),
            peer: Client::new(peer.clone()),
            id_counter: Counter::default(),
            plugins: PluginCatalog::default(),
            launching_plugins: HashSet::new(),
            scheduled_plugin_restarts: HashSet::new(),
            stopping_plugins: HashMap::new(),
            plugin_restart_state: HashMap::new(),
            pending_plugin_commands: Vec::new(),
            running_plugins: Vec::new(),
        }
    }

    fn next_view_id(&self) -> ViewId {
        ViewId(self.id_counter.next())
    }

    fn next_buffer_id(&self) -> BufferId {
        BufferId(self.id_counter.next())
    }

    fn next_plugin_id(&self) -> PluginId {
        PluginPid(self.id_counter.next())
    }

    pub(crate) fn finish_setup(&mut self, self_ref: WeakXiCore) {
        self.self_ref = Some(self_ref);

        // instead of having to do this here, config should just own
        // the plugin catalog and reload automatically
        let plugin_paths = self.config_manager.get_plugin_paths();
        self.plugins.reload_from_paths(&plugin_paths).into_iter().for_each(|err| {
            warn!("error loading plugin {:?}", err);
        });
        let languages = self.plugins.make_languages_map();
        let languages_ids = languages.iter().map(|l| l.name.clone()).collect::<Vec<_>>();
        self.peer.available_languages(languages_ids);
        let lang_config_changes = self.config_manager.set_languages(languages);
        self.handle_config_changes(lang_config_changes);

        self.ensure_manifest_plugins_started();
    }

    /// Sets (overwriting) the config for a given domain.
    fn set_config(&mut self, domain: ConfigDomain, table: Table) {
        match self.config_manager.set_user_config(domain, table) {
            Err(e) => self.peer.alert(format!("{}", &e)),
            Ok(changes) => self.handle_config_changes(changes),
        }
    }

    /// Notify editors/views/plugins of config changes.
    fn handle_config_changes(&self, changes: Vec<(BufferId, Table)>) {
        for (id, table) in changes {
            let view_id = self
                .views
                .values()
                .find(|v| v.borrow().get_buffer_id() == id)
                .map(|v| v.borrow().get_view_id())
                .unwrap();

            self.make_context(view_id).unwrap().config_changed(&table)
        }
    }
}

/// Handling client events
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
    fn iter_groups<'a>(&'a self) -> Iter<'a, Box<dyn Iterator<Item = &'a ViewId> + 'a>> {
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
            //TODO: make file_path be an Option<PathBuf>
            //TODO: make this a notification
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
            BlockTextPreview { view_id, start_line, end_line, left_col, right_col } => {
                self.do_block_text_preview(view_id, start_line, end_line, left_col, right_col)
            }
            SelectCharsPreview { view_id, count } => self.do_select_chars_preview(view_id, count),
        }
    }

    fn do_edit(&mut self, view_id: ViewId, cmd: EditNotification) {
        if let Some(mut edit_ctx) = self.make_context(view_id) {
            edit_ctx.do_edit(cmd);
        }
    }

    fn do_select_chars_preview(
        &mut self,
        view_id: ViewId,
        count: usize,
    ) -> Result<Value, RemoteError> {
        let mut ctx = self
            .make_context(view_id)
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        Ok(json!(ctx.preview_select_chars(count)))
    }

    fn do_selected_text_preview(
        &mut self,
        view_id: ViewId,
        linewise: bool,
    ) -> Result<Value, RemoteError> {
        let mut ctx = self
            .make_context(view_id)
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        Ok(json!(ctx.preview_selected_text(linewise)))
    }

    fn do_selections_preview(&mut self, view_id: ViewId) -> Result<Value, RemoteError> {
        let mut ctx = self
            .make_context(view_id)
            .ok_or_else(|| RemoteError::custom(404, "missing view", None))?;
        Ok(json!(ctx.preview_selections()))
    }

    fn do_block_text_preview(
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

    fn do_set_config(&mut self, domain: ConfigDomainExternal, changes: Table) {
        let Some(domain) = self.resolve_config_domain(domain) else {
            return;
        };
        self.set_config(domain, changes);
    }

    fn do_new_view(&mut self, path: Option<PathBuf>) -> Result<Value, RemoteError> {
        let view_id = self.next_view_id();
        let buffer_id = self.next_buffer_id();

        let open_result = match path.as_ref() {
            Some(p) => self.file_manager.open(p, buffer_id)?,
            None => OpenResult::Rope { text: Rope::from(""), mode: DocumentMode::Normal },
        };
        let editor = match open_result {
            OpenResult::Rope { text, mode } => RefCell::new(Editor::with_text_mode(text, mode)),
            OpenResult::Vlf(store) => RefCell::new(Editor::with_vlf_store(*store)),
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

    fn do_substitute_preview(
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

    fn do_filter_selections_preview(
        &mut self,
        view_id: ViewId,
        pattern: &str,
        remove: bool,
    ) -> Result<Value, RemoteError> {
        let mut ctx = self.make_context(view_id).ok_or_not_found("view not found")?;
        let selections: Vec<SelectionRange> = ctx.preview_filter_selections(pattern, remove)?;
        Ok(json!(selections))
    }

    fn do_save<P>(&mut self, view_id: ViewId, path: P)
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

        let mut save_ctx = self.make_context(view_id).unwrap();
        let fin_text = save_ctx.text_for_save();

        if let Err(e) = self.file_manager.save(path, &fin_text, buffer_id) {
            let error_message = e.to_string();
            error!("File error: {:?}", error_message);
            self.peer.alert(error_message);
            return;
        }

        let changes = self.config_manager.update_buffer_path(buffer_id, path);
        let language = self.config_manager.get_buffer_language(buffer_id);

        self.make_context(view_id).unwrap().after_save(path);
        self.make_context(view_id).unwrap().language_changed(&language);

        // update the config _after_ sending save related events
        if let Some(changes) = changes {
            self.make_context(view_id).unwrap().config_changed(&changes);
        }
    }

    fn do_close_view(&mut self, view_id: ViewId) {
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

    fn resolve_config_domain(&self, domain: ConfigDomainExternal) -> Option<ConfigDomain> {
        match domain {
            ConfigDomainExternal::General => Some(ConfigDomain::General),
            ConfigDomainExternal::Language(language) => Some(ConfigDomain::Language(language)),
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

    fn do_start_plugin(&mut self, _view_id: ViewId, plugin: &str) {
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

    fn do_stop_plugin(&mut self, _view_id: ViewId, plugin: &str) {
        if let Some(plugin) = self.running_plugins.iter().find(|running| running.name == plugin) {
            self.begin_plugin_shutdown(plugin.id, StopReason::Manual);
        }
    }

    fn do_restart_plugin(&mut self, view_id: ViewId, plugin: &str) {
        if let Some(plugin) = self.running_plugins.iter().find(|running| running.name == plugin) {
            self.begin_plugin_shutdown(plugin.id, StopReason::Restart);
            return;
        }

        self.do_start_plugin(view_id, plugin);
    }

    fn do_plugin_rpc(&mut self, view_id: ViewId, receiver: &str, method: &str, params: &Value) {
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

    fn after_stop_plugin(&mut self, plugin: &Plugin) {
        self.iter_groups().for_each(|mut cx| cx.plugin_stopped(plugin));
    }

    fn notify_plugin_terminated(&mut self, plugin_name: &str, reason: &PluginTerminationReason) {
        self.iter_groups().for_each(|cx| cx.plugin_terminated(plugin_name, reason));
    }
}

impl CoreState {
    fn ensure_manifest_plugins_started(&mut self) {
        let to_start = self
            .plugins
            .iter()
            .filter(|manifest| {
                manifest.activates_on_startup()
                    || self
                        .views
                        .values()
                        .map(|view| {
                            self.config_manager.get_buffer_language(view.borrow().get_buffer_id())
                        })
                        .any(|language| manifest.receives_updates_for(&language))
            })
            .collect::<Vec<_>>();

        for manifest in to_start {
            self.start_plugin(manifest);
        }
    }

    fn ensure_plugins_for_language(&mut self, language: &LanguageId) {
        let to_start = self
            .plugins
            .iter()
            .filter(|manifest| manifest.receives_updates_for(language))
            .collect::<Vec<_>>();

        for manifest in to_start {
            self.start_plugin(manifest);
        }
    }

    fn start_plugin(&mut self, manifest: Arc<PluginDescription>) {
        if !self.begin_plugin_launch(&manifest.name) {
            return;
        }

        self.scheduled_plugin_restarts.remove(&manifest.name);
        self.plugin_restart_state.entry(manifest.name.clone()).or_default().last_start =
            Some(Instant::now());
        start_plugin_process(
            manifest,
            self.next_plugin_id(),
            self.self_ref.as_ref().unwrap().clone(),
        );
    }

    fn begin_plugin_launch(&mut self, plugin_name: &str) -> bool {
        if self.running_plugins.iter().any(|plugin| plugin.name == plugin_name)
            || self.launching_plugins.contains(plugin_name)
        {
            return false;
        }

        self.launching_plugins.insert(plugin_name.to_string());
        true
    }

    fn begin_plugin_shutdown(&mut self, plugin_id: PluginId, reason: StopReason) {
        let Some(plugin) = self.running_plugins.iter().find(|plugin| plugin.id == plugin_id) else {
            return;
        };

        if self.stopping_plugins.contains_key(&plugin_id) {
            return;
        }

        let weak_core = self.self_ref.as_ref().unwrap().clone();
        let process = plugin.controller_handle();
        let plugin_name = plugin.name.clone();
        plugin.shutdown();
        self.stopping_plugins.insert(plugin_id, reason);

        std::thread::spawn(move || {
            let deadline = Instant::now() + PLUGIN_SHUTDOWN_TIMEOUT;
            loop {
                let wait_result = process.has_exited().ok();
                if wait_result == Some(true) {
                    break;
                }

                if Instant::now() >= deadline {
                    weak_core.plugin_stderr(
                        plugin_name.clone(),
                        format!(
                            "shutdown timed out after {:?}; terminating plugin",
                            PLUGIN_SHUTDOWN_TIMEOUT
                        ),
                    );
                    let _ = process.terminate();
                    break;
                }

                std::thread::sleep(Duration::from_millis(25));
            }
        });
    }

    fn should_keep_running(&self, manifest: &PluginDescription) -> bool {
        manifest.activates_on_startup()
            || self
                .views
                .values()
                .map(|view| self.config_manager.get_buffer_language(view.borrow().get_buffer_id()))
                .any(|language| manifest.receives_updates_for(&language))
            || self
                .pending_plugin_commands
                .iter()
                .any(|command| command.plugin_name == manifest.name)
    }

    fn next_restart_delay(&mut self, plugin_name: &str) -> Duration {
        let state = self.plugin_restart_state.entry(plugin_name.to_string()).or_default();
        if state.last_start.is_some_and(|last_start| last_start.elapsed() >= PLUGIN_STABLE_UPTIME) {
            state.consecutive_failures = 0;
        }

        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        let factor = 1_u64 << state.consecutive_failures.saturating_sub(1).min(6);
        Duration::from_millis(
            (PLUGIN_RESTART_BASE_DELAY_MS * factor).min(PLUGIN_RESTART_MAX_DELAY_MS),
        )
    }

    fn schedule_plugin_restart(&mut self, plugin_name: &str) {
        if self.scheduled_plugin_restarts.contains(plugin_name) {
            return;
        }

        let Some(manifest) = self.plugins.get_named(plugin_name) else {
            return;
        };
        if matches!(manifest.scope, crate::plugins::manifest::PluginScope::SingleInvocation)
            || !self.should_keep_running(&manifest)
        {
            return;
        }

        let delay = self.next_restart_delay(plugin_name);
        let weak_core = self.self_ref.as_ref().unwrap().clone();
        let restart_name = plugin_name.to_string();
        self.scheduled_plugin_restarts.insert(restart_name.clone());
        warn!("plugin {} exited unexpectedly; restarting in {:?}", restart_name, delay);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            weak_core.restart_plugin(restart_name);
        });
    }

    pub(crate) fn restart_plugin(&mut self, plugin_name: &str) {
        self.scheduled_plugin_restarts.remove(plugin_name);
        if self.launching_plugins.contains(plugin_name)
            || self.running_plugins.iter().any(|plugin| plugin.name == plugin_name)
        {
            return;
        }

        if let Some(manifest) = self.plugins.get_named(plugin_name)
            && self.should_keep_running(&manifest)
        {
            self.start_plugin(manifest);
        }
    }
}

/// Idle, tracing, and file event handling
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
            other => panic!("unexpected idle token {}", other),
        };
    }

    fn finalize_new_views(&mut self) {
        let to_start = mem::take(&mut self.pending_views);

        to_start.iter().for_each(|(id, config)| {
            let modified = self.detect_whitespace(*id, config);
            let config = modified.as_ref().unwrap_or(config);
            let mut edit_ctx = self.make_context(*id).unwrap();
            edit_ctx.finish_init(config);
        });
    }

    // Detects whitespace settings from the file and merges them with the config
    fn detect_whitespace(&mut self, id: ViewId, config: &Table) -> Option<Table> {
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

    fn schedule_line_ending_verification(&mut self, buffer_id: BufferId) {
        if self.pending_line_ending_verifications.contains(&buffer_id) {
            return;
        }
        self.pending_line_ending_verifications.push(buffer_id);
        self.peer.schedule_idle(VERIFY_LINE_ENDINGS_IDLE_TOKEN);
    }

    fn verify_pending_line_endings(&mut self) {
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

    fn handle_render_timer(&mut self, token: usize) {
        let id: ViewId = token.into();
        if let Some(mut ctx) = self.make_context(id) {
            ctx._finish_delayed_render();
        }
    }

    /// Callback for doing word wrap on a view
    fn handle_rewrap_callback(&mut self, token: usize) {
        let id: ViewId = token.into();
        if let Some(mut ctx) = self.make_context(id) {
            ctx.do_rewrap_batch();
        }
    }

    /// Callback for doing incremental find in a view
    fn handle_find_callback(&mut self, token: usize) {
        let id: ViewId = token.into();
        if let Some(mut ctx) = self.make_context(id) {
            ctx.do_incremental_find();
        }
    }

    #[cfg(feature = "notify")]
    fn handle_fs_events(&mut self) {
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
    fn handle_fs_events(&mut self) {}

    /// Handles a file system event related to a currently open file
    #[cfg(feature = "notify")]
    fn handle_open_file_fs_event(&mut self, event: Event) {
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
        //TODO: currently we only use the file's modification time when
        // determining if a file has been changed by another process.
        // A more robust solution would also hash the file's contents.

        if has_changes && is_pristine {
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
    fn handle_plugin_fs_event(&mut self, event: Event) {
        use notify::event::*;
        match event.kind {
            EventKind::Create(CreateKind::Any) | EventKind::Modify(ModifyKind::Any) => {
                self.plugins.load_from_paths(&[event.paths[0].clone()]).into_iter().for_each(
                    |err| {
                        warn!("error loading plugin {:?}", err);
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
                        warn!("error loading plugin {:?}", err);
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

/// plugin event handling
impl CoreState {
    /// Called from a plugin's thread after trying to start the plugin.
    pub(crate) fn plugin_connect(&mut self, plugin: Result<Plugin, PluginStartError>) {
        match plugin {
            Ok(plugin) => {
                self.launching_plugins.remove(&plugin.name);
                let pending_commands = self.take_pending_plugin_commands(&plugin.name);
                let init_info = self.plugin_init_info(&plugin, &pending_commands);
                let should_shutdown =
                    pending_commands.iter().any(|command| command.shutdown_after_dispatch)
                        || (plugin.is_single_invocation() && !pending_commands.is_empty());
                let plugin_id = plugin.id;
                plugin.initialize(init_info);
                pending_commands.iter().for_each(|command| {
                    plugin.dispatch_command(command.view_id, &command.method, &command.params);
                });
                self.plugin_restart_state.entry(plugin.name.clone()).or_default().last_start =
                    Some(Instant::now());
                self.running_plugins.push(plugin);
                if should_shutdown {
                    self.begin_plugin_shutdown(plugin_id, StopReason::SingleInvocation);
                }
            }
            Err(err) => {
                self.launching_plugins.remove(&err.name);
                error!("failed to start plugin {}: {:?}", err.name, err.source);
                let detail = match err.source {
                    PluginStartErrorKind::Io(source) => source.to_string(),
                    PluginStartErrorKind::UnsupportedTransport(transport) => {
                        format!("unsupported transport {transport:?}")
                    }
                    PluginStartErrorKind::Sandbox(detail) | PluginStartErrorKind::Wasm(detail) => {
                        detail
                    }
                };
                self.peer.alert(format!("failed to start plugin {}: {}", err.name, detail));
                self.schedule_plugin_restart(&err.name);
            }
        }
    }

    pub(crate) fn plugin_exit(&mut self, id: PluginId, error: Result<(), ReadError>) {
        warn!("plugin {:?} exited with result {:?}", id, error);
        let running_idx = self.running_plugins.iter().position(|p| p.id == id);
        if let Some(idx) = running_idx {
            let plugin = self.running_plugins.remove(idx);
            self.launching_plugins.remove(&plugin.name);
            let stop_reason = self.stopping_plugins.remove(&id);
            self.after_stop_plugin(&plugin);
            if let Some(StopReason::ResourceLimit(reason)) = stop_reason {
                self.notify_plugin_terminated(&plugin.name, &reason);
                self.scheduled_plugin_restarts.remove(&plugin.name);
                self.plugin_restart_state.remove(&plugin.name);
            } else if stop_reason == Some(StopReason::Restart) {
                self.scheduled_plugin_restarts.remove(&plugin.name);
                self.plugin_restart_state.remove(&plugin.name);
                self.restart_plugin(&plugin.name);
            } else if stop_reason.is_none() {
                self.schedule_plugin_restart(&plugin.name);
            } else {
                self.scheduled_plugin_restarts.remove(&plugin.name);
                self.plugin_restart_state.remove(&plugin.name);
            }
        }
    }

    pub(crate) fn plugin_terminated(&mut self, id: PluginId, reason: PluginTerminationReason) {
        self.stopping_plugins.insert(id, StopReason::ResourceLimit(reason));
    }

    /// Handles the response to a sync update sent to a plugin.
    pub(crate) fn plugin_update(
        &mut self,
        _plugin_id: PluginId,
        view_id: ViewId,
        response: Result<Value, xi_rpc::Error>,
    ) {
        if let Some(mut edit_ctx) = self.make_context(view_id) {
            edit_ctx.do_plugin_update(response);
        }
    }

    pub(crate) fn plugin_hover(
        &mut self,
        _plugin_id: PluginId,
        view_id: ViewId,
        request_id: usize,
        response: Result<Value, xi_rpc::Error>,
    ) {
        if let Some(mut edit_ctx) = self.make_context(view_id) {
            edit_ctx.do_plugin_hover(request_id, response);
        }
    }

    pub(crate) fn plugin_notification(
        &mut self,
        _ctx: &RpcCtx,
        view_id: ViewId,
        plugin_id: PluginId,
        cmd: PluginNotification,
    ) {
        if let Some(mut edit_ctx) = self.make_context(view_id) {
            edit_ctx.do_plugin_cmd(plugin_id, cmd)
        }
    }

    pub(crate) fn plugin_notification_from_host(
        &mut self,
        view_id: ViewId,
        plugin_id: PluginId,
        cmd: PluginNotification,
    ) {
        if let Some(mut edit_ctx) = self.make_context(view_id) {
            edit_ctx.do_plugin_cmd(plugin_id, cmd)
        }
    }

    pub(crate) fn plugin_request(
        &mut self,
        _ctx: &RpcCtx,
        view_id: ViewId,
        plugin_id: PluginId,
        cmd: PluginRequest,
    ) -> Result<Value, RemoteError> {
        if let Some(mut edit_ctx) = self.make_context(view_id) {
            edit_ctx.do_plugin_cmd_sync(plugin_id, cmd)
        } else {
            Err(RemoteError::custom(404, "missing view", None))
        }
    }

    pub(crate) fn plugin_request_from_host(
        &mut self,
        view_id: ViewId,
        plugin_id: PluginId,
        cmd: PluginRequest,
    ) -> Result<Value, RemoteError> {
        if let Some(mut edit_ctx) = self.make_context(view_id) {
            edit_ctx.do_plugin_cmd_sync(plugin_id, cmd)
        } else {
            Err(RemoteError::custom(404, "missing view", None))
        }
    }

    pub(crate) fn plugin_stderr(&self, plugin_name: &str, line: &str) {
        error!("plugin {} stderr: {}", plugin_name, line);
        if stderr_is_user_visible(line) {
            self.peer.alert(format!("plugin {}: {}", plugin_name, line));
        }
    }

    fn take_pending_plugin_commands(&mut self, plugin_name: &str) -> Vec<PendingPluginCommand> {
        let mut retained = Vec::with_capacity(self.pending_plugin_commands.len());
        let mut pending = Vec::new();

        for command in self.pending_plugin_commands.drain(..) {
            if command.plugin_name == plugin_name {
                pending.push(command);
            } else {
                retained.push(command);
            }
        }

        self.pending_plugin_commands = retained;
        pending
    }

    fn plugin_init_info(
        &self,
        plugin: &Plugin,
        pending_commands: &[PendingPluginCommand],
    ) -> Vec<crate::plugins::rpc::PluginBufferInfo> {
        let mut seen_buffers = HashSet::new();
        let mut init_info = Vec::new();

        for mut context in self.iter_groups() {
            if plugin.receives_updates_for(&context.language)
                && seen_buffers.insert(context.buffer_id)
            {
                init_info.push(context.plugin_info());
            }
        }

        for command in pending_commands {
            if let Some(mut context) = self.make_context(command.view_id)
                && seen_buffers.insert(context.buffer_id)
            {
                init_info.push(context.plugin_info());
            }
        }

        init_info
    }
}

fn stderr_is_user_visible(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("panic")
        || lower.contains("panicked")
        || lower.contains("error")
        || lower.contains("failed")
}

/// test helpers
impl CoreState {
    pub fn _test_open_editors(&self) -> Vec<BufferId> {
        self.editors.keys().cloned().collect()
    }

    pub fn _test_open_views(&self) -> Vec<ViewId> {
        self.views.keys().cloned().collect()
    }
}

pub mod test_helpers {
    use super::{BufferId, ViewId};

    pub fn new_view_id(id: usize) -> ViewId {
        ViewId(id)
    }

    pub fn new_buffer_id(id: usize) -> BufferId {
        BufferId(id)
    }
}

/// A multi-view aware iterator over `EventContext`s. A view which appears
/// as a sibling will not appear again as a main view.
pub struct Iter<'a, I> {
    views: I,
    seen: HashSet<ViewId>,
    inner: &'a CoreState,
}

impl<'a, I> Iterator for Iter<'a, I>
where
    I: Iterator<Item = &'a ViewId>,
{
    type Item = EventContext<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let &mut Iter { ref mut views, ref mut seen, inner } = self;
        loop {
            let next_view = match views.next() {
                None => return None,
                Some(v) if seen.contains(v) => continue,
                Some(v) => v,
            };
            let context = inner.make_context(*next_view).unwrap();
            context.siblings.iter().for_each(|sibl| {
                let _ = seen.insert(sibl.borrow().get_view_id());
            });
            return Some(context);
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Counter(Cell<usize>);

impl Counter {
    pub(crate) fn next(&self) -> usize {
        let n = self.0.get();
        self.0.set(n + 1);
        n + 1
    }
}

// these two only exist so that we can use ViewIds as idle tokens
impl From<usize> for ViewId {
    fn from(src: usize) -> ViewId {
        ViewId(src)
    }
}

impl From<ViewId> for usize {
    fn from(src: ViewId) -> usize {
        src.0
    }
}

impl fmt::Display for ViewId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "view-id-{}", self.0)
    }
}

impl Serialize for ViewId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ViewId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.trim_start_matches("view-id-")
            .parse::<usize>()
            .map(ViewId)
            .map_err(|_| de::Error::invalid_value(Unexpected::Str(&s), &"view id"))
    }
}

impl fmt::Display for BufferId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "buffer-id-{}", self.0)
    }
}

impl BufferId {
    pub fn new(val: usize) -> Self {
        BufferId(val)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use serde::Deserialize;
    use serde_json::json;
    use xi_rpc::test_utils::DummyPeer;
    use xi_rpc::{Handler, Peer, RpcCtx};

    use super::{
        CoreState, NEW_VIEW_IDLE_TOKEN, PLUGIN_RESTART_MAX_DELAY_MS,
        VERIFY_LINE_ENDINGS_IDLE_TOKEN, ViewId, stderr_is_user_visible,
    };

    #[test]
    fn begin_plugin_launch_blocks_duplicate_starts() {
        let peer = Box::new(DummyPeer);
        let mut state = CoreState::new(&peer.box_clone(), None, None);

        assert!(state.begin_plugin_launch("test-plugin"));
        assert!(!state.begin_plugin_launch("test-plugin"));
        assert!(state.launching_plugins.contains("test-plugin"));
    }

    #[test]
    fn stderr_visibility_filters_noise() {
        assert!(stderr_is_user_visible("plugin panicked at line 7"));
        assert!(stderr_is_user_visible("ERROR: broken transport"));
        assert!(stderr_is_user_visible("request failed"));
        assert!(!stderr_is_user_visible("info: warming cache"));
    }

    #[test]
    fn restart_delay_backs_off_for_repeated_crashes() {
        let peer = Box::new(DummyPeer);
        let mut state = CoreState::new(&peer.box_clone(), None, None);

        let first = state.next_restart_delay("test-plugin");
        let second = state.next_restart_delay("test-plugin");
        let third = state.next_restart_delay("test-plugin");

        assert!(first < second);
        assert!(second <= third);
        assert!(third <= Duration::from_millis(PLUGIN_RESTART_MAX_DELAY_MS));
    }

    #[test]
    fn test_deserialize_view_id() {
        let de = json!("view-id-1");
        assert_eq!(ViewId::deserialize(&de).unwrap(), ViewId(1));

        let de = json!("not-a-view-id");
        assert!(ViewId::deserialize(&de).unwrap_err().is_data());
    }

    #[test]
    fn large_file_line_ending_detection_is_deferred_after_open() {
        let peer = Box::new(DummyPeer);
        let ctx = RpcCtx::new(peer.box_clone());
        let mut core = crate::XiCore::new();
        core.handle_notification(
            &ctx,
            crate::rpc::CoreNotification::ClientStarted {
                config_dir: None,
                client_extras_dir: None,
            },
        );
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for _ in 0..10_000 {
            tmp.write_all(b"  item\r\n").unwrap();
        }
        tmp.flush().unwrap();

        let view_id_value = core.inner().do_new_view(Some(tmp.path().to_path_buf())).unwrap();
        let view_id: ViewId = serde_json::from_value(view_id_value).unwrap();
        core.inner().handle_idle(NEW_VIEW_IDLE_TOKEN);

        let buffer_id = core.inner().views.get(&view_id).unwrap().borrow().get_buffer_id();
        let initial = core.inner().config_manager.get_buffer_config(buffer_id).items.clone();
        assert!(initial.translate_tabs_to_spaces);
        assert_eq!(initial.tab_size, 2);
        assert_eq!(initial.line_ending, "\n");

        core.inner().handle_idle(VERIFY_LINE_ENDINGS_IDLE_TOKEN);

        let verified = core.inner().config_manager.get_buffer_config(buffer_id).items.clone();
        assert_eq!(verified.line_ending, "\r\n");
    }
}
